//! LAN discovery without multicast (Android needs a Java MulticastLock for multicast).
//!
//! The client sends `REQUEST` as a UDP broadcast to `DISCOVERY_PORT` (255.255.255.255
//! and each interface's directed broadcast). The daemon answers unicast with a
//! postcard-encoded [`Reply`].

use serde::{Deserialize, Serialize};

use crate::pairing::Fingerprint;

pub const REQUEST: &[u8] = b"HYPRVR?1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Reply {
    pub magic: [u8; 8],
    pub version: u32,
    pub name: String,
    /// QUIC port.
    pub port: u16,
    /// Server certificate fingerprint, so a client can pick its paired server.
    pub fingerprint: Fingerprint,
}

pub const REPLY_MAGIC: [u8; 8] = *b"HYPRVR!1";

impl Reply {
    pub fn new(name: String, port: u16, fingerprint: Fingerprint) -> Self {
        Reply { magic: REPLY_MAGIC, version: crate::PROTO_VERSION, name, port, fingerprint }
    }
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("serialize")
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        postcard::from_bytes::<Reply>(b).ok().filter(|r| r.magic == REPLY_MAGIC)
    }
}
