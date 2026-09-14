use rag_rat_sync::codec::{self, CodecError};
use rag_rat_sync::table_codec::{self, TableCodecError};
use rag_rat_sync::table_wire::TableFrame;
use rag_rat_sync::wire::Frame;

#[tokio::test]
async fn framing_compat_pins_account_and_table_bytes() {
    let account = b"\x00\x00\x00\x17\x82\x74rag-rat/sync-frame/1\x02";
    let table = b"\x00\x00\x00\x1e\x82\x78\x1arag-rat/table-sync-frame/3\x06";
    let mut output = Vec::new();
    codec::write_frame(&mut output, &Frame::Done).await.unwrap();
    assert_eq!(output, account);
    assert_eq!(codec::read_frame(&mut account.as_slice()).await.unwrap(), Frame::Done);
    output.clear();
    table_codec::write_frame(&mut output, &TableFrame::Done).await.unwrap();
    assert_eq!(output, table);
    assert_eq!(table_codec::read_frame(&mut table.as_slice()).await.unwrap(), TableFrame::Done);
}

#[tokio::test]
async fn framing_compat_pins_caps_and_eof_classification() {
    assert_eq!(codec::MAX_FRAME_BYTES, 24 * 1024 * 1024);
    assert_eq!(table_codec::MAX_TABLE_FRAME_BYTES, 4 * 1024 * 1024);
    // Both codecs historically classify even a partial prefix as Eof, but a
    // truncated body as Io. Sharing the reader must preserve this distinction.
    for bytes in [b"".as_slice(), b"\x00\x00"] {
        let (mut account, mut table) = (bytes, bytes);
        assert!(matches!(codec::read_frame(&mut account).await, Err(CodecError::Eof)));
        assert!(matches!(table_codec::read_frame(&mut table).await, Err(TableCodecError::Eof)));
    }
    let truncated = b"\x00\x00\x00\x03\xaa";
    assert!(
        matches!(codec::read_frame(&mut truncated.as_slice()).await, Err(CodecError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
    );
    assert!(
        matches!(table_codec::read_frame(&mut truncated.as_slice()).await, Err(TableCodecError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
    );
    let account_cap = (codec::MAX_FRAME_BYTES + 1).to_be_bytes();
    let table_cap = (table_codec::MAX_TABLE_FRAME_BYTES + 1).to_be_bytes();
    assert_eq!(
        codec::read_frame(&mut account_cap.as_slice()).await.unwrap_err().to_string(),
        "sync frame declared 25165825 bytes, over 25165824"
    );
    assert_eq!(
        table_codec::read_frame(&mut table_cap.as_slice()).await.unwrap_err().to_string(),
        "table-sync frame declared 4194305 bytes, over 4194304"
    );
}
