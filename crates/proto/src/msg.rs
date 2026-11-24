//! All message types. Everything is serialized with `postcard`.
//!
//! Rule for evolving the protocol: only append enum variants / bump `PROTO_VERSION`.

use serde::{Deserialize, Serialize};

/// Stable id of a screen for the lifetime of the daemon (not reused).
pub type ScreenId = u32;

/// Rigid pose in the headset's STAGE reference space (meters, OpenXR conventions:
/// +Y up, -Z forward). Orientation is a unit quaternion `[x, y, z, w]`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: [f32; 3],
    pub orientation: [f32; 4],
}

impl Pose {
    pub const IDENTITY: Pose = Pose { position: [0.0, 0.0, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] };
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Curvature {
    Flat,
    /// Cylinder layer; the panel's width is the arc length.
    Curved { radius_m: f32 },
}

/// Placement of one screen in 3D. The panel's height follows from `width_m` and the
/// screen's aspect ratio. `pose` is the panel center (for curved panels: the center
/// of the arc's surface, facing +Z toward the user like a quad).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct PanelLayout {
    pub screen: ScreenId,
    pub pose: Pose,
    pub width_m: f32,
    pub curvature: Curvature,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenKind {
    /// Headless Hyprland output created by the daemon; exists only in VR.
    Virtual,
    /// A physical output (e.g. eDP-1) mirrored into VR.
    Mirror,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScreenInfo {
    pub id: ScreenId,
    /// Hyprland output name, e.g. "HYPRVR-1" or "eDP-1".
    pub output: String,
    pub kind: ScreenKind,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioTarget {
    Laptop,
    Headset,
}

/// A rectangle of the room (the laptop keyboard) that always shows passthrough.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct KeyboardCutout {
    /// Center of the rectangle; the rectangle lies in the pose's local XZ plane
    /// (i.e. flat on the desk; local +Y is the surface normal).
    pub pose: Pose,
    /// Extent along local X and local Z, meters.
    pub size_m: [f32; 2],
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Environment {
    /// Camera passthrough, darkened by `Settings::dim`.
    Passthrough,
    /// Solid dark void.
    Void,
    /// Equirectangular image provided by the daemon (name from `ServerState::environments`).
    Image(String),
}

/// Headset-side preferences, persisted by the daemon so they survive reinstalls.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    pub environment: Environment,
    /// 0.0 = full passthrough brightness, 1.0 = black.
    pub dim: f32,
    pub keyboard_cutout: Option<KeyboardCutout>,
    pub show_stats: bool,
    /// The laser moves the desktop pointer while just pointing. Off: only pinch/trigger moves
    /// and clicks, so sweeping a hand across windows doesn't hover/highlight them.
    #[serde(default)]
    pub laser_hover: bool,
    /// Per-environment yaw in degrees (multiples of 90), so its "front" faces the user's desk.
    #[serde(default)]
    pub env_yaw: std::collections::BTreeMap<String, u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { environment: Environment::Passthrough, dim: 0.3, keyboard_cutout: None, show_stats: false, laser_hover: false, env_yaw: Default::default() }
    }
}

/// Complete daemon state as seen by the client. Sent after auth and on every change.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ServerState {
    pub screens: Vec<ScreenInfo>,
    /// Exactly one entry per screen in `screens` (the daemon fills in defaults).
    pub layout: Vec<PanelLayout>,
    pub audio: AudioTarget,
    pub settings: Settings,
    /// Names usable with `Environment::Image` / `ClientMsg::GetEnvironment`.
    pub environments: Vec<String>,
    /// Physical outputs that can be mirrored with `ClientMsg::AddMirror`.
    pub mirrorable: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// Pixel rectangle inside a screen.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClientStats {
    /// Mean capture->displayed latency over the last second, ms (uses clock offset from Ping).
    pub latency_ms: f32,
    pub decoded_fps: f32,
    pub dropped_frames: u32,
    pub render_fps: f32,
}

/// Client -> server, on the control stream.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ClientMsg {
    /// Must be the first message.
    Hello { version: u32, device_name: String },
    /// SPAKE2 side A message (PIN entered by the user).
    PairStart { spake: Vec<u8> },
    /// HMAC proving the client derived the same key; see `pairing`.
    PairConfirm { mac: [u8; 32] },
    SetLayout(Vec<PanelLayout>),
    SetSettings(Settings),
    AddScreen { width: u32, height: u32 },
    AddMirror { output: String },
    RemoveScreen { id: ScreenId },
    ResizeScreen { id: ScreenId, width: u32, height: u32 },
    SetAudio(AudioTarget),
    /// Pointer button from laser/pinch. Position comes from the latest `Datagram::PointerMove`;
    /// `x`/`y` repeat it so a click lands correctly even if datagrams were lost.
    Button { screen: ScreenId, x: f32, y: f32, button: PointerButton, pressed: bool },
    /// Scroll in wheel notches (1.0 = one notch, fractional allowed); +y scrolls down.
    Scroll { screen: ScreenId, dx: f32, dy: f32 },
    /// Ask for a lossless refresh of a whole screen (e.g. after the client lost its canvas).
    RequestRefresh { screen: ScreenId },
    /// Ask the server to send an environment image on a new uni stream.
    GetEnvironment { name: String },
    Ping { client_time_us: u64 },
    Stats(ClientStats),
    /// Ask for small previews of all environments (one uni stream of `EnvironmentThumb`s).
    GetEnvironmentThumbs,
}

/// Server -> client, on the control stream.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ServerMsg {
    /// Reply to Hello. If `paired` is false the client must pair (the PIN is shown on the PC).
    Welcome { version: u32, server_name: String, paired: bool },
    /// SPAKE2 side B message.
    PairStart { spake: Vec<u8> },
    /// Pairing succeeded; `mac` lets the client verify the server too.
    PairOk { mac: [u8; 32] },
    /// Wrong PIN or too many attempts. The connection is closed afterwards.
    PairFailed { reason: String },
    State(ServerState),
    /// Cursor image. Referenced by `Datagram::Cursor::shape_serial`.
    CursorShape { serial: u32, width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8> },
    Pong { client_time_us: u64, server_time_us: u64 },
    Error { message: String },
}

/// Messages on a server-opened unidirectional stream. The first message on the stream
/// selects its purpose: `ScreenHeader` (screen stream) or `EnvironmentImage`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum StreamMsg {
    ScreenHeader { screen: ScreenId, width: u32, height: u32 },
    /// Lossless tiles; payload format in `tiles`. Applied over the canvas in order.
    Tiles { seq: u64, capture_us: u64, payload: Vec<u8> },
    /// From now on `rect` is covered by video. `rect.x/y/w/h` are multiples of 16 except
    /// where clipped by the screen edge; the decoded picture is `rect.w x rect.h`
    /// (encoder-side padding is cropped via the SPS).
    VideoStart { seq: u64, codec: VideoCodec, rect: Rect },
    /// One Annex-B access unit. Keyframes carry SPS/PPS (and VPS for HEVC) in-band.
    VideoFrame { seq: u64, capture_us: u64, keyframe: bool, data: Vec<u8> },
    /// Video region ended. Lossless tiles covering the rect follow.
    VideoStop { seq: u64 },
    /// Screen resolution changed. A full lossless refresh follows.
    Resized { width: u32, height: u32 },
    /// Whole environment file (JPEG or PNG, equirectangular), on its own stream.
    EnvironmentImage { name: String, data: Vec<u8> },
    /// Small JPEG preview of an environment; a stream carries one per environment.
    EnvironmentThumb { name: String, data: Vec<u8> },
}

/// QUIC datagrams (unreliable, unordered). Keep under ~1100 bytes serialized.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Datagram {
    /// S->C. Cursor position in screen pixels (hotspot position). `screen: None` = hidden /
    /// not on any streamed screen. `seq` increases monotonically; drop older ones.
    Cursor { seq: u32, screen: Option<ScreenId>, x: f32, y: f32, shape_serial: u32 },
    /// S->C. `AUDIO_FRAMES_PER_PACKET` interleaved s16le stereo frames.
    Audio { seq: u32, pcm: Vec<u8> },
    /// C->S. Laser/pinch pointer position, screen pixels. Latest seq wins.
    PointerMove { seq: u32, screen: ScreenId, x: f32, y: f32 },
}

/// Encode a datagram.
pub fn encode_datagram(d: &Datagram) -> Vec<u8> {
    postcard::to_stdvec(d).expect("datagram serialization cannot fail")
}

pub fn decode_datagram(b: &[u8]) -> Result<Datagram, postcard::Error> {
    postcard::from_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_datagram_fits() {
        let d = Datagram::Audio { seq: u32::MAX, pcm: vec![0u8; crate::AUDIO_FRAMES_PER_PACKET * 4] };
        assert!(encode_datagram(&d).len() < 1100);
    }

    #[test]
    fn roundtrip() {
        let d = Datagram::Cursor { seq: 7, screen: Some(2), x: 10.5, y: 3.0, shape_serial: 1 };
        assert_eq!(decode_datagram(&encode_datagram(&d)).unwrap(), d);
    }
}
