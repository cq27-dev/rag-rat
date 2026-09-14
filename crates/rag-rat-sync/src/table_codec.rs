//! Length-prefixed framing for the dedicated table-sync protocol.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::codec::{self, FramingError};
use crate::table_wire::{TableFrame, TableWireError};

/// Hard frame cap, checked from the length prefix before allocating the body.
pub const MAX_TABLE_FRAME_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TableCodecError {
    #[error("table-sync stream io: {0}")]
    Io(#[from] std::io::Error),
    #[error("table-sync frame declared {0} bytes, over {max}", max = MAX_TABLE_FRAME_BYTES)]
    FrameTooLarge(u32),
    #[error(transparent)]
    Wire(TableWireError),
    #[error("table-sync stream closed at a frame boundary")]
    Eof,
}

impl From<FramingError> for TableCodecError {
    fn from(error: FramingError) -> Self {
        match error {
            FramingError::OverCap(len) =>
                Self::FrameTooLarge(u32::try_from(len).unwrap_or(u32::MAX)),
            FramingError::Eof(_) => Self::Eof,
            FramingError::Io(error) => Self::Io(error),
        }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &TableFrame,
) -> Result<(), TableCodecError> {
    Ok(codec::write_framed(writer, &frame.encode(), MAX_TABLE_FRAME_BYTES).await?)
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<TableFrame, TableCodecError> {
    let body = codec::read_framed(reader, MAX_TABLE_FRAME_BYTES).await?;
    TableFrame::decode(&body).map_err(TableCodecError::Wire)
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[test]
    fn codec_errors_render_their_cause() {
        assert!(TableCodecError::Io(std::io::Error::other("boom")).to_string().contains("boom"));
        assert!(TableCodecError::FrameTooLarge(7).to_string().contains("7 bytes"));
        assert!(
            TableCodecError::Wire(TableWireError::Malformed("bad".into()))
                .to_string()
                .contains("bad")
        );
        assert!(TableCodecError::Eof.to_string().contains("closed"));
    }

    #[tokio::test]
    async fn total_frame_bound_is_checked_before_allocation() {
        let (mut sender, mut receiver) = tokio::io::duplex(16);
        sender.write_all(&(MAX_TABLE_FRAME_BYTES + 1).to_be_bytes()).await.unwrap();
        assert!(matches!(read_frame(&mut receiver).await, Err(TableCodecError::FrameTooLarge(_))));

        let mut empty = tokio::io::empty();
        assert!(matches!(read_frame(&mut empty).await, Err(TableCodecError::Eof)));

        let mut sink = tokio::io::sink();
        let oversized = TableFrame::Entries {
            stream_id: [0; 32],
            device_fingerprint: [1; 32],
            entries: vec![vec![0; MAX_TABLE_FRAME_BYTES as usize]],
        };
        assert!(matches!(
            write_frame(&mut sink, &oversized).await,
            Err(TableCodecError::FrameTooLarge(_))
        ));
    }
}
