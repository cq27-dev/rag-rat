//! Length-prefixed framing over an async byte stream (phase D, #406).
//!
//! Each frame is a 4-byte big-endian length followed by that many CBOR bytes. The length is capped
//! so a peer cannot announce a multi-gigabyte frame and force an unbounded read before the frame is
//! even parsed — the transport-level half of "bounded frames, no amplification". This layer is
//! transport-agnostic: it runs over an iroh bi-stream in production and over an in-memory duplex in
//! tests, so the session logic is exercised without the network.
//!
//! [`write_framed`] and [`read_framed`] are the one implementation of that layout; every lane's
//! codec (account and content here, [`crate::table_codec`], and the discovery client's
//! [`crate::discovery::wire`]) wraps them with its own cap and error type. The enrollment exchange
//! alone frames by hand, because it gives every chunk of a body its own progress deadline.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::wire::{Frame, WireError};

/// The largest frame this codec will read or write. Chosen well above one full [`Frame::Entries`]
/// page of account entries (each entry is at most the §18a envelope, 64 KiB) plus overhead. A
/// larger declared length is refused before any allocation, and a larger local frame before any
/// byte is written.
pub const MAX_FRAME_BYTES: u32 = 24 * 1024 * 1024;

/// What can go wrong moving a frame over the wire.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The underlying stream failed or closed mid-frame.
    #[error("sync stream io: {0}")]
    Io(std::io::Error),
    /// A frame's length — declared by the peer, or of a local frame about to be written — exceeded
    /// [`MAX_FRAME_BYTES`].
    #[error("sync frame declared {0} bytes, over {max}", max = MAX_FRAME_BYTES)]
    FrameTooLarge(u32),
    /// The frame bytes were not a valid protocol frame.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The stream ended cleanly at a frame boundary — not an error, but distinguished so the
    /// session can tell "peer hung up" from "peer sent garbage".
    #[error("sync stream closed at a frame boundary")]
    Eof,
}

/// A length-prefixed frame that could not be moved, before any lane decodes its body. Each lane
/// maps it onto its own error type.
#[derive(Debug)]
pub(crate) enum FramingError {
    /// A body length over the frame cap: a local body refused before any byte is written, or a
    /// peer's length prefix refused before the body is allocated.
    OverCap(usize),
    /// The stream ended cleanly before the next length prefix.
    Eof(std::io::Error),
    /// The stream failed, or closed mid-frame.
    Io(std::io::Error),
}

impl From<FramingError> for CodecError {
    fn from(error: FramingError) -> Self {
        match error {
            FramingError::OverCap(len) =>
                Self::FrameTooLarge(u32::try_from(len).unwrap_or(u32::MAX)),
            FramingError::Eof(_) => Self::Eof,
            FramingError::Io(error) => Self::Io(error),
        }
    }
}

/// Write `body` as one frame: a 4-byte big-endian length prefix, then the body. A body longer than
/// `max` is refused before any byte is written, so no length prefix is ever truncated or sent for a
/// frame the peer would refuse.
pub(crate) async fn write_framed<W: AsyncWrite + Unpin>(
    w: &mut W,
    body: &[u8],
    max: u32,
) -> Result<(), FramingError> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= max)
        .ok_or(FramingError::OverCap(body.len()))?;
    w.write_all(&len.to_be_bytes()).await.map_err(FramingError::Io)?;
    w.write_all(body).await.map_err(FramingError::Io)
}

/// Read one frame's body, refusing a length prefix over `max` BEFORE allocating: the length is
/// peer-supplied, so trusting it is a trivial memory-exhaustion lever.
pub(crate) async fn read_framed<R: AsyncRead + Unpin>(
    r: &mut R,
    max: u32,
) -> Result<Vec<u8>, FramingError> {
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            FramingError::Eof(error)
        } else {
            FramingError::Io(error)
        }
    })?;
    let len = u32::from_be_bytes(prefix);
    if len > max {
        return Err(FramingError::OverCap(len as usize));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await.map_err(FramingError::Io)?;
    Ok(body)
}

/// Write one frame: a 4-byte big-endian length prefix, then the CBOR body.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), CodecError> {
    // Local frames are built within the caps, so this only fires on a programmer bug, not a wire
    // condition — but refuse it here rather than send a frame every peer's reader would refuse.
    Ok(write_framed(w, &frame.encode(), MAX_FRAME_BYTES).await?)
}

/// Read one frame, or [`CodecError::Eof`] if the stream ends cleanly before the next length prefix.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, CodecError> {
    read_frame_within(r, MAX_FRAME_BYTES).await
}

/// [`read_frame`] with a caller-supplied maximum, checked against the length prefix BEFORE any
/// allocation. The auth phase passes a tight bound so an unauthenticated peer cannot force the full
/// [`MAX_FRAME_BYTES`] allocation with its first frame (#881) — the frame-level cap is what
/// actually bounds the pre-auth allocation, since the body is sized from the length prefix.
pub async fn read_frame_within<R: AsyncRead + Unpin>(
    r: &mut R,
    max_bytes: u32,
) -> Result<Frame, CodecError> {
    let body = read_framed(r, max_bytes).await?;
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
            Frame::Ack,
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

    #[tokio::test]
    async fn an_oversized_local_frame_is_refused_before_anything_is_written() {
        // One entry past the cap: the peer's reader would refuse the prefix, so the writer must
        // not put it on the wire at all.
        let oversized =
            Frame::Entries { entries: vec![vec![0; MAX_FRAME_BYTES as usize]], more: false };
        let mut wire = Vec::new();
        assert!(matches!(
            write_frame(&mut wire, &oversized).await,
            Err(CodecError::FrameTooLarge(len)) if len > MAX_FRAME_BYTES,
        ));
        assert!(wire.is_empty(), "no length prefix or body byte was written");
    }

    #[tokio::test]
    async fn a_capped_read_refuses_a_prefix_over_its_smaller_limit() {
        // The auth phase reads with a tight cap so an unauthenticated peer cannot force the full
        // MAX_FRAME_BYTES allocation. A 2 KiB prefix under a 1 KiB cap is refused from the prefix
        // alone — the 2 KiB body is never allocated.
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&2048u32.to_be_bytes()).await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame_within(&mut b, 1024).await,
            Err(CodecError::FrameTooLarge(2048)),
        ));
    }
}
