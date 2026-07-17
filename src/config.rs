use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Clone, Debug, Parser)]
#[command(
    name = "derper-rs",
    version,
    about = "High-performance Tailscale DERP relay"
)]
pub struct Config {
    /// TCP address for DERP and HTTP endpoints.
    #[arg(short = 'a', long, default_value = "0.0.0.0:3340")]
    pub addr: SocketAddr,
    /// UDP STUN address; set to "off" to disable.
    #[arg(long, default_value = "0.0.0.0:3478")]
    pub stun_addr: String,
    /// Persistent DERP node private-key file.
    #[arg(long, default_value = "derper.key")]
    pub private_key: PathBuf,
    /// PEM TLS certificate chain. Must be paired with --tls-key.
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    /// PEM TLS private key.
    #[arg(long)]
    pub tls_key: Option<PathBuf>,
    /// Regional mesh PSK file (64 lowercase hexadecimal characters).
    #[arg(long)]
    pub mesh_psk_file: Option<PathBuf>,
    /// Comma-separated DERP mesh peers (https://host:port/derp).
    #[arg(long, value_delimiter = ',')]
    pub mesh_with: Vec<String>,
    /// Admission controller implementing DERPAdmitClientRequest.
    #[arg(long)]
    pub verify_client_url: Option<String>,
    /// Allow clients if the admission controller is unreachable.
    #[arg(long,default_value_t=true,action=clap::ArgAction::Set)]
    pub verify_client_fail_open: bool,
    /// Bounded packet queue per client.
    #[arg(long, default_value_t = 64)]
    pub queue_depth: usize,
    /// Per-client inbound bytes/sec; zero disables rate limiting.
    #[arg(long, default_value_t = 0)]
    pub rate_limit: u64,
    /// Per-client rate-limit burst bytes.
    #[arg(long, default_value_t = 1_048_576)]
    pub rate_burst: u64,
    /// Maximum DERP TCP write duration.
    #[arg(long,default_value="2s",value_parser=parse_duration)]
    pub write_timeout: Duration,
    /// Optional static JSON object returned by /bootstrap-dns.
    #[arg(long)]
    pub bootstrap_dns_file: Option<PathBuf>,
    /// Grace period advertised in FrameRestarting during shutdown.
    #[arg(long,default_value="5s",value_parser=parse_duration)]
    pub shutdown_grace: Duration,
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let (value, mult) = if let Some(v) = value.strip_suffix("ms") {
        (v, 1_000_000)
    } else if let Some(v) = value.strip_suffix('s') {
        (v, 1_000_000_000)
    } else if let Some(v) = value.strip_suffix('m') {
        (v, 60_000_000_000)
    } else {
        return Err("duration needs ms, s, or m suffix".into());
    };
    let n: u64 = value.parse().map_err(|_| "invalid duration")?;
    Ok(Duration::from_nanos(n.saturating_mul(mult)))
}
