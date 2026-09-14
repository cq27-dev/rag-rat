//! Enrollment over the endpoint: both dialers, the owner-side acceptor, and closing an
//! enrollment connection.

use iroh::endpoint::Connection as IrohConnection;
use iroh::{Endpoint, EndpointAddr};
use rag_rat_oplog::{AccountId, DeviceFingerprint};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use super::accept::GRACEFUL_CLOSE_TIMEOUT;
use crate::enrollment::{
    ENROLL_ALPN, EnrollmentAcceptorOutcome, EnrollmentReceipt, EnrollmentRequest, InviteError,
    InviteTicketKind, RESPONSE_ACK, RESPONSE_ACK_TIMEOUT, run_enrollment_acceptor,
    run_enrollment_dialer,
};
use crate::session::DEFAULT_IDLE_TIMEOUT;

/// Dial an owner over the dedicated enrollment ALPN, verify the founder-signed receipt, ingest its
/// account bootstrap, and adopt that accepted genesis locally before returning. The next Closed
/// account-log session can therefore mutually authorize without a fresh-device bootstrap deadlock.
pub async fn connect_and_enroll(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    database: &Connection,
    expected_account: AccountId,
    request: &EnrollmentRequest,
    now_ms: i64,
) -> Result<EnrollmentReceipt, InviteError> {
    // The budget and held-entry inventory are always recomputed from the local store — the
    // acceptor measures its exact receipt against them before consuming the nonce — so the
    // values the caller set are irrelevant.
    //
    // The declaration is a point-in-time read: a competing LOCAL write that shrinks capacity
    // before adoption could still burn the nonce. Do NOT fix that by holding the database's
    // writer reservation across the network exchange — a slow or malicious owner can stretch
    // the transfer (per-chunk progress windows) and starve every local writer past its busy
    // timeout. The caller MUST instead serialize capacity-consuming local writes across this
    // call — the same per-database sync-session lock `sync serve` and device sync already
    // respect — which the enrollment CLI flow holds for the whole enrollment.
    let mut request = request.clone();
    request.expected_account = expected_account;
    // The QUIC connection authenticates as THIS endpoint's transport identity, so the request
    // must name it — a caller-supplied value is either redundant or a guaranteed WrongNode.
    request.transport_node_id = *endpoint.id().as_bytes();
    request.budget = rag_rat_oplog::enrollment_budget(database, expected_account, now_ms)?;
    request.held_entry_hashes =
        rag_rat_oplog::held_account_entry_hashes(database, expected_account)?;
    validate_enrollment_request_identity(database, expected_account, &request, now_ms)?;
    let (conn, mut send, mut recv) = dial_enroll(endpoint, peer, InviteTicketKind::Pairing).await?;
    let receipt = run_enrollment_dialer(&mut recv, &mut send, expected_account, &request).await?;
    let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
        &receipt.account_entries,
        expected_account,
        receipt.device_add_hash,
        &receipt.device_add_signed,
        request.ed25519_pubkey,
        request.x25519_pubkey,
    )
    .map_err(|error| InviteError::Malformed(format!("invalid enrollment receipt: {error}")))?;
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into());
    rag_rat_oplog::adopt_enrollment_bootstrap(database, rag_rat_oplog::EnrollmentBootstrap {
        account_entries: &receipt.account_entries,
        account_id: expected_account,
        genesis_hash,
        device_fingerprint: fingerprint,
        device_add_hash: receipt.device_add_hash,
        now_ms,
    })?;
    // Best-effort maintenance AFTER the durable adoption: retry parked rows the receipt's keys
    // may now resolve. Kept out of the one-time adoption transaction so a newly resolvable
    // parked sibling can never enter its fold, and a maintenance failure cannot invalidate the
    // completed enrollment (normal sync retries the same queues later).
    if let Err(error) =
        rag_rat_oplog::retry_enrollment_pre_verify(database, expected_account, now_ms)
    {
        tracing::warn!(%error, "post-enrollment pre-verify retry failed");
    }
    conn.close(0u32.into(), b"done");
    Ok(receipt)
}

/// Dial the ticket's inviter and redeem a WRITER invite: the owner authors the grant naming
/// `contributor_account` and the receipt comes back with the grant id and target stream. The
/// contributor still has to pull the owner's log afterwards (the grant lives in the OWNER's
/// control log); the redeeming CLI does that inline over the same route.
pub async fn connect_and_redeem_writer(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    ticket: &crate::InviteTicket,
    contributor_account: AccountId,
) -> Result<crate::WriterGrantReceipt, InviteError> {
    let (conn, mut send, mut recv) = dial_enroll(endpoint, peer, InviteTicketKind::Writer).await?;
    let receipt = crate::enrollment::run_writer_grant_dialer(
        &mut recv,
        &mut send,
        ticket,
        contributor_account,
    )
    .await?;
    conn.close(0u32.into(), b"done");
    Ok(receipt)
}

/// Dial `peer` on [`ENROLL_ALPN`] and open the exchange stream for a `kind` redemption. Both waits
/// are peer-controlled, so each is bounded by [`DEFAULT_IDLE_TIMEOUT`] like every other dial here.
async fn dial_enroll(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    kind: InviteTicketKind,
) -> Result<(IrohConnection, iroh::endpoint::SendStream, iroh::endpoint::RecvStream), InviteError> {
    let (dial_timed_out, stream_timed_out) = match kind {
        InviteTicketKind::Pairing =>
            ("enrollment dial timed out", "opening enrollment stream timed out"),
        InviteTicketKind::Writer =>
            ("writer invite dial timed out", "opening invite stream timed out"),
    };
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, endpoint.connect(peer, ENROLL_ALPN))
        .await
        .map_err(|_| InviteError::Transport(dial_timed_out.into()))?
        .map_err(|error| InviteError::Transport(error.to_string()))?;
    let (send, recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| InviteError::Transport(stream_timed_out.into()))?
        .map_err(|error| InviteError::Transport(error.to_string()))?;
    Ok((conn, send, recv))
}

/// Refuse a request assembled for a different store before making network contact: redemption is
/// one-shot, and adopting a receipt whose device keys are not locally held would leave Closed sync
/// unable to authenticate or decrypt the delivered stream-key wraps.
pub(super) fn validate_enrollment_request_identity(
    database: &Connection,
    expected_account: AccountId,
    request: &EnrollmentRequest,
    now_ms: i64,
) -> Result<(), InviteError> {
    if let Some(existing_account) = rag_rat_oplog::read_local_account(database)?
        && existing_account != expected_account
    {
        return Err(InviteError::Malformed(
            "enrollment account does not match the store's existing local account".into(),
        ));
    }
    let local = rag_rat_oplog::local_device(database, now_ms)?;
    if request.ed25519_pubkey != local.ed25519_public_key() {
        return Err(InviteError::Malformed(
            "enrollment request ed25519 key does not match the local device identity".into(),
        ));
    }
    if request.x25519_pubkey != local.x25519_public_key() {
        return Err(InviteError::Malformed(
            "enrollment request X25519 key does not match the local device identity".into(),
        ));
    }
    Ok(())
}

/// Close an enrollment connection after the exchange. The dialer acks the response the moment it
/// is DECODED, so the ack byte is the delivery signal for both outcomes: once it lands, close
/// immediately — an enrolled receipt needs no graceful-close wait, and a refused peer
/// (unauthenticated; any random nonce reaches a refusal) is bounded by [`RESPONSE_ACK_TIMEOUT`]
/// instead of holding the serial accept loop for [`GRACEFUL_CLOSE_TIMEOUT`]. An invite-holding
/// peer that never acks still gets the graceful-close fallback so its streamed receipt lands.
async fn close_enrollment_connection(
    conn: iroh::endpoint::Connection,
    recv: &mut iroh::endpoint::RecvStream,
    enrolled: bool,
) {
    let mut ack = [0u8; 1];
    let delivered =
        matches!(timeout(RESPONSE_ACK_TIMEOUT, recv.read_exact(&mut ack)).await, Ok(Ok(_)))
            && ack == [RESPONSE_ACK];
    if enrolled && !delivered {
        let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    }
    conn.close(0u32.into(), b"done");
}

/// Close an enrollment connection once its acceptor exchange has an outcome: a served receipt or
/// grant keeps [`close_enrollment_connection`]'s graceful-close fallback, a refusal does not.
pub(super) async fn finish_enrollment_stream(
    conn: IrohConnection,
    recv: &mut iroh::endpoint::RecvStream,
    outcome: &EnrollmentAcceptorOutcome,
) {
    let served = matches!(
        outcome,
        EnrollmentAcceptorOutcome::Enrolled(..) | EnrollmentAcceptorOutcome::WriterGranted(..)
    );
    close_enrollment_connection(conn, recv, served).await;
}

/// Accept one owner-side enrollment connection and atomically redeem its invite.
pub async fn accept_enrollment(
    endpoint: &Endpoint,
    database: &Connection,
    now_ms: impl Fn() -> i64,
) -> Result<EnrollmentReceipt, InviteError> {
    let incoming =
        endpoint.accept().await.ok_or_else(|| InviteError::Transport("endpoint closed".into()))?;
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| InviteError::Transport("enrollment handshake timed out".into()))?
        .map_err(|error| InviteError::Transport(error.to_string()))?;
    if conn.alpn() != ENROLL_ALPN {
        conn.close(0u32.into(), b"wrong-alpn");
        return Err(InviteError::Malformed("connection did not negotiate enrollment ALPN".into()));
    }
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| InviteError::Transport("peer opened no enrollment stream".into()))?
        .map_err(|error| InviteError::Transport(error.to_string()))?;
    let outcome =
        run_enrollment_acceptor(&mut recv, &mut send, database, remote_node, now_ms).await?;
    finish_enrollment_stream(conn, &mut recv, &outcome).await;
    match outcome {
        EnrollmentAcceptorOutcome::Enrolled(receipt, _) => Ok(receipt),
        EnrollmentAcceptorOutcome::WriterGranted(_) => Err(InviteError::Malformed(
            "the connection redeemed a writer invite, not a pairing enrollment".into(),
        )),
        EnrollmentAcceptorOutcome::Refused(error) => Err(error),
    }
}

/// Whether `enrollment_database`'s local account is exactly `account_id` — see the ENROLL_ALPN
/// branch of [`dispatch_connection`](super::dispatch::dispatch_connection). A database with no
/// minted account cannot redeem anything, so it does not match either.
pub(super) fn enrollment_database_matches(
    enrollment_database: &Connection,
    account_id: [u8; 32],
) -> anyhow::Result<bool> {
    Ok(rag_rat_oplog::read_local_account(enrollment_database)?
        == Some(AccountId::from_bytes(account_id)))
}
