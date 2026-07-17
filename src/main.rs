use std::{fs, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::Parser;
use derp_rs::{
    Config, NodeKeyPair, http, mesh,
    server::{BoxStream, DerpServer},
    stun,
};
use tokio::{net::TcpListener, sync::watch};
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("derp_rs=info")),
        )
        .init();
    let config = Config::parse();
    let key = NodeKeyPair::load_or_create(&config.private_key).context("load DERP private key")?;
    let mesh_key = load_mesh_key(config.mesh_psk_file.as_deref())?;
    let tls = load_tls(&config)?;
    let server = DerpServer::new(config.clone(), key, mesh_key);
    mesh::spawn(server.clone(), &config.mesh_with, mesh_key);
    info!(public_key=%server.key.public(),address=%config.addr,tls=tls.is_some(),"derper-rs starting");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    if let Some(addr) = parse_stun_addr(&config.stun_addr)? {
        let metrics = server.relay.metrics.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = stun::serve(addr, metrics, rx).await {
                error!(error=%e,"STUN server stopped")
            }
        });
    }
    let listener = TcpListener::bind(config.addr)
        .await
        .context("bind DERP listener")?;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            signal=&mut shutdown=>{signal?;info!("shutdown requested");break;}
            accepted=listener.accept()=>{
                let(stream,remote)=accepted?;stream.set_nodelay(true)?;crate_metrics_accept(&server);
                let server=server.clone();let tls=tls.clone();
                tokio::spawn(async move{
                    let io: Result<BoxStream> = async {
                        if let Some(acceptor) = tls {
                            Ok::<BoxStream, anyhow::Error>(Box::new(
                                acceptor.accept(stream).await.context("TLS handshake")?,
                            ))
                        } else {
                            Ok::<BoxStream, anyhow::Error>(Box::new(stream))
                        }
                    }.await;
                    match io {Ok(io)=>if let Err(e)=http::serve_connection(server,io,remote).await{warn!(%remote,error=%format!("{e:#}"),"connection ended")},Err(e)=>warn!(%remote,error=%format!("{e:#}"),"connection setup failed")}
                });
            }
        }
    }
    let ms = config.shutdown_grace.as_millis().min(u32::MAX as u128) as u32;
    server.relay.broadcast_restart(ms, ms.saturating_mul(3));
    let _ = shutdown_tx.send(true);
    tokio::time::sleep(config.shutdown_grace).await;
    info!("derper-rs stopped");
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn crate_metrics_accept(server: &DerpServer) {
    derp_rs::metrics::Metrics::inc(&server.relay.metrics.accepts, 1);
}

fn parse_stun_addr(value: &str) -> Result<Option<std::net::SocketAddr>> {
    if value.eq_ignore_ascii_case("off") {
        Ok(None)
    } else {
        Ok(Some(value.parse().context("invalid --stun-addr")?))
    }
}

fn load_mesh_key(path: Option<&std::path::Path>) -> Result<Option<[u8; 32]>> {
    let Some(path) = path else { return Ok(None) };
    let raw =
        hex::decode(fs::read_to_string(path)?.trim()).context("mesh PSK must be hexadecimal")?;
    let key: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("mesh PSK must be exactly 32 bytes"))?;
    Ok(Some(key))
}

fn load_tls(config: &Config) -> Result<Option<TlsAcceptor>> {
    let (cert_path, key_path) = match (&config.tls_cert, &config.tls_key) {
        (None, None) => return Ok(None),
        (Some(c), Some(k)) => (c, k),
        _ => bail!("--tls-cert and --tls-key must be used together"),
    };
    let mut cert_reader = std::io::BufReader::new(fs::File::open(cert_path)?);
    let certs =
        rustls_pemfile::certs(&mut cert_reader).collect::<std::result::Result<Vec<_>, _>>()?;
    let mut key_reader = std::io::BufReader::new(fs::File::open(key_path)?);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .context("TLS key file contained no private key")?;
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Some(TlsAcceptor::from(Arc::new(tls))))
}
