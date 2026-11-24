//! Shared wire protocol for hyprvr: the PC daemon (`vrd`) and the Quest client.
//!
//! See `docs/PROTOCOL.md` for the full description. In short, one QUIC connection
//! per headset carries:
//! * one client-opened bidirectional **control stream** ([`msg::ClientMsg`] / [`msg::ServerMsg`]),
//! * one server-opened unidirectional **screen stream** per screen ([`msg::StreamMsg`]),
//! * QUIC **datagrams** for latency-sensitive, loss-tolerant data ([`msg::Datagram`]).

pub mod discovery;
pub mod environments;
pub mod framing;
pub mod msg;
pub mod pairing;
pub mod tiles;
pub mod tls;

/// Bump on any incompatible wire change.
pub const PROTO_VERSION: u32 = 2;
/// QUIC (UDP) port the daemon listens on.
pub const DEFAULT_PORT: u16 = 47800;
/// UDP port for LAN discovery requests.
pub const DISCOVERY_PORT: u16 = 47801;
/// TLS ALPN identifier.
pub const ALPN: &[u8] = b"hyprvr/1";
/// Side length of a lossless tile in pixels.
/// QUIC application close code a headset uses when it leaves on purpose (app closed,
/// unpaired). The server then removes virtual outputs immediately instead of keeping
/// them for the reconnect grace period.
pub const CLOSE_GOODBYE: u32 = 0x4859;
pub const TILE_SIZE: u32 = 64;
/// Audio format carried in [`msg::Datagram::Audio`]: interleaved s16le stereo.
pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u32 = 2;
/// Frames (per channel) in one audio datagram: 5 ms at 48 kHz -> 960 bytes payload.
pub const AUDIO_FRAMES_PER_PACKET: usize = 240;
