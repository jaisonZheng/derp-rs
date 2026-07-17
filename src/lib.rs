//! A high-performance, wire-compatible implementation of Tailscale DERP.

pub mod config;
pub mod crypto;
pub mod http;
pub mod mesh;
pub mod metrics;
pub mod protocol;
pub mod relay;
pub mod server;
pub mod stun;

pub use config::Config;
pub use crypto::{NodeKey, NodeKeyPair};
pub use protocol::{Frame, FrameType};
pub use relay::Relay;
