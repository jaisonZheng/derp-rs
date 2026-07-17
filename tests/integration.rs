use std::{path::PathBuf, sync::Arc, time::Duration};

use bytes::Bytes;
use derp_rs::{
    Config, NodeKeyPair, http, mesh,
    protocol::{self, ClientInfo, Frame, ServerInfo},
    server::DerpServer,
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

fn config(addr: std::net::SocketAddr) -> Config {
    Config {
        addr,
        stun_addr: "off".into(),
        private_key: PathBuf::from("unused-test-key"),
        tls_cert: None,
        tls_key: None,
        mesh_psk_file: None,
        mesh_with: Vec::new(),
        verify_client_url: None,
        verify_client_fail_open: true,
        queue_depth: 64,
        rate_limit: 0,
        rate_burst: 1 << 20,
        write_timeout: Duration::from_secs(2),
        bootstrap_dns_file: None,
        shutdown_grace: Duration::from_millis(1),
    }
}

async fn listen(server: Arc<DerpServer>) -> std::net::SocketAddr {
    let listener = TcpListener::bind(server.config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, remote) = listener.accept().await.unwrap();
            let server = server.clone();
            tokio::spawn(async move {
                let _ = http::serve_connection(server, Box::new(stream), remote).await;
            });
        }
    });
    addr
}

async fn handshake<S>(mut io: S) -> (NodeKeyPair, S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Frame::ServerKey(server_key) = protocol::read_frame(&mut io, 1024).await.unwrap() else {
        panic!("missing server key")
    };
    let key = NodeKeyPair::generate();
    let info = ClientInfo {
        version: protocol::PROTOCOL_VERSION,
        can_ack_pings: true,
        ..ClientInfo::default()
    };
    let sealed = key
        .seal_to(server_key, &serde_json::to_vec(&info).unwrap())
        .unwrap();
    protocol::write_frame(
        &mut io,
        &Frame::ClientInfo {
            key: key.public(),
            sealed: Bytes::from(sealed),
        },
    )
    .await
    .unwrap();
    io.flush().await.unwrap();
    let Frame::ServerInfo(sealed) = protocol::read_frame(&mut io, 1 << 20).await.unwrap() else {
        panic!("missing server info")
    };
    let clear = key.open_from(server_key, &sealed).unwrap();
    let info: ServerInfo = serde_json::from_slice(&clear).unwrap();
    assert_eq!(info.version, protocol::PROTOCOL_VERSION);
    (key, io)
}

async fn raw_client(addr: std::net::SocketAddr) -> (NodeKeyPair, TcpStream) {
    let mut io = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /derp HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: DERP\r\nDerp-Fast-Start: 1\r\n\r\n"
    );
    io.write_all(request.as_bytes()).await.unwrap();
    handshake(io).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_derp_handshake_and_ping() {
    let server = DerpServer::new(
        config("127.0.0.1:0".parse().unwrap()),
        NodeKeyPair::generate(),
        None,
    );
    let addr = listen(server).await;
    let mut request = format!("ws://{addr}/derp").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("derp"));
    let (mut ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], "derp");
    let (app, mut tunnel) = tokio::io::duplex(64 << 10);
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            tokio::select! {
                msg = ws.next() => match msg { Some(Ok(Message::Binary(data))) => tunnel.write_all(&data).await.unwrap(), _ => return },
                n = tunnel.read(&mut buf) => { let n=n.unwrap(); if n==0{return}; ws.send(Message::Binary(buf[..n].to_vec().into())).await.unwrap(); }
            }
        }
    });
    let (_, mut io) = handshake(app).await;
    protocol::write_frame(&mut io, &Frame::Ping(*b"12345678"))
        .await
        .unwrap();
    io.flush().await.unwrap();
    assert_eq!(
        protocol::read_frame(&mut io, 1024).await.unwrap(),
        Frame::Pong(*b"12345678")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regional_mesh_forwards_between_servers() {
    let mesh_key = [0xabu8; 32];
    let a = DerpServer::new(
        config("127.0.0.1:0".parse().unwrap()),
        NodeKeyPair::generate(),
        Some(mesh_key),
    );
    let a_addr = listen(a.clone()).await;
    let (a_client_key, mut a_client) = raw_client(a_addr).await;

    let b = DerpServer::new(
        config("127.0.0.1:0".parse().unwrap()),
        NodeKeyPair::generate(),
        Some(mesh_key),
    );
    let b_addr = listen(b.clone()).await;
    mesh::spawn(
        b.clone(),
        &[format!("http://{a_addr}/derp")],
        Some(mesh_key),
    );
    let (_, mut b_client) = raw_client(b_addr).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    protocol::write_frame(
        &mut b_client,
        &Frame::SendPacket {
            dst: a_client_key.public(),
            packet: Bytes::from_static(b"across-mesh"),
        },
    )
    .await
    .unwrap();
    b_client.flush().await.unwrap();
    let received = tokio::time::timeout(
        Duration::from_secs(2),
        protocol::read_frame(&mut a_client, 1024),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(received, Frame::RecvPacket { packet, .. } if packet == b"across-mesh"[..]));
}
