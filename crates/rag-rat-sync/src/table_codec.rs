//! Length-prefixed framing for the dedicated table-sync protocol.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::table_wire::{TableFrame, TableWireError};

/// Hard frame cap, checked from the length prefix before allocating the body.
pub const MAX_TABLE_FRAME_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum TableCodecError {
    Io(std::io::Error),
    FrameTooLarge(u32),
    Wire(TableWireError),
    Eof,
}

impl std::fmt::Display for TableCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "table-sync stream io: {error}"),
            Self::FrameTooLarge(bytes) =>
                write!(f, "table-sync frame declared {bytes} bytes, over {MAX_TABLE_FRAME_BYTES}"),
            Self::Wire(error) => write!(f, "{error}"),
            Self::Eof => write!(f, "table-sync stream closed at a frame boundary"),
        }
    }
}

impl std::error::Error for TableCodecError {}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &TableFrame,
) -> Result<(), TableCodecError> {
    let body = frame.encode();
    let len = u32::try_from(body.len()).map_err(|_| TableCodecError::FrameTooLarge(u32::MAX))?;
    if len > MAX_TABLE_FRAME_BYTES {
        return Err(TableCodecError::FrameTooLarge(len));
    }
    writer.write_all(&len.to_be_bytes()).await.map_err(TableCodecError::Io)?;
    writer.write_all(&body).await.map_err(TableCodecError::Io)
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<TableFrame, TableCodecError> {
    let mut len_bytes = [0; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => {},
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(TableCodecError::Eof);
        },
        Err(error) => return Err(TableCodecError::Io(error)),
    }
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_TABLE_FRAME_BYTES {
        return Err(TableCodecError::FrameTooLarge(len));
    }
    let mut body = vec![0; len as usize];
    reader.read_exact(&mut body).await.map_err(TableCodecError::Io)?;
    TableFrame::decode(&body).map_err(TableCodecError::Wire)
}

#[cfg(test)]
mod tests {
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
            entries: vec![vec![0; MAX_TABLE_FRAME_BYTES as usize]],
            more: false,
        };
        assert!(matches!(
            write_frame(&mut sink, &oversized).await,
            Err(TableCodecError::FrameTooLarge(_))
        ));
    }
}
