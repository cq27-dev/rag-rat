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
//! `lamport = seq`. The seq is dense and monotone under the single local writer, and it IS the
//! projection LWW key ([`crate::project`] orders on `(lamport, device)`); a non-monotone
//! value would let a `NodeUpdate` lose to its own earlier `NodeCreate`.
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
use super::super::keywrap::ContentKey;
use super::super::limits::CONTENT_ENVELOPE_MAX_BYTES;
use super::super::secrets::{self, SealingKeyOutcome};
use super::super::{AccountId, storage as account_storage};
use super::envelope::{self, ContentEntryHeader, VerifiedContentEntry};
use super::storage as content_storage;
use crate::op::{self, DeviceFingerprint, MemoryOp};
use crate::stream::StreamId;
use crate::{LocalDevice, content_projection, local_device};

type EntryHash = [u8; 32];

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
pub fn content_op_is_authorable(op: &MemoryOp) -> bool {
    op::encode(op).len() <= CONTENT_OP_BODY_MAX_BYTES
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
/// entry once the AEAD nonce + tag are added to its body (S2, #608). C5b's live reconcile uses this
/// to QUARANTINE an un-authorable op on a sealed stream — exactly as the suite-0 predicate does on
/// a plaintext one — instead of `bail!`ing the whole batch at sign time.
pub fn content_op_is_sealed_authorable(op: &MemoryOp) -> bool {
    op::encode(op).len() <= CONTENT_SEALED_OP_BODY_MAX_BYTES
}

/// The `/3` chain tail for one `(stream, author, device)` coordinate: its highest-`seq` entry.
struct ContentChainTail {
    seq: u64,
    entry_hash: EntryHash,
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
) -> anyhow::Result<Vec<EntryHash>> {
    // Owner-authored: the store's single local account is both author and owner. Resolve it (and
    // its genesis entry hash, the `roster_ref`) from the pointer WITHOUT minting — the account
    // must already exist.
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author /3 content before the store's local account is minted (call local_account \
         first)",
    )?;
    let device = local_device(tx, now_ms)?;
    let fingerprint = device.fingerprint();
    // The freshness seam, read in THIS snapshot (see the module header): cite our own current
    // effective control-fold length as both auth_len fields.
    let auth_len = account_storage::account_effective_count(tx, account_id)?;

    let mut authored = Vec::with_capacity(ops.len());
    for op in ops {
        // Mint from the candidate tail. Under verify-accepted+rollback the candidate tail IS the
        // accepted tail, so no separate accepted-tail read is needed; the in-txn read sees the
        // entries this loop already inserted, so each op chains off the one before it.
        let (seq, prev_hash) = match content_chain_tail(tx, stream_id, account_id, fingerprint)? {
            Some(tail) => (
                tail.seq
                    .checked_add(1)
                    .context("/3 content chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail.entry_hash),
            ),
            None => (0, None),
        };
        let header = ContentEntryHeader {
            stream_id,
            author_account_id: account_id,
            device_fingerprint: fingerprint,
            seq,
            // Monotone with seq — it is the projection LWW key (see the module header).
            lamport: seq,
            prev_hash,
            // Owner-authored: author == owner, so no delegated grant.
            grant_id: None,
            roster_ref: genesis_hash,
            owner_auth_len: auth_len,
            author_auth_len: auth_len,
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
            other => anyhow::bail!(
                "authored /3 content entry did not fold accepted (status {other:?}); rolling back \
                 the batch",
            ),
        }
    }

    // Acceptance changed on this stream → refresh its accepted-/3 → memory projection in the same
    // txn (the memory-layer fold that decodes op bodies; the acceptance layer is body-agnostic).
    content_projection::reproject_accepted_content_stream(tx, stream_id)?;

    Ok(authored)
}

/// The privacy intent for a `/3` content batch (sync phase C5, #608). The caller states it
/// EXPLICITLY — wrap presence is a downgrade-ratchet INPUT, never the privacy oracle: a private
/// stream's accepted-wrap set can legitimately empty under sync lag or a retro-condemn, and
/// treating "no wrap ⇒ public" would author plaintext on a private stream (a confidentiality leak).
///
/// UNWIRED in C5a: the live memory path still authors through [`author_content_batch_in_tx`]
/// (suite 0). This policy-aware, self-transacting seam is exercised only by tests until C5b lands
/// decrypt-at-projection, the read-side keyring, and the `sync enable` intent source. Authoring a
/// sealed entry into the live path before C5b's decrypt exists triggers the reconcile anti-join
/// duplication loop `content_projection` documents, which is why nothing live calls this yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealPolicy {
    /// Author plaintext suite-0 entries — REFUSED on a stream that has ratcheted to sealed.
    Plaintext,
    /// Author sealed suite-1 entries under the stream's current content key; fail closed (bail the
    /// whole batch) if no key can be resolved — never plaintext-fallback, never a plaintext park.
    Sealed,
}

/// Author `ops` as owner-authored `/3` content on `stream_id` under an explicit [`SealPolicy`],
/// managing its OWN transactions — unlike [`author_content_batch_in_tx`], which runs inside a
/// caller's txn. `Sealed` acquires the stream's content key across THREE transactions (lazy
/// rotation; then a key resolve whose security-event writes autocommit; then seal + author +
/// verify-accepted), so it cannot nest inside a caller txn. Returns the authored entry hashes in
/// authoring order. Requires the store's local account to be minted already.
///
/// UNWIRED (C5a): nothing live calls this — see [`SealPolicy`].
pub fn author_content_batch(
    conn: &Connection,
    stream_id: StreamId,
    ops: &[MemoryOp],
    policy: SealPolicy,
    now_ms: i64,
) -> anyhow::Result<Vec<EntryHash>> {
    // An empty batch authors nothing, so it must run NO key management: the `Sealed` arm would
    // otherwise rotate or mint + commit the stream's first `StreamKeyWrap`, and on an unkeyed owned
    // stream that committed wrap arms the downgrade ratchet — permanently blocking later plaintext
    // authoring after a call that looked like a no-op. Short-circuit before EITHER arm's
    // ratchet-affecting side effects.
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    match policy {
        SealPolicy::Plaintext => author_plaintext_batch_ratcheted(conn, stream_id, ops, now_ms),
        SealPolicy::Sealed => author_sealed_batch(conn, stream_id, ops, now_ms),
    }
}

/// The `SealPolicy::Plaintext` arm: refuse if the stream has ratcheted to sealed, then author
/// suite-0 through the existing in-tx seam — all in ONE self-owned txn so the ratchet gate and the
/// authoring commit atomically (a concurrent wrap/seal cannot slip between them).
fn author_plaintext_batch_ratcheted(
    conn: &Connection,
    stream_id: StreamId,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Vec<EntryHash>> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Downgrade ratchet: once a stream carries an accepted StreamKeyWrap (any epoch) OR an accepted
    // suite-1 entry, plaintext authoring is a silent downgrade — refuse it.
    let account_id = require_local_account_id(&tx)?;
    if stream_has_sealed_ratchet(&tx, account_id, stream_id)? {
        anyhow::bail!(
            "refusing plaintext /3 authoring on a stream that has ratcheted to sealed (an \
             accepted key wrap or a sealed entry exists)"
        );
    }
    let authored = author_content_batch_in_tx(&tx, stream_id, ops, now_ms)?;
    tx.commit()?;
    Ok(authored)
}

/// The `SealPolicy::Sealed` arm: the S4 three-txn key acquisition, then seal + author. Fail-closed
/// — if no content key resolves, the whole batch bails, NEVER plaintext-fallback and NEVER a
/// plaintext park (the row simply never enters the tables; the reconcile anti-join is the retry
/// once this device gains a key).
fn author_sealed_batch(
    conn: &Connection,
    stream_id: StreamId,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Vec<EntryHash>> {
    // Authoring needs a local device, so minting it here is fine; `current_sealing_key` must NOT
    // mint (a read API), so we resolve it once and hand it in.
    let device = local_device(conn, now_ms)?;
    let account_id = require_local_account_id(conn)?;

    // (txn A) Lazy rotation on device removal, committed on its OWN txn so a rotation (a fresh
    // higher-epoch wrap) survives a later authoring failure. Every RotationOutcome is non-error: an
    // owner rotates, a member sees StaleButNotOwner and seals under the current key — only an
    // infra/DB failure is `Err`, so the outcome is intentionally discarded.
    {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        secrets::ensure_stream_key_current_in_tx(&tx, stream_id, now_ms)?;
        tx.commit()?;
    }

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

    // (txn B) Seal + author + verify-accepted, re-validating the selection under the write lock so
    // a rotation that committed after resolution can never make us seal under the stale key.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    revalidate_sealing_selection(&tx, account_id, stream_id, &resolved)?;
    let authored = seal_and_author_in_tx(&tx, stream_id, ops, &key, &device, now_ms)?;
    tx.commit()?;
    Ok(authored)
}

/// Re-confirm, under txn B's IMMEDIATE write lock, that `stream_id`'s current sealing selection is
/// STILL the `(key_epoch, key_id)` the pre-txn resolution named.
///
/// A `DeviceRemove` + rotation can commit a fresh higher-epoch `StreamKeyWrap` in the autocommit
/// window between resolving the key ([`author_sealed_batch`]) and opening this txn. Sealing under
/// the now-stale key would let a device that rotation revoked decrypt this post-removal entry — a
/// confidentiality regression, and NOT the §15 sync-lag window, because the removal is LOCALLY
/// committed. Because txn B holds the write lock, no rotation can commit until it ends, so a
/// selection that still matches here is authoritative for the seal that follows. On mismatch, bail
/// so the caller retries under the fresh key. A PURE read (never `current_sealing_key`).
fn revalidate_sealing_selection(
    tx: &Transaction<'_>,
    account_id: AccountId,
    stream_id: StreamId,
    resolved: &secrets::SelectedWrap,
) -> anyhow::Result<()> {
    let current = secrets::select_current_sealing_wrap(tx, account_id, stream_id)?.context(
        "sealed /3 authoring: the stream's sealing wrap vanished under the authoring lock (a \
         concurrent condemn); retry",
    )?;
    anyhow::ensure!(
        current.key_epoch == resolved.key_epoch && current.key_id == resolved.key_id,
        "sealed /3 authoring: the stream's sealing key rotated between resolution and sealing \
         (was epoch {}, now epoch {}); retry with the fresh key",
        resolved.key_epoch,
        current.key_epoch,
    );
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
/// under `key` (suite 1). VERIFY-ACCEPTED reads `content_entry_status` ONLY, NEVER "the projection
/// contains my nodes" (false for a sealed entry until C5b's decrypt-at-projection lands —
/// `op::decode` fails on ciphertext). For the same reason the plaintext seam's
/// `reproject_accepted_content_stream` call is INTENTIONALLY omitted: reprojecting a sealed stream
/// pre-C5b would skip every sealed entry, and wiring that into the live reconcile is the anti-join
/// duplication trap C5a avoids by staying unwired.
fn seal_and_author_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    ops: &[MemoryOp],
    key: &ContentKey,
    device: &LocalDevice,
    now_ms: i64,
) -> anyhow::Result<Vec<EntryHash>> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?
        .context("cannot author sealed /3 content before the store's local account is minted")?;
    let fingerprint = device.fingerprint();
    let auth_len = account_storage::account_effective_count(tx, account_id)?;

    let mut authored = Vec::with_capacity(ops.len());
    for op in ops {
        let (seq, prev_hash) = match content_chain_tail(tx, stream_id, account_id, fingerprint)? {
            Some(tail) => (
                tail.seq
                    .checked_add(1)
                    .context("/3 content chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail.entry_hash),
            ),
            None => (0, None),
        };
        // `crypto_suite`/`key_id` stay 0/None here — `seal_and_sign_content_entry` finalizes them
        // to suite 1 + the key's id, so a suite-1-over-plaintext header is unconstructible.
        let header = ContentEntryHeader {
            stream_id,
            author_account_id: account_id,
            device_fingerprint: fingerprint,
            seq,
            lamport: seq,
            prev_hash,
            grant_id: None,
            roster_ref: genesis_hash,
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
    tx: &Transaction<'_>,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    if secrets::select_current_sealing_wrap(tx, account_id, stream_id)?.is_some() {
        return Ok(true);
    }
    stream_has_accepted_sealed_entry(tx, stream_id)
}

/// Whether any accepted `/3` entry on `stream_id` is suite-1 (sealed). There is no `crypto_suite`
/// column, so each accepted entry's header is decoded from its stored signed bytes; a row whose
/// bytes fail to decode cannot be a valid suite-1 entry and is skipped. Early-returns on the first
/// match.
fn stream_has_accepted_sealed_entry(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    let mut stmt = tx.prepare(
        "SELECT signed_bytes FROM content_entries WHERE stream_id = ?1 AND accepted = 1",
    )?;
    let mut rows = stmt.query(params![stream_id.to_bytes().as_slice()])?;
    while let Some(row) = rows.next()? {
        let signed_bytes: Vec<u8> = row.get(0)?;
        if let Ok(signed) = envelope::decode_content_signed(&signed_bytes)
            && signed.header.crypto_suite != 0
        {
            return Ok(true);
        }
    }
    Ok(false)
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
        Ok(ContentChainTail { seq, entry_hash: fixed::<32>(&entry_hash)? })
    })
    .transpose()
}

/// The current `/3` status of one entry, or `None` if the refold wrote no status row for it.
fn content_status(
    tx: &Transaction<'_>,
    entry_hash: &EntryHash,
) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
        [entry_hash.as_slice()],
        |row| row.get(0),
    )
    .optional()
}

fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("expected {N} bytes, got {}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rag_rat_query::memory::EdgeRelation;
    use rusqlite::{Connection, TransactionBehavior};

    use super::*;
    use crate::account::{account_ingest, ensure_owned_stream_v2_in_tx, local_account};
    use crate::op::{EdgeSpec, NodeContent, NodeId};

    const NOW: i64 = 1_700_000_000_000;
    const STREAM_A: [u8; 32] = [0x44; 32];
    const STREAM_B: [u8; 32] = [0x55; 32];

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    /// Mint the store's local account, then seed the `StreamOwn` fact for `stream` the way the C3.3
    /// acceptance tests do — `local_account` already folds the founder's roster fact (role owner)
    /// and the `account_auth_state` freshness row, so ownership is the only fact left to seed for
    /// an owner-authored entry to accept. Returns the local `account_id`.
    fn owned_stream_account(conn: &Connection, stream: StreamId) -> AccountId {
        let account_id = bootstrap::local_account(conn, NOW).expect("mint local account");
        seed_ownership(conn, stream, account_id);
        account_id
    }

    fn seed_ownership(conn: &Connection, stream: StreamId, owner: AccountId) {
        conn.execute(
            "INSERT INTO account_stream_ownership(stream_id, account_id, own_id, effective_at)
             VALUES(?1, ?2, ?3, 1)",
            params![
                stream.to_bytes().as_slice(),
                owner.to_bytes().as_slice(),
                [0x66_u8; 32].as_slice()
            ],
        )
        .unwrap();
    }

    fn content(title: &str) -> NodeContent {
        NodeContent {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            tags: Vec::new(),
            payload: None,
        }
    }

    fn node_create(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeCreate { node_id: NodeId::from(id), content: content(title) }
    }

    fn node_update(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeUpdate { node_id: NodeId::from(id), content: content(title) }
    }

    #[test]
    fn content_op_is_authorable_agrees_with_the_query_side_write_caps() {
        use rag_rat_query::memory::{
            MAX_EDGE_ANCHOR_LEN, MAX_MEMORY_BODY_LEN, MAX_MEMORY_PAYLOAD_LEN, MAX_MEMORY_TITLE_LEN,
        };
        // A memory at EVERY query-side write cap must still fit the signed /3 envelope. Worst case
        // for the char-counted title/body is a 4-byte char, plus a max-byte payload — this pins the
        // two crates' caps consistent even though the dependency only flows oplog → query (#680).
        let wide = '𝄞'.to_string(); // 4 UTF-8 bytes
        let maxed = NodeContent {
            kind: "Invariant".to_string(),
            title: wide.repeat(MAX_MEMORY_TITLE_LEN),
            body: wide.repeat(MAX_MEMORY_BODY_LEN),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            tags: Vec::new(),
            payload: Some("x".repeat(MAX_MEMORY_PAYLOAD_LEN)),
        };
        assert!(
            content_op_is_authorable(&MemoryOp::NodeCreate {
                node_id: NodeId::from("mem_max"),
                content: maxed,
            }),
            "a memory at every write cap must still be authorable",
        );
        // A body past the envelope cap is un-authorable — exactly what the reconcile quarantines.
        let oversized = NodeContent { body: "x".repeat(300 * 1024), ..content("t") };
        assert!(
            !content_op_is_authorable(&MemoryOp::NodeCreate {
                node_id: NodeId::from("mem_big"),
                content: oversized,
            }),
            "an oversized body exceeds the /3 envelope",
        );

        // The EDGE twin (#680): both free-form edge fields at the query-side cap — plus realistic
        // short source/owner ids — must still fit the signed /3 envelope, so `add_edge`'s write cap
        // can never mint an un-authorable EdgeAdd. This pins `MAX_EDGE_ANCHOR_LEN` consistent with
        // the oplog envelope bound even though the dependency only flows oplog → query.
        let maxed_edge = EdgeSpec {
            source_node_id: NodeId::from("mem_1700000000000_abcdef"),
            relation: EdgeRelation::DependsOn,
            target_repo_id: "r".repeat(MAX_EDGE_ANCHOR_LEN),
            target_kind: "node".to_string(),
            target_anchor: "a".repeat(MAX_EDGE_ANCHOR_LEN),
            owner_repo_id: "owner-repo-id".to_string(),
        };
        assert!(
            content_op_is_authorable(&MemoryOp::EdgeAdd { edge: maxed_edge }),
            "an edge with both free-form fields at the write cap must still be authorable",
        );
        // An anchor past the cap is un-authorable — exactly what the write cap rejects and the
        // reconcile quarantines.
        assert!(
            !content_op_is_authorable(&edge_add("mem_src", &"a".repeat(300 * 1024))),
            "an oversized edge anchor exceeds the /3 envelope",
        );
    }

    /// A `NodeCreate` on `id` whose canonical-CBOR body (`op::encode`) is EXACTLY `encoded_len`
    /// bytes — used to place an op precisely relative to the authorable bound. Within one CBOR
    /// text-length class each extra body char is exactly one extra encoded byte, so measuring the
    /// fixed op-envelope framing on a body already in the ~256 KiB (5-byte-prefix) class lets us
    /// size the real body to hit `encoded_len` on the nose.
    fn node_create_sized(id: &str, encoded_len: usize) -> MemoryOp {
        let build = |body: String| MemoryOp::NodeCreate {
            node_id: NodeId::from(id),
            content: NodeContent { body, ..content("t") },
        };
        let probe = 100_000;
        let framing = op::encode(&build("a".repeat(probe))).len() - probe;
        build("a".repeat(encoded_len - framing))
    }

    #[test]
    fn an_op_in_the_band_the_old_margin_over_quarantined_is_authorable_and_signs() {
        // #680 (P2b): the old body bound was `CAP - 1024`, but the real signed-entry overhead is
        // far smaller, so an op whose encoded body sat between `CAP - 1024` and the true
        // limit was permanently quarantined even though `sign_content_entry` would accept
        // it. Place an op squarely in that reclaimed band and prove BOTH that the predicate
        // now admits it AND that authoring it actually folds accepted — no `bail!`, no
        // wedge.
        let op = node_create_sized("mem_band", CONTENT_ENVELOPE_MAX_BYTES - 512);
        let encoded = op::encode(&op).len();
        assert_eq!(encoded, CONTENT_ENVELOPE_MAX_BYTES - 512, "the op is sized on the nose");
        assert!(
            encoded > CONTENT_ENVELOPE_MAX_BYTES - 1024,
            "the op sits in the band the old 1 KiB margin wrongly quarantined",
        );
        assert!(
            content_op_is_authorable(&op),
            "the reclaimed-band op is authorable under the exact overhead bound",
        );

        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        owned_stream_account(&conn, stream);
        let hashes = author_committed(&conn, stream, std::slice::from_ref(&op));
        assert_eq!(
            content_status(&conn.unchecked_transaction().unwrap(), &hashes[0]).unwrap().as_deref(),
            Some("accepted"),
            "the reclaimed-band op signs and folds accepted — the predicate did not \
             over-quarantine",
        );
    }

    #[test]
    fn an_op_one_byte_over_the_exact_bound_is_quarantined() {
        // Just past `CONTENT_OP_BODY_MAX_BYTES`: the predicate must return false. Returning true
        // here would let the `/3` author's §18a size check `bail!` on the whole batch — the
        // #680 wedge the quarantine exists to prevent.
        let op = node_create_sized("mem_over", CONTENT_OP_BODY_MAX_BYTES + 1);
        assert_eq!(op::encode(&op).len(), CONTENT_OP_BODY_MAX_BYTES + 1);
        assert!(!content_op_is_authorable(&op), "an op past the exact body bound is quarantined");
    }

    #[test]
    fn content_entry_max_overhead_bounds_the_real_signed_envelope() {
        // Drift guard: prove the hand-derived overhead constant against the REAL encoders. A
        // payload of exactly `CONTENT_OP_BODY_MAX_BYTES`, wrapped under the WIDEST header
        // `sign_content_entry` accepts, must sign and land at or under the §18a envelope cap — i.e.
        // the constant reserves enough headroom, and the measured overhead never exceeds it.
        let secret = crate::device::DeviceSecret::from_seed(&[9; 32]);
        // The widest signable header: crypto_suite 0 (⇒ key_id null), seq != 0 (⇒ prev_hash
        // present), a present grant_id, and all u64 fields maxed.
        let header = ContentEntryHeader {
            stream_id: StreamId::from_bytes([0x11; 32]),
            author_account_id: AccountId::from_bytes([0x22; 32]),
            device_fingerprint: secret.public().fingerprint(),
            seq: u64::MAX,
            lamport: u64::MAX,
            prev_hash: Some([0x33; 32]),
            grant_id: Some([0x44; 32]),
            roster_ref: [0x55; 32],
            owner_auth_len: u64::MAX,
            author_auth_len: u64::MAX,
            crypto_suite: 0,
            key_id: None,
        };
        // A canonical-CBOR payload of exactly `CONTENT_OP_BODY_MAX_BYTES` bytes — one bstr
        // (`0x5a` + 4-byte length + content), the same near-limit shape envelope.rs's own size test
        // uses.
        let mut payload = vec![0x5a];
        payload.extend_from_slice(&((CONTENT_OP_BODY_MAX_BYTES - 5) as u32).to_be_bytes());
        payload.resize(CONTENT_OP_BODY_MAX_BYTES, 0);
        let signed = envelope::sign_content_entry(&secret, &header, &payload)
            .expect("a body at the exact bound signs under the worst-case header");
        assert!(
            signed.signed_bytes.len() <= CONTENT_ENVELOPE_MAX_BYTES,
            "the derived overhead keeps the signed envelope within the §18a cap ({} > {})",
            signed.signed_bytes.len(),
            CONTENT_ENVELOPE_MAX_BYTES,
        );
        assert!(
            signed.signed_bytes.len() - payload.len() <= CONTENT_ENTRY_MAX_OVERHEAD_BYTES,
            "the real envelope overhead ({}) is within the derived worst-case constant ({})",
            signed.signed_bytes.len() - payload.len(),
            CONTENT_ENTRY_MAX_OVERHEAD_BYTES,
        );
    }

    fn edge_add(source: &str, anchor: &str) -> MemoryOp {
        MemoryOp::EdgeAdd {
            edge: EdgeSpec {
                source_node_id: NodeId::from(source),
                relation: EdgeRelation::DependsOn,
                target_repo_id: "repo".to_string(),
                target_kind: "node".to_string(),
                target_anchor: anchor.to_string(),
                owner_repo_id: "repo".to_string(),
            },
        }
    }

    /// Run the in-tx seam in its own IMMEDIATE txn and commit — the shape a live mutation uses.
    fn author_committed(conn: &Connection, stream: StreamId, ops: &[MemoryOp]) -> Vec<EntryHash> {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let hashes = author_content_batch_in_tx(&tx, stream, ops, NOW).expect("author batch");
        tx.commit().unwrap();
        hashes
    }

    fn genesis_ref(conn: &Connection) -> EntryHash {
        conn.query_row(
            "SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map(|bytes| fixed::<32>(&bytes).unwrap())
        .unwrap()
    }

    fn tail(conn: &Connection, stream: StreamId, account: AccountId) -> Option<ContentChainTail> {
        let fingerprint = local_device(conn, NOW).unwrap().fingerprint();
        let tx = conn.unchecked_transaction().unwrap();
        content_chain_tail(&tx, stream, account, fingerprint).unwrap()
    }

    fn stored_status(conn: &Connection, entry_hash: &EntryHash) -> Option<String> {
        conn.query_row(
            "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
            [entry_hash.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
    }

    fn header_of(conn: &Connection, entry_hash: &EntryHash) -> ContentEntryHeader {
        let signed_bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
                [entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        envelope::decode_content_signed(&signed_bytes).unwrap().header
    }

    #[test]
    fn a_batch_authors_owner_content_that_folds_accepted_with_dense_seqs_and_lamport_eq_seq() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream);

        let hashes = author_committed(&conn, stream, &[
            node_create("n1", "first"),
            node_update("n1", "second"),
        ]);
        assert_eq!(hashes.len(), 2, "two ops author two entries");

        // Every authored entry folds accepted.
        for entry_hash in &hashes {
            assert_eq!(stored_status(&conn, entry_hash).as_deref(), Some("accepted"));
            let (accepted,): (i64,) = conn
                .query_row(
                    "SELECT accepted FROM content_entries WHERE entry_hash = ?1",
                    [entry_hash.as_slice()],
                    |row| Ok((row.get(0)?,)),
                )
                .unwrap();
            assert_eq!(accepted, 1, "the entry carries accepted = 1");
        }

        // Dense seqs 0, 1 with lamport == seq (the projection LWW key).
        for (ordinal, entry_hash) in hashes.iter().enumerate() {
            let header = header_of(&conn, entry_hash);
            assert_eq!(header.seq, ordinal as u64, "seqs are dense from 0");
            assert_eq!(header.lamport, header.seq, "lamport == seq");
            assert_eq!(header.grant_id, None, "owner-authored: no grant");
            assert_eq!(header.roster_ref, genesis_ref(&conn), "roster_ref is the genesis hash");
            assert_eq!(header.author_account_id, account, "authored under the local account");
        }

        // The tail advanced to the highest seq.
        let advanced = tail(&conn, stream, account).expect("non-empty chain has a tail");
        assert_eq!(advanced.seq, 1, "the tail advanced to seq 1");
        assert_eq!(advanced.entry_hash, hashes[1], "the tail names the last authored entry");
    }

    #[test]
    fn the_projection_fold_materializes_the_accepted_dag_keyed_by_stream() {
        let conn = db();
        let stream_a = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream_a);

        // A NodeCreate/NodeUpdate/EdgeAdd batch on stream A: the update (seq 1, higher lamport)
        // wins the node content register over the create (seq 0).
        author_committed(&conn, stream_a, &[
            node_create("n1", "created"),
            node_update("n1", "updated"),
            edge_add("n1", "n2"),
        ]);

        let (content_json, status): (String, String) = conn
            .query_row(
                "SELECT content_json, status FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = ?2",
                params![stream_a.to_bytes().as_slice(), "n1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("stream A projects node n1");
        assert!(content_json.contains("updated"), "the NodeUpdate content wins the LWW register");
        assert!(!content_json.contains("created"), "the superseded create content is gone");
        assert_eq!(status, "active", "default node status");

        let edge_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_projected_edges WHERE stream_id = ?1",
                params![stream_a.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edge_rows, 1, "the EdgeAdd projects one present edge");

        // A SECOND stream (same local owner) authors its own node n1 — the stream-keying must keep
        // the two projections from colliding.
        let stream_b = StreamId::from_bytes(STREAM_B);
        seed_ownership(&conn, stream_b, account);
        author_committed(&conn, stream_b, &[node_create("n1", "b-only")]);

        let b_content: String = conn
            .query_row(
                "SELECT content_json FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = ?2",
                params![stream_b.to_bytes().as_slice(), "n1"],
                |row| row.get(0),
            )
            .expect("stream B projects its own node n1");
        assert!(b_content.contains("b-only"), "stream B keeps its own content");

        // Stream A's node n1 is untouched by stream B's authoring, and stream B has no edge rows.
        let a_content_after: String = conn
            .query_row(
                "SELECT content_json FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = ?2",
                params![stream_a.to_bytes().as_slice(), "n1"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            a_content_after.contains("updated"),
            "stream A's projection did not collide with B"
        );
        let b_edges: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_projected_edges WHERE stream_id = ?1",
                params![stream_b.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(b_edges, 0, "stream B authored no edges");
    }

    #[test]
    fn a_withheld_stream_own_makes_the_batch_roll_back_with_no_content_stored() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        // Mint the account but DO NOT seed ownership: with no `StreamOwn` fact the stream
        // declassifies, the entries never accept, and verify-accepted must roll the whole batch
        // back. Without verify-accepted this would silently store unaccepted /3 candidates.
        let _account = bootstrap::local_account(&conn, NOW).expect("mint local account");

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let result = author_content_batch_in_tx(&tx, stream, &[node_create("n1", "first")], NOW);
        assert!(result.is_err(), "verify-accepted rejects an unaccepted batch");
        drop(tx); // no commit → the IMMEDIATE txn rolls back

        let stored: i64 =
            conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(stored, 0, "the rolled-back batch stored no /3 entry");
        let projected: i64 = conn
            .query_row("SELECT count(*) FROM content_projected_nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(projected, 0, "no projection row survives the rollback");
    }

    #[test]
    fn the_projection_skips_an_accepted_row_whose_body_is_not_a_decodable_op() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream);
        // A good accepted entry via the seam.
        author_committed(&conn, stream, &[node_create("n1", "good")]);

        // Inject a second accepted /3 row on the same stream whose signed ENVELOPE is valid but
        // whose BODY is canonical CBOR that is not a `MemoryOp` — the shape a foreign entry could
        // take, since the acceptance layer (§8) never decodes the body. `content_ingest` would have
        // accepted it; the projection must SKIP it, not `bail!` and crash every later local author.
        let device = local_device(&conn, NOW).unwrap();
        let header = ContentEntryHeader {
            stream_id: stream,
            author_account_id: account,
            device_fingerprint: device.fingerprint(),
            seq: 99,
            lamport: 99,
            prev_hash: Some([0xab; 32]),
            grant_id: None,
            roster_ref: genesis_ref(&conn),
            owner_auth_len: 0,
            author_auth_len: 0,
            crypto_suite: 0,
            key_id: None,
        };
        // A bare canonical CBOR integer: decodes as CBOR, but is not the op envelope ⇒ `op::decode`
        // errors (the path the fix must tolerate).
        let signed = envelope::sign_content_entry(device.secret(), &header, &[0x01]).unwrap();
        conn.execute(
            "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?8, 1, ?9, 0)",
            params![
                signed.entry_hash.as_slice(),
                stream.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                device.fingerprint().to_bytes().as_slice(),
                99_u64.to_be_bytes().as_slice(),
                [0xab_u8; 32].as_slice(),
                genesis_ref(&conn).as_slice(),
                0_u64.to_be_bytes().as_slice(),
                signed.signed_bytes,
            ],
        )
        .unwrap();

        // Reproject the stream: it must NOT error, and only the good node projects.
        let tx = conn.unchecked_transaction().unwrap();
        content_projection::reproject_accepted_content_stream(&tx, stream)
            .expect("an undecodable body is skipped, not fatal");
        tx.commit().unwrap();
        let node_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_projected_nodes WHERE stream_id = ?1",
                params![stream.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(node_count, 1, "only the decodable node projects; the bad body is skipped");
    }

    #[test]
    fn the_chain_tail_reader_reports_genesis_for_an_empty_chain_and_the_head_when_populated() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream);

        // Empty chain ⇒ None (the seam's genesis gate: seq 0, prev None).
        assert!(tail(&conn, stream, account).is_none(), "empty chain has no tail");

        let hashes =
            author_committed(&conn, stream, &[node_create("n1", "a"), node_create("n2", "b")]);

        let head = tail(&conn, stream, account).expect("populated chain has a tail");
        assert_eq!(head.seq, 1, "the tail seq is the highest authored seq");
        assert_eq!(head.entry_hash, hashes[1], "the tail names the highest-seq entry");
    }

    // ── C5a: the sealed suite-1 author seam (UNWIRED) ──

    /// Mint the store's local account and publish an owned `/2` stream via the REAL ownership seam
    /// (`ensure_owned_stream_v2_in_tx`) — the sealed mint needs a genuine effective `StreamOwn`,
    /// not the direct-seeded `account_stream_ownership` shortcut the plaintext acceptance tests
    /// use.
    fn owned_v2(conn: &Connection) -> (AccountId, StreamId) {
        let account = local_account(conn, NOW).expect("mint local account");
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let stream =
            ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).expect("publish owned /2 stream");
        tx.commit().unwrap();
        (account, stream)
    }

    #[test]
    fn a_sealed_batch_authors_suite_1_entries_that_fold_accepted_locally() {
        let conn = db();
        let (account, stream) = owned_v2(&conn);
        let hashes = author_content_batch(
            &conn,
            stream,
            &[node_create("n1", "first"), node_update("n1", "second")],
            SealPolicy::Sealed,
            NOW,
        )
        .expect("seal + author");
        assert_eq!(hashes.len(), 2, "two ops author two sealed entries");
        for entry_hash in &hashes {
            assert_eq!(
                stored_status(&conn, entry_hash).as_deref(),
                Some("accepted"),
                "a sealed entry folds accepted locally (acceptance is key-independent)",
            );
            let header = header_of(&conn, entry_hash);
            assert_eq!(header.crypto_suite, 1, "authored as suite 1");
            assert!(header.key_id.is_some(), "a sealed entry carries a key_id");
        }
        // The first-key mint left an accepted content-key wrap on the secrets log.
        assert!(
            secrets::select_current_sealing_wrap(&conn, account, stream).unwrap().is_some(),
            "a content key was minted for the stream",
        );
    }

    #[test]
    fn a_sealed_entry_folds_accepted_through_content_ingest_on_a_keyless_peer() {
        // S3 FOLD-FIREWALL TRIPWIRE: acceptance of a sealed `/3` entry is header/citation-level, so
        // a peer that replicated the account roster but never received the content key still folds
        // it `accepted`. This is the coverage the account-log and decode-only sealed tests don't
        // provide — a suite-1 entry driven through `content_ingest` + refold.
        let author = db();
        let (account, stream) = owned_v2(&author);
        let hashes = author_content_batch(
            &author,
            stream,
            &[node_create("n1", "secret")],
            SealPolicy::Sealed,
            NOW,
        )
        .expect("seal + author");
        let sealed_bytes: Vec<u8> = author
            .query_row(
                "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
                [hashes[0].as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            envelope::decode_content_signed(&sealed_bytes).unwrap().header.crypto_suite,
            1,
            "the authored entry is sealed",
        );

        // The author's CONTROL log (genesis + StreamOwn) is the public roster a peer replicates.
        // The secrets-log StreamKeyWrap is deliberately NOT replicated: the peer holds no
        // content key.
        let control: Vec<Vec<u8>> = {
            let mut stmt = author
                .prepare(
                    "SELECT signed_bytes FROM account_entries
                     WHERE account_id = ?1 AND log_id = 0 ORDER BY seq",
                )
                .unwrap();
            stmt.query_map([account.to_bytes().as_slice()], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };

        let peer = db();
        for bytes in &control {
            account_ingest(&peer, bytes, NOW).expect("replicate the account control log");
        }
        // The peer cannot resolve the content key — no wrap was replicated.
        assert!(
            secrets::select_current_sealing_wrap(&peer, account, stream).unwrap().is_none(),
            "the keyless peer has no content key for the stream",
        );

        // Ingest the sealed content entry and settle its deferred acceptance fold.
        content_storage::content_ingest(&peer, &sealed_bytes, NOW).expect("ingest sealed entry");
        content_storage::settle_pending_content_refolds(&peer).expect("settle the refold");

        let (status, accepted): (String, i64) = peer
            .query_row(
                "SELECT s.status, e.accepted FROM content_entries e
                 JOIN content_entry_status s ON s.entry_hash = e.entry_hash
                 WHERE e.entry_hash = ?1",
                [hashes[0].as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (status.as_str(), accepted),
            ("accepted", 1),
            "the sealed entry folds accepted on a peer with no content key",
        );
    }

    #[test]
    fn sealing_a_stream_the_account_does_not_own_bails_and_leaks_no_row() {
        // A Sealed-intent stream the account does NOT own: NoCurrentKey → try to mint the first
        // key, but the mint is owner-gated on an effective StreamOwn, so it bails → the
        // whole sealed batch fails closed. NO plaintext-fallback, and no row leaked into
        // any projectable state.
        let conn = db();
        local_account(&conn, NOW).expect("mint local account");
        let unowned = StreamId::from_bytes([0x99; 32]);
        let result = author_content_batch(
            &conn,
            unowned,
            &[node_create("n1", "x")],
            SealPolicy::Sealed,
            NOW,
        );
        assert!(result.is_err(), "a keyless Sealed stream fails closed");
        let entries: i64 =
            conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(entries, 0, "no /3 content row survives the fail-closed bail");
        let projected: i64 = conn
            .query_row("SELECT count(*) FROM content_projected_nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(projected, 0, "no projection row leaks from the bailed sealed batch");
    }

    #[test]
    fn plaintext_authoring_is_refused_after_a_stream_has_sealed() {
        let conn = db();
        let (_account, stream) = owned_v2(&conn);
        // Sealing mints a wrap AND authors a suite-1 entry — both downgrade-ratchet inputs.
        author_content_batch(&conn, stream, &[node_create("n1", "x")], SealPolicy::Sealed, NOW)
            .expect("seal");
        let before: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
                [stream.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        // Plaintext authoring on the now-sealed stream is refused — no silent downgrade to
        // cleartext.
        let result = author_content_batch(
            &conn,
            stream,
            &[node_create("n2", "y")],
            SealPolicy::Plaintext,
            NOW,
        );
        assert!(result.is_err(), "plaintext authoring is refused after the stream sealed");
        let after: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
                [stream.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, after, "the refused plaintext batch stored no row");
    }

    #[test]
    fn the_downgrade_ratchet_fires_on_an_accepted_wrap_or_an_accepted_sealed_entry() {
        let conn = db();
        let (account, stream) = owned_v2(&conn);

        // A fresh owned stream (plaintext-only) does not ratchet.
        {
            let tx = conn.unchecked_transaction().unwrap();
            assert!(
                !stream_has_sealed_ratchet(&tx, account, stream).unwrap(),
                "a fresh owned stream is not sealed",
            );
        }

        // The WRAP branch: mint a wrap only (no content) → the ratchet fires on wrap presence.
        {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            secrets::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = conn.unchecked_transaction().unwrap();
            assert!(
                stream_has_sealed_ratchet(&tx, account, stream).unwrap(),
                "an accepted StreamKeyWrap ratchets the stream",
            );
        }

        // The SEALED-ENTRY branch, independent of any wrap: a different stream with an accepted
        // suite-1 row but NO wrap still ratchets (proves the OR clause, not just wrap-presence).
        let other = StreamId::from_bytes([0x7a; 32]);
        let device = local_device(&conn, NOW).unwrap();
        let key = ContentKey::from_seed(&[0x40; 32]);
        let header = ContentEntryHeader {
            stream_id: other,
            author_account_id: account,
            device_fingerprint: device.fingerprint(),
            seq: 0,
            lamport: 0,
            prev_hash: None,
            grant_id: None,
            roster_ref: genesis_ref(&conn),
            owner_auth_len: 0,
            author_auth_len: 0,
            crypto_suite: 0,
            key_id: None,
        };
        // The nonce value is not load-bearing here (only suite-1 presence arms the ratchet), so
        // seal via the random-nonce production entry point rather than a fixed injected
        // nonce.
        let signed =
            envelope::seal_and_sign_content_entry(device.secret(), &header, &[0x01], &key).unwrap();
        conn.execute(
            "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 1, ?8, 0)",
            params![
                signed.entry_hash.as_slice(),
                other.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                device.fingerprint().to_bytes().as_slice(),
                0_u64.to_be_bytes().as_slice(),
                genesis_ref(&conn).as_slice(),
                0_u64.to_be_bytes().as_slice(),
                signed.signed_bytes,
            ],
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert!(
            secrets::select_current_sealing_wrap(&tx, account, other).unwrap().is_none(),
            "the other stream has no wrap",
        );
        assert!(
            stream_has_sealed_ratchet(&tx, account, other).unwrap(),
            "an accepted sealed entry ratchets the stream even with no wrap",
        );
    }

    #[test]
    fn plaintext_policy_authors_suite_0_on_a_fresh_stream_unchanged() {
        // `SealPolicy::Plaintext` on a never-sealed owned stream authors suite-0 through the
        // existing in-tx seam — public authoring is unchanged.
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        owned_stream_account(&conn, stream);
        let hashes = author_content_batch(
            &conn,
            stream,
            &[node_create("n1", "x")],
            SealPolicy::Plaintext,
            NOW,
        )
        .expect("plaintext author");
        assert_eq!(stored_status(&conn, &hashes[0]).as_deref(), Some("accepted"));
        let header = header_of(&conn, &hashes[0]);
        assert_eq!(header.crypto_suite, 0, "the Plaintext policy authors suite 0");
        assert!(header.key_id.is_none(), "a suite-0 entry has no key_id");
    }

    #[test]
    fn content_op_is_sealed_authorable_reserves_the_aead_overhead() {
        // The sealed body bound is exactly 40 bytes (24 nonce + 16 tag) tighter than the plaintext
        // one, so a sealed op can never mint an un-signable entry.
        assert_eq!(CONTENT_SEALED_OP_BODY_MAX_BYTES, CONTENT_OP_BODY_MAX_BYTES - 40);
        let at = node_create_sized("mem_at", CONTENT_SEALED_OP_BODY_MAX_BYTES);
        assert_eq!(op::encode(&at).len(), CONTENT_SEALED_OP_BODY_MAX_BYTES, "sized on the nose");
        assert!(content_op_is_sealed_authorable(&at), "an op at the sealed bound is authorable");
        let over = node_create_sized("mem_over", CONTENT_SEALED_OP_BODY_MAX_BYTES + 1);
        assert!(
            !content_op_is_sealed_authorable(&over),
            "one byte over the sealed bound is quarantined",
        );

        // Drift guard: a body at the sealed bound, sealed under the WIDEST header, signs at or
        // under the §18a envelope cap — the reserve is enough (mirrors the plaintext
        // overhead guard).
        let secret = crate::device::DeviceSecret::from_seed(&[9; 32]);
        let key = ContentKey::from_seed(&[0x20; 32]);
        let header = ContentEntryHeader {
            stream_id: StreamId::from_bytes([0x11; 32]),
            author_account_id: AccountId::from_bytes([0x22; 32]),
            device_fingerprint: secret.public().fingerprint(),
            seq: u64::MAX,
            lamport: u64::MAX,
            prev_hash: Some([0x33; 32]),
            grant_id: Some([0x44; 32]),
            roster_ref: [0x55; 32],
            owner_auth_len: u64::MAX,
            author_auth_len: u64::MAX,
            crypto_suite: 0,
            key_id: None,
        };
        let mut op_body = vec![0x5a];
        op_body.extend_from_slice(&((CONTENT_SEALED_OP_BODY_MAX_BYTES - 5) as u32).to_be_bytes());
        op_body.resize(CONTENT_SEALED_OP_BODY_MAX_BYTES, 0);
        // The nonce value is not load-bearing here (the size is nonce-width-independent), so seal
        // via the random-nonce production entry point rather than a fixed injected nonce.
        let signed =
            envelope::seal_and_sign_content_entry(&secret, &header, &op_body, &key).unwrap();
        assert!(
            signed.signed_bytes.len() <= CONTENT_ENVELOPE_MAX_BYTES,
            "the sealed reserve keeps the signed envelope within the §18a cap ({} > {})",
            signed.signed_bytes.len(),
            CONTENT_ENVELOPE_MAX_BYTES,
        );
    }

    #[test]
    fn a_rotation_in_the_resolution_window_makes_txn_b_bail_not_seal_stale() {
        // P1: the sealed author resolves the content key in an autocommit window between txn A and
        // txn B. A DeviceRemove + rotation committing a fresh higher-epoch wrap in that window
        // would otherwise let txn B seal under the STALE key — acceptance is
        // key-independent, so it folds accepted — and a device the rotation revoked could
        // decrypt this post-removal entry. `revalidate_sealing_selection` re-reads the
        // selection under txn B's write lock and bails when it no longer names the resolved
        // key. Simulated directly: resolve, rotate the accepted wrap set to a higher epoch,
        // then assert the re-validation bails on the stale baseline.
        let conn = db();
        let (account, stream) = owned_v2(&conn);
        // Establish the epoch-0 sealing wrap the resolution would name (the stale baseline).
        author_content_batch(&conn, stream, &[node_create("n1", "first")], SealPolicy::Sealed, NOW)
            .expect("mint the initial key + author");
        let baseline = secrets::select_current_sealing_wrap(&conn, account, stream)
            .unwrap()
            .expect("an epoch-0 selection");
        assert_eq!(baseline.key_epoch, 0, "the initial mint is epoch 0");

        // The concurrent rotation: commit a fresh epoch-1 wrap (what a DeviceRemove-driven rotation
        // authors), so the current selection no longer names the resolved epoch-0 key.
        {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            secrets::rotate_stream_key_in_tx(&tx, stream, NOW).expect("rotate to epoch 1");
            tx.commit().unwrap();
        }
        let rotated = secrets::select_current_sealing_wrap(&conn, account, stream)
            .unwrap()
            .expect("an epoch-1 selection");
        assert_eq!(rotated.key_epoch, 1, "the rotation advanced the selection to epoch 1");

        // Under txn B's write lock the stale epoch-0 baseline no longer matches → bail (retry).
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = revalidate_sealing_selection(&tx, account, stream, &baseline).unwrap_err();
        drop(tx);
        assert!(
            err.to_string().contains("rotated between resolution and sealing"),
            "the re-validation bails on a rotation in the resolution window: {err}",
        );

        // The guard is not a blanket refusal: the fresh selection still validates.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        revalidate_sealing_selection(&tx, account, stream, &rotated)
            .expect("the current selection re-validates");
        tx.commit().unwrap();
    }

    #[test]
    fn an_empty_sealed_batch_is_a_noop_and_does_not_arm_the_ratchet() {
        // P2: an empty Sealed batch must run NO key management. Minting the stream's first
        // StreamKeyWrap would arm the downgrade ratchet, permanently refusing later plaintext
        // authoring after a call that authored nothing.
        let conn = db();
        let (account, stream) = owned_v2(&conn);
        let hashes = author_content_batch(&conn, stream, &[], SealPolicy::Sealed, NOW)
            .expect("an empty sealed batch is a no-op");
        assert!(hashes.is_empty(), "an empty batch authors no entries");
        // No wrap was minted → the stream never sealed.
        assert!(
            secrets::select_current_sealing_wrap(&conn, account, stream).unwrap().is_none(),
            "the empty sealed batch minted no content key",
        );
        // Plaintext authoring still works — the downgrade ratchet was never armed.
        let authored = author_content_batch(
            &conn,
            stream,
            &[node_create("n1", "x")],
            SealPolicy::Plaintext,
            NOW,
        )
        .expect("plaintext authoring still works after an empty sealed batch");
        assert_eq!(authored.len(), 1, "the empty sealed batch did not arm the downgrade ratchet");
    }
}
