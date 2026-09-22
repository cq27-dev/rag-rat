//! Owner-side invite minting and one-time redemption against the durable `sync_invites` rows.

use std::mem::MaybeUninit;
use std::time::Duration;

use rag_rat_oplog::{
    AccountId, AuthoredDurability, AuthorityBoundary, AuthorityQuery, CatchUpReport,
    DeviceFingerprint, DeviceRole, EnrollingDevice, account_entries_for_enrollment,
    author_enrollment_device_add_in_tx, enroll_stream_keys_for_device_in_tx,
    enrollment_authoring_fits, load_local_device, owned_streams_for_account,
    owner_control_authority_in_snapshot, read_local_account, retry_enrollment_pre_verify,
    validate_device_add_label,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::error::InviteError;
use super::ticket::{InviteTicket, InviteTicketKind, validate_enrollment_route};
use super::wire::{
    EnrollmentReceipt, EnrollmentRequest, EnrollmentResponse, MAX_ENROLL_BOOTSTRAP_ENTRIES,
    MAX_ENROLL_RESPONSE_FRAME, WriterGrantReceipt, WriterGrantRequest, ensure_frame_len,
};

pub(super) const RECEIPT_REPLAY_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

pub struct InviteSpec<'a> {
    pub account_id: AccountId,
    pub inviter_node_id: [u8; 32],
    pub relay_url: String,
    pub role: DeviceRole,
    pub label: Option<&'a str>,
    /// Mint clock. Read ONCE after the writer transaction is acquired — a pre-lock timestamp can
    /// go stale behind SQLite's busy timeout and would mint an already-expired ticket.
    pub now_ms: &'a dyn Fn() -> i64,
    pub ttl: Duration,
}

struct StoredInvite {
    account_bytes: Vec<u8>,
    /// Parsed once at the DB boundary; an unknown token remains loadable.
    kind: Result<StoredInviteKind, String>,
    /// The stream a WRITER invite grants on (`Some` exactly for `role = 'writer'` rows).
    stream_id: Option<Vec<u8>>,
    label: Option<String>,
    expires_at_ms: i64,
    used_at_ms: Option<i64>,
    used_transport_node: Option<Vec<u8>>,
    used_ed25519_pubkey: Option<Vec<u8>>,
    used_x25519_pubkey: Option<Vec<u8>>,
    receipt_hash: Option<Vec<u8>>,
    receipt_signed: Option<Vec<u8>>,
    receipt_entries: Option<Vec<u8>>,
    /// Legacy full-receipt copy (pre-V092). Never written anymore; retained so invites consumed
    /// before V092 keep replaying through their 24h window. The manifest form is preferred.
    receipt_bytes: Option<Vec<u8>>,
}

impl StoredInvite {
    /// An unrecognized token fails only the flow that must read a device role from it;
    /// a writer screen keeps refusing it `Unknown`.
    fn kind(&self) -> anyhow::Result<StoredInviteKind> {
        self.kind.as_ref().copied().map_err(|message| anyhow::anyhow!("{message}"))
    }
}

/// What a `sync_invites` row redeems into. Its persisted `role` column holds the three
/// [`DeviceRole`] tokens for a device pairing plus `writer` for a writer grant — a domain wider
/// than `DeviceRole`, so a writer row must never reach `DeviceRole::from_db_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredInviteKind {
    Pairing(DeviceRole),
    Writer,
}

impl StoredInviteKind {
    const WRITER: &str = "writer";

    fn as_db_str(self) -> &'static str {
        match self {
            Self::Pairing(role) => role.as_db_str(),
            Self::Writer => Self::WRITER,
        }
    }

    fn from_db_str(value: &str) -> anyhow::Result<Self> {
        if value == Self::WRITER {
            Ok(Self::Writer)
        } else {
            DeviceRole::from_db_str(value).map(Self::Pairing)
        }
    }
}

pub fn mint_invite(conn: &Connection, spec: InviteSpec<'_>) -> Result<InviteTicket, InviteError> {
    let InviteSpec { account_id, inviter_node_id, relay_url, role, label, now_ms, ttl } = spec;
    validate_enrollment_route(&inviter_node_id, &relay_url)?;
    validate_device_add_label(label).map_err(|error| InviteError::Malformed(error.to_string()))?;
    let ttl_ms = i64::try_from(ttl.as_millis())
        .map_err(|_| InviteError::Malformed("invite TTL is too large".into()))?;
    // A zero or sub-millisecond TTL mints a ticket that is already expired — redemption treats
    // `now_ms >= expires_at_ms` as expired, so this is a deterministic failure that must not
    // cross the invite-issuance boundary.
    if ttl_ms == 0 {
        return Err(InviteError::Malformed("invite TTL must be at least one millisecond".into()));
    }
    let mut nonce_bytes = [MaybeUninit::uninit(); 32];
    let nonce = getrandom::fill_uninit(&mut nonce_bytes)
        .map_err(|error| InviteError::Storage(anyhow::anyhow!("invite entropy: {error}")))?
        .try_into()
        .map(|nonce: &mut [u8; 32]| *nonce)
        .expect("the fixed-size destination preserves its length");
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    // BEGIN IMMEDIATE can wait out the busy timeout behind another writer: read the mint clock
    // ONLY NOW, or a ticket minted after a long lock wait would carry a pre-wait expiry and be
    // born already unusable (mirrors redemption's post-lock clock, #945).
    let now_ms = now_ms();
    let expires_at_ms = now_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| InviteError::Malformed("invite expiry overflows i64".into()))?;
    require_founder_enrollment_authority(&tx, account_id)?;
    // The candidate store is grow-only — capacity never drains — so if it cannot fit THIS
    // redemption's DeviceAdd plus its stream-key wraps, the ticket would be permanently
    // unusable: a deterministic failure that must gate the invite-issuance boundary (#945),
    // checked in the same commit snapshot as the authority preflight. Minting then RESERVES
    // that exact requirement against the shared candidate counters, so ordinary ingest or a
    // second mint cannot consume the headroom this ticket was measured against; the reservation
    // is released only by redemption (under the writer lock) or expiry.
    let streams = owned_streams_for_account(&tx, account_id)?;
    enrollment_authoring_fits(&tx, account_id, &streams, role, label)?;
    let (reserved_entries, reserved_bytes) =
        rag_rat_oplog::enrollment_authoring_requirements(&tx, account_id, &streams, role, label)?;
    rag_rat_oplog::upsert_account_candidate_reservation_in_tx(
        &tx,
        account_id,
        nonce,
        reserved_entries,
        reserved_bytes,
        reserved_entries.saturating_sub(1),
        expires_at_ms,
    )?;
    prune_expired_invites_in_tx(&tx, now_ms)?;
    tx.execute(
        "INSERT INTO sync_invites(
             nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![
            nonce.as_slice(),
            account_id.to_bytes().as_slice(),
            StoredInviteKind::Pairing(role).as_db_str(),
            label,
            expires_at_ms,
            now_ms,
        ],
    )
    .map_err(|error| InviteError::Storage(error.into()))?;
    // Stamp the pin the account ACTUALLY carries, read in the mint transaction — never a caller
    // argument, so a caller can neither forget it nor forge one. `None` while the account is
    // unpinned, which is every ticket today: minting is still refused under a pin until the
    // enrollment gates open.
    let checkpoint_digest = match rag_rat_oplog::account_control_policy(&tx, account_id)
        .map_err(InviteError::Storage)?
    {
        rag_rat_oplog::AccountControlPolicy::LegacyV1 => None,
        rag_rat_oplog::AccountControlPolicy::ControlV2(pin)
        | rag_rat_oplog::AccountControlPolicy::UnsupportedVersion(pin) =>
            Some(pin.checkpoint_digest),
    };
    tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
    Ok(InviteTicket {
        kind: InviteTicketKind::Pairing,
        account_id,
        inviter_node_id,
        relay_url,
        nonce,
        expires_at_ms,
        checkpoint_digest,
    })
}

fn require_founder_enrollment_authority(
    conn: &Connection,
    account_id: AccountId,
) -> Result<(), InviteError> {
    rag_rat_oplog::require_supported_account_control(conn, account_id)
        .map_err(InviteError::from)?;
    if read_local_account(conn)
        .map_err(InviteError::from)?
        .filter(|local| *local == account_id)
        .is_none()
    {
        return Err(InviteError::Storage(anyhow::anyhow!(
            "invite account is not the local account"
        )));
    }
    let local_device = load_local_device(conn)
        .map_err(InviteError::from)?
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("local device identity is missing")))?;
    let genesis_bytes: Vec<u8> = conn
        .query_row("SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0", [], |row| {
            row.get(0)
        })
        .map_err(|error| InviteError::Storage(error.into()))?;
    let genesis_hash = genesis_bytes.try_into().map_err(|_| {
        InviteError::Storage(anyhow::anyhow!("local account genesis hash is not 32 bytes"))
    })?;
    let authority = owner_control_authority_in_snapshot(
        conn,
        account_id,
        genesis_hash,
        local_device.fingerprint(),
    )
    .map_err(InviteError::from)?;
    if !matches!(
        authority,
        AuthorityQuery::Effective(authority)
            if authority.device_boundary == AuthorityBoundary::Open
                && authority.incarnation_boundary == AuthorityBoundary::Open
    ) {
        return Err(InviteError::Storage(anyhow::anyhow!(
            "local device lacks open founder authority to enroll devices"
        )));
    }
    Ok(())
}

/// Everything the owner needs to mint a WRITER invite (`sync invite-writer`). Unlike enrollment,
/// no candidate capacity is reserved at mint: redemption authors ONE small account entry inside
/// the same transaction that consumes the nonce, so a capacity failure rolls the whole redemption
/// back with the nonce intact — a deterministic retry, not a stranded ticket.
pub struct WriterInviteSpec<'a> {
    pub account_id: AccountId,
    /// The `PublicRead` stream the redeemed grant targets, resolved at mint — the serving
    /// acceptor has no repo scope at redemption.
    pub stream_id: [u8; 32],
    pub inviter_node_id: [u8; 32],
    pub relay_url: String,
    /// Mint clock; read once after the writer transaction is acquired (see [`InviteSpec`]).
    pub now_ms: &'a dyn Fn() -> i64,
    pub ttl: Duration,
}

/// Mint a one-time WRITER invite and return its ticket. The grant itself is authored at
/// redemption — the grantee account is not known yet; that is the whole point of the ticket.
pub fn mint_writer_invite(
    conn: &Connection,
    spec: WriterInviteSpec<'_>,
) -> Result<InviteTicket, InviteError> {
    let WriterInviteSpec { account_id, stream_id, inviter_node_id, relay_url, now_ms, ttl } = spec;
    validate_enrollment_route(&inviter_node_id, &relay_url)?;
    let ttl_ms = i64::try_from(ttl.as_millis())
        .map_err(|_| InviteError::Malformed("invite TTL is too large".into()))?;
    if ttl_ms == 0 {
        return Err(InviteError::Malformed("invite TTL must be at least one millisecond".into()));
    }
    let mut nonce_bytes = [MaybeUninit::uninit(); 32];
    let nonce = getrandom::fill_uninit(&mut nonce_bytes)
        .map_err(|error| InviteError::Storage(anyhow::anyhow!("invite entropy: {error}")))?
        .try_into()
        .map(|nonce: &mut [u8; 32]| *nonce)
        .expect("the fixed-size destination preserves its length");
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    // Post-lock clock, as in [`mint_invite`]: a ticket minted after a long lock wait must not be
    // born expired.
    let now_ms = now_ms();
    let expires_at_ms = now_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| InviteError::Malformed("invite expiry overflows i64".into()))?;
    require_founder_enrollment_authority(&tx, account_id)?;
    prune_expired_invites_in_tx(&tx, now_ms)?;
    tx.execute(
        "INSERT INTO sync_invites(
             nonce, account_id, role, stream_id, expires_at_ms, created_at_ms, used_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![
            nonce.as_slice(),
            account_id.to_bytes().as_slice(),
            StoredInviteKind::Writer.as_db_str(),
            stream_id.as_slice(),
            expires_at_ms,
            now_ms,
        ],
    )
    .map_err(|error| InviteError::Storage(error.into()))?;
    tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
    Ok(InviteTicket {
        kind: InviteTicketKind::Writer,
        account_id,
        inviter_node_id,
        relay_url,
        nonce,
        expires_at_ms,
        // A writer grant is cross-account and pins nothing; decode refuses one that carries a
        // digest, so this is the only value it may take.
        checkpoint_digest: None,
    })
}

/// Redeem a WRITER invite on the owner side: author the `StreamGrant` naming the contributor and
/// consume the nonce, atomically — a failed authoring rolls back with the nonce intact. A replay
/// of the SAME redemption (nonce + contributor) inside the retention window returns the same
/// receipt, so a dialer that lost the response can retry; any other reuse refuses `Used`.
pub fn redeem_writer_invite(
    conn: &Connection,
    request: &WriterGrantRequest,
    authenticated_remote_node: [u8; 32],
    now_ms: &dyn Fn() -> i64,
) -> Result<WriterGrantReceipt, InviteError> {
    let locked = match screen_under_writer_lock(
        conn,
        request.nonce,
        now_ms,
        |_| Ok(()),
        |conn, invite, at_ms| screen_writer_invite(conn, request, invite, at_ms),
    )? {
        LockedRedemption::Replay(receipt) => return Ok(receipt),
        LockedRedemption::Proceed(locked) => *locked,
    };
    let LockedInvite { ref tx, commit_ms, .. } = locked;
    let stream_id = writer_invite_stream(&locked.invite)?;
    let grant_id = rag_rat_oplog::author_stream_grant_in_tx(
        tx,
        rag_rat_oplog::StreamId::from_bytes(stream_id),
        request.contributor_account,
        rag_rat_oplog::GrantRole::Writer,
        commit_ms,
    )
    .map_err(|error| InviteError::Storage(anyhow::anyhow!("authoring the grant: {error:#}")))?;
    tx.execute(
        "UPDATE sync_invites SET used_at_ms = ?2, used_transport_node = ?3, receipt_hash = ?4
         WHERE nonce = ?1",
        params![
            request.nonce.as_slice(),
            commit_ms,
            authenticated_remote_node.as_slice(),
            grant_id.as_slice(),
        ],
    )
    .map_err(|error| InviteError::Storage(error.into()))?;
    locked.tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
    Ok(WriterGrantReceipt { grant_id: grant_id.into(), stream_id })
}

/// The deterministic writer-redemption refusals, evaluated identically before and inside the
/// writer transaction: role/account binding, self-grant, and expiry. Replay is handled
/// separately — a used nonce reaches here only when it is NOT a same-redemption replay.
fn writer_redeem_preflight(
    invite: &StoredInvite,
    request: &WriterGrantRequest,
    now_ms: i64,
) -> Result<(), InviteError> {
    // An enrollment nonce presented to the grant flow is as unknown as a random one — do not
    // leak which flow a guessed nonce belongs to.
    if !matches!(invite.kind(), Ok(StoredInviteKind::Writer)) {
        return Err(InviteError::Unknown);
    }
    if request.expected_account != stored_invite_account(invite)? {
        return Err(InviteError::AccountMismatch);
    }
    // The fold rejects a self-grant as ineffective; refusing it here keeps the nonce alive
    // instead of burning the exchange on an authoring failure.
    if request.contributor_account == stored_invite_account(invite)? {
        return Err(InviteError::AccountMismatch);
    }
    if invite.used_at_ms.is_some() {
        return Err(InviteError::Used);
    }
    if now_ms >= invite.expires_at_ms {
        return Err(InviteError::Expired);
    }
    Ok(())
}

/// The stream a writer invite grants on — `Some` exactly for writer rows by the V116 CHECK.
fn writer_invite_stream(invite: &StoredInvite) -> Result<[u8; 32], InviteError> {
    invite
        .stream_id
        .as_deref()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("writer invite has no stream id")))
}

/// Same-redemption replay: the nonce was consumed, and the grant it authored names exactly this
/// contributor — return the original receipt so a dialer that lost the response can retry within
/// the retention window. Any other contributor gets `Used` from the preflight.
fn writer_replay_receipt(
    conn: &Connection,
    invite: &StoredInvite,
    request: &WriterGrantRequest,
) -> Result<Option<WriterGrantReceipt>, InviteError> {
    if !matches!(invite.kind(), Ok(StoredInviteKind::Writer)) || invite.used_at_ms.is_none() {
        return Ok(None);
    }
    let Some(grant_id) =
        invite.receipt_hash.as_deref().and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return Ok(None);
    };
    let grantee: Option<Vec<u8>> = conn
        .query_row(
            "SELECT grantee_account_id FROM account_stream_grants WHERE grant_id = ?1",
            [grant_id.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| InviteError::Storage(error.into()))?;
    if grantee.as_deref() != Some(request.contributor_account.to_bytes().as_slice()) {
        return Ok(None);
    }
    Ok(Some(WriterGrantReceipt { grant_id, stream_id: writer_invite_stream(invite)? }))
}

pub fn redeem_invite(
    conn: &Connection,
    request: EnrollmentRequest,
    authenticated_remote_node: [u8; 32],
    now_ms: &dyn Fn() -> i64,
) -> Result<(EnrollmentReceipt, CatchUpReport), InviteError> {
    if request.transport_node_id != authenticated_remote_node {
        return Err(InviteError::WrongNode);
    }
    let locked = match screen_under_writer_lock(
        conn,
        request.nonce,
        now_ms,
        |invite| {
            // A writer nonce presented to the pairing flow is as unknown as a random one, refused
            // ahead of the account check exactly as the grant flow refuses a pairing nonce, so
            // neither flow reveals which one a guessed nonce belongs to.
            if matches!(invite.kind(), Ok(StoredInviteKind::Writer)) {
                return Err(InviteError::Unknown);
            }
            ensure_expected_account(&request, invite)
        },
        |conn, invite, at_ms| screen_invite(conn, &request, invite, at_ms),
    )? {
        LockedRedemption::Replay(receipt) => return Ok((receipt, empty_catch_up(&request))),
        LockedRedemption::Proceed(locked) => *locked,
    };
    let LockedInvite { ref tx, account_id, commit_ms, .. } = locked;
    let role = match locked.invite.kind()? {
        StoredInviteKind::Pairing(role) => role,
        // Refused after each row load above; kept as the same refusal rather than a panic.
        StoredInviteKind::Writer => return Err(InviteError::Unknown),
    };
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into());
    // Release THIS invite's reservation under the writer lock, then RE-MEASURE the mandatory
    // requirement against current state: key targets may have grown since minting, and the
    // reservation covered only the mint-time set. The fits check runs with our reservation
    // released (other outstanding invites' reservations still count), so it passes only if the
    // DeviceAdd plus the CURRENT wraps genuinely fit — a shortfall rolls back, preserving the
    // nonce and restoring the reservation instead of stranding the ticket mid-redemption.
    rag_rat_oplog::release_account_candidate_reservation_in_tx(tx, request.nonce)?;
    // Resolve ownership in this same redemption snapshot. A long-running server can ingest
    // StreamOwn/StreamRevoke entries after startup; caching this set would either omit a newly
    // owned stream's key wrap or make a stale, no-longer-owned stream abort the whole enrollment.
    let streams = owned_streams_for_account(tx, account_id)?;
    enrollment_authoring_fits(tx, account_id, &streams, role, locked.invite.label.as_deref())?;
    let device_add = author_enrollment_device_add_in_tx(
        tx,
        EnrollingDevice {
            ed25519_pubkey: request.ed25519_pubkey,
            x25519_pubkey: request.x25519_pubkey,
            label: locked.invite.label,
        },
        role,
        commit_ms,
    )?;
    let device_add_signed = tx
        .query_row(
            "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
            [device_add.as_slice()],
            |row| row.get(0),
        )
        .map_err(|error| InviteError::Storage(error.into()))?;
    let catch_up = enroll_stream_keys_for_device_in_tx(tx, fingerprint, &streams, commit_ms)?;
    let bootstrap_entries = account_entries_for_enrollment(tx, account_id)?;
    // Every candidate the joiner claims to hold MUST be one the owner's authenticated snapshot
    // also holds. An unrepresented hash means the joiner carries history this receipt cannot
    // reconcile (a competing control branch, or a false claim); adoption would refold the union
    // of the receipt and those grow-only extras and could leave the acknowledged DeviceAdd
    // ineffective — burning the nonce on a bootstrap that can never succeed. Refuse BEFORE the
    // consume boundary, rolling back the authored entries and restoring the reservation.
    let receipt_hashes: std::collections::HashSet<[u8; 32]> =
        bootstrap_entries.iter().map(|entry| entry.entry_hash.to_bytes()).collect();
    if request.held_entry_hashes.iter().any(|hash| !receipt_hashes.contains(hash)) {
        return Err(InviteError::HeldStateConflict);
    }
    let account_entries =
        bootstrap_entries.iter().map(|entry| entry.signed_bytes.clone()).collect();
    let receipt = EnrollmentReceipt {
        device_add_hash: device_add.into(),
        device_add_signed,
        account_entries,
    };
    // The receipt must FIT the state the joiner declared: candidate capacity is grow-only, so
    // consuming the one-time nonce for a receipt the joiner can never hold would burn the
    // enrollment — a deterministic failure checked BEFORE the consume boundary (#945).
    //
    // The charge is the joiner's ACTUAL adoption cost, not the receipt's raw size:
    // - entries whose hash the joiner already holds are FREE (`insert_candidate` returns
    //   `AlreadyPresent`) — only the confirmed intersection the joiner proved is credited;
    // - every NEW authenticated candidate is charged against the declared candidate budgets.
    let held: std::collections::HashSet<&[u8; 32]> = request.held_entry_hashes.iter().collect();
    let mut new_entries = 0u64;
    let mut new_bytes = 0u64;
    for entry in &bootstrap_entries {
        if held.contains(&entry.entry_hash.to_bytes()) {
            continue;
        }
        new_entries += 1;
        new_bytes += entry.signed_bytes.len() as u64;
    }
    let budget = &request.budget;
    if new_entries > budget.account_entries_remaining
        || new_entries > budget.global_entries_remaining
        || new_bytes > budget.account_bytes_remaining
        || new_bytes > budget.global_bytes_remaining
    {
        return Err(InviteError::JoinerCapacity);
    }
    ensure_frame_len(
        EnrollmentResponse::Enrolled(receipt.clone()).encode().len(),
        MAX_ENROLL_RESPONSE_FRAME,
        "response",
    )?;
    // Only the joiner-specific DeviceAdd plus the manifest of receipt entry hashes is persisted:
    // the bootstrap is already durable in the grow-only candidate DAG, and replay reconstructs
    // the EXACT acknowledged receipt from it rather than storing one full copy per invite
    // (quadratic across a fleet, #945).
    let mut receipt_manifest = Vec::with_capacity(32 * bootstrap_entries.len());
    for entry in &bootstrap_entries {
        receipt_manifest.extend_from_slice(entry.entry_hash.as_slice());
    }
    let changed = tx
        .execute(
            "UPDATE sync_invites
                SET used_at_ms = ?2,
                    used_transport_node = ?3,
                    used_ed25519_pubkey = ?4,
                    used_x25519_pubkey = ?5,
                    receipt_hash = ?6,
                    receipt_signed = ?7,
                    receipt_entries = ?8
              WHERE nonce = ?1 AND used_at_ms IS NULL AND expires_at_ms > ?2",
            params![
                request.nonce.as_slice(),
                commit_ms,
                request.transport_node_id.as_slice(),
                request.ed25519_pubkey.as_slice(),
                request.x25519_pubkey.as_slice(),
                receipt.device_add_hash.as_slice(),
                receipt.device_add_signed,
                receipt_manifest,
            ],
        )
        .map_err(|error| InviteError::Storage(error.into()))?;
    if changed != 1 {
        return Err(InviteError::Used);
    }
    locked.tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
    if let Err(error) = retry_enrollment_pre_verify(conn, account_id, commit_ms) {
        tracing::warn!(%error, "post-enrollment pre-verify retry failed");
    }
    Ok((receipt, catch_up))
}

/// How a redemption screen resolved an invite. Each redemption screens twice, identically: once
/// before the writer lock, so a random nonce is refused without the database-wide reservation, and
/// again after BEGIN IMMEDIATE against the re-read clock.
enum Screened<R> {
    /// A same-redemption replay inside the retention window: answer with the original receipt.
    Replay(R),
    /// A consumed nonce past its replay retention: prune it and answer `Used`, never a receipt.
    ReplayExpired,
    /// A live invite past every deterministic refusal: redeem it.
    Proceed(Box<StoredInvite>),
}

/// Where [`screen_under_writer_lock`] left a redemption: answered by a same-redemption replay, or
/// holding the writer lock over a live invite that only its own authoring remains for.
enum LockedRedemption<'c, R> {
    Replay(R),
    Proceed(Box<LockedInvite<'c>>),
}

/// A live invite past both screens, under the writer lock, with expired rows pruned and its
/// account confirmed as the local one.
///
/// Field order is drop order: `tx` rolls back (or has already committed) before `_durability`
/// restores the connection's synchronous setting, which must never happen inside an open
/// transaction. Callers borrow or move single fields and never destructure the whole into owned
/// bindings, whose drop order would follow the pattern instead.
struct LockedInvite<'c> {
    tx: Transaction<'c>,
    _durability: AuthoredDurability<'c>,
    invite: StoredInvite,
    account_id: AccountId,
    commit_ms: i64,
}

/// The control flow both redemptions share up to their own authoring. The invite is screened once
/// BEFORE the writer lock, so a random unauthenticated nonce is refused without SQLite's
/// database-wide writer reservation, then re-loaded and re-screened after BEGIN IMMEDIATE against
/// a re-read clock. A same-redemption replay answers with its receipt at either screen; a consumed
/// nonce past its retention is pruned and refused `Used`.
///
/// `after_load` runs straight after each row load. Before the lock that is ahead of the arrival
/// clock, which is read AFTER the lookup so a lookup crossing the replay deadline is judged at its
/// end (a stale earlier sample could replay a receipt the deadline has already retired).
/// Enrollment checks the request's account there, so a wrong-account request never reads the
/// clock. The writer flow checks nothing there: its account check sits in its screen, behind the
/// kind check, because a pairing nonce presented to the grant flow must be refused `Unknown`
/// before any account comparison could reveal which flow it belongs to. Its arrival clock read
/// ahead of that check only times the replay and expiry screens and never changes which refusal
/// wins.
fn screen_under_writer_lock<'c, R>(
    conn: &'c Connection,
    nonce: [u8; 32],
    now_ms: &dyn Fn() -> i64,
    after_load: impl Fn(&StoredInvite) -> Result<(), InviteError>,
    screen: impl Fn(&Connection, StoredInvite, i64) -> Result<Screened<R>, InviteError>,
) -> Result<LockedRedemption<'c, R>, InviteError> {
    let initial = {
        let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)
            .map_err(|error| InviteError::Storage(error.into()))?;
        let invite = load_invite(&read_tx, nonce)?;
        after_load(&invite)?;
        let arrival_ms = now_ms();
        (screen(&read_tx, invite, arrival_ms)?, arrival_ms)
    };
    let (initial, arrival_ms) = initial;
    match initial {
        Screened::Replay(receipt) => return Ok(LockedRedemption::Replay(receipt)),
        Screened::ReplayExpired => {
            prune_expired_invites(conn, arrival_ms)?;
            return Err(InviteError::Used);
        },
        Screened::Proceed(_) => {},
    }
    let durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    // BEGIN IMMEDIATE can wait out the busy timeout behind another writer: re-read the clock
    // NOW that the writer lock is held, or an invite that expired during the wait would be
    // consumed against the stale pre-wait timestamp.
    let commit_ms = now_ms();
    let invite = load_invite(&tx, nonce)?;
    after_load(&invite)?;
    let invite = match screen(&tx, invite, commit_ms)? {
        Screened::Replay(receipt) => return Ok(LockedRedemption::Replay(receipt)),
        Screened::ReplayExpired => {
            prune_expired_invites_in_tx(&tx, commit_ms)?;
            tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
            return Err(InviteError::Used);
        },
        Screened::Proceed(invite) => *invite,
    };
    let account_id = stored_invite_account(&invite)?;
    prune_expired_invites_in_tx(&tx, commit_ms)?;
    if read_local_account(&tx)
        .map_err(InviteError::from)?
        .filter(|local| *local == account_id)
        .is_none()
    {
        return Err(InviteError::Storage(anyhow::anyhow!(
            "invite account is not the local account"
        )));
    }
    Ok(LockedRedemption::Proceed(Box::new(LockedInvite {
        tx,
        _durability: durability,
        invite,
        account_id,
        commit_ms,
    })))
}

/// A redemption's invite row, `Unknown` for a nonce this store never minted. Before the writer
/// lock the row is loaded before the arrival clock is read; under the lock the commit clock is
/// read first and the row is re-loaded after it.
fn load_invite(conn: &Connection, nonce: [u8; 32]) -> Result<StoredInvite, InviteError> {
    stored_invite(conn, nonce)?.ok_or(InviteError::Unknown)
}

/// Refuse an enrollment whose expected account is not the invite's. Runs straight after each
/// [`load_invite`], ahead of the arrival clock, so a wrong-account request never reads the clock.
fn ensure_expected_account(
    request: &EnrollmentRequest,
    invite: &StoredInvite,
) -> Result<(), InviteError> {
    if request.expected_account != stored_invite_account(invite)? {
        return Err(InviteError::AccountMismatch);
    }
    Ok(())
}

/// The enrollment screen over a loaded, account-checked invite: the bounded replay window, an
/// exact-request replay, then expiry.
fn screen_invite(
    conn: &Connection,
    request: &EnrollmentRequest,
    invite: StoredInvite,
    at_ms: i64,
) -> Result<Screened<EnrollmentReceipt>, InviteError> {
    rag_rat_oplog::require_supported_account_control(conn, stored_invite_account(&invite)?)
        .map_err(InviteError::from)?;
    if receipt_replay_expired(&invite, at_ms) {
        return Ok(Screened::ReplayExpired);
    }
    if let Some(receipt) = replay_receipt(conn, &invite, request)? {
        return Ok(Screened::Replay(receipt));
    }
    if at_ms >= invite.expires_at_ms {
        return Err(InviteError::Expired);
    }
    Ok(Screened::Proceed(Box::new(invite)))
}

/// The writer-invite screen over a loaded invite: the replay window bounded exactly as
/// enrollment's, a same-redemption replay, then the writer preflight refusals.
fn screen_writer_invite(
    conn: &Connection,
    request: &WriterGrantRequest,
    invite: StoredInvite,
    at_ms: i64,
) -> Result<Screened<WriterGrantReceipt>, InviteError> {
    rag_rat_oplog::require_supported_account_control(conn, stored_invite_account(&invite)?)
        .map_err(InviteError::from)?;
    rag_rat_oplog::require_supported_account_control(conn, request.contributor_account)
        .map_err(InviteError::from)?;
    if receipt_replay_expired(&invite, at_ms) {
        return Ok(Screened::ReplayExpired);
    }
    if let Some(receipt) = writer_replay_receipt(conn, &invite, request)? {
        return Ok(Screened::Replay(receipt));
    }
    writer_redeem_preflight(&invite, request, at_ms)?;
    Ok(Screened::Proceed(Box::new(invite)))
}

fn stored_invite(conn: &Connection, nonce: [u8; 32]) -> Result<Option<StoredInvite>, InviteError> {
    conn.query_row(
        "SELECT account_id, role, stream_id, label, expires_at_ms, used_at_ms,
                used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
                receipt_hash, receipt_signed, receipt_entries, receipt_bytes
           FROM sync_invites WHERE nonce = ?1",
        [nonce.as_slice()],
        |row| {
            let role: String = row.get(1)?;
            let kind = StoredInviteKind::from_db_str(&role).map_err(|error| error.to_string());
            Ok(StoredInvite {
                account_bytes: row.get(0)?,
                kind,
                stream_id: row.get(2)?,
                label: row.get(3)?,
                expires_at_ms: row.get(4)?,
                used_at_ms: row.get(5)?,
                used_transport_node: row.get(6)?,
                used_ed25519_pubkey: row.get(7)?,
                used_x25519_pubkey: row.get(8)?,
                receipt_hash: row.get(9)?,
                receipt_signed: row.get(10)?,
                receipt_entries: row.get(11)?,
                receipt_bytes: row.get(12)?,
            })
        },
    )
    .optional()
    .map_err(|error| InviteError::Storage(error.into()))
}

fn stored_invite_account(invite: &StoredInvite) -> Result<AccountId, InviteError> {
    Ok(AccountId::from_bytes(invite.account_bytes.as_slice().try_into().map_err(|_| {
        InviteError::Storage(anyhow::anyhow!("sync_invites account_id is not 32 bytes"))
    })?))
}

fn replay_receipt(
    conn: &Connection,
    invite: &StoredInvite,
    request: &EnrollmentRequest,
) -> Result<Option<EnrollmentReceipt>, InviteError> {
    if invite.used_at_ms.is_none() {
        return Ok(None);
    }
    let same_request = invite.used_transport_node.as_deref()
        == Some(request.transport_node_id.as_slice())
        && invite.used_ed25519_pubkey.as_deref() == Some(request.ed25519_pubkey.as_slice())
        && invite.used_x25519_pubkey.as_deref() == Some(request.x25519_pubkey.as_slice());
    if !same_request {
        return Err(InviteError::Used);
    }
    let device_add_hash: [u8; 32] = invite
        .receipt_hash
        .as_deref()
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("used invite has no receipt hash")))?
        .try_into()
        .map_err(|_| {
            InviteError::Storage(anyhow::anyhow!("stored receipt hash is not 32 bytes"))
        })?;
    let device_add_signed = invite
        .receipt_signed
        .clone()
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("used invite has no DeviceAdd")))?;
    let account_id = stored_invite_account(invite)?;
    let receipt = if let Some(manifest) = invite.receipt_entries.as_deref() {
        if manifest.len() % 32 != 0 || manifest.len() / 32 > MAX_ENROLL_BOOTSTRAP_ENTRIES as usize {
            return Err(InviteError::Storage(anyhow::anyhow!(
                "stored receipt manifest is not a bounded hash list"
            )));
        }
        // Reconstruct the EXACT acknowledged receipt from the grow-only candidate DAG: the
        // manifest pins the original entry set and order, so the capacity and frame checks the
        // original redemption measured stay valid no matter how much history arrived since —
        // the replay never ships an unadoptable or oversized response.
        let mut signed_by_hash: std::collections::HashMap<[u8; 32], Vec<u8>> =
            account_entries_for_enrollment(conn, account_id)?
                .into_iter()
                .map(|entry| (entry.entry_hash.to_bytes(), entry.signed_bytes))
                .collect();
        let mut account_entries = Vec::with_capacity(manifest.len() / 32);
        for &hash in manifest.as_chunks::<32>().0 {
            let bytes = signed_by_hash.remove(&hash).ok_or_else(|| {
                InviteError::Storage(anyhow::anyhow!(
                    "receipt entry missing from the candidate DAG"
                ))
            })?;
            account_entries.push(bytes);
        }
        EnrollmentReceipt { device_add_hash, device_add_signed, account_entries }
    } else {
        // Pre-V092 legacy form: the exact receipt was stored whole and replays as-is through the
        // remainder of its 24h window.
        let receipt_bytes = invite
            .receipt_bytes
            .as_deref()
            .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("used invite has no receipt")))?;
        let receipt = EnrollmentReceipt::decode(receipt_bytes)?;
        if receipt.device_add_hash != device_add_hash
            || receipt.device_add_signed != device_add_signed
        {
            return Err(InviteError::Storage(anyhow::anyhow!(
                "stored receipt replay columns disagree"
            )));
        }
        receipt
    };
    // A replay must not outlive the enrollment it replays: if the owner removed the device
    // after redemption, re-releasing the stored bootstrap and its stream-key wraps would arm a
    // revoked device and report success for an enrollment Closed sync can no longer authorize.
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into());
    let still_effective: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM account_roster_history
                  WHERE account_id = ?1
                    AND device_fingerprint = ?2
                    AND roster_ref = ?3
                    AND closed_at IS NULL
            )",
            params![
                account_id.to_bytes().as_slice(),
                fingerprint.to_bytes().as_slice(),
                receipt.device_add_hash.as_slice(),
            ],
            |row| row.get(0),
        )
        .map_err(|error| InviteError::Storage(error.into()))?;
    if !still_effective {
        return Err(InviteError::Revoked);
    }
    Ok(Some(receipt))
}

fn receipt_replay_expired(invite: &StoredInvite, now_ms: i64) -> bool {
    invite
        .used_at_ms
        .is_some_and(|used_at_ms| now_ms >= used_at_ms.saturating_add(RECEIPT_REPLAY_RETENTION_MS))
}

fn prune_expired_invites(conn: &Connection, now_ms: i64) -> Result<(), InviteError> {
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    prune_expired_invites_in_tx(&tx, now_ms)?;
    tx.commit().map_err(|error| InviteError::Storage(error.into()))
}

fn prune_expired_invites_in_tx(conn: &Connection, now_ms: i64) -> Result<(), InviteError> {
    let replay_cutoff_ms = now_ms.saturating_sub(RECEIPT_REPLAY_RETENTION_MS);
    conn.execute(
        "DELETE FROM sync_invites
          WHERE (used_at_ms IS NULL AND expires_at_ms <= ?1)
             OR (used_at_ms IS NOT NULL AND used_at_ms <= ?2)",
        params![now_ms, replay_cutoff_ms],
    )
    .map_err(|error| InviteError::Storage(error.into()))?;
    rag_rat_oplog::prune_account_candidate_reservations_in_tx(conn, now_ms)?;
    Ok(())
}

fn empty_catch_up(request: &EnrollmentRequest) -> CatchUpReport {
    CatchUpReport {
        target: DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into()),
        authored: Vec::new(),
        already_covered: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stored_invite_kind_round_trips_through_its_persisted_token() {
        for (kind, token) in [
            (StoredInviteKind::Pairing(DeviceRole::ReadOnly), "read_only"),
            (StoredInviteKind::Pairing(DeviceRole::Member), "member"),
            (StoredInviteKind::Pairing(DeviceRole::Owner), "owner"),
            (StoredInviteKind::Writer, "writer"),
        ] {
            assert_eq!(kind.as_db_str(), token);
            assert_eq!(StoredInviteKind::from_db_str(token).unwrap(), kind);
        }
        assert!(StoredInviteKind::from_db_str("admin").is_err());
    }
}
