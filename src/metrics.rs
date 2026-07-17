use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    pub accepts: AtomicU64,
    pub current_clients: AtomicU64,
    pub preferred_clients: AtomicU64,
    pub home_moves_in: AtomicU64,
    pub home_moves_out: AtomicU64,
    pub packets_received: AtomicU64,
    pub packets_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub queue_dropped: AtomicU64,
    pub unknown_frames: AtomicU64,
    pub websocket_accepts: AtomicU64,
    pub stun_requests: AtomicU64,
    pub admission_rejected: AtomicU64,
    pub mesh_forwarded_in: AtomicU64,
    pub mesh_forwarded_out: AtomicU64,
}

impl Metrics {
    pub fn inc(v: &AtomicU64, n: u64) {
        v.fetch_add(n, Ordering::Relaxed);
    }
    pub fn dec(v: &AtomicU64) {
        v.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn prometheus(&self) -> String {
        let mut out = String::with_capacity(1024);
        macro_rules! m {
            ($name:literal,$field:ident,$kind:literal) => {{
                out.push_str(concat!("# TYPE ", $name, " ", $kind, "\n", $name, " "));
                out.push_str(&self.$field.load(Ordering::Relaxed).to_string());
                out.push('\n');
            }};
        }
        m!("derp_accepts_total", accepts, "counter");
        m!("derp_current_clients", current_clients, "gauge");
        m!("derp_preferred_clients", preferred_clients, "gauge");
        m!("derp_home_moves_in_total", home_moves_in, "counter");
        m!("derp_home_moves_out_total", home_moves_out, "counter");
        m!("derp_packets_received_total", packets_received, "counter");
        m!("derp_packets_sent_total", packets_sent, "counter");
        m!("derp_bytes_received_total", bytes_received, "counter");
        m!("derp_bytes_sent_total", bytes_sent, "counter");
        m!("derp_packets_dropped_total", packets_dropped, "counter");
        m!("derp_queue_dropped_total", queue_dropped, "counter");
        m!("derp_unknown_frames_total", unknown_frames, "counter");
        m!("derp_websocket_accepts_total", websocket_accepts, "counter");
        m!("derp_stun_requests_total", stun_requests, "counter");
        m!(
            "derp_admission_rejected_total",
            admission_rejected,
            "counter"
        );
        m!("derp_mesh_forwarded_in_total", mesh_forwarded_in, "counter");
        m!(
            "derp_mesh_forwarded_out_total",
            mesh_forwarded_out,
            "counter"
        );
        out
    }
}
