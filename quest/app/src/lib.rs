//! hyprvr Quest client.

pub mod jitter;
pub mod net;

#[cfg(target_os = "android")]
mod android;
