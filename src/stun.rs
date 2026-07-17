use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use tokio::{net::UdpSocket, sync::watch};
use tracing::{debug, info};

use crate::metrics::Metrics;

const COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

pub async fn serve(
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let socket = UdpSocket::bind(addr).await?;
    info!(address=%socket.local_addr()?,"STUN listening");
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {_ = shutdown.changed()=>{if *shutdown.borrow(){return Ok(())}},received=socket.recv_from(&mut buf)=>{let(n,source)=received?;if let Some(response)=respond(&buf[..n],source){Metrics::inc(&metrics.stun_requests,1);let _=socket.send_to(&response,source).await;debug!(%source,"STUN binding response");}}}
    }
}

pub fn respond(packet: &[u8], source: SocketAddr) -> Option<Vec<u8>> {
    if packet.len() < 20 || packet[0..2] != [0, 1] || packet[4..8] != COOKIE {
        return None;
    }
    let declared = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if packet.len() < 20 + declared {
        return None;
    }
    // Tailscale binding requests advertise software="tailnode" and end in a
    // valid RFC 5389 fingerprint. Ignore unrelated public-STUN traffic.
    let attrs = &packet[20..20 + declared];
    let mut pos = 0;
    let mut software = false;
    let mut fingerprint = None;
    while pos + 4 <= attrs.len() {
        let kind = u16::from_be_bytes([attrs[pos], attrs[pos + 1]]);
        let len = u16::from_be_bytes([attrs[pos + 2], attrs[pos + 3]]) as usize;
        if pos + 4 + len > attrs.len() {
            return None;
        }
        if kind == 0x8022 && &attrs[pos + 4..pos + 4 + len] == b"tailnode" {
            software = true;
        }
        if kind == 0x8028 && len == 4 && pos + 8 == attrs.len() {
            fingerprint = Some((
                pos,
                u32::from_be_bytes(attrs[pos + 4..pos + 8].try_into().ok()?),
            ));
        }
        pos += 4 + ((len + 3) & !3);
    }
    let (fp_pos, got) = fingerprint?;
    let absolute = 20 + fp_pos;
    let want = crc32fast::hash(&packet[..absolute]) ^ 0x5354_554e;
    if !software || got != want {
        return None;
    }
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(&[0x01, 0x01, 0, 0]);
    out.extend_from_slice(&COOKIE);
    out.extend_from_slice(&packet[8..20]);
    out.extend_from_slice(&0x0020u16.to_be_bytes());
    match source.ip() {
        IpAddr::V4(ip) => {
            out.extend_from_slice(&8u16.to_be_bytes());
            out.extend_from_slice(&[0, 1]);
            out.extend_from_slice(&(source.port() ^ 0x2112).to_be_bytes());
            for (i, b) in ip.octets().iter().enumerate() {
                out.push(*b ^ COOKIE[i]);
            }
        }
        IpAddr::V6(ip) => {
            out.extend_from_slice(&20u16.to_be_bytes());
            out.extend_from_slice(&[0, 2]);
            out.extend_from_slice(&(source.port() ^ 0x2112).to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&COOKIE);
            mask[4..].copy_from_slice(&packet[8..20]);
            for (i, b) in ip.octets().iter().enumerate() {
                out.push(*b ^ mask[i]);
            }
        }
    }
    let attr_len = out.len() - 20;
    out[2..4].copy_from_slice(&(attr_len as u16).to_be_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_tailscale() {
        assert!(respond(&[0; 20], "127.0.0.1:1".parse().unwrap()).is_none());
    }
    #[test]
    fn responds_to_official_vector() {
        let tx = *b"hello world!";
        let mut p = vec![0, 1, 0, 20, 0x21, 0x12, 0xa4, 0x42];
        p.extend_from_slice(&tx);
        p.extend_from_slice(&[0x80, 0x22, 0, 8]);
        p.extend_from_slice(b"tailnode");
        let fp = crc32fast::hash(&p) ^ 0x5354554e;
        p.extend_from_slice(&[0x80, 0x28, 0, 4]);
        p.extend_from_slice(&fp.to_be_bytes());
        let r = respond(&p, "192.0.2.1:1234".parse().unwrap()).unwrap();
        assert_eq!(&r[..2], &[1, 1]);
        assert_eq!(&r[8..20], &tx);
    }
}
