//! The async enrollment and writer-grant exchanges over an open stream, with their per-chunk
//! progress framing.

use std::time::Duration;

use rag_rat_oplog::{AccountId, CatchUpReport, verify_enrollment_device_add};
use rusqlite::Connection;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::error::InviteError;
use super::redeem::{redeem_invite, redeem_writer_invite};
use super::ticket::{InviteTicket, InviteTicketKind};
use super::wire::{
    EnrollmentReceipt, EnrollmentRequest, EnrollmentResponse, GRANT_REQUEST_DOMAIN,
    MAX_ENROLL_REQUEST_FRAME, MAX_ENROLL_RESPONSE_FRAME, WriterGrantReceipt, WriterGrantRequest,
    WriterGrantResponse, ensure_frame_len, refusal_code, request_blob_domain,
};

/// Per-chunk progress window for enrollment frame IO: every 64 KiB slice of a frame body must
/// arrive (or drain) within this window. Mirrors the session's per-frame idle reset
/// (`read_frame_before`), so a slow but progressing multi-megabyte receipt never times out while
/// a stalled peer dies within one window.
pub const ENROLL_PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);

/// Chunk size for progress-tracked frame IO: 64 KiB per window tolerates ~1 KiB/s links, and a
/// byte-at-a-time trickle can never complete a chunk, so the slow-loris floor is one window.
const ENROLL_PROGRESS_CHUNK_BYTES: usize = 64 * 1024;

/// The tighter window applied to frames any UNAUTHENTICATED peer can reach — the request and a
/// refusal response (a random nonce gets that far). Both are at most a couple of chunks, so a
/// bogus peer's hold on the serial accept loop stays in seconds, not minutes; the full
/// [`ENROLL_PROGRESS_TIMEOUT`] applies only once a valid nonce has authorized the receipt.
const ENROLL_UNAUTH_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);

/// One byte the dialer sends the moment it has DECODED the response — the acceptor's delivery
/// signal, letting it close without a peer-controlled graceful-close wait (#945). Not a
/// length-prefixed blob: it is the terminal byte of the exchange, written after the response
/// frame and read raw by the acceptor.
pub(crate) const RESPONSE_ACK: u8 = 0x01;

/// Shared bound for sending or receiving the terminal response acknowledgement. Ack delivery is
/// best-effort after the response is decoded, so a stalled reverse stream cannot mask the result.
pub(crate) const RESPONSE_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Owner-side result after a complete enrollment request. Refusals are sent over the wire before
/// being returned, so the caller may gracefully close the QUIC connection without hiding the
/// semantic error from the joiner.
pub enum EnrollmentAcceptorOutcome {
    Enrolled(EnrollmentReceipt, CatchUpReport),
    /// A WRITER invite was redeemed over the enrollment ALPN: the owner authored the grant.
    WriterGranted(WriterGrantReceipt),
    Refused(InviteError),
}

/// The contributor half of the writer exchange, over an open enrollment-ALPN stream.
pub async fn run_writer_grant_dialer<R, W>(
    recv: &mut R,
    send: &mut W,
    ticket: &InviteTicket,
    contributor_account: AccountId,
) -> Result<WriterGrantReceipt, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    ticket.expect_kind(InviteTicketKind::Writer)?;
    let request = WriterGrantRequest {
        nonce: ticket.nonce,
        expected_account: ticket.account_id,
        contributor_account,
    };
    let progress = ENROLL_UNAUTH_PROGRESS_TIMEOUT;
    write_blob(send, &request.encode(), MAX_ENROLL_REQUEST_FRAME, "grant request", progress)
        .await?;
    let response = WriterGrantResponse::decode(
        &read_blob(recv, MAX_ENROLL_RESPONSE_FRAME, "grant response", progress).await?,
    )?;
    // Ack before post-processing, exactly as enrollment does, so the acceptor can close.
    let ack_window = progress.min(RESPONSE_ACK_TIMEOUT);
    if write_within(send, &[RESPONSE_ACK], ack_window).await.is_ok() {
        let _ = flush_within(send, ack_window).await;
    }
    match response {
        WriterGrantResponse::Granted(receipt) => Ok(receipt),
        WriterGrantResponse::Refused(code) => Err(code.into_error()),
    }
}

pub async fn run_enrollment_acceptor<R, W, F>(
    recv: &mut R,
    send: &mut W,
    conn: &Connection,
    authenticated_remote_node: [u8; 32],
    now_ms: F,
) -> Result<EnrollmentAcceptorOutcome, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn() -> i64,
{
    run_enrollment_acceptor_with_progress(
        recv,
        send,
        conn,
        authenticated_remote_node,
        now_ms,
        ENROLL_PROGRESS_TIMEOUT,
    )
    .await
}

pub async fn run_enrollment_acceptor_with_progress<R, W, F>(
    recv: &mut R,
    send: &mut W,
    conn: &Connection,
    authenticated_remote_node: [u8; 32],
    now_ms: F,
    progress: Duration,
) -> Result<EnrollmentAcceptorOutcome, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn() -> i64,
{
    // The request and a refusal are reachable by ANY peer (a random nonce gets both): read and
    // write them under the tighter unauthenticated window so a bogus peer's hold on the serial
    // accept loop stays in seconds. The enrolled receipt — gated on a valid nonce — gets the
    // full progress window.
    let unauth = progress.min(ENROLL_UNAUTH_PROGRESS_TIMEOUT);
    let blob = read_blob(recv, MAX_ENROLL_REQUEST_FRAME, "request", unauth).await?;
    // The enrollment ALPN carries BOTH redemption kinds; the request's leading domain string
    // picks the flow, so one serve loop redeems pairings and writer invites alike.
    if request_blob_domain(&blob) == Some(GRANT_REQUEST_DOMAIN) {
        let request = WriterGrantRequest::decode(&blob)?;
        return match redeem_writer_invite(conn, &request, authenticated_remote_node, &now_ms) {
            Ok(receipt) => {
                write_blob(
                    send,
                    &WriterGrantResponse::Granted(receipt.clone()).encode(),
                    MAX_ENROLL_RESPONSE_FRAME,
                    "grant response",
                    unauth,
                )
                .await?;
                Ok(EnrollmentAcceptorOutcome::WriterGranted(receipt))
            },
            Err(error) if refusal_code(&error).is_some() => {
                let code = refusal_code(&error).expect("guarded above");
                write_blob(
                    send,
                    &WriterGrantResponse::Refused(code).encode(),
                    MAX_ENROLL_RESPONSE_FRAME,
                    "grant response",
                    unauth,
                )
                .await?;
                Ok(EnrollmentAcceptorOutcome::Refused(error))
            },
            Err(error) => Err(error),
        };
    }
    let request = EnrollmentRequest::decode(&blob)?;
    // Evaluate expiry only after the complete peer-controlled request arrives. A timestamp read
    // before this await would let a peer hold the stream open past expiry and still redeem.
    match redeem_invite(conn, request, authenticated_remote_node, &now_ms) {
        Ok((receipt, catch_up)) => {
            write_blob(
                send,
                &EnrollmentResponse::Enrolled(receipt.clone()).encode(),
                MAX_ENROLL_RESPONSE_FRAME,
                "response",
                progress,
            )
            .await?;
            Ok(EnrollmentAcceptorOutcome::Enrolled(receipt, catch_up))
        },
        Err(error) if refusal_code(&error).is_some() => {
            let code = refusal_code(&error).expect("guarded above");
            write_blob(
                send,
                &EnrollmentResponse::Refused(code).encode(),
                MAX_ENROLL_RESPONSE_FRAME,
                "response",
                unauth,
            )
            .await?;
            Ok(EnrollmentAcceptorOutcome::Refused(error))
        },
        Err(error) => Err(error),
    }
}

pub async fn run_enrollment_dialer<R, W>(
    recv: &mut R,
    send: &mut W,
    expected_account: AccountId,
    request: &EnrollmentRequest,
) -> Result<EnrollmentReceipt, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_enrollment_dialer_with_progress(
        recv,
        send,
        expected_account,
        request,
        ENROLL_PROGRESS_TIMEOUT,
    )
    .await
}

pub async fn run_enrollment_dialer_with_progress<R, W>(
    recv: &mut R,
    send: &mut W,
    expected_account: AccountId,
    request: &EnrollmentRequest,
    progress: Duration,
) -> Result<EnrollmentReceipt, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if request.expected_account != expected_account {
        return Err(InviteError::AccountMismatch);
    }
    write_blob(send, &request.encode(), MAX_ENROLL_REQUEST_FRAME, "request", progress).await?;
    let response = EnrollmentResponse::decode(
        &read_blob(recv, MAX_ENROLL_RESPONSE_FRAME, "response", progress).await?,
    )?;
    // Ack the response BEFORE any post-processing (verification, adoption): the byte tells the
    // acceptor the response reached the application, so it can close immediately instead of
    // waiting on us to finish local work and tear down.
    let ack_window = progress.min(RESPONSE_ACK_TIMEOUT);
    if write_within(send, &[RESPONSE_ACK], ack_window).await.is_ok() {
        let _ = flush_within(send, ack_window).await;
    }
    match response {
        EnrollmentResponse::Enrolled(receipt) => {
            verify_enrollment_device_add(
                &receipt.account_entries,
                expected_account,
                receipt.device_add_hash,
                &receipt.device_add_signed,
                request.ed25519_pubkey,
                request.x25519_pubkey,
            )
            .map_err(|error| {
                InviteError::Malformed(format!("invalid enrollment receipt: {error}"))
            })?;
            Ok(receipt)
        },
        EnrollmentResponse::Refused(code) => Err(code.into_error()),
    }
}

pub(super) async fn write_blob<W: AsyncWrite + Unpin>(
    send: &mut W,
    body: &[u8],
    max_len: u32,
    frame_name: &str,
    progress: Duration,
) -> Result<(), InviteError> {
    let len = ensure_frame_len(body.len(), max_len, frame_name)?;
    write_within(send, &len.to_be_bytes(), progress).await?;
    // Chunked so each slice gets its own progress window: a slow-reading peer back-pressures
    // `write_all`, and a monolithic write would turn the whole-exchange deadline into a
    // total-transfer deadline the peer controls.
    for chunk in body.chunks(ENROLL_PROGRESS_CHUNK_BYTES) {
        write_within(send, chunk, progress).await?;
    }
    flush_within(send, progress).await?;
    Ok(())
}

async fn flush_within<W: AsyncWrite + Unpin>(
    send: &mut W,
    window: Duration,
) -> Result<(), InviteError> {
    tokio::time::timeout(window, send.flush()).await.map_err(|_| {
        InviteError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "enrollment frame flush stalled",
        ))
    })??;
    Ok(())
}

async fn write_within<W: AsyncWrite + Unpin>(
    send: &mut W,
    bytes: &[u8],
    window: Duration,
) -> Result<(), InviteError> {
    tokio::time::timeout(window, send.write_all(bytes)).await.map_err(|_| {
        InviteError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "enrollment frame write stalled",
        ))
    })??;
    Ok(())
}

pub(super) async fn read_blob<R: AsyncRead + Unpin>(
    recv: &mut R,
    max_len: u32,
    frame_name: &str,
    progress: Duration,
) -> Result<Vec<u8>, InviteError> {
    let mut prefix = [0u8; 4];
    read_within(recv, &mut prefix, progress).await?;
    let len = ensure_frame_len(u32::from_be_bytes(prefix) as usize, max_len, frame_name)?;
    let mut body = vec![0; len as usize];
    for chunk in body.chunks_mut(ENROLL_PROGRESS_CHUNK_BYTES) {
        read_within(recv, chunk, progress).await?;
    }
    Ok(body)
}

async fn read_within<R: AsyncRead + Unpin>(
    recv: &mut R,
    buf: &mut [u8],
    window: Duration,
) -> Result<(), InviteError> {
    tokio::time::timeout(window, recv.read_exact(buf)).await.map_err(|_| {
        InviteError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "enrollment frame read stalled",
        ))
    })??;
    Ok(())
}
