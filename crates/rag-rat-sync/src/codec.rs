//! Length-prefixed framing over an async byte stream (phase D, #406).
//!
//! Each frame is a 4-byte big-endian length followed by that many CBOR bytes. The length is capped
//! so a peer cannot announce a multi-gigabyte frame and force an unbounded read before the frame is
//! even parsed — the transport-level half of "bounded frames, no amplification". This layer is
//! transport-agnostic: it runs over an iroh bi-stream in production and over an in-memory duplex in
//! tests, so the session logic is exercised without the network.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::wire::{Frame, WireError};

/// The largest frame this codec will read. Chosen well above one full [`Frame::Entries`] page of
/// account entries (each entry is at most the §18a envelope, 64 KiB) plus overhead. A larger
/// declared length is refused before any allocation.
pub const MAX_FRAME_BYTES: u32 = 24 * 1024 * 1024;

/// What can go wrong moving a frame over the wire.
#[derive(Debug)]
pub enum CodecError {
    /// The underlying stream failed or closed mid-frame.
    Io(std::io::Error),
    /// A frame's declared length exceeded [`MAX_FRAME_BYTES`].
    FrameTooLarge(u32),
    /// The frame bytes were not a valid protocol frame.
    Wire(WireError),
    /// The stream ended cleanly at a frame boundary — not an error, but distinguished so the
    /// session can tell "peer hung up" from "peer sent garbage".
    Eof,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Io(e) => write!(f, "sync stream io: {e}"),
            CodecError::FrameTooLarge(n) => {
                write!(f, "sync frame declared {n} bytes, over {MAX_FRAME_BYTES}")
            },
            CodecError::Wire(e) => write!(f, "{e}"),
            CodecError::Eof => write!(f, "sync stream closed at a frame boundary"),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<WireError> for CodecError {
    fn from(e: WireError) -> Self {
        CodecError::Wire(e)
    }
}

/// Write one frame: a 4-byte big-endian length prefix, then the CBOR body.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), CodecError> {
    let body = frame.encode();
    // Local frames are built within the caps, so this only fires on a programmer bug, not a wire
    // condition — but guard rather than truncate a silently-huge length prefix.
    let len = u32::try_from(body.len()).map_err(|_| CodecError::FrameTooLarge(u32::MAX))?;
    w.write_all(&len.to_be_bytes()).await.map_err(CodecError::Io)?;
    w.write_all(&body).await.map_err(CodecError::Io)?;
    Ok(())
}

/// Read one frame, or [`CodecError::Eof`] if the stream ends cleanly before the next length prefix.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, CodecError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {},
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(CodecError::Eof),
        Err(e) => return Err(CodecError::Io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await.map_err(CodecError::Io)?;
    Ok(Frame::decode(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip_over_a_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = vec![
            Frame::Hello { account_id: [3; 32], have: vec![[1; 32]] },
            Frame::Entries { entries: vec![vec![9, 9, 9]], more: false },
            Frame::Done,
        ];
        let to_send = sent.clone();
        let writer = tokio::spawn(async move {
            for f in &to_send {
                write_frame(&mut a, f).await.unwrap();
            }
            // drop `a` → clean EOF on `b`
        });
        let mut got = Vec::new();
        loop {
            match read_frame(&mut b).await {
                Ok(f) => got.push(f),
                Err(CodecError::Eof) => break,
                Err(e) => panic!("unexpected {e}"),
            }
        }
        writer.await.unwrap();
        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_refused_before_allocating() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(MAX_FRAME_BYTES + 1).to_be_bytes()).await.unwrap();
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(CodecError::FrameTooLarge(_))));
    }
}
