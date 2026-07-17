use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, split},
    net::TcpStream,
    sync::mpsc,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};
use tracing::{info, warn};
use url::Url;

use crate::{
    protocol::{self, ClientInfo, Frame},
    server::{BoxStream, DerpServer},
};

static NEXT_MESH_ID: AtomicU64 = AtomicU64::new(1);

pub fn spawn(server: Arc<DerpServer>, peers: &[String], mesh_key: Option<[u8; 32]>) {
    let Some(mesh_key) = mesh_key else {
        if !peers.is_empty() {
            warn!("--mesh-with ignored without --mesh-psk-file");
        }
        return;
    };
    for peer in peers {
        let peer = peer.clone();
        let server = server.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = run(server.clone(), &peer, mesh_key).await {
                    warn!(%peer,error=%format!("{error:#}"),"mesh connection ended");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}

async fn run(server: Arc<DerpServer>, url: &str, mesh_key: [u8; 32]) -> Result<()> {
    let url = Url::parse(url).context("invalid mesh URL")?;
    let host = url.host_str().context("mesh URL has no host")?.to_string();
    let port = url
        .port_or_known_default()
        .context("mesh URL has no port")?;
    let tcp = TcpStream::connect((host.as_str(), port)).await?;
    tcp.set_nodelay(true)?;
    let mut io: BoxStream = match url.scheme() {
        "http" => Box::new(tcp),
        "https" => {
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = ServerName::try_from(host.clone()).context("invalid TLS mesh hostname")?;
            Box::new(TlsConnector::from(Arc::new(tls)).connect(name, tcp).await?)
        }
        other => bail!("unsupported mesh URL scheme {other}"),
    };
    let path = if url.path().is_empty() {
        "/derp"
    } else {
        url.path()
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: Upgrade\r\nUpgrade: DERP\r\n\r\n"
    );
    io.write_all(request.as_bytes()).await?;
    io.flush().await?;
    read_upgrade(&mut io).await?;
    let Frame::ServerKey(remote_key) = protocol::read_frame(&mut io, 1024).await? else {
        bail!("mesh peer sent no server key")
    };
    let info = ClientInfo {
        mesh_key: Some(mesh_key),
        version: protocol::PROTOCOL_VERSION,
        can_ack_pings: true,
        is_prober: false,
    };
    let sealed = Bytes::from(
        server
            .key
            .seal_to(remote_key, &serde_json::to_vec(&info)?)?,
    );
    protocol::write_frame(
        &mut io,
        &Frame::ClientInfo {
            key: server.key.public(),
            sealed,
        },
    )
    .await?;
    io.flush().await?;
    let Frame::ServerInfo(_) = protocol::read_frame(&mut io, protocol::MAX_INFO_SIZE).await? else {
        bail!("mesh peer sent no server info")
    };
    protocol::write_frame(&mut io, &Frame::WatchConns).await?;
    io.flush().await?;
    let mesh_id = NEXT_MESH_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel(server.config.queue_depth.max(64));
    let (read, mut write) = split(io);
    let mut read = read;
    info!(%url,mesh_id,"DERP mesh connected");
    let writer = async {
        while let Some(frame) = rx.recv().await {
            protocol::write_frame(&mut write, &frame).await?;
            write.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let reader = async {
        loop {
            match protocol::read_frame(&mut read, protocol::MAX_PACKET_SIZE + 128).await? {
                Frame::PeerPresent { peer, .. } => {
                    server.relay.add_mesh_route(mesh_id, peer, tx.clone())
                }
                Frame::PeerGone { peer, .. } => server.relay.remove_mesh_route(mesh_id, peer),
                Frame::Ping(v) => {
                    let _ = tx.try_send(Frame::Pong(v));
                }
                Frame::KeepAlive | Frame::ServerInfo(_) => {}
                other => warn!(?other, "unexpected frame from mesh peer"),
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(writer);
    tokio::pin!(reader);
    let result = tokio::select! {r=&mut writer=>r,r=&mut reader=>r};
    server.relay.remove_mesh(mesh_id);
    result
}

async fn read_upgrade(io: &mut BoxStream) -> Result<()> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 32 << 10 {
            bail!("mesh HTTP response too large")
        }
        io.read_exact(&mut byte).await?;
        buf.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&buf);
    if !line.starts_with("HTTP/1.1 101 ") {
        bail!(
            "mesh upgrade failed: {}",
            line.lines().next().unwrap_or("empty response")
        )
    }
    Ok(())
}
