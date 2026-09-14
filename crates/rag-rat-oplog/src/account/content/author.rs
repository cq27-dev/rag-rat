//! The in-tx `/3` content-author seam (sync phase C3.4b-i, #663).
//!
//! The local writer's counterpart to the `/1` trio in [`crate::store`]
//! (`author_in_tx` / `author_batch_in_tx` / `author_genesis_in_tx`): it authors a batch of
//! [`MemoryOp`]s as **owner-authored** `/3` content on one `/2` stream, inside the caller's
//! IMMEDIATE transaction, minting each entry from the local chain tail. It is the LOCAL-authoring
//! path, kept deliberately distinct from [`super::storage::content_ingest`] — the REMOTE-input path
//! — which self-transacts, is §18b quota-capped, and refolds per entry. Local authoring stays
//! linear (§16.2): a single local writer from the accepted tail, quota-free, one refold per batch.
//!
//! OWNER-AUTHORED. The store's local account (author) is also the owner of its `/2` streams, so
//! every entry carries `grant_id = None` and `roster_ref` = the account's own genesis entry hash
//! (the roster the founder device is enrolled under). `owner_auth_len == author_auth_len ==` the
//! account's current control-fold `effective_count`, read in the SAME snapshot as authoring —
//! citing our own current fold length means our entries never park `auth_len_ahead` against our own
//! fold.
//!
//! `lamport` is the stream-global LWW clock (#1164): `max(accepted stream lamports) + 1`, the
//! projection LWW key ([`crate::project`] orders on `(lamport, device)`). Under a single writer it
//! coincides with the dense per-author `seq` (that chain is the only source of accepted lamports),
//! but a granted contributor authoring on the SAME owner stream mints above the owner's tip so its
//! later edit cannot lose to a shorter chain. `seq` stays per-`(stream, author, device)` dense for
//! chain integrity, so `lamport == seq` no longer holds once a second writer shares the stream.
//!
//! VERIFY-ACCEPTED. After the single batch refold, the seam reads back each authored entry's status
//! and `bail!`s if any is not `accepted`, so the whole batch — and the mutation that triggered it —
//! rolls back. A local entry CAN park/declassify (a missing `StreamOwn`, a stale `auth_len`, a
//! contested account), and a silently-stored unaccepted entry would desync the candidate tail from
//! the accepted tail and self-fork the next author's seq. Enforcing accept-or-rollback INDUCES the
//! invariant that no unaccepted local candidate ever survives a commit — which is exactly why
//! minting from the plain candidate tail (below) is the accepted tail.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::super::bootstrap::{self, LocalAccountRef};
#[cfg(test)]
use super::super::id::OwnerId;
use super::super::id::{AccountEntryHash, GrantId, RosterRef};
use super::super::keywrap::ContentKey;
use super::super::limits::CONTENT_ENVELOPE_MAX_BYTES;
use super::super::secrets::{self, SealingKeyOutcome};
use super::super::{AccountId, storage as account_storage};
use super::envelope::{self, ContentEntryHeader, VerifiedContentEntry};
use super::storage::{self as content_storage, fixed};
use crate::op::{self, DeviceFingerprint, MemoryOp};
use crate::stream::StreamId;
use crate::{LocalDevice, content_projection, local_device};

/// The PROVEN worst-case byte overhead a signed `/3` content entry adds around an op body —
/// `signed_bytes.len() - payload.len()` maximized over every header field value and every payload
/// size class, derived directly from `envelope::sign_content_entry` / `encode_header` (NOT a
/// guessed margin, so it cannot silently drift).
/// `content_entry_max_overhead_bounds_the_real_signed_envelope` pins it against the real encoders.
///
/// The sum is computed from named parts so the compiler checks the arithmetic and the breakdown is
/// legible: CBOR encodes a 32-byte bstr as a 2-byte prefix + 32; a `u64` as at most 1 + 8; an
/// `n`-byte str as its prefix + `n`. Every optional-hash field counts as PRESENT (34 B) — its
/// widest form, which upper-bounds the null form (1 B) unconditionally, so the constant holds
/// regardless of the header's nullity coupling.
const CONTENT_ENTRY_MAX_OVERHEAD_BYTES: usize = {
    // `encode_header`: the 13-part `rag-rat/entry/3` array, every field at its MAX CBOR width — an
    // unconditional upper bound on the header bytes.
    const HEADER_MAX: usize = 1                 // array(13) head
        + (1 + 15)                              // domain str "rag-rat/entry/3"
        + (2 + 32) * 3                          // stream_id, author_account_id, device_fingerprint
        + (1 + 8) * 2                           // seq, lamport
        + (2 + 32) * 2                          // prev_hash, grant_id (present ≥ null)
        + (2 + 32)                              // roster_ref
        + (1 + 8) * 3                           // owner_auth_len, author_auth_len, crypto_suite
        + (2 + 32); // key_id (present ≥ null)
    // `encode_body` = cbor([header_bytes, payload]).
    const BODY_FRAMING: usize = 1               // array(2) head
        + 3                                     // header_bytes bstr prefix (HEADER_MAX = 300 ⇒ 0x59+2)
        + 5; // payload bstr prefix (a ~256 KiB body ⇒ 0x5a+4)
    // `encode_signed` = cbor([domain, body_bytes, signature]).
    const SIGNED_FRAMING: usize = 1             // array(3) head
        + (1 + 22)                              // domain str "rag-rat/signed-entry/1"
        + 5                                     // body_bytes bstr prefix (~256 KiB ⇒ 0x5a+4)
        + (2 + 64); // signature bstr
    HEADER_MAX + BODY_FRAMING + SIGNED_FRAMING // = 300 + 9 + 95 = 404
};

/// The largest op BODY (canonical CBOR) that always fits inside a signed `/3` content entry: the
/// §18a `CONTENT_ENVELOPE_MAX_BYTES` cap minus the PROVEN worst-case envelope overhead
/// ([`CONTENT_ENTRY_MAX_OVERHEAD_BYTES`]). A body at or under this bound signs to at most exactly
/// `CONTENT_ENVELOPE_MAX_BYTES`, so it clears both of `sign_content_entry`'s size checks. The prior
/// loose 1 KiB margin here permanently quarantined rows between `CAP - 1024` and the real limit
/// that `sign_content_entry` would in fact accept — legitimate imported data left unprojected
/// forever; this exact bound is a true lower-bound-safe mirror of the sign-time check (#680).
const CONTENT_OP_BODY_MAX_BYTES: usize =
    CONTENT_ENVELOPE_MAX_BYTES - CONTENT_ENTRY_MAX_OVERHEAD_BYTES;

/// Whether `op` can be authored as a `/3` content entry without exceeding the §18a envelope cap.
///
/// The local reconcile uses this to QUARANTINE a row whose op is un-authorable (an oversized
/// raw/imported memory body or payload) — skipping it instead of `bail!`ing the whole batch — so
/// one bad row can never wedge every other memory write; the write path uses the same predicate at
/// the create/update boundary to reject oversized input before the row is persisted (#680). Checks
/// the encoded body only; the header + signature are the fixed overhead `CONTENT_OP_BODY_MAX_BYTES`
/// already reserves for.
///
/// Also gates the STRUCTURAL limits `op::decode` enforces, not just size: an op can be small and
/// still unreadable (an over-cap or duplicate-carrying anchor set). `/3` ingest never decodes op
/// bytes, so such an entry would be signed, accepted and forwarded, then silently skipped at
/// projection by every peer including its author — a shape worth refusing before it is durable.
pub fn content_op_is_authorable(op: &MemoryOp) -> bool {
    op::within_wire_limits(op) && op::encode(op).len() <= CONTENT_OP_BODY_MAX_BYTES
}

/// The AEAD expansion a suite-1 seal adds to the op body on the wire: the 24-byte XChaCha nonce +
/// the 16-byte Poly1305 tag ([`envelope::SEALED_NONCE_LEN`] + [`envelope::SEALED_AEAD_TAG_LEN`]).
/// [`CONTENT_OP_BODY_MAX_BYTES`] reserves ZERO AEAD overhead (it mirrors the suite-0 sign check),
/// so the sealed body bound subtracts this — otherwise a sealed op between the two bounds would
/// pass [`content_op_is_sealed_authorable`] yet `bail!` the batch at sign time (the #680 wedge, on
/// sealed streams).
const CONTENT_SEALED_AEAD_OVERHEAD_BYTES: usize =
    envelope::SEALED_NONCE_LEN + envelope::SEALED_AEAD_TAG_LEN;

/// The largest op BODY (canonical CBOR) that always fits inside a signed SUITE-1 `/3` entry: the
/// plaintext body bound ([`CONTENT_OP_BODY_MAX_BYTES`]) minus the sealed AEAD expansion.
const CONTENT_SEALED_OP_BODY_MAX_BYTES: usize =
    CONTENT_OP_BODY_MAX_BYTES - CONTENT_SEALED_AEAD_OVERHEAD_BYTES;

/// The sealed-path twin of [`content_op_is_authorable`]: whether `op` fits a signed suite-1 `/3`
/// entry once the AEAD nonce + tag are added to its body (S2, #608). The live reconcile uses
/// this to QUARANTINE an un-authorable op on a sealed stream — exactly as the suite-0 predicate
/// does on a plaintext one — instead of `bail!`ing the whole batch at sign time.
///
/// Gates the same structural limits as its twin: a shape `op::decode` refuses is unreadable whether
/// or not it is sealed, so both predicates must agree on it.
pub fn content_op_is_sealed_authorable(op: &MemoryOp) -> bool {
    op::within_wire_limits(op) && op::encode(op).len() <= CONTENT_SEALED_OP_BODY_MAX_BYTES
}

/// The `/3` chain tail for one `(stream, author, device)` coordinate: its highest-`seq` entry.
struct ContentChainTail {
    seq: u64,
    entry_hash: AccountEntryHash,
}

/// Author `ops` as owner-authored `/3` content on `stream_id` WITHIN the caller's transaction:
/// chain each entry from the current tail (genesis when the chain is empty), insert it as a
/// candidate, refold the stream ONCE, then verify every entry folded `accepted` — else `bail!` so
/// the whole batch rolls back. Neither opens nor commits the txn. Returns the authored entry hashes
/// in authoring order. Requires the store's local account to be minted already (see
/// [`bootstrap::local_account`]); the caller mints it before opening this txn (that mint
/// self-transacts and cannot nest here).
pub fn author_content_batch_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    // Owner-authored: the store's single local account is both author and owner. Resolve it (and
    // its genesis entry hash, the `roster_ref`) from the pointer WITHOUT minting — the account
    // must already exist.
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author /3 content before the store's local account is minted (call local_account \
         first)",
    )?;
    let device = local_device(tx, now_ms)?;
    // The freshness seam, read in THIS snapshot (see the module header): cite our own current
    // effective control-fold length as both auth_len fields.
    let auth_len = account_storage::account_effective_count(tx, account_id)?;
    author_batch_in_tx(
        tx,
        &BatchAuthoring {
            stream_id,
            account_id,
            device: &device,
            roster_ref: genesis_hash.into(),
            // Owner-authored: author == owner, so no delegated grant.
            grant_id: None,
            owner_auth_len: auth_len,
            author_auth_len: auth_len,
        },
        ops,
        now_ms,
    )
}

/// Who authors one plaintext `/3` batch and the authority its entries cite — the only difference
/// between the owner seam and the granted-contributor seam.
struct BatchAuthoring<'a> {
    stream_id: StreamId,
    account_id: AccountId,
    device: &'a LocalDevice,
    roster_ref: RosterRef,
    /// `None` for the stream owner; the delegating grant for a contributor.
    grant_id: Option<GrantId>,
    owner_auth_len: u64,
    author_auth_len: u64,
}

/// The shared body of both plaintext batch seams: chain each op from the author's tail, insert it
/// as a candidate, refold the stream ONCE, verify every entry folded `accepted` (else `bail!` so
/// the whole batch rolls back), then reproject.
fn author_batch_in_tx(
    tx: &Transaction<'_>,
    authoring: &BatchAuthoring<'_>,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    let BatchAuthoring {
        stream_id,
        account_id,
        device,
        roster_ref,
        grant_id,
        owner_auth_len,
        author_auth_len,
    } = *authoring;
    crate::account::require_supported_account_control(tx, account_id)?;
    super::super::control_policy::require_supported_stream_control(tx, stream_id)?;
    let fingerprint = device.fingerprint();

    // Stream-global LWW clock (#1164): the next lamport is `max(accepted stream lamports) + 1`, so
    // a granted contributor authoring on this owner stream orders AFTER everything already
    // accepted — a per-author chain-tail lamport would let a short-chain writer's later edit
    // lose the `(lamport, device)` projection LWW. For a SINGLE writer this stays equal to
    // `seq` (its own dense chain is the only source of accepted lamports), preserving prior
    // behavior.
    let lamport_base = match stream_max_content_lamport(tx, stream_id)? {
        Some(max) => max.checked_add(1).context("/3 stream lamport clock overflow")?,
        None => 0,
    };
    let mut authored = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        // Mint the seq from the candidate tail. Under verify-accepted+rollback the candidate tail
        // IS the accepted tail, so no separate accepted-tail read is needed; the in-txn
        // read sees the entries this loop already inserted, so each op chains off the one
        // before it. A contributor chains its OWN dense seq on the owner's stream, independent of
        // the owner's chain (the chain tail is keyed by (stream, author, device)).
        let (seq, prev_hash) = match content_chain_tail(tx, stream_id, account_id, fingerprint)? {
            Some(tail) => (
                tail.seq
                    .checked_add(1)
                    .context("/3 content chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail.entry_hash),
            ),
            None => (0, None),
        };
        let lamport = batch_lamport(lamport_base, index)?;
        let header = ContentEntryHeader {
            stream_id,
            author_account_id: account_id,
            device_fingerprint: fingerprint,
            seq,
            lamport,
            prev_hash,
            grant_id,
            roster_ref,
            owner_auth_len,
            author_auth_len,
            crypto_suite: 0,
            key_id: None,
        };
        // The `/3` body is the op's canonical CBOR verbatim (an opaque bstr the projection later
        // `op::decode`s). No `candidate_capacity` check: that is the §18b remote-abuse budget, not
        // a local-authoring bound.
        let payload = op::encode(op);
        let signed = envelope::sign_content_entry(device.secret(), &header, &payload)?;
        let verified = VerifiedContentEntry {
            header: signed.header,
            payload: signed.payload,
            header_bytes: signed.header_bytes,
            entry_hash: signed.entry_hash,
        };
        content_storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)?;
        authored.push(verified.entry_hash);
    }

    // ONE authority+branch refold for the whole batch (§16.2), the only writer of `accepted = 1`.
    content_storage::refold_content_stream(tx, stream_id)?;

    // verify-accepted: an owner authoring on its own stream accepts, so anything else means an
    // authority gap (missing `StreamOwn`, stale `auth_len`, contested account). Roll the batch back
    // rather than leave an unaccepted local candidate the next author's seq would collide with.
    for entry_hash in &authored {
        match content_status(tx, entry_hash)?.as_deref() {
            Some("accepted") => {},
            other if grant_id.is_none() => anyhow::bail!(
                "authored /3 content entry did not fold accepted (status {other:?}); rolling back \
                 the batch",
            ),
            other => anyhow::bail!(
                "authored granted /3 content did not fold accepted (status {other:?}); rolling \
                 back the batch — the grant may be missing/closed, the owner log unsynced, or the \
                 role not Writer",
            ),
        }
    }

    // Acceptance changed on this stream → refresh its accepted-/3 → memory projection in the same
    // txn (the memory-layer fold that decodes op bodies; the acceptance layer is body-agnostic).
    content_projection::reproject_accepted_content_stream(tx, stream_id)?;

    Ok(authored)
}

/// Author a batch of ops as a GRANTED CONTRIBUTOR (#1164): the local account is NOT the stream
/// owner but holds an effective Writer grant, so it authors onto the OWNER's `stream_id` citing
/// `grant_id`, under its OWN account + device, with `owner_auth_len` read from the owner's synced
/// control fold and `author_auth_len` from its own. Verify-accepted-or-rollback like the owner
/// seam.
///
/// Plaintext only — v1 grants target public streams, which carry no content keys (a sealed grantee
/// path would need stream-key wraps to the grantee). The caller resolves `stream_id` /
/// `owner_account_id` / `grant_id` from the synced grant (see `storage::effective_writer_grant`).
/// The grantee's `roster_ref` is its OWN genesis: it is the founder of its own account, so no
/// non-founder authority machinery is needed — the cross-account authorization is the `grant_id`,
/// not the roster.
pub fn author_grantee_content_batch_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    owner_account_id: AccountId,
    grant_id: GrantId,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?
        .context("cannot author granted /3 content before the store's local account is minted")?;
    anyhow::ensure!(
        account_id != owner_account_id,
        "author_grantee_content_batch_in_tx is for a NON-owner contributor; the stream owner \
         authors via author_content_batch_in_tx",
    );
    let device = local_device(tx, now_ms)?;
    // Cite the OWNER's current fold count for the ownership/grant citations' freshness, and our OWN
    // count for our roster citation — the two provenance halves the acceptance fold checks
    // separately.
    let owner_auth_len = account_storage::account_effective_count(tx, owner_account_id)?;
    let author_auth_len = account_storage::account_effective_count(tx, account_id)?;
    author_batch_in_tx(
        tx,
        &BatchAuthoring {
            stream_id,
            account_id,
            device: &device,
            roster_ref: genesis_hash.into(),
            grant_id: Some(grant_id),
            owner_auth_len,
            author_auth_len,
        },
        ops,
        now_ms,
    )
}

/// The privacy intent for a `/3` content batch (sync phase C5, #608). The caller states it
/// EXPLICITLY — wrap presence is a downgrade-ratchet INPUT, never the privacy oracle: a private
/// stream's accepted-wrap set can legitimately empty under sync lag or a retro-condemn, and
/// treating "no wrap ⇒ public" would author plaintext on a private stream (a confidentiality leak).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealPolicy {
    /// Author plaintext suite-0 entries — REFUSED on a stream that has ratcheted to sealed.
    Plaintext,
    /// Author sealed suite-1 entries under the stream's current content key; fail closed (bail the
    /// whole batch) if no key can be resolved — never plaintext-fallback, never a plaintext park.
    Sealed,
}

/// Policy-aware `/3` authoring state prepared before a caller opens its write transaction.
///
/// The fields are deliberately opaque: sealed preparation owns the recovered content key and the
/// local signing capability, but callers can only hand both back to
/// [`author_prepared_content_batch_in_tx`]. [`ContentKey`] zeroizes its bytes on drop.
pub struct PreparedContentAuthoring {
    stream_id: StreamId,
    account_id: AccountId,
    kind: PreparedContentAuthoringKind,
}

enum PreparedContentAuthoringKind {
    Plaintext,
    Sealed(Box<PreparedSealedContentAuthoring>),
}

struct PreparedSealedContentAuthoring {
    key: ContentKey,
    device: LocalDevice,
    resolved: secrets::SelectedWrap,
    rotation: secrets::RotationOutcome,
}

/// Prepare policy-aware `/3` authoring before the caller opens its write transaction.
///
/// Plaintext preparation is read-only; the downgrade ratchet is checked later under the caller's
/// write lock. Sealed preparation performs the existing pre-transaction key protocol: lazy
/// rotation in its own transaction, key resolution outside a transaction so security events
/// autocommit, and first-key minting when necessary. The returned value is bound to `stream_id` and
/// the current local account and exposes no key material.
pub fn prepare_content_authoring(
    conn: &Connection,
    stream_id: StreamId,
    policy: SealPolicy,
    now_ms: i64,
) -> anyhow::Result<PreparedContentAuthoring> {
    let account_id = require_local_account_id(conn)?;
    let kind = match policy {
        SealPolicy::Plaintext => PreparedContentAuthoringKind::Plaintext,
        SealPolicy::Sealed =>
            prepare_sealed_content_authoring(conn, stream_id, account_id, now_ms)?,
    };
    Ok(PreparedContentAuthoring { stream_id, account_id, kind })
}

/// Author a prepared policy-aware batch inside the caller's transaction. Neither opens nor commits
/// the transaction. All ratchet and sealing-selection checks run under this write lock before any
/// content row is inserted; authoring then refolds once, reprojects, and verifies every entry was
/// accepted.
pub fn author_prepared_content_batch_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    ops: &[MemoryOp],
    prepared: &PreparedContentAuthoring,
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    crate::account::require_supported_account_control(tx, prepared.account_id)?;
    super::super::control_policy::require_supported_stream_control(tx, stream_id)?;
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        prepared.stream_id == stream_id,
        "prepared /3 authoring belongs to a different stream"
    );
    anyhow::ensure!(
        require_local_account_id(tx)? == prepared.account_id,
        "prepared /3 authoring belongs to a different local account"
    );

    match &prepared.kind {
        PreparedContentAuthoringKind::Plaintext => {
            if stream_has_sealed_ratchet(tx, prepared.account_id, stream_id)? {
                anyhow::bail!(
                    "refusing plaintext /3 authoring on a stream that has ratcheted to sealed (an \
                     accepted key wrap or a sealed entry exists)"
                );
            }
            author_content_batch_in_tx(tx, stream_id, ops, now_ms)
        },
        PreparedContentAuthoringKind::Sealed(sealed) => {
            revalidate_sealing_selection(
                tx,
                prepared.account_id,
                stream_id,
                &sealed.resolved,
                &sealed.rotation,
            )?;
            seal_and_author_in_tx(tx, stream_id, ops, &sealed.key, &sealed.device, now_ms)
        },
    }
}

/// Author `ops` as owner-authored `/3` content on `stream_id` under an explicit [`SealPolicy`],
/// as a convenience composition of [`prepare_content_authoring`] and
/// [`author_prepared_content_batch_in_tx`] with an owned IMMEDIATE transaction. Returns the
/// authored entry hashes in authoring order. Requires the store's local account to be minted
/// already.
pub fn author_content_batch(
    conn: &Connection,
    stream_id: StreamId,
    ops: &[MemoryOp],
    policy: SealPolicy,
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    // An empty batch authors nothing, so it must run NO key management: the `Sealed` arm would
    // otherwise rotate or mint + commit the stream's first `StreamKeyWrap`, and on an unkeyed owned
    // stream that committed wrap arms the downgrade ratchet — permanently blocking later plaintext
    // authoring after a call that looked like a no-op. Short-circuit before EITHER arm's
    // ratchet-affecting side effects.
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    let prepared = prepare_content_authoring(conn, stream_id, policy, now_ms)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let authored = author_prepared_content_batch_in_tx(&tx, stream_id, ops, &prepared, now_ms)?;
    tx.commit()?;
    Ok(authored)
}

/// Prepare the sealed arm's pre-transaction key protocol. Fail closed if no content key resolves;
/// plaintext fallback is never represented by the returned type.
fn prepare_sealed_content_authoring(
    conn: &Connection,
    stream_id: StreamId,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<PreparedContentAuthoringKind> {
    // Authoring needs a local device, so minting it here is fine; `current_sealing_key` must NOT
    // mint (a read API), so we resolve it once and hand it in.
    let device = local_device(conn, now_ms)?;

    // (txn A) Lazy rotation on device removal, committed on its OWN txn so a rotation (a fresh
    // higher-epoch wrap) survives a later authoring failure. Every RotationOutcome is non-error —
    // an owner rotates, a member sees StaleButNotOwner and seals under the current key; only an
    // infra/DB failure is `Err`. The outcome is CARRIED INTO txn B: a member that saw
    // StaleButNotOwner cannot rotate, so txn B's rotation-need re-check must not re-fail the state
    // txn A already classified — retrying can never change it, and sealed authoring would stay
    // permanently unavailable to the member until an owner happened to rotate.
    let rotation = {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let outcome = secrets::ensure_stream_key_current_in_tx(&tx, stream_id, now_ms)?;
        tx.commit()?;
        outcome
    };

    // (autocommit) Resolve the content key. `current_sealing_key`'s `sync_security_events` INSERTs
    // autocommit on `&Connection`, so it MUST run outside any txn that could roll back.
    let key = match secrets::current_sealing_key(conn, account_id, stream_id, &device, now_ms)? {
        SealingKeyOutcome::Ready(key) => key,
        // Never keyed: an owner mints the first key (its own txn) and re-resolves; a non-owner's
        // mint bails (owner-gated), fail-closing the whole mutation.
        SealingKeyOutcome::NoCurrentKey =>
            mint_first_key_then_resolve(conn, stream_id, account_id, &device, now_ms)?,
        // Fail closed on a Sealed-intent stream: bail, never author plaintext.
        SealingKeyOutcome::NotRecipient | SealingKeyOutcome::FailedClosed => anyhow::bail!(
            "cannot seal /3 content: no resolvable content key for this device (fail-closed)"
        ),
    };

    // The `(key_epoch, key_id)` the resolved key names — the baseline txn B re-validates under its
    // write lock. Read via `select_current_sealing_wrap` (a PURE read), never `current_sealing_key`
    // (whose adoption cross-check autocommits a security-event row and so must stay here, pre-txn).
    let resolved = secrets::select_current_sealing_wrap(conn, account_id, stream_id)?.context(
        "sealed /3 authoring: a content key resolved but the current sealing wrap is empty",
    )?;
    // Close the resolution window itself: a rotation committed between `current_sealing_key` and
    // this read would leave `resolved` naming a key this device did NOT recover, so require the
    // selection to still name the resolved key before adopting it as the baseline.
    anyhow::ensure!(
        resolved.key_id == key.key_id(),
        "sealed /3 authoring: the sealing selection changed during key resolution (retry)",
    );

    Ok(PreparedContentAuthoringKind::Sealed(Box::new(PreparedSealedContentAuthoring {
        key,
        device,
        resolved,
        rotation,
    })))
}

/// Re-confirm, under txn B's IMMEDIATE write lock, that `stream_id` is STILL safe to seal under the
/// key the pre-txn resolution named — i.e. the selection is unchanged AND no rotation is now due.
///
/// Two roster changes can commit in the autocommit window between resolving the key
/// ([`prepare_content_authoring`]) and opening this txn, and BOTH must abort the seal:
///
/// - A `DeviceRemove` + rotation commits a fresh higher-epoch `StreamKeyWrap`, so the resolved
///   `(epoch, key_id)` no longer names the current selection — caught by the selection-unchanged
///   check.
/// - A BARE `DeviceRemove` (no rotation yet) makes a recipient of the current wrap no longer
///   roster-effective WITHOUT minting a higher-epoch wrap. No rotation has happened, so the
///   selection is UNCHANGED and the check above passes — yet sealing under this key would let the
///   just-removed device decrypt this post-removal entry. The real invariant is "rotation is not
///   NOW needed", not merely "the selection did not change", so we also re-check the C4.4
///   rotation-need predicate.
///
/// Either way, sealing under the now-stale key is a confidentiality regression (NOT the §15
/// sync-lag window, because the removal is LOCALLY committed) — acceptance is key-independent, so
/// the batch would fold accepted anyway. Because txn B holds the write lock, no removal/rotation
/// can commit until it ends, so both checks here are authoritative for the seal that follows. On
/// either failure, bail so the caller retries: the retry's txn A `ensure_stream_key_current_in_tx`
/// rotates to a fresh key excluding the removed device, and txn B then seals under that. A PURE
/// read (both predicates derive on read; neither is `current_sealing_key`, which autocommits).
///
/// The rotation-need re-check is EXEMPT when `txn_a_outcome` is
/// [`secrets::RotationOutcome::StaleButNotOwner`]: txn A already classified this exact state
/// (rotation needed, but this device is a member and CANNOT rotate) under its own IMMEDIATE lock,
/// and `ensure_stream_key_current_in_tx`'s contract is that the member proceeds to seal under the
/// current key — roster membership is READ access, not authoring authority. Re-failing here would
/// make sealed authoring permanently unavailable to the member (a retry can never change a
/// non-owner into an owner). The exemption is safe because the selection-unchanged check above
/// still guards the member: an owner rotating in the resolution window changes the selection, so
/// the member bails and its retry seals under the FRESH key. What the exemption gives up — a
/// SECOND bare removal committing between txn A and txn B while we are a member — is
/// indistinguishable from the state txn A already accepted, and the member has no remedy for it
/// either way.
fn revalidate_sealing_selection(
    tx: &Transaction<'_>,
    account_id: AccountId,
    stream_id: StreamId,
    resolved: &secrets::SelectedWrap,
    txn_a_outcome: &secrets::RotationOutcome,
) -> anyhow::Result<()> {
    let current = secrets::select_current_sealing_wrap(tx, account_id, stream_id)?.context(
        "sealed /3 authoring: the stream's sealing wrap vanished under the authoring lock (a \
         concurrent condemn); retry",
    )?;
    // The rotation-already-happened case: a fresh higher-epoch wrap advanced the selection off the
    // resolved key (defense in depth — the bare-removal check below covers the not-yet-rotated
    // case).
    anyhow::ensure!(
        current.key_epoch == resolved.key_epoch && current.key_id == resolved.key_id,
        "sealed /3 authoring: the stream's sealing key rotated between resolution and sealing \
         (was epoch {}, now epoch {}); retry with the fresh key",
        resolved.key_epoch,
        current.key_epoch,
    );
    // The bare-removal case: a `DeviceRemove` committed after txn A's rotation check and before
    // this txn leaves the selection unchanged (no higher-epoch wrap) yet makes a recipient of
    // the current sealing key no-longer-effective. Re-checking the rotation-need predicate here
    // — authoritative under the write lock — bails so the retry rotates to a key that excludes
    // the removed device. SKIPPED when txn A already returned StaleButNotOwner: a member cannot
    // rotate, so this state is the one it is contracted to seal under (see the fn doc).
    if !matches!(txn_a_outcome, secrets::RotationOutcome::StaleButNotOwner) {
        anyhow::ensure!(
            !secrets::stream_key_rotation_needed(tx, account_id, stream_id)?,
            "sealed /3 authoring: rotation became needed under the authoring lock (a recipient of \
             the current sealing key was removed from the roster); retry with the rotated key",
        );
    }
    Ok(())
}

/// A stream that has never been keyed: mint the first content key in its OWN txn, then re-resolve.
/// `mint_and_author_stream_key_wrap_in_tx` is owner-gated + verify-accepted, so a non-owner (or a
/// stream this account does not own) makes it `Err` here — which fail-closes the sealed mutation (a
/// non-owner `NoCurrentKey` bails, never plaintext).
fn mint_first_key_then_resolve(
    conn: &Connection,
    stream_id: StreamId,
    account_id: AccountId,
    device: &LocalDevice,
    now_ms: i64,
) -> anyhow::Result<ContentKey> {
    {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        secrets::mint_and_author_stream_key_wrap_in_tx(&tx, stream_id, now_ms)?;
        tx.commit()?;
    }
    match secrets::current_sealing_key(conn, account_id, stream_id, device, now_ms)? {
        SealingKeyOutcome::Ready(key) => Ok(key),
        _ => anyhow::bail!(
            "sealed /3 authoring: the freshly minted content key did not resolve to Ready \
             (fail-closed)"
        ),
    }
}

/// The `SealPolicy::Sealed` txn B core — mirrors [`author_content_batch_in_tx`] but SEALS each op
/// under `key` (suite 1). VERIFY-ACCEPTED reads `content_entry_status` ONLY, never projection rows;
/// after acceptance changes, the stream is reprojected in the same transaction just like suite 0.
fn seal_and_author_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    ops: &[MemoryOp],
    key: &ContentKey,
    device: &LocalDevice,
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?
        .context("cannot author sealed /3 content before the store's local account is minted")?;
    let fingerprint = device.fingerprint();
    let auth_len = account_storage::account_effective_count(tx, account_id)?;

    // Stream-global LWW clock (#1164) — see `author_content_batch_in_tx`.
    let lamport_base = match stream_max_content_lamport(tx, stream_id)? {
        Some(max) => max.checked_add(1).context("/3 stream lamport clock overflow")?,
        None => 0,
    };
    let mut authored = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        let (seq, prev_hash) = match content_chain_tail(tx, stream_id, account_id, fingerprint)? {
            Some(tail) => (
                tail.seq
                    .checked_add(1)
                    .context("/3 content chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail.entry_hash),
            ),
            None => (0, None),
        };
        let lamport = batch_lamport(lamport_base, index)?;
        // `crypto_suite`/`key_id` stay 0/None here — `seal_and_sign_content_entry` finalizes them
        // to suite 1 + the key's id, so a suite-1-over-plaintext header is unconstructible.
        let header = ContentEntryHeader {
            stream_id,
            author_account_id: account_id,
            device_fingerprint: fingerprint,
            seq,
            lamport,
            prev_hash,
            grant_id: None,
            roster_ref: genesis_hash.into(),
            owner_auth_len: auth_len,
            author_auth_len: auth_len,
            crypto_suite: 0,
            key_id: None,
        };
        let op_bytes = op::encode(op);
        let signed =
            envelope::seal_and_sign_content_entry(device.secret(), &header, &op_bytes, key)?;
        let verified = VerifiedContentEntry {
            header: signed.header,
            payload: signed.payload,
            header_bytes: signed.header_bytes,
            entry_hash: signed.entry_hash,
        };
        content_storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)?;
        authored.push(verified.entry_hash);
    }

    content_storage::refold_content_stream(tx, stream_id)?;

    for entry_hash in &authored {
        match content_status(tx, entry_hash)?.as_deref() {
            Some("accepted") => {},
            other => anyhow::bail!(
                "sealed /3 content entry did not fold accepted (status {other:?}); rolling back \
                 the batch",
            ),
        }
    }
    content_projection::reproject_accepted_content_stream(tx, stream_id)?;
    Ok(authored)
}

/// The store's local account id, resolved WITHOUT minting — it must already exist (the caller mints
/// it before the sealed author path, exactly as [`author_content_batch_in_tx`] requires).
fn require_local_account_id(conn: &Connection) -> anyhow::Result<AccountId> {
    Ok(bootstrap::local_account_ref(conn)?
        .context("cannot author /3 content before the store's local account is minted")?
        .account_id)
}

/// Whether `stream_id` has ratcheted to sealed: it has an accepted `StreamKeyWrap` (any epoch) OR
/// an accepted suite-1 `/3` entry. Either makes plaintext authoring a silent downgrade. DERIVED ON
/// READ (no sticky flag), so it converges: a retro-condemn that empties the wrap set still sees
/// surviving sealed entries, and a re-minted wrap re-arms the gate. Wrap presence is one ratchet
/// INPUT, never the sole privacy oracle.
fn stream_has_sealed_ratchet(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    if secrets::accepted_stream_key_wrap_exists_strict(conn, account_id, stream_id)? {
        return Ok(true);
    }
    stream_has_accepted_sealed_entry(conn, stream_id)
}

/// Whether a stream has irreversibly ratcheted to sealed authoring through an accepted key wrap or
/// accepted suite-1 content. This is a downgrade guard, not the privacy-intent source: callers must
/// persist intent separately because a private stream can temporarily have no accepted wraps.
pub fn content_stream_has_sealed_ratchet(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    let Some(local) = bootstrap::local_account_ref(conn)? else {
        return Ok(false);
    };
    stream_has_sealed_ratchet(conn, local.account_id, stream_id)
}

/// Whether any accepted `/3` entry on `stream_id` is suite-1 (sealed). There is no `crypto_suite`
/// column, so each accepted entry's header is decoded from its stored signed bytes. Decode failure
/// is corruption at rest and must abort the authoring decision: treating it as "not sealed" could
/// downgrade a corrupt accepted suite-1 row to plaintext. Every row is decoded even after finding
/// a sealed entry, so unrelated accepted-row corruption cannot be hidden by query order.
fn stream_has_accepted_sealed_entry(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM content_entries WHERE stream_id = ?1 AND accepted = 1",
    )?;
    let mut rows = stmt.query(params![stream_id.to_bytes().as_slice()])?;
    let mut has_sealed_entry = false;
    while let Some(row) = rows.next()? {
        let signed_bytes: Vec<u8> = row.get(0)?;
        let signed = envelope::decode_content_signed(&signed_bytes)
            .context("stored accepted /3 entry failed to decode while checking sealed ratchet")?;
        if signed.header.crypto_suite != 0 {
            has_sealed_entry = true;
        }
    }
    Ok(has_sealed_entry)
}

/// Whether the `/2` stream's `/3` content chain is EMPTY — no `content_entries` row on it at all.
/// Under the single local writer the store's own account+device are the only chain on the stream,
/// so "no rows for this stream" is the whole chain: the genesis case where the memory reconcile
/// elides a create-time `active` status (a fresh chain holds no stale status register to override).
/// A pure read opening no transaction, so it is safe inside the caller's IMMEDIATE txn (a
/// `&Transaction` derefs to `&Connection`).
pub fn content_stream_is_empty(conn: &Connection, stream_id: StreamId) -> anyhow::Result<bool> {
    let has_row: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM content_entries WHERE stream_id = ?1)",
        params![stream_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(!has_row)
}

/// The `(stream, author, device)` chain's highest-`seq` `/3` candidate, or `None` for an empty
/// chain (→ genesis: seq 0, no predecessor). `seq` is stored as an 8-byte big-endian blob, so a
/// blob `ORDER BY seq DESC` compares byte-wise and is numerically correct for the fixed width.
fn content_chain_tail(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    author_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<Option<ContentChainTail>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT seq, entry_hash FROM content_entries
             WHERE stream_id = ?1 AND author_account_id = ?2 AND device_fingerprint = ?3
             ORDER BY seq DESC LIMIT 1",
            params![
                stream_id.to_bytes().as_slice(),
                author_account_id.to_bytes().as_slice(),
                device_fingerprint.to_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(seq, entry_hash)| {
        let seq = u64::from_be_bytes(fixed::<8>(&seq)?);
        Ok(ContentChainTail {
            seq,
            entry_hash: AccountEntryHash::from_bytes(fixed::<32>(&entry_hash)?),
        })
    })
    .transpose()
}

/// The highest `lamport` among a stream's ACCEPTED `/3` entries — the stream-global LWW clock, or
/// `None` for a stream with no accepted entries. Authoring mints the next tick as `max + 1` so a
/// SECOND writer (a granted contributor) always orders after everything already accepted, keeping
/// the `(lamport, device)` projection LWW causal across authors; a per-author chain-tail lamport
/// would let a short-chain writer's later edit lose.
///
/// The primary read is the refold-persisted `content_stream_clocks` floor, which also counts the
/// in-bound CONDEMNED basis — so a writer's revocation does not deflate the clock below what
/// honest dependents already minted against (the wedge that would defeat revocation as the repair
/// path). A stream the refold has not yet clocked falls back to an indexed `MAX` over the V114
/// denormalized column (`idx_content_entries_stream_accepted_lamport`). Neither path decodes an
/// envelope: the ingest-time bounded-advance gate reads this clock for any authenticated envelope
/// claiming a high lamport, so a per-read O(stream) decode was a remote CPU/writer-lock burn for
/// a hostile roster device re-sending one envelope. A NULL column value (an undecodable legacy
/// blob the backfill skipped) is invisible to `MAX`, exactly as the decoding scan treated rows it
/// could not decode.
pub(super) fn stream_max_content_lamport(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<Option<u64>> {
    let clock: Option<i64> = conn
        .query_row(
            "SELECT clock FROM content_stream_clocks WHERE stream_id = ?1",
            params![stream_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(clock) = clock {
        return Ok(Some(u64::try_from(clock).unwrap_or(0)));
    }
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(lamport) FROM content_entries WHERE stream_id = ?1 AND accepted = 1",
        params![stream_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(max.map(|value| u64::try_from(value).unwrap_or(0)))
}

/// The lamport for the `index`-th entry of an authoring batch: `base + index`, kept strictly
/// below the protocol ceiling. Reserving the ceiling on the authoring side (as `/5`'s
/// `next_stream_lamport` does) means no locally authored entry can ever trip a peer's
/// ingest-time ceiling reject — the honest path never approaches it, so hitting this means the
/// stream's accepted clock was poisoned and needs the offending entry retro-condemned.
fn batch_lamport(lamport_base: u64, index: usize) -> anyhow::Result<u64> {
    let lamport =
        lamport_base.checked_add(index as u64).context("/3 stream lamport clock overflow")?;
    anyhow::ensure!(lamport < crate::entry::MAX_ENTRY_LAMPORT, "/3 stream lamport ceiling reached");
    Ok(lamport)
}

/// The current `/3` status of one entry, or `None` if the refold wrote no status row for it.
fn content_status(
    tx: &Transaction<'_>,
    entry_hash: &AccountEntryHash,
) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
        [entry_hash.as_slice()],
        |row| row.get(0),
    )
    .optional()
}

#[cfg(test)]
#[path = "author/tests.rs"]
mod tests;
