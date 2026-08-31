//! Length-prefixed framing for [`crate::proto::Envelope`]: a big-endian
//! `u32` byte length followed by a `bincode`-encoded envelope. Small, fixed
//! control messages only — the audio payload itself never crosses this
//! channel (see the shared buffer volume apollo/audio-worker use instead).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::proto::Envelope;

/// Sanity bound on a single frame's encoded size — control envelopes are a
/// handful of fields; anything near this is a corrupt length prefix, not a
/// legitimate message.
const MAX_FRAME_LEN: u32 = 1 << 20; // 1 MiB

#[derive(Debug)]
pub enum FramingError {
    Io(std::io::Error),
    Codec(String),
    FrameTooLarge(u32),
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IPC I/O error: {e}"),
            Self::Codec(e) => write!(f, "IPC codec error: {e}"),
            Self::FrameTooLarge(len) => write!(f, "IPC frame too large: {len} bytes"),
        }
    }
}

impl std::error::Error for FramingError {}

impl From<std::io::Error> for FramingError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> Result<(), FramingError> {
    let body = bincode::serde::encode_to_vec(envelope, bincode::config::standard())
        .map_err(|e| FramingError::Codec(e.to_string()))?;
    let len = u32::try_from(body.len()).map_err(|_| FramingError::FrameTooLarge(u32::MAX))?;
    if len > MAX_FRAME_LEN {
        return Err(FramingError::FrameTooLarge(len));
    }
    // One combined buffer/write rather than two separate `write_all` calls
    // (and thus syscalls) for the length prefix and body.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one frame. `Ok(None)` on a clean EOF at a frame boundary (the
/// remote closed the connection between frames, not mid-frame).
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, FramingError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(FramingError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    let (envelope, _) = bincode::serde::decode_from_slice(&buf, bincode::config::standard())
        .map_err(|e| FramingError::Codec(e.to_string()))?;
    Ok(Some(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Event, Request};

    #[tokio::test]
    async fn round_trips_a_request() {
        let mut buf = Vec::new();
        let envelope = Envelope::Request {
            id: 42,
            body: Request::Leave { guild_id: 7 },
        };
        write_frame(&mut buf, &envelope).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_frame(&mut cursor).await.unwrap().unwrap();
        match decoded {
            Envelope::Request {
                id,
                body: Request::Leave { guild_id },
            } => {
                assert_eq!(id, 42);
                assert_eq!(guild_id, 7);
            }
            other => panic!("unexpected envelope: {other:?}"),
        }
    }

    #[tokio::test]
    async fn round_trips_an_event() {
        let mut buf = Vec::new();
        let envelope = Envelope::Event(Event::ConnectionLost { guild_id: 99 });
        write_frame(&mut buf, &envelope).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_frame(&mut cursor).await.unwrap().unwrap();
        assert!(matches!(
            decoded,
            Envelope::Event(Event::ConnectionLost { guild_id: 99 })
        ));
    }

    #[tokio::test]
    async fn clean_eof_at_frame_boundary_yields_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        let envelope = Envelope::Event(Event::ConnectionLost { guild_id: 1 });
        write_frame(&mut buf, &envelope).await.unwrap();
        buf.truncate(buf.len() - 1); // chop the last byte of the payload
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected_without_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_LEN + 1).to_be_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        match read_frame(&mut cursor).await {
            Err(FramingError::FrameTooLarge(len)) => assert_eq!(len, MAX_FRAME_LEN + 1),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }
}
