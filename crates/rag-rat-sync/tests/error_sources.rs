use std::error::Error;

use rag_rat_sync::SyncFailure;
use rag_rat_sync::auth::AuthError;
use rag_rat_sync::codec::CodecError;
use rag_rat_sync::enrollment::InviteError;
use rag_rat_sync::session::SessionError;
use rag_rat_sync::table_codec::TableCodecError;
use rag_rat_sync::table_session::TableSessionError;

#[test]
fn session_failure_exposes_the_codec_and_io_cause_without_changing_display() {
    let failure = SyncFailure::Session(SessionError::Codec(CodecError::Io(std::io::Error::other(
        "broken stream",
    ))));
    assert_eq!(failure.to_string(), "sync session transport: sync stream io: broken stream");
    // Transparent wrappers forward the wrapped error's source, so SyncFailure and
    // SessionError render the same message while the chain begins at CodecError.
    let codec = failure.source().expect("codec cause");
    assert!(codec.is::<CodecError>());
    let io = codec.source().expect("IO cause");
    assert!(io.is::<std::io::Error>());
    assert_eq!(io.to_string(), "broken stream");
    assert!(io.source().is_none());
}

#[test]
fn every_prefixed_wrapper_exposes_its_source_and_keeps_its_display() {
    let errors: Vec<(Box<dyn Error>, &str)> = vec![
        (Box::new(CodecError::Io(std::io::Error::other("boom"))), "sync stream io: boom"),
        (
            Box::new(TableCodecError::Io(std::io::Error::other("boom"))),
            "table-sync stream io: boom",
        ),
        (
            Box::new(SessionError::Codec(CodecError::Io(std::io::Error::other("boom")))),
            "sync session transport: sync stream io: boom",
        ),
        (
            Box::new(TableSessionError::Codec(TableCodecError::Io(std::io::Error::other("boom")))),
            "table-sync session transport: table-sync stream io: boom",
        ),
        (
            Box::new(AuthError::Codec(CodecError::Io(std::io::Error::other("boom")))),
            "sync auth transport: sync stream io: boom",
        ),
        (Box::new(InviteError::Io(std::io::Error::other("boom"))), "enrollment stream: boom"),
    ];
    for (error, display) in errors {
        assert_eq!(error.to_string(), display);
        assert!(error.source().is_some(), "{display}");
    }
}
