//! Length-prefixed postcard framing for QUIC streams: `u32 LE length` + postcard body.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Upper bound for one message (a full lossless 4K refresh or an environment image fits).
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
    #[error("message too large: {0} bytes")]
    TooLarge(usize),
    #[error("stream closed")]
    Closed,
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).expect("serialization cannot fail");
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    w.write_all(&encode(msg)).await?;
    Ok(())
}

/// Reads one message. Returns `FrameError::Closed` on a clean end of stream between messages.
pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::ClientMsg;

    #[tokio::test]
    async fn roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let m = ClientMsg::Hello { version: 1, device_name: "quest".into() };
        write_msg(&mut a, &m).await.unwrap();
        drop(a);
        let got: ClientMsg = read_msg(&mut b).await.unwrap();
        assert_eq!(got, m);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut b).await, Err(FrameError::Closed)));
    }
}
