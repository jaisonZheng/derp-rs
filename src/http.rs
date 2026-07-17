use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{Context as _, Result, bail};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};
use tracing::debug;

use crate::{
    metrics::Metrics,
    server::{BoxStream, DerpServer},
};

const MAX_HTTP_HEADER: usize = 32 << 10;

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

pub async fn serve_connection(
    server: Arc<DerpServer>,
    mut io: BoxStream,
    remote: SocketAddr,
) -> Result<()> {
    let (request, leftover) = read_request(&mut io).await?;
    debug!(method=%request.method,path=%request.path,%remote,"HTTP request");
    if request.path == "/derp" {
        let upgrade = request
            .headers
            .get("upgrade")
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        let websocket = upgrade == "websocket"
            && request
                .headers
                .get("sec-websocket-protocol")
                .is_some_and(|v| v.split(',').any(|s| s.trim() == "derp"));
        if websocket {
            return websocket_derp(server, io, leftover, remote, &request).await;
        }
        if upgrade != "derp" && upgrade != "websocket" {
            write_response(
                &mut io,
                426,
                "Upgrade Required",
                &[("Content-Type", "text/plain")],
                b"DERP requires connection upgrade\n",
            )
            .await?;
            return Ok(());
        }
        let fast = request
            .headers
            .get("derp-fast-start")
            .is_some_and(|v| v == "1");
        if !fast {
            let header = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: DERP\r\nConnection: Upgrade\r\nDerp-Version: 2\r\nDerp-Public-Key: {}\r\n\r\n",
                server.key.public().to_hex()
            );
            io.write_all(header.as_bytes()).await?;
            io.flush().await?;
        }
        let not_ideal = request.headers.contains_key("ideal-node");
        return server
            .serve_stream(Box::new(PrefixedIo::new(leftover, io)), remote, not_ideal)
            .await;
    }
    match request.path.split('?').next().unwrap_or("") {
        "/derp/probe"|"/derp/latency-check" if request.method=="GET"||request.method=="HEAD"=>write_response(&mut io,200,"OK",&[("Access-Control-Allow-Origin","*")],b"").await?,
        "/derp/probe"|"/derp/latency-check"=>write_response(&mut io,405,"Method Not Allowed",&[],b"bogus probe method\n").await?,
        "/generate_204"=>{let mut headers=vec![("Cache-Control","no-cache, no-store, must-revalidate, no-transform, max-age=0")];let response; if let Some(challenge)=request.headers.get("x-tailscale-challenge").filter(|s|s.len()<=64&&s.chars().all(valid_challenge)){response=format!("response {challenge}");headers.push(("X-Tailscale-Response",&response));}write_response(&mut io,204,"No Content",&headers,b"").await?;},
        "/bootstrap-dns"=>{let body=if let Some(path)=&server.config.bootstrap_dns_file{tokio::fs::read(path).await.unwrap_or_else(|_|b"{}\n".to_vec())}else{b"{}\n".to_vec()};write_response(&mut io,200,"OK",&[("Content-Type","application/json"),("Access-Control-Allow-Origin","*")],&body).await?;},
        "/metrics"|"/debug/vars"=>{let body=server.relay.metrics.prometheus();write_response(&mut io,200,"OK",&[("Content-Type","text/plain; version=0.0.4")],body.as_bytes()).await?;},
        "/debug/check"=>{let body=format!("derp-rs consistency check okay; {} sessions, {} peers\n",server.relay.client_count(),server.relay.public_peer_count());write_response(&mut io,200,"OK",&[("Content-Type","text/plain")],body.as_bytes()).await?;},
        "/robots.txt"=>write_response(&mut io,200,"OK",&[("Content-Type","text/plain")],b"User-agent: *\nDisallow: /\n").await?,
        "/"=>write_response(&mut io,200,"OK",&[("Content-Type","text/html; charset=utf-8")],b"<!doctype html><html><body><h1>DERP</h1><p>High-performance Tailscale DERP relay implemented in Rust.</p></body></html>\n").await?,
        _=>write_response(&mut io,404,"Not Found",&[("Content-Type","text/plain")],b"not found\n").await?,
    }
    Ok(())
}

async fn websocket_derp(
    server: Arc<DerpServer>,
    mut io: BoxStream,
    leftover: Vec<u8>,
    remote: SocketAddr,
    request: &Request,
) -> Result<()> {
    let key = request
        .headers
        .get("sec-websocket-key")
        .context("missing WebSocket key")?;
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64::engine::general_purpose::STANDARD.encode(sha.finalize());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: derp\r\n\r\n"
    );
    io.write_all(response.as_bytes()).await?;
    io.flush().await?;
    Metrics::inc(&server.relay.metrics.websocket_accepts, 1);
    let prefixed = PrefixedIo::new(leftover, io);
    let ws = WebSocketStream::from_raw_socket(prefixed, Role::Server, None).await;
    let (session_io, bridge_io) = tokio::io::duplex(256 << 10);
    let serve = server.clone().serve_stream(
        Box::new(session_io),
        remote,
        request.headers.contains_key("ideal-node"),
    );
    let bridge = bridge_websocket(ws, bridge_io);
    tokio::pin!(serve);
    tokio::pin!(bridge);
    tokio::select! {r=&mut serve=>r,r=&mut bridge=>r}
}

async fn bridge_websocket<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    mut stream: tokio::io::DuplexStream,
) -> Result<()> {
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        tokio::select! {
            message=ws.next()=>match message {Some(Ok(Message::Binary(data)))=>stream.write_all(&data).await?,Some(Ok(Message::Ping(data)))=>ws.send(Message::Pong(data)).await?,Some(Ok(Message::Close(_)))|None=>return Ok(()),Some(Ok(_))=>{},Some(Err(e))=>return Err(e.into())},
            n=stream.read(&mut buffer)=>{let n=n?;if n==0{return Ok(());}ws.send(Message::Binary(buffer[..n].to_vec().into())).await?;}
        }
    }
}

async fn read_request(io: &mut BoxStream) -> Result<(Request, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(4096);
    let header_end;
    loop {
        if let Some(pos) = buffer.windows(4).position(|v| v == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buffer.len() >= MAX_HTTP_HEADER {
            bail!("HTTP header too large")
        }
        let mut chunk = [0u8; 4096];
        let n = io.read(&mut chunk).await?;
        if n == 0 {
            bail!("EOF before HTTP request")
        };
        buffer.extend_from_slice(&chunk[..n]);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    parsed
        .parse(&buffer[..header_end])?
        .is_complete()
        .then_some(())
        .context("incomplete HTTP request")?;
    let method = parsed.method.context("missing method")?.to_string();
    let path = parsed.path.context("missing path")?.to_string();
    let headers = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).trim().to_string(),
            )
        })
        .collect();
    Ok((
        Request {
            method,
            path,
            headers,
        },
        buffer[header_end..].to_vec(),
    ))
}

async fn write_response<W: AsyncWrite + Unpin>(
    io: &mut W,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        response.push_str(k);
        response.push_str(": ");
        response.push_str(v);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    io.write_all(response.as_bytes()).await?;
    if !body.is_empty() {
        io.write_all(body).await?;
    }
    io.flush().await
}
fn valid_challenge(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':')
}

struct PrefixedIo<S> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: S,
}
impl<S> PrefixedIo<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: std::io::Cursor::new(prefix),
            inner,
        }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for PrefixedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let pos = self.prefix.position() as usize;
        if pos < self.prefix.get_ref().len() {
            let data = &self.prefix.get_ref()[pos..];
            let n = data.len().min(buf.remaining());
            buf.put_slice(&data[..n]);
            self.prefix.set_position((pos + n) as u64);
            Poll::Ready(Ok(()))
        } else {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
