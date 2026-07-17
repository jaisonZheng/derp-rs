use std::{fmt, fs, io::Write, path::Path};

use crypto_box::{Nonce, PublicKey, SalsaBox, SecretKey, aead::Aead};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::{KEY_LEN, NONCE_LEN, ProtocolError};

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeKey([u8; KEY_LEN]);

impl NodeKey {
    pub fn new(bytes: [u8; KEY_LEN]) -> Result<Self, ProtocolError> {
        if bytes == [0; KEY_LEN] {
            Err(ProtocolError::InvalidKey)
        } else {
            Ok(Self(bytes))
        }
    }
    pub fn from_slice(value: &[u8]) -> Result<Self, ProtocolError> {
        let bytes: [u8; KEY_LEN] = value.try_into().map_err(|_| ProtocolError::InvalidKey)?;
        Self::new(bytes)
    }
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
    pub fn short(self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "nodekey:{}", self.short())
    }
}
impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "nodekey:{}", self.to_hex())
    }
}

#[derive(Clone)]
pub struct NodeKeyPair {
    secret: SecretKey,
}

impl NodeKeyPair {
    pub fn generate() -> Self {
        let mut raw = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut raw);
        clamp(&mut raw);
        Self {
            secret: SecretKey::from(raw),
        }
    }
    pub fn from_bytes(mut raw: [u8; KEY_LEN]) -> Result<Self, KeyError> {
        if raw == [0; KEY_LEN] {
            return Err(KeyError::Invalid);
        }
        clamp(&mut raw);
        Ok(Self {
            secret: SecretKey::from(raw),
        })
    }
    pub fn private_bytes(&self) -> [u8; KEY_LEN] {
        self.secret.to_bytes()
    }
    pub fn public(&self) -> NodeKey {
        NodeKey::new(self.secret.public_key().to_bytes()).expect("non-zero public key")
    }
    pub fn seal_to(&self, peer: NodeKey, plaintext: &[u8]) -> Result<Vec<u8>, KeyError> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let box_ = SalsaBox::new(&PublicKey::from(*peer.as_bytes()), &self.secret);
        let encrypted = box_
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| KeyError::Crypto)?;
        let mut out = Vec::with_capacity(NONCE_LEN + encrypted.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&encrypted);
        Ok(out)
    }
    pub fn open_from(&self, peer: NodeKey, sealed: &[u8]) -> Result<Vec<u8>, KeyError> {
        if sealed.len() < NONCE_LEN {
            return Err(KeyError::Crypto);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        SalsaBox::new(&PublicKey::from(*peer.as_bytes()), &self.secret)
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| KeyError::Crypto)
    }

    /// Loads a raw hex key, creating it atomically with mode 0600 if absent.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, KeyError> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(value) => {
                let value = value
                    .trim()
                    .strip_prefix("privkey:")
                    .unwrap_or(value.trim());
                let raw = hex::decode(value)?;
                Self::from_bytes(raw.try_into().map_err(|_| KeyError::Invalid)?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                let mut file = {
                    use std::os::unix::fs::OpenOptionsExt;
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path)?
                };
                #[cfg(not(unix))]
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                writeln!(file, "privkey:{}", hex::encode(key.private_bytes()))?;
                file.sync_all()?;
                Ok(key)
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn clamp(raw: &mut [u8; KEY_LEN]) {
    raw[0] &= 248;
    raw[31] &= 127;
    raw[31] |= 64;
}

#[derive(Debug, Error)]
pub enum KeyError {
    #[error("invalid DERP private key")]
    Invalid,
    #[error("DERP crypto box authentication failed")]
    Crypto,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Hex(#[from] hex::FromHexError),
}

impl Serialize for NodeKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for NodeKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let s = s.strip_prefix("nodekey:").unwrap_or(&s);
        let raw = hex::decode(s).map_err(serde::de::Error::custom)?;
        NodeKey::from_slice(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crypto_box_round_trip() {
        let a = NodeKeyPair::generate();
        let b = NodeKeyPair::generate();
        let mut sealed = a.seal_to(b.public(), b"hello").unwrap();
        assert_eq!(b.open_from(a.public(), &sealed).unwrap(), b"hello");
        *sealed.last_mut().unwrap() ^= 1;
        assert!(b.open_from(a.public(), &sealed).is_err());
    }
}
