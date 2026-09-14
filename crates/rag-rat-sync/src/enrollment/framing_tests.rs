use std::time::Duration;

use super::{session, wire};

#[tokio::test]
async fn framing_compat_pins_enrollment_bytes_caps_and_errors() {
    assert_eq!(wire::MAX_ENROLL_REQUEST_FRAME, 144 * 1024);
    assert_eq!(wire::MAX_ENROLL_RESPONSE_FRAME, 24 * 1024 * 1024);
    let window = Duration::from_secs(1);
    let bytes = b"\x00\x00\x00\x03\xaa\xbb\xcc";
    let mut output = Vec::new();
    session::write_blob(&mut output, &[0xaa, 0xbb, 0xcc], 3, "request", window).await.unwrap();
    assert_eq!(output, bytes);
    assert_eq!(session::read_blob(&mut bytes.as_slice(), 3, "request", window).await.unwrap(), [
        0xaa, 0xbb, 0xcc
    ]);
    let error = session::read_blob(&mut bytes.as_slice(), 2, "request", window).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "malformed enrollment data: enrollment request frame exceeds 2 bytes"
    );
    let mut output = Vec::new();
    let error = session::write_blob(&mut output, &[0xaa, 0xbb, 0xcc], 2, "request", window)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "malformed enrollment data: enrollment request frame exceeds 2 bytes"
    );
    assert!(output.is_empty());
}
