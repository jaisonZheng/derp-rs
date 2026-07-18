use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::crypto::NodeKey;

pub const MAGIC: &[u8; 8] = b"DERP\xF0\x9F\x94\x91";
pub const PROTOCOL_VERSION: u32 = 2;
pub const FRAME_HEADER_LEN: usize = 5;
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const MAX_PACKET_SIZE: usize = 64 << 10;
pub const MAX_INFO_SIZE: usize = 1 << 20;
pub const MAX_CLIENT_INFO_SIZE: usize = 256 << 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FrameType {
    ServerKey = 0x01,
    ClientInfo = 0x02,
    ServerInfo = 0x03,
    SendPacket = 0x04,
    RecvPacket = 0x05,
    KeepAlive = 0x06,
    NotePreferred = 0x07,
    PeerGone = 0x08,
    PeerPresent = 0x09,
    ForwardPacket = 0x0a,
    WatchConns = 0x10,
    ClosePeer = 0x11,
    Ping = 0x12,
    Pong = 0x13,
    Health = 0x14,
    Restarting = 0x15,
}

impl FrameType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x01 => Self::ServerKey,
            0x02 => Self::ClientInfo,
            0x03 => Self::ServerInfo,
            0x04 => Self::SendPacket,
            0x05 => Self::RecvPacket,
            0x06 => Self::KeepAlive,
            0x07 => Self::NotePreferred,
            0x08 => Self::PeerGone,
            0x09 => Self::PeerPresent,
            0x0a => Self::ForwardPacket,
            0x10 => Self::WatchConns,
            0x11 => Self::ClosePeer,
            0x12 => Self::Ping,
            0x13 => Self::Pong,
            0x14 => Self::Health,
            0x15 => Self::Restarting,
            _ => return None,
        })
    }
}

pub mod peer_gone_reason {
    pub const DISCONNECTED: u8 = 0x00;
    pub const NOT_HERE: u8 = 0x01;
}

pub mod peer_present_flags {
    pub const REGULAR: u8 = 1 << 0;
    pub const MESH: u8 = 1 << 1;
    pub const PROBER: u8 = 1 << 2;
    pub const NOT_IDEAL: u8 = 1 << 3;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    ServerKey(NodeKey),
    ClientInfo {
        key: NodeKey,
        sealed: Bytes,
    },
    ServerInfo(Bytes),
    SendPacket {
        dst: NodeKey,
        packet: Bytes,
    },
    ForwardPacket {
        src: NodeKey,
        dst: NodeKey,
        packet: Bytes,
    },
    RecvPacket {
        src: NodeKey,
        packet: Bytes,
    },
    KeepAlive,
    NotePreferred(bool),
    PeerGone {
        peer: NodeKey,
        reason: u8,
    },
    PeerPresent {
        peer: NodeKey,
        endpoint: Option<SocketAddr>,
        flags: Option<u8>,
        extra: Bytes,
    },
    WatchConns,
    ClosePeer(NodeKey),
    Ping([u8; 8]),
    Pong([u8; 8]),
    Health(Bytes),
    Restarting {
        reconnect_in_ms: u32,
        try_for_ms: u32,
    },
    Unknown {
        kind: u8,
        payload: Bytes,
    },
}

impl Frame {
    pub fn kind(&self) -> u8 {
        match self {
            Self::ServerKey(_) => FrameType::ServerKey as u8,
            Self::ClientInfo { .. } => FrameType::ClientInfo as u8,
            Self::ServerInfo(_) => FrameType::ServerInfo as u8,
            Self::SendPacket { .. } => FrameType::SendPacket as u8,
            Self::ForwardPacket { .. } => FrameType::ForwardPacket as u8,
            Self::RecvPacket { .. } => FrameType::RecvPacket as u8,
            Self::KeepAlive => FrameType::KeepAlive as u8,
            Self::NotePreferred(_) => FrameType::NotePreferred as u8,
            Self::PeerGone { .. } => FrameType::PeerGone as u8,
            Self::PeerPresent { .. } => FrameType::PeerPresent as u8,
            Self::WatchConns => FrameType::WatchConns as u8,
            Self::ClosePeer(_) => FrameType::ClosePeer as u8,
            Self::Ping(_) => FrameType::Ping as u8,
            Self::Pong(_) => FrameType::Pong as u8,
            Self::Health(_) => FrameType::Health as u8,
            Self::Restarting { .. } => FrameType::Restarting as u8,
            Self::Unknown { kind, .. } => *kind,
        }
    }

    pub fn encode(&self) -> Result<Bytes, ProtocolError> {
        let mut out = BytesMut::new();
        self.encode_into(&mut out)?;
        Ok(out.freeze())
    }

    /// Appends an encoded frame directly to an existing batch buffer.
    ///
    /// The server writer uses this to avoid a temporary payload allocation
    /// and an extra payload copy for every forwarded packet.
    pub fn encode_into(&self, out: &mut BytesMut) -> Result<(), ProtocolError> {
        let frame_start = out.len();
        out.put_u8(self.kind());
        out.put_u32(0);
        let payload_start = out.len();
        let encoded = (|| {
            match self {
                Self::ServerKey(key) => {
                    out.extend_from_slice(MAGIC);
                    out.extend_from_slice(key.as_bytes());
                }
                Self::ClientInfo { key, sealed } => {
                    out.extend_from_slice(key.as_bytes());
                    out.extend_from_slice(sealed);
                }
                Self::ServerInfo(sealed) | Self::Health(sealed) => out.extend_from_slice(sealed),
                Self::SendPacket { dst, packet } => {
                    validate_packet(packet)?;
                    out.extend_from_slice(dst.as_bytes());
                    out.extend_from_slice(packet);
                }
                Self::ForwardPacket { src, dst, packet } => {
                    validate_packet(packet)?;
                    out.extend_from_slice(src.as_bytes());
                    out.extend_from_slice(dst.as_bytes());
                    out.extend_from_slice(packet);
                }
                Self::RecvPacket { src, packet } => {
                    validate_packet(packet)?;
                    out.extend_from_slice(src.as_bytes());
                    out.extend_from_slice(packet);
                }
                Self::KeepAlive | Self::WatchConns => {}
                Self::NotePreferred(value) => out.put_u8(u8::from(*value)),
                Self::PeerGone { peer, reason } => {
                    out.extend_from_slice(peer.as_bytes());
                    out.put_u8(*reason);
                }
                Self::PeerPresent {
                    peer,
                    endpoint,
                    flags,
                    extra,
                } => {
                    out.extend_from_slice(peer.as_bytes());
                    if let Some(endpoint) = endpoint {
                        let ip = match endpoint.ip() {
                            IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                            IpAddr::V6(v6) => v6,
                        };
                        out.extend_from_slice(&ip.octets());
                        out.put_u16(endpoint.port());
                        if let Some(flags) = flags {
                            out.put_u8(*flags);
                        }
                        out.extend_from_slice(extra);
                    }
                }
                Self::ClosePeer(peer) => out.extend_from_slice(peer.as_bytes()),
                Self::Ping(value) | Self::Pong(value) => out.extend_from_slice(value),
                Self::Restarting {
                    reconnect_in_ms,
                    try_for_ms,
                } => {
                    out.put_u32(*reconnect_in_ms);
                    out.put_u32(*try_for_ms);
                }
                Self::Unknown { payload, .. } => out.extend_from_slice(payload),
            }
            let payload_len = out.len() - payload_start;
            let payload_len = u32::try_from(payload_len)
                .map_err(|_| ProtocolError::FrameTooLarge(payload_len))?;
            out[frame_start + 1..frame_start + FRAME_HEADER_LEN]
                .copy_from_slice(&payload_len.to_be_bytes());
            Ok(())
        })();
        if encoded.is_err() {
            out.truncate(frame_start);
        }
        encoded
    }

    pub fn decode(kind: u8, payload: Bytes) -> Result<Self, ProtocolError> {
        let Some(frame_type) = FrameType::from_u8(kind) else {
            return Ok(Self::Unknown { kind, payload });
        };
        Ok(match frame_type {
            FrameType::ServerKey => {
                if payload.len() < MAGIC.len() + KEY_LEN || &payload[..MAGIC.len()] != MAGIC {
                    return Err(ProtocolError::Invalid("bad server greeting"));
                }
                Self::ServerKey(NodeKey::from_slice(
                    &payload[MAGIC.len()..MAGIC.len() + KEY_LEN],
                )?)
            }
            FrameType::ClientInfo => {
                require_len(&payload, KEY_LEN + NONCE_LEN, "short client info")?;
                Self::ClientInfo {
                    key: NodeKey::from_slice(&payload[..KEY_LEN])?,
                    sealed: payload.slice(KEY_LEN..),
                }
            }
            FrameType::ServerInfo => {
                require_len(&payload, NONCE_LEN, "short server info")?;
                Self::ServerInfo(payload)
            }
            FrameType::SendPacket => {
                require_len(&payload, KEY_LEN, "short send packet")?;
                let packet = payload.slice(KEY_LEN..);
                validate_packet(&packet)?;
                Self::SendPacket {
                    dst: NodeKey::from_slice(&payload[..KEY_LEN])?,
                    packet,
                }
            }
            FrameType::ForwardPacket => {
                require_len(&payload, KEY_LEN * 2, "short forward packet")?;
                let packet = payload.slice(KEY_LEN * 2..);
                validate_packet(&packet)?;
                Self::ForwardPacket {
                    src: NodeKey::from_slice(&payload[..KEY_LEN])?,
                    dst: NodeKey::from_slice(&payload[KEY_LEN..KEY_LEN * 2])?,
                    packet,
                }
            }
            FrameType::RecvPacket => {
                require_len(&payload, KEY_LEN, "short recv packet")?;
                let packet = payload.slice(KEY_LEN..);
                validate_packet(&packet)?;
                Self::RecvPacket {
                    src: NodeKey::from_slice(&payload[..KEY_LEN])?,
                    packet,
                }
            }
            FrameType::KeepAlive => {
                require_exact(&payload, 0, "keepalive")?;
                Self::KeepAlive
            }
            FrameType::NotePreferred => {
                require_exact(&payload, 1, "note preferred")?;
                Self::NotePreferred(payload[0] != 0)
            }
            FrameType::PeerGone => {
                require_len(&payload, KEY_LEN, "short peer gone")?;
                Self::PeerGone {
                    peer: NodeKey::from_slice(&payload[..KEY_LEN])?,
                    reason: payload
                        .get(KEY_LEN)
                        .copied()
                        .unwrap_or(peer_gone_reason::DISCONNECTED),
                }
            }
            FrameType::PeerPresent => {
                require_len(&payload, KEY_LEN, "short peer present")?;
                let peer = NodeKey::from_slice(&payload[..KEY_LEN])?;
                let endpoint = if payload.len() >= KEY_LEN + 18 {
                    let mut ip = [0; 16];
                    ip.copy_from_slice(&payload[KEY_LEN..KEY_LEN + 16]);
                    Some(SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::from(ip)),
                        u16::from_be_bytes([payload[KEY_LEN + 16], payload[KEY_LEN + 17]]),
                    ))
                } else {
                    None
                };
                let flags = payload.get(KEY_LEN + 18).copied();
                let extra = if payload.len() > KEY_LEN + 19 {
                    payload.slice(KEY_LEN + 19..)
                } else {
                    Bytes::new()
                };
                Self::PeerPresent {
                    peer,
                    endpoint,
                    flags,
                    extra,
                }
            }
            FrameType::WatchConns => {
                require_exact(&payload, 0, "watch conns")?;
                Self::WatchConns
            }
            FrameType::ClosePeer => {
                require_exact(&payload, KEY_LEN, "close peer")?;
                Self::ClosePeer(NodeKey::from_slice(&payload)?)
            }
            FrameType::Ping | FrameType::Pong => {
                require_len(&payload, 8, "short ping/pong")?;
                if payload.len() > 1000 {
                    return Err(ProtocolError::Invalid("oversized ping/pong"));
                }
                let mut v = [0; 8];
                v.copy_from_slice(&payload[..8]);
                if frame_type == FrameType::Ping {
                    Self::Ping(v)
                } else {
                    Self::Pong(v)
                }
            }
            FrameType::Health => Self::Health(payload),
            FrameType::Restarting => {
                require_len(&payload, 8, "short restarting")?;
                Self::Restarting {
                    reconnect_in_ms: u32::from_be_bytes(payload[..4].try_into().unwrap()),
                    try_for_ms: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
                }
            }
        })
    }
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Frame, ProtocolError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    reader.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    if len > max {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut payload = BytesMut::zeroed(len);
    reader.read_exact(&mut payload).await?;
    Frame::decode(header[0], payload.freeze())
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), ProtocolError> {
    writer.write_all(&frame.encode()?).await?;
    Ok(())
}

fn validate_packet(packet: &[u8]) -> Result<(), ProtocolError> {
    if packet.len() > MAX_PACKET_SIZE {
        Err(ProtocolError::PacketTooLarge(packet.len()))
    } else {
        Ok(())
    }
}
fn require_len(payload: &[u8], len: usize, why: &'static str) -> Result<(), ProtocolError> {
    if payload.len() < len {
        Err(ProtocolError::Invalid(why))
    } else {
        Ok(())
    }
}
fn require_exact(payload: &[u8], len: usize, why: &'static str) -> Result<(), ProtocolError> {
    if payload.len() != len {
        Err(ProtocolError::Invalid(why))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(
        rename = "meshKey",
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_hex_key"
    )]
    pub mesh_key: Option<[u8; 32]>,
    #[serde(default)]
    pub version: u32,
    #[serde(rename = "CanAckPings", default)]
    pub can_ack_pings: bool,
    #[serde(rename = "IsProber", default)]
    pub is_prober: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: u32,
    #[serde(
        rename = "TokenBucketBytesPerSecond",
        skip_serializing_if = "Option::is_none"
    )]
    pub token_bucket_bytes_per_second: Option<u64>,
    #[serde(
        rename = "TokenBucketBytesBurst",
        skip_serializing_if = "Option::is_none"
    )]
    pub token_bucket_bytes_burst: Option<u64>,
}

mod optional_hex_key {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(k) => s.serialize_some(&hex::encode(k)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let v = Option::<String>::deserialize(d)?;
        v.map(|s| {
            let b = hex::decode(s).map_err(serde::de::Error::custom)?;
            b.try_into()
                .map_err(|_| serde::de::Error::custom("mesh key must be 32 bytes"))
        })
        .transpose()
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("DERP frame too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("DERP packet too large: {0} bytes")]
    PacketTooLarge(usize),
    #[error("invalid DERP frame: {0}")]
    Invalid(&'static str),
    #[error("invalid node key")]
    InvalidKey,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_frames_round_trip() {
        let key = NodeKey::new([7; 32]).unwrap();
        let cases = [
            Frame::ServerKey(key),
            Frame::Ping(*b"12345678"),
            Frame::Pong(*b"abcdefgh"),
            Frame::KeepAlive,
            Frame::NotePreferred(true),
            Frame::PeerGone {
                peer: key,
                reason: 1,
            },
            Frame::Restarting {
                reconnect_in_ms: 10,
                try_for_ms: 20,
            },
        ];
        for frame in cases {
            let raw = frame.encode().unwrap();
            let got = Frame::decode(raw[0], raw.slice(5..)).unwrap();
            assert_eq!(frame, got);
        }
    }
    #[test]
    fn official_greeting_vector() {
        let key = NodeKey::new([1; 32]).unwrap();
        let raw = Frame::ServerKey(key).encode().unwrap();
        assert_eq!(&raw[..5], &[1, 0, 0, 0, 40]);
        assert_eq!(&raw[5..13], MAGIC);
    }

    #[test]
    fn encodes_multiple_frames_into_one_batch() {
        let frames = [
            Frame::Ping(*b"12345678"),
            Frame::Health(Bytes::from_static(b"healthy")),
            Frame::KeepAlive,
        ];
        let mut batch = BytesMut::new();
        for frame in &frames {
            frame.encode_into(&mut batch).unwrap();
        }
        let mut batch = batch.freeze();
        for frame in frames {
            let kind = batch[0];
            let len = u32::from_be_bytes(batch[1..5].try_into().unwrap()) as usize;
            let encoded = batch.split_to(FRAME_HEADER_LEN + len);
            assert_eq!(
                Frame::decode(kind, encoded.slice(FRAME_HEADER_LEN..)).unwrap(),
                frame
            );
        }
        assert!(batch.is_empty());
    }
}
