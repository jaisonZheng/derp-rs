use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, split},
    time::{interval, timeout},
};
use tracing::{debug, warn};

use crate::{
    config::Config,
    crypto::{NodeKey, NodeKeyPair},
    protocol::{self, ClientInfo, Frame, ServerInfo},
    relay::{Relay, SessionHandle},
};

pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}
pub type BoxStream = Box<dyn AsyncStream>;

pub struct DerpServer {
    pub config: Arc<Config>,
    pub key: Arc<NodeKeyPair>,
    pub relay: Arc<Relay>,
    http: reqwest::Client,
}

impl DerpServer {
    pub fn new(config: Config, key: NodeKeyPair, mesh_key: Option<[u8; 32]>) -> Arc<Self> {
        Arc::new(Self {
            relay: Relay::new(config.queue_depth, mesh_key),
            config: Arc::new(config),
            key: Arc::new(key),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("HTTP client"),
        })
    }

    pub async fn serve_stream(
        self: Arc<Self>,
        mut stream: BoxStream,
        remote: SocketAddr,
        not_ideal: bool,
    ) -> Result<()> {
        timeout(
            Duration::from_secs(10),
            protocol::write_frame(&mut stream, &Frame::ServerKey(self.key.public())),
        )
        .await
        .context("server greeting timeout")??;
        stream.flush().await?;
        let frame = timeout(
            Duration::from_secs(10),
            protocol::read_frame(
                &mut stream,
                protocol::MAX_CLIENT_INFO_SIZE + protocol::KEY_LEN + 64,
            ),
        )
        .await
        .context("client info timeout")??;
        let Frame::ClientInfo {
            key: client_key,
            sealed,
        } = frame
        else {
            bail!("first client frame was not ClientInfo")
        };
        let plaintext = self
            .key
            .open_from(client_key, &sealed)
            .context("invalid client identity proof")?;
        if plaintext.len() > protocol::MAX_CLIENT_INFO_SIZE {
            bail!("client info JSON too large")
        }
        let info: ClientInfo =
            serde_json::from_slice(&plaintext).context("invalid client info JSON")?;
        let can_mesh = self.relay.is_mesh_key(info.mesh_key);
        if !can_mesh {
            self.admit(client_key, remote.ip()).await?;
        }
        let mut registered = self.relay.register(
            client_key,
            remote,
            can_mesh,
            info.can_ack_pings,
            info.is_prober,
            not_ideal,
        );
        let handle = registered.handle.clone();
        let result = async {
            let server_info = ServerInfo {
                version: protocol::PROTOCOL_VERSION,
                token_bucket_bytes_per_second: (self.config.rate_limit > 0)
                    .then_some(self.config.rate_limit),
                token_bucket_bytes_burst: (self.config.rate_limit > 0)
                    .then_some(self.config.rate_burst),
            };
            let sealed = Bytes::from(
                self.key
                    .seal_to(client_key, &serde_json::to_vec(&server_info)?)?,
            );
            protocol::write_frame(&mut stream, &Frame::ServerInfo(sealed)).await?;
            stream.flush().await?;
            let (read, write) = split(stream);
            let writer = self.writer(write, &handle, &mut registered.rx);
            let reader = self.reader(read, &handle);
            tokio::pin!(writer);
            tokio::pin!(reader);
            tokio::select! {r=&mut reader=>r,w=&mut writer=>w}
        }
        .await;
        self.relay.unregister(&handle);
        debug!(peer=%client_key, %remote, "DERP client disconnected");
        match result {
            Err(error) if is_disconnect(&error) => Ok(()),
            other => other,
        }
    }

    async fn reader<R: AsyncRead + Unpin>(
        &self,
        mut read: R,
        handle: &Arc<SessionHandle>,
    ) -> Result<()> {
        let mut limiter = TokenBucket::new(self.config.rate_limit, self.config.rate_burst);
        loop {
            let frame = protocol::read_frame(
                &mut read,
                protocol::MAX_PACKET_SIZE + protocol::KEY_LEN * 2 + 1024,
            )
            .await?;
            if !handle.can_mesh {
                limiter.consume(frame_cost(&frame)).await;
            }
            self.relay.note_activity(handle);
            match frame {
                Frame::SendPacket { dst, packet } => {
                    self.relay.route(handle, handle.key, dst, packet, false)
                }
                Frame::ForwardPacket { src, dst, packet } if handle.can_mesh => {
                    self.relay.route(handle, src, dst, packet, true)
                }
                Frame::ForwardPacket { .. } | Frame::WatchConns | Frame::ClosePeer(_)
                    if !handle.can_mesh =>
                {
                    bail!("insufficient mesh permissions")
                }
                Frame::WatchConns => {
                    if !self.relay.watch(handle) {
                        bail!("insufficient mesh permissions");
                    }
                }
                Frame::ClosePeer(peer) => {
                    self.relay.close_peer(handle, peer);
                }
                Frame::Ping(value) => {
                    handle.send(Frame::Pong(value), &self.relay.metrics);
                }
                Frame::NotePreferred(preferred) => {
                    self.relay.note_preferred(handle, preferred);
                }
                Frame::Pong(_) | Frame::KeepAlive => {}
                Frame::Unknown { .. } => {
                    crate::metrics::Metrics::inc(&self.relay.metrics.unknown_frames, 1)
                }
                _ => bail!("unexpected client frame type"),
            }
        }
    }

    async fn writer<W: AsyncWrite + Unpin>(
        &self,
        mut write: W,
        handle: &SessionHandle,
        rx: &mut tokio::sync::mpsc::Receiver<Frame>,
    ) -> Result<()> {
        const SHRINK_AFTER: Duration = Duration::from_secs(15);
        let mut batch = BytesMut::new();
        let mut last_data_write = Instant::now();
        let mut keepalive = interval(Duration::from_secs(60));
        keepalive.tick().await;
        let mut close_check = interval(Duration::from_millis(250));
        close_check.tick().await;
        let mut shrink_check = interval(SHRINK_AFTER);
        shrink_check.tick().await;
        loop {
            tokio::select! {
                biased;
                frame=rx.recv()=>{
                    let Some(frame)=frame else{return Ok(())};
                    self.write_batch(&mut write,frame,rx,&mut batch).await?;
                    last_data_write=Instant::now();
                }
                _=keepalive.tick()=>{
                    let frame = if handle.can_ack_pings {
                        Frame::Ping(rand_core::OsRng.next_u64().to_be_bytes())
                    } else {
                        Frame::KeepAlive
                    };
                    self.write_control(&mut write,&frame,&mut batch).await?;
                }
                _=close_check.tick()=>{if handle.is_closed(){return Ok(());}}
                _=shrink_check.tick()=>{
                    if batch.capacity() > (8 << 10) && last_data_write.elapsed() >= SHRINK_AFTER {
                        batch=BytesMut::new();
                    }
                }
            }
        }
    }

    async fn write_batch<W: AsyncWrite + Unpin>(
        &self,
        write: &mut W,
        first: Frame,
        rx: &mut tokio::sync::mpsc::Receiver<Frame>,
        batch: &mut BytesMut,
    ) -> Result<()> {
        const MAX_BATCH_BYTES: usize = 64 << 10;
        let duration = self.config.write_timeout;
        timeout(duration, async {
            batch.clear();
            first.encode_into(batch)?;
            for _ in 0..63 {
                if batch.len() >= MAX_BATCH_BYTES {
                    break;
                }
                match rx.try_recv() {
                    Ok(frame) => frame.encode_into(batch)?,
                    Err(_) => break,
                }
            }
            write.write_all(batch).await?;
            write.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("DERP write timeout")??;
        Ok(())
    }

    async fn write_control<W: AsyncWrite + Unpin>(
        &self,
        write: &mut W,
        frame: &Frame,
        batch: &mut BytesMut,
    ) -> Result<()> {
        batch.clear();
        frame.encode_into(batch)?;
        timeout(self.config.write_timeout, async {
            write.write_all(batch).await?;
            write.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("DERP control write timeout")??;
        Ok(())
    }

    async fn admit(&self, key: NodeKey, source: IpAddr) -> Result<()> {
        let Some(url) = &self.config.verify_client_url else {
            return Ok(());
        };
        let request = AdmitRequest {
            node_public: key.to_string(),
            source: source.to_string(),
        };
        let result = self.http.post(url).json(&request).send().await;
        let response = match result {
            Ok(v) => v,
            Err(e) if self.config.verify_client_fail_open => {
                warn!(error=%e,"admission controller unavailable; failing open");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if !response.status().is_success() {
            bail!("admission controller returned {}", response.status())
        }
        let body: AdmitResponse = response.json().await?;
        if !body.allow {
            crate::metrics::Metrics::inc(&self.relay.metrics.admission_rejected, 1);
            bail!("client rejected by admission controller")
        }
        Ok(())
    }
}

fn is_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            )
        })
    })
}

#[derive(Serialize)]
struct AdmitRequest {
    #[serde(rename = "NodePublic")]
    node_public: String,
    #[serde(rename = "Source")]
    source: String,
}
#[derive(Deserialize)]
struct AdmitResponse {
    #[serde(rename = "Allow", alias = "allow", default)]
    allow: bool,
}

fn frame_cost(frame: &Frame) -> usize {
    match frame {
        Frame::SendPacket { packet, .. } | Frame::ForwardPacket { packet, .. } => packet.len() + 64,
        _ => 64,
    }
}
struct TokenBucket {
    rate: u64,
    burst: f64,
    tokens: f64,
    last: Instant,
}
impl TokenBucket {
    fn new(rate: u64, burst: u64) -> Self {
        Self {
            rate,
            burst: burst as f64,
            tokens: burst as f64,
            last: Instant::now(),
        }
    }
    async fn consume(&mut self, n: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.last).as_secs_f64() * self.rate as f64)
            .min(self.burst);
        self.last = now;
        let n = n as f64;
        if self.tokens >= n {
            self.tokens -= n;
            return;
        }
        let wait = (n - self.tokens) / self.rate as f64;
        self.tokens = 0.0;
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        self.last = Instant::now();
    }
}
