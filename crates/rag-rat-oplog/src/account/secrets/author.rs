//! The in-tx secrets-log (`log_id = 1`) content-key mint + `StreamKeyWrap` author seam (C4.3a,
//! #607).
//!
//! The owner's counterpart to the C4.2b acceptance evaluator: it mints a fresh per-stream content
//! key, seals it to every roster-EFFECTIVE device, and authors a `StreamKeyWrap` onto the account's
//! secrets chain inside the caller's IMMEDIATE transaction — verify-accepted-or-rollback, mirroring
//! [`super::super::content::author_content_batch_in_tx`]. It authors wraps that fold `accepted`;
//! nothing consumes the key for content SEALING yet (that is C5), and the `key_id` adoption
//! cross-check + the derived sealing-key projection + `sync enable` are C4.3b.
//!
//! C4.4 (#607) adds LAZY rotation on device removal: [`rotate_stream_key_in_tx`] mints a fresh key
//! at a HIGHER epoch re-sealed only to the remaining effective devices — rotation is exactly a mint
//! whose SOLE delta from the initial one is the epoch, so it reuses
//! [`author_stream_key_wrap_in_tx`] unchanged. [`ensure_stream_key_current_in_tx`] is the lazy
//! trigger the seal path calls: it rotates only when a removed device still holds the current key,
//! and returns a typed [`RotationOutcome`] (never an error) so a MEMBER device — which cannot
//! author a rotation but may legitimately seal — does not roll its seal txn back. Nothing calls the
//! C4.4 entry points yet (machinery one slice ahead of its consumer).
//!
//! Three load-bearing invariants the fold alone does NOT enforce:
//!
//! - **The secrets chain is `(account, device)`-scoped across ALL streams, never per-stream.**
//!   Every stream's wraps interleave on one dense chain, so the seq is read from the shared `log =
//!   1` tail ([`authoring::account_chain_tail`] with [`SECRETS_LOG`]), NOT a per-stream tail. A
//!   per-stream tail would restart `seq` at 0 per stream, collide the second stream's `wrap@0` at
//!   the same `(account, log, device, seq)` accepted slot, fork, and roll the mint back.
//! - **Wrap-to-self is mandatory and must be byte-reproducible at adoption.** The plaintext key is
//!   EPHEMERAL (no plaintext store); the minter recovers it later via the uniform adoption unwrap
//!   (C4.3b), so it must seal to itself. Because the local device is a roster-effective recipient,
//!   its wrap is already in the fan-out; an authoring-time self-unwrap round-trip
//!   ([`assert_self_wrap_round_trips`]) proves recoverability, which verify-accepted does NOT (a
//!   WrapContext drift folds `accepted` yet bricks the stream at C5).
//! - **`WrapContext.key_epoch` at seal MUST equal the op's `key_epoch`.** They are independent
//!   values in code; a mismatch leaves the wrap unopenable by everyone, the minter included, and is
//!   invisible at epoch 0.

use anyhow::Context;
use rusqlite::Transaction;

use super::super::bootstrap::{self, LocalAccountRef};
use super::super::envelope::{AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
use super::super::fold::{SECRETS_LOG, SUPPORTED_OP_VERSION};
use super::super::keywrap::{self, ContentKey, WrapContext};
use super::super::storage::{self, CandidateInsert};
use super::super::{AccountId, authoring};
use super::ops::{self, StreamKeyWrap, WrapEntry};
use crate::identity::LocalDevice;
use crate::local_device;
use crate::stream::StreamId;

type EntryHash = [u8; 32];

/// The key epoch a stream's content key first mints at. C4.4 lazy rotation bumps it on device
/// removal; C4.3a only ever mints the initial epoch.
const INITIAL_KEY_EPOCH: u64 = 0;

/// Mint a fresh content key for `stream_id` and author a `StreamKeyWrap` sealing it to every
/// roster-effective device, WITHIN the caller's transaction: verify the wrap folds `accepted` and
/// roll back otherwise. Returns the authored entry hash. Neither opens nor commits the txn.
///
/// The caller must have (1) minted the store's local account ([`bootstrap::local_account`], which
/// self-transacts and cannot nest here) and (2) made `stream_id` OWNED and effective (e.g. via
/// [`authoring::ensure_owned_stream_v2_in_tx`]) before calling — a wrap authored before its
/// `StreamOwn` is effective parks `unknown_account` and rolls the batch back.
///
/// The plaintext key is EPHEMERAL: sealed to every recipient, self-unwrap-verified, then dropped
/// (zeroized on drop) — it is NOT persisted anywhere in plaintext, so C4.3a writes only the wrap
/// op.
pub fn mint_and_author_stream_key_wrap_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<EntryHash> {
    let key = ContentKey::generate()?;
    author_stream_key_wrap_in_tx(tx, stream_id, &key, INITIAL_KEY_EPOCH, now_ms)
}

/// The outcome of [`ensure_stream_key_current_in_tx`]. Every variant is a NON-error signal: `Err`
/// from the ensure/rotate path is reserved for infra/DB failures, never a policy outcome (a member
/// legitimately can't rotate, and that must not roll back a seal txn).
#[derive(Debug)]
pub enum RotationOutcome {
    /// Rotation was needed and this (owner) device authored a fresh higher-epoch `StreamKeyWrap`.
    Rotated(EntryHash),
    /// No rotation needed — every recipient of the current wrap is still roster-effective (or the
    /// stream has no current wrap at all, so there is nothing to rotate).
    Current,
    /// Rotation IS needed but this device holds no live owner incarnation, so it cannot author one.
    /// A member device still seals under the CURRENT key — roster membership is READ access, not
    /// authoring authority — so the caller must proceed with the seal, NOT fail it. (An additive
    /// `CatchUp` arm belongs here when new-device catch-up ships.)
    StaleButNotOwner,
}

/// Rotate `stream_id`'s content key: mint a FRESH key at `current_max_accepted_epoch + 1` and
/// author it sealed to the roster-EFFECTIVE devices, WITHIN the caller's transaction
/// (verify-accepted-or- rollback). Returns the authored entry hash. Neither opens nor commits the
/// txn.
///
/// Rotation is a mint whose ONLY delta from the initial one is the epoch: recipients (which now
/// EXCLUDE the removed device), the owner-only `authority_ref`, the self-unwrap round-trip, and
/// verify-accepted all carry from [`author_stream_key_wrap_in_tx`] unchanged. It does NOT cite the
/// triggering `DeviceRemove` — lazy rotation is local policy, not a chained authority act.
///
/// Errors if the stream has NO prior accepted wrap (nothing to rotate; the seal path mints an
/// initial key via `current_sealing_key` → `NoCurrentKey` instead), or if the local device is not a
/// current owner (the inherited owner-only gate bails). [`ensure_stream_key_current_in_tx`] never
/// reaches either error — it returns `Current`/`StaleButNotOwner` first.
pub fn rotate_stream_key_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<EntryHash> {
    let account_id = bootstrap::local_account_ref(tx)?
        .context(
            "cannot rotate a StreamKeyWrap before the store's local account is minted (call \
             local_account first)",
        )?
        .account_id;

    // Epoch = accepted_max + 1 over EFFECTIVE ACCEPTED wraps only. `select_current_sealing_wrap`
    // reads `accepted = 1` rows, so condemned wraps never feed the max — this is deliberate: taking
    // the max over condemned wraps would let a removed owner author epoch `u64::MAX` (→ condemned)
    // and DoS every future rotation by forcing the overflow error below.
    let current = super::sealing::select_current_sealing_wrap(tx, account_id, stream_id)?.context(
        "cannot rotate a stream with no prior accepted StreamKeyWrap (nothing to rotate)",
    )?;
    // `checked_add`: ERROR at `u64::MAX`, never wrap. A wrap-to-0 would silently lose the max-epoch
    // selection and regress sealing to an old key — a confidentiality footgun. Re-stamping a
    // numeric epoch that a condemned wrap once used is fine and deliberate (the condemned wrap
    // is gone from the accepted set); we add NO high-water mark (sticky local state would
    // diverge derive-on-read).
    let next_epoch = current
        .key_epoch
        .checked_add(1)
        .context("stream key epoch is at u64::MAX; cannot rotate")?;

    let key = ContentKey::generate()?;
    author_stream_key_wrap_in_tx(tx, stream_id, &key, next_epoch, now_ms)
}

/// The lazy rotation trigger the seal path (and a future CLI) calls before sealing `stream_id`:
/// rotate the content key IF a removed device still holds the current key, otherwise noop. Returns
/// a typed [`RotationOutcome`], never an error for a policy reason.
///
/// The rotation-NEEDED test ([`super::sealing::stream_key_rotation_needed`]) is device-independent
/// (current-wrap recipients vs the roster). The owner gate is checked HERE, BEFORE calling
/// [`rotate_stream_key_in_tx`]: a non-owner would make `rotate` bail (→ `Err` → the caller's seal
/// txn rolls back), which a member's legitimate seal must not suffer — so a member sees
/// `StaleButNotOwner` and seals under the current key instead.
pub fn ensure_stream_key_current_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<RotationOutcome> {
    let account_id = bootstrap::local_account_ref(tx)?
        .context(
            "cannot ensure a stream key is current before the store's local account is minted",
        )?
        .account_id;

    if !super::sealing::stream_key_rotation_needed(tx, account_id, stream_id)? {
        return Ok(RotationOutcome::Current);
    }

    // Rotation is needed — but only a current owner can author one. Gate on the live owner
    // incarnation (exactly what `rotate` → `author_stream_key_wrap_in_tx` would require) so a
    // member returns `StaleButNotOwner` rather than triggering a rollback-inducing `bail!` in
    // `rotate`.
    let device = local_device(tx, now_ms)?;
    if storage::effective_owner_incarnation_for_device(tx, account_id, device.fingerprint())?
        .is_none()
    {
        return Ok(RotationOutcome::StaleButNotOwner);
    }

    Ok(RotationOutcome::Rotated(rotate_stream_key_in_tx(tx, stream_id, now_ms)?))
}

/// The mint core with the content key and epoch injected, so a test can pin the key
/// (confidentiality checks) and drive a NONZERO epoch (the `WrapContext.key_epoch == op.key_epoch`
/// invariant is invisible at epoch 0). The caller owns the key's lifetime; it drops (and zeroizes)
/// at the call site once this returns.
fn author_stream_key_wrap_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    key: &ContentKey,
    key_epoch: u64,
    now_ms: i64,
) -> anyhow::Result<EntryHash> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author a StreamKeyWrap before the store's local account is minted (call \
         local_account first)",
    )?;
    let device = local_device(tx, now_ms)?;
    let fingerprint = device.fingerprint();

    // Recipients = the roster-EFFECTIVE devices (SHOULD-FIX-1). Sealing a fresh key to a REMOVED
    // device would re-grant it read access and defeat rotation-on-removal, so
    // `stored_device_pubkeys` (fold-independent, keeps removed devices) is the wrong reader
    // here.
    let recipients = storage::list_effective_roster_x25519_pubkeys(tx, account_id)?;

    let mut wraps = Vec::with_capacity(recipients.len());
    for (recipient_fp, recipient_pub) in &recipients {
        // The AAD binds (account, stream, epoch, recipient) so no wrap can be transplanted across
        // any of them. `key_epoch` here is the op's `key_epoch` — the two MUST be one value.
        let ctx = WrapContext {
            account_id: account_id.to_bytes(),
            stream_id: stream_id.to_bytes(),
            key_epoch,
            recipient_pub: recipient_pub.to_bytes(),
        };
        let sealed = keywrap::seal_content_key(key, &ctx, recipient_pub)?;
        wraps.push(WrapEntry { recipient_fp: *recipient_fp, sealed });
    }

    // Prove the minter can recover the key from its OWN wrap BEFORE committing — verify-accepted
    // proves the op folds accepted, not that it is unwrappable (independent failure modes).
    assert_self_wrap_round_trips(&device, account_id, stream_id, key_epoch, key, &wraps)?;

    let wrap = StreamKeyWrap { stream_id, key_id: key.key_id().to_bytes(), key_epoch, wraps };

    // Dense seq from the shared `(account, device)` secrets tail across ALL streams (BLOCKER-1).
    // The secrets log has no genesis, so an empty chain is the legitimate first-wrap case (seq
    // 0, no predecessor) — unlike the control chain, where an empty tail is a programming
    // error.
    let (seq, prev_hash) =
        match authoring::account_chain_tail(tx, account_id, fingerprint, SECRETS_LOG)? {
            Some((tail_seq, tail_hash)) => (
                tail_seq
                    .checked_add(1)
                    .context("secrets chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail_hash),
            ),
            None => (0, None),
        };

    // Owner-only authority (B3): cite the local device's LIVE owner incarnation, resolved from the
    // current fold. For the founder this resolves to the genesis hash (a founder's owner_id IS its
    // genesis), but a demoted-then-repromoted or non-founder owner gets its CURRENT incarnation — a
    // hard-coded genesis would cite a CLOSED incarnation and roll every mint back. `None` means the
    // local device is not a current owner, so it cannot author a StreamKeyWrap at all.
    let authority_ref =
        storage::effective_owner_incarnation_for_device(tx, account_id, fingerprint)?.context(
            "the local device holds no live owner incarnation; cannot author a StreamKeyWrap \
             (owner-only authority)",
        )?;

    let header = AccountEntryHeader {
        account_id,
        log_id: SECRETS_LOG,
        device_fingerprint: fingerprint,
        seq,
        prev_hash,
        // `parent_ref = genesis_hash` is authoring convention only (the secrets evaluator does not
        // gate on it); it mirrors the control convention.
        parent_ref: Some(genesis_hash),
        entry_type: ops::entry_type_of(&wrap),
        op_version: SUPPORTED_OP_VERSION,
        crypto_suite: 0,
        // A secrets header's `auth_len` asserts the author's CONTROL-fold effective count (there is
        // no "secrets fold length"); cited as-of now so our own entry never parks `auth_len_ahead`
        // against our own fold. Read in THIS snapshot.
        auth_len: storage::account_effective_count(tx, account_id)?,
        // The header `key_id` selects a `/3` content key; a secrets op carries its own key_id
        // inside the payload, so the header field is null.
        key_id: None,
        // The local device's LIVE owner incarnation, resolved above from the current fold (the
        // founder's is its genesis, but any current owner works — not pinned to the founder).
        authority_ref: Some(authority_ref),
    };
    let payload = ops::encode(&wrap)
        .map_err(|err| anyhow::anyhow!("encoding the StreamKeyWrap op failed: {err}"))?;
    let signed = sign_account_entry(device.secret(), &header, &payload)?;
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    match storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
        CandidateInsert::AtCapacity(scope) => anyhow::bail!(
            "the account candidate store is at capacity ({scope:?}); cannot author the \
             StreamKeyWrap",
        ),
    }

    // ONE account-scoped refold (its secrets pass classifies the wrap), then verify-accepted. NEVER
    // persist an unaccepted wrap: the secrets candidate tail must stay equal to the accepted tail,
    // or the next mint's seq self-forks off an orphaned candidate. An owner authoring on its
    // own owned stream accepts, so anything else is an authority gap (missing/uneffective
    // `StreamOwn`, a stale `auth_len`, a contested account) and the whole caller mutation must
    // roll back.
    let statuses = storage::refold_in_tx(tx, account_id)?;
    match statuses.get(&verified.entry_hash).map(String::as_str) {
        Some("accepted") => {},
        other => anyhow::bail!(
            "authored StreamKeyWrap did not fold accepted (status {other:?}); rolling back",
        ),
    }
    Ok(verified.entry_hash)
}

/// Prove the minting device recovers the key from its OWN wrap in the fan-out, under the exact
/// `WrapContext` adoption (C4.3b) will reconstruct: `{owning account, stream, op.key_epoch, the
/// device's ROSTER x25519}`. The minter must be a recipient (its key is ephemeral, with no
/// plaintext store), and any drift in that context — wrong account/stream/epoch, or a non-roster
/// x25519 — would brick the stream at C5 with no recovery, so it is caught HERE rather than left to
/// the fold.
fn assert_self_wrap_round_trips(
    device: &LocalDevice,
    account_id: AccountId,
    stream_id: StreamId,
    key_epoch: u64,
    key: &ContentKey,
    wraps: &[WrapEntry],
) -> anyhow::Result<()> {
    let fingerprint = device.fingerprint();
    let self_wrap = wraps.iter().find(|w| w.recipient_fp == fingerprint).context(
        "the minting device is not among the wrap recipients; the content key would be \
         unrecoverable (the roster-effective recipient set must include the local device)",
    )?;
    // `recipient_pub` is the device's ROSTER x25519 — the same key the genesis certified and the
    // adoption unwrap reconstructs from; NOT a freshly re-derived one.
    let ctx = WrapContext {
        account_id: account_id.to_bytes(),
        stream_id: stream_id.to_bytes(),
        key_epoch,
        recipient_pub: device.x25519_public().to_bytes(),
    };
    let recovered = keywrap::unwrap_content_key(&self_wrap.sealed, device.x25519_secret(), &ctx)
        .context("the minting device cannot unwrap its own StreamKeyWrap")?;
    anyhow::ensure!(
        recovered.key_id() == key.key_id(),
        "self-unwrap recovered a key whose key_id differs from the minted key",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rusqlite::{Connection, TransactionBehavior};

    use super::*;
    use crate::account::cut::Cut;
    use crate::account::fold::CONTROL_LOG;
    use crate::account::keywrap::unwrap_content_key;
    use crate::account::ops::{AccountOp, DeviceRole};
    use crate::account::secrets::ops::{
        DecodedSecretsOp, decode, entry_type as secrets_entry_type,
    };
    use crate::account::{select_current_sealing_wrap, stream_key_rotation_needed};
    use crate::device::DeviceX25519Secret;

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    /// Mint the local account and ensure a repo's `/2` stream is owned, returning `(account,
    /// stream)`. This is the real prerequisite state a live `sync enable` caller establishes before
    /// minting a key wrap.
    fn account_with_owned_stream(conn: &Connection, repo: &str) -> (AccountId, StreamId) {
        let account = bootstrap::local_account(conn, NOW).expect("mint local account");
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let stream_id =
            authoring::ensure_owned_stream_v2_in_tx(&tx, repo, NOW).expect("own stream");
        tx.commit().unwrap();
        (account, stream_id)
    }

    /// Run the mint seam in its own IMMEDIATE txn and commit — the shape a live caller uses.
    fn mint_committed(conn: &Connection, stream: StreamId) -> EntryHash {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let hash = mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).expect("mint + author");
        tx.commit().unwrap();
        hash
    }

    fn status(conn: &Connection, hash: &EntryHash) -> Option<(String, Option<String>)> {
        conn.query_row(
            "SELECT status, detail FROM account_entry_status WHERE entry_hash = ?1",
            [hash.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    }

    /// Decode the stored `StreamKeyWrap` op for one authored entry hash.
    fn stored_wrap(conn: &Connection, hash: &EntryHash) -> StreamKeyWrap {
        let signed_bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
                [hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let signed = crate::account::envelope::decode_account_signed(&signed_bytes).unwrap();
        let DecodedSecretsOp::Known(wrap) =
            decode(secrets_entry_type::STREAM_KEY_WRAP, &signed.payload).unwrap()
        else {
            panic!("stored entry is a known StreamKeyWrap");
        };
        wrap
    }

    fn header_of(conn: &Connection, hash: &EntryHash) -> AccountEntryHeader {
        let signed_bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
                [hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        crate::account::envelope::decode_account_signed(&signed_bytes).unwrap().header
    }

    #[test]
    fn a_single_device_mint_folds_accepted_and_seals_to_the_founder() {
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let hash = mint_committed(&conn, stream);

        assert_eq!(status(&conn, &hash), Some(("accepted".to_string(), None)), "the wrap accepts");
        let wrap = stored_wrap(&conn, &hash);
        assert_eq!(wrap.stream_id, stream, "the op names the owned stream");
        assert_eq!(wrap.key_epoch, INITIAL_KEY_EPOCH, "the first mint stamps epoch 0");
        assert_eq!(wrap.wraps.len(), 1, "a lone founder roster has exactly one recipient");

        // The header cites the owner incarnation and stamps the secrets-log coordinates.
        let header = header_of(&conn, &hash);
        assert_eq!(header.log_id, SECRETS_LOG, "authored on the secrets log");
        assert_eq!(header.seq, 0, "the first wrap on the device's secrets chain is seq 0");
        assert_eq!(header.prev_hash, None, "the first secrets entry has no predecessor");
        assert_eq!(header.entry_type, secrets_entry_type::STREAM_KEY_WRAP);
        assert!(header.authority_ref.is_some(), "cites the founder owner incarnation");
        let _ = account;
    }

    #[test]
    fn the_founder_can_unwrap_its_own_wrap_back_to_the_op_key_id() {
        // Round-trip through the REAL authored op: the recipient unwraps under the WrapContext the
        // adoption seam will reconstruct, and the recovered key's key_id matches the op's key_id.
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let hash = mint_committed(&conn, stream);
        let wrap = stored_wrap(&conn, &hash);

        let device = local_device(&conn, NOW).unwrap();
        let self_wrap = wrap
            .wraps
            .iter()
            .find(|w| w.recipient_fp == device.fingerprint())
            .expect("the founder is a recipient of its own mint");
        let ctx = WrapContext {
            account_id: account.to_bytes(),
            stream_id: stream.to_bytes(),
            key_epoch: wrap.key_epoch,
            recipient_pub: device.x25519_public().to_bytes(),
        };
        let recovered = unwrap_content_key(&self_wrap.sealed, device.x25519_secret(), &ctx)
            .expect("the founder unwraps its own wrap");
        assert_eq!(
            recovered.key_id().to_bytes(),
            wrap.key_id,
            "the recovered key's key_id matches the op's signed key_id",
        );
    }

    #[test]
    fn a_multi_device_roster_seals_a_wrap_to_every_effective_member() {
        // Add a second (member) device to the roster; the mint must seal to BOTH the founder and
        // the member, and the member must be able to unwrap its wrap.
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
        let member = add_member_device(&conn, account, &member_x);

        let hash = mint_committed(&conn, stream);
        let wrap = stored_wrap(&conn, &hash);
        assert_eq!(wrap.wraps.len(), 2, "founder + member are both recipients");

        let member_wrap = wrap
            .wraps
            .iter()
            .find(|w| w.recipient_fp == member)
            .expect("the added member is a recipient");
        let ctx = WrapContext {
            account_id: account.to_bytes(),
            stream_id: stream.to_bytes(),
            key_epoch: wrap.key_epoch,
            recipient_pub: member_x.public().to_bytes(),
        };
        unwrap_content_key(&member_wrap.sealed, &member_x, &ctx)
            .expect("the member unwraps its wrap under the reproducible WrapContext");
    }

    #[test]
    fn a_removed_device_is_not_a_recipient_of_a_later_mint() {
        // SHOULD-FIX-1: the recipient set is roster-EFFECTIVE. Sealing a FRESH key to a removed
        // device would re-grant it read access and defeat rotation-on-removal. This test disagrees
        // with the fold-independent `stored_device_pubkeys` fallback (which still returns the
        // removed member), so it fails unless the effective-roster reader is the one
        // actually used.
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
        let member = add_member_device(&conn, account, &member_x);

        // Remove the member (empty cuts — it authored nothing on any chain), then mint a fresh key.
        author_control_op(&conn, account, &AccountOp::DeviceRemove {
            device_fingerprint: member,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        });

        let hash = mint_committed(&conn, stream);
        let wrap = stored_wrap(&conn, &hash);
        let device = local_device(&conn, NOW).unwrap();
        assert_eq!(wrap.wraps.len(), 1, "only the still-effective founder is a recipient");
        assert_eq!(
            wrap.wraps[0].recipient_fp,
            device.fingerprint(),
            "the recipient is the founder"
        );
        assert!(
            !wrap.wraps.iter().any(|w| w.recipient_fp == member),
            "the removed member is NOT sealed a fresh key (rotation-on-removal holds)",
        );
    }

    #[test]
    fn a_forked_sibling_device_add_never_shadows_the_effective_x25519_key() {
        // P1: the recipient key must bind to the EXACT accepted enrollment (via roster_ref), not to
        // the fingerprint across all stored candidates. Enroll the member with K_good, then poison
        // account_entries with a rejected/forked sibling DeviceAdd (same fingerprint, K_bad, a
        // different entry_hash). The mint must seal to K_good; the old fingerprint-keyed reader
        // could pick K_bad, so the member's K_good unwrap below would fail under the buggy
        // code.
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let member_x_good = DeviceX25519Secret::from_seed(&[0x5c; 32]);
        let member = add_member_device(&conn, account, &member_x_good);

        let member_x_bad = DeviceX25519Secret::from_seed(&[0xbb; 32]);
        insert_forked_member_device_add(&conn, account, &member_x_bad);

        let hash = mint_committed(&conn, stream);
        let wrap = stored_wrap(&conn, &hash);
        let member_wrap = wrap
            .wraps
            .iter()
            .find(|w| w.recipient_fp == member)
            .expect("the effective member is a recipient");
        // The wrap opens under the member's REAL (K_good) secret + roster WrapContext ⇒ the mint
        // sealed to K_good. Under the fingerprint-keyed bug it would have sealed to K_bad and this
        // unwrap would fail.
        let good_ctx = WrapContext {
            account_id: account.to_bytes(),
            stream_id: stream.to_bytes(),
            key_epoch: wrap.key_epoch,
            recipient_pub: member_x_good.public().to_bytes(),
        };
        unwrap_content_key(&member_wrap.sealed, &member_x_good, &good_ctx)
            .expect("the wrap sealed to the effective K_good key, not the forked sibling's K_bad");
        // The poisoned K_bad secret cannot open it — corroborates the wrap is NOT bound to K_bad.
        let bad_ctx = WrapContext { recipient_pub: member_x_bad.public().to_bytes(), ..good_ctx };
        assert!(
            unwrap_content_key(&member_wrap.sealed, &member_x_bad, &bad_ctx).is_err(),
            "the forked sibling's K_bad must not open the wrap",
        );
    }

    #[test]
    fn two_mints_on_different_streams_share_one_dense_secrets_chain() {
        // BLOCKER-1: the secrets chain is (account, device)-scoped across ALL streams. A per-stream
        // tail would restart seq at 0 for the second stream and self-fork; the shared tail advances
        // seq densely and BOTH wraps accept.
        let conn = db();
        let (_account, stream_a) = account_with_owned_stream(&conn, "repo-a");
        let stream_b = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let id = authoring::ensure_owned_stream_v2_in_tx(&tx, "repo-b", NOW).expect("own b");
            tx.commit().unwrap();
            id
        };

        let a = mint_committed(&conn, stream_a);
        let b = mint_committed(&conn, stream_b);

        assert_eq!(
            status(&conn, &a),
            Some(("accepted".to_string(), None)),
            "stream A wrap accepts"
        );
        assert_eq!(
            status(&conn, &b),
            Some(("accepted".to_string(), None)),
            "stream B wrap accepts"
        );
        assert_eq!(header_of(&conn, &a).seq, 0, "first mint is seq 0 on the shared chain");
        assert_eq!(header_of(&conn, &b).seq, 1, "second mint is seq 1 on the SAME chain");
        assert_eq!(
            header_of(&conn, &b).prev_hash,
            Some(a),
            "the second wrap chains off the first — one dense (account, device) secrets chain",
        );
    }

    #[test]
    fn a_nonzero_epoch_mint_round_trips_through_the_op() {
        // SHOULD-FIX-3: WrapContext.key_epoch at seal MUST equal op.key_epoch. This is invisible at
        // epoch 0, so drive the private core at a nonzero epoch and prove the self-wrap opens under
        // op.key_epoch (mismatched epoch → nobody can unwrap).
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let key = ContentKey::from_seed(&[0x77; 32]);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let hash =
            author_stream_key_wrap_in_tx(&tx, stream, &key, 7, NOW).expect("mint at epoch 7");
        tx.commit().unwrap();

        let wrap = stored_wrap(&conn, &hash);
        assert_eq!(wrap.key_epoch, 7, "the op carries the requested nonzero epoch");
        let device = local_device(&conn, NOW).unwrap();
        let self_wrap = wrap.wraps.iter().find(|w| w.recipient_fp == device.fingerprint()).unwrap();
        // Unwrapping under op.key_epoch succeeds …
        let good_ctx = WrapContext {
            account_id: account.to_bytes(),
            stream_id: stream.to_bytes(),
            key_epoch: wrap.key_epoch,
            recipient_pub: device.x25519_public().to_bytes(),
        };
        assert!(
            unwrap_content_key(&self_wrap.sealed, device.x25519_secret(), &good_ctx).is_ok(),
            "unwrap under the op's epoch succeeds",
        );
        // … and under the WRONG epoch the AAD binding rejects it, which is exactly the drift the
        // authoring-time self-check guards against.
        let wrong_ctx = WrapContext { key_epoch: 0, ..good_ctx };
        assert!(
            unwrap_content_key(&self_wrap.sealed, device.x25519_secret(), &wrong_ctx).is_err(),
            "a mismatched epoch cannot unwrap (AAD transplant guard)",
        );
    }

    #[test]
    fn the_plaintext_content_key_is_never_persisted() {
        // The key is ephemeral: only its SEALED wraps are stored. Prove the raw 32 key bytes appear
        // in no stored candidate blob (they are inside the AEAD ciphertext, never in the clear).
        let conn = db();
        let (_account, stream) = account_with_owned_stream(&conn, "repo-x");
        let key = ContentKey::from_seed(&[0xa5; 32]);
        let raw = key.as_slice().to_vec();

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_stream_key_wrap_in_tx(&tx, stream, &key, INITIAL_KEY_EPOCH, NOW).expect("mint");
        tx.commit().unwrap();

        let mut stmt = conn.prepare("SELECT signed_bytes FROM account_entries").unwrap();
        let blobs: Vec<Vec<u8>> = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for blob in &blobs {
            assert!(
                !blob.windows(raw.len()).any(|w| w == raw.as_slice()),
                "the plaintext content key must not appear in any stored candidate blob",
            );
        }
    }

    #[test]
    fn a_wrap_authored_before_ownership_is_effective_rolls_back() {
        // verify-accepted-or-rollback: with no StreamOwn fact the wrap parks `unknown_account`, so
        // the mint must roll back and store NO secrets entry (an unaccepted candidate would desync
        // the secrets tail from the accepted tail and self-fork the next mint).
        let conn = db();
        let _account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        // Derive an (unowned) /2 stream id without publishing its StreamOwn.
        let stream = crate::account::owned_stream_v2_id(&conn, "repo-x")
            .unwrap()
            .expect("derive the /2 stream id");

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let result = mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW);
        assert!(result.is_err(), "verify-accepted rejects a wrap that does not fold accepted");
        drop(tx); // no commit → the IMMEDIATE txn rolls back

        let secrets_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries WHERE log_id = ?1",
                [SECRETS_LOG],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(secrets_rows, 0, "the rolled-back mint stored no secrets entry");
    }

    // ── C4.4: lazy rotation on device removal ──

    #[test]
    fn removing_a_recipient_makes_rotation_needed_and_rotate_reseals_to_survivors() {
        // The primary behavioral path, THROUGH THE REAL MINT SEAM (a hand-rolled wrap that omits
        // self would mask that the predicate's soundness rests on wrap-to-self): mint to the full
        // roster, remove a device, and rotate to a fresh higher-epoch key held only by the
        // survivors — the removed device can no longer unwrap.
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
        let member = add_member_device(&conn, account, &member_x);

        let mint = mint_committed(&conn, stream);
        assert_eq!(stored_wrap(&conn, &mint).wraps.len(), 2, "founder + member are recipients");
        assert!(
            !stream_key_rotation_needed(&conn, account, stream).unwrap(),
            "no rotation needed while every current-wrap recipient is effective",
        );

        // Remove the member: its wrap for the current key is now held by a non-effective device.
        author_control_op(&conn, account, &AccountOp::DeviceRemove {
            device_fingerprint: member,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        });
        assert!(
            stream_key_rotation_needed(&conn, account, stream).unwrap(),
            "a removed recipient of the current wrap triggers rotation",
        );

        // Rotate → a fresh key at epoch 1 sealed ONLY to the surviving founder.
        let rotated = rotate_committed(&conn, stream);
        assert_eq!(
            status(&conn, &rotated),
            Some(("accepted".to_string(), None)),
            "the rotation wrap folds accepted",
        );
        let rot_wrap = stored_wrap(&conn, &rotated);
        assert_eq!(rot_wrap.key_epoch, 1, "rotation stamps accepted_max + 1");
        let device = local_device(&conn, NOW).unwrap();
        assert_eq!(rot_wrap.wraps.len(), 1, "only the surviving founder is a recipient");
        assert_eq!(rot_wrap.wraps[0].recipient_fp, device.fingerprint(), "sealed to the founder");
        assert!(
            !rot_wrap.wraps.iter().any(|w| w.recipient_fp == member),
            "the removed member gets no wrap for the fresh key (it cannot unwrap the rotated key)",
        );

        // The C4.3b selection now returns epoch 1, and rotation is no longer needed (the current
        // epoch-1 wrap names only effective devices — the stale epoch-0 wrap is not the selection).
        let selected = select_current_sealing_wrap(&conn, account, stream).unwrap().unwrap();
        assert_eq!(selected.key_epoch, 1, "the selection advances to the rotated epoch");
        assert!(
            !stream_key_rotation_needed(&conn, account, stream).unwrap(),
            "after rotation the current wrap is held only by effective devices",
        );
    }

    #[test]
    fn ensure_rotates_for_an_owner_then_reports_current() {
        // The lazy trigger end-to-end: an owner sees Current before a removal, Rotated after, and
        // Current again once the fresh key is in place (idempotent).
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");
        let member_x = DeviceX25519Secret::from_seed(&[0x5c; 32]);
        let member = add_member_device(&conn, account, &member_x);
        mint_committed(&conn, stream);

        assert!(
            matches!(ensure_committed(&conn, stream), RotationOutcome::Current),
            "nothing to rotate while every recipient is effective",
        );

        author_control_op(&conn, account, &AccountOp::DeviceRemove {
            device_fingerprint: member,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        });

        let RotationOutcome::Rotated(rotated) = ensure_committed(&conn, stream) else {
            panic!("an owner with a stale current-wrap recipient rotates");
        };
        assert_eq!(stored_wrap(&conn, &rotated).key_epoch, 1, "ensure rotated to epoch 1");
        assert_eq!(status(&conn, &rotated), Some(("accepted".to_string(), None)));

        assert!(
            matches!(ensure_committed(&conn, stream), RotationOutcome::Current),
            "a second ensure is a noop — the current wrap is fresh",
        );
    }

    #[test]
    fn rotating_a_stream_with_no_prior_wrap_errors() {
        // Q6: rotate with no prior accepted wrap is an ERROR (not treat-as-mint, not no-op).
        // `ensure` never reaches it — no wrap ⇒ predicate false ⇒ Current — but the raw
        // entry point guards.
        let conn = db();
        let (_account, stream) = account_with_owned_stream(&conn, "repo-x");
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = rotate_stream_key_in_tx(&tx, stream, NOW).unwrap_err();
        drop(tx);
        assert!(
            err.to_string().contains("no prior accepted"),
            "rotation with no prior wrap errors: {err}",
        );
    }

    #[test]
    fn rotation_errors_at_u64_max_epoch_instead_of_wrapping() {
        // S3: `checked_add` the epoch; ERROR at u64::MAX, never wrap. A wrap-to-0 would silently
        // lose the max-epoch selection and regress sealing to an old key. Drive an accepted
        // wrap to the ceiling via the private core (the evaluator reads no key_epoch, so it
        // folds accepted).
        let conn = db();
        let (_account, stream) = account_with_owned_stream(&conn, "repo-x");
        let key = ContentKey::from_seed(&[0x9e; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_stream_key_wrap_in_tx(&tx, stream, &key, u64::MAX, NOW).expect("author at u64::MAX");
        tx.commit().unwrap();

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = rotate_stream_key_in_tx(&tx, stream, NOW).unwrap_err();
        drop(tx);
        assert!(
            err.to_string().contains("u64::MAX"),
            "rotation refuses to overflow the epoch: {err}",
        );
    }

    #[test]
    fn a_member_device_with_rotation_needed_is_stale_but_not_owner() {
        // B1: a device that is NOT a current owner cannot author a rotation, but must NOT error or
        // roll back — a member legitimately seals under the current key. The local device founds
        // the account, mints, then a SECOND owner demotes it to a plain member; a
        // still-removed recipient keeps rotation needed, so `ensure` must return
        // StaleButNotOwner (not Err, not Rotated).
        let conn = db();
        let (account, stream) = account_with_owned_stream(&conn, "repo-x");

        // A second owner whose secret we control, so it can author the founder's demotion.
        let owner_b_ed = crate::device::DeviceSecret::from_seed(&[0x2b; 32]);
        let owner_b_x = DeviceX25519Secret::from_seed(&[0xb2; 32]);
        let owner_id_b = author_control_op(&conn, account, &AccountOp::DeviceAdd {
            device_fingerprint: owner_b_ed.public().fingerprint(),
            ed25519_pubkey: owner_b_ed.public().to_bytes(),
            x25519_pubkey: owner_b_x.public().to_bytes(),
            role: DeviceRole::Owner,
            label: None,
        });

        // A member that will be removed to make rotation needed.
        let m_ed = crate::device::DeviceSecret::from_seed(&[0x3d; 32]);
        let m_x = DeviceX25519Secret::from_seed(&[0xd3; 32]);
        let m_fp = m_ed.public().fingerprint();
        author_control_op(&conn, account, &AccountOp::DeviceAdd {
            device_fingerprint: m_fp,
            ed25519_pubkey: m_ed.public().to_bytes(),
            x25519_pubkey: m_x.public().to_bytes(),
            role: DeviceRole::Member,
            label: None,
        });

        // Mint via the real seam while the founder is still an owner → seals to all three.
        let mint = mint_committed(&conn, stream);
        assert_eq!(stored_wrap(&conn, &mint).wraps.len(), 3, "founder + owner_b + member sealed");

        // The founder (still owner) removes the member → rotation needed.
        author_control_op(&conn, account, &AccountOp::DeviceRemove {
            device_fingerprint: m_fp,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        });
        assert!(
            stream_key_rotation_needed(&conn, account, stream).unwrap(),
            "the removed member is still a recipient of the current wrap",
        );

        // owner_b demotes the founder, preserving the founder's whole history via cuts at its chain
        // tails (the mint stays accepted; only the owner ROLE closes).
        let founder = local_device(&conn, NOW).unwrap();
        let founder_owner_id =
            storage::effective_owner_incarnation_for_device(&conn, account, founder.fingerprint())
                .unwrap()
                .expect("the founder is an owner before the demotion");
        let ctrl_tail = chain_tail(&conn, account, founder.fingerprint(), CONTROL_LOG)
            .expect("the founder has a control chain");
        let secrets_tail = chain_tail(&conn, account, founder.fingerprint(), SECRETS_LOG)
            .expect("the founder authored the mint on its secrets chain");
        author_control_op_as(&conn, account, &owner_b_ed, owner_id_b, &AccountOp::OwnerDemote {
            device_fingerprint: founder.fingerprint(),
            owner_id: founder_owner_id,
            control_cut: Cut::At { seq: ctrl_tail.0, hash: ctrl_tail.1 },
            secrets_cut: Cut::At { seq: secrets_tail.0, hash: secrets_tail.1 },
            reason: "demote".to_string(),
        });

        // The founder is now a plain member: still a recipient (can seal), but not an owner.
        assert!(
            storage::effective_owner_incarnation_for_device(&conn, account, founder.fingerprint())
                .unwrap()
                .is_none(),
            "the founder holds no live owner incarnation after the demotion",
        );
        assert!(
            stream_key_rotation_needed(&conn, account, stream).unwrap(),
            "rotation is still needed after the demotion (the mint survives, the member is gone)",
        );
        assert!(
            matches!(ensure_committed(&conn, stream), RotationOutcome::StaleButNotOwner),
            "a member that finds rotation needed is StaleButNotOwner — no rotate, no error",
        );

        // StaleButNotOwner authored nothing: only the original mint is on the secrets log.
        let secrets_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries WHERE log_id = ?1 AND accepted = 1",
                [SECRETS_LOG],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(secrets_rows, 1, "StaleButNotOwner does not author a rotation");
    }

    /// Rotate the stream key in its own IMMEDIATE txn and commit — the shape a live caller uses.
    fn rotate_committed(conn: &Connection, stream: StreamId) -> EntryHash {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let hash = rotate_stream_key_in_tx(&tx, stream, NOW).expect("rotate");
        tx.commit().unwrap();
        hash
    }

    /// Run the lazy rotation trigger in its own IMMEDIATE txn and commit.
    fn ensure_committed(conn: &Connection, stream: StreamId) -> RotationOutcome {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let outcome = ensure_stream_key_current_in_tx(&tx, stream, NOW).expect("ensure");
        tx.commit().unwrap();
        outcome
    }

    /// The (seq, hash) tail of a device's `(account, device, log)` chain — used to place tight
    /// cuts.
    fn chain_tail(
        conn: &Connection,
        account: AccountId,
        fp: crate::op::DeviceFingerprint,
        log: u8,
    ) -> Option<(u64, EntryHash)> {
        let tx = conn.unchecked_transaction().unwrap();
        authoring::account_chain_tail(&tx, account, fp, log).unwrap()
    }

    /// Author + refold a control op signed by an ARBITRARY device (not just the founder), citing
    /// `authority_ref`, on that device's own dense control chain. The non-founder counterpart of
    /// [`author_control_op`].
    fn author_control_op_as(
        conn: &Connection,
        account: AccountId,
        signer: &crate::device::DeviceSecret,
        authority_ref: EntryHash,
        op: &AccountOp,
    ) -> EntryHash {
        use crate::account::ops as control_ops;

        let signer_fp = signer.public().fingerprint();
        let LocalAccountRef { genesis_hash, .. } =
            bootstrap::local_account_ref(conn).unwrap().unwrap();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let (seq, prev_hash) =
            match authoring::account_chain_tail(&tx, account, signer_fp, CONTROL_LOG).unwrap() {
                Some((tail_seq, tail_hash)) => (tail_seq + 1, Some(tail_hash)),
                None => (0, None),
            };
        let header = AccountEntryHeader {
            account_id: account,
            log_id: CONTROL_LOG,
            device_fingerprint: signer_fp,
            seq,
            prev_hash,
            parent_ref: Some(genesis_hash),
            entry_type: control_ops::entry_type_of(op),
            op_version: 1,
            crypto_suite: 0,
            auth_len: storage::account_effective_count(&tx, account).unwrap(),
            key_id: None,
            authority_ref: Some(authority_ref),
        };
        let payload = control_ops::encode(op).unwrap();
        let signed = sign_account_entry(signer, &header, &payload).unwrap();
        let verified = VerifiedAccountEntry {
            header: signed.header,
            payload: signed.payload,
            entry_hash: signed.entry_hash,
        };
        storage::insert_candidate(&tx, &verified, &signed.signed_bytes, NOW).unwrap();
        storage::refold_in_tx(&tx, account).unwrap();
        tx.commit().unwrap();
        verified.entry_hash
    }

    /// Author a control op under the founder (the local device) citing its own genesis incarnation,
    /// in its own IMMEDIATE txn, and refold — the shape that publishes a roster mutation.
    fn author_control_op(conn: &Connection, account: AccountId, op: &AccountOp) -> EntryHash {
        use crate::account::ops as control_ops;

        let founder = local_device(conn, NOW).unwrap();
        let LocalAccountRef { genesis_hash, .. } =
            bootstrap::local_account_ref(conn).unwrap().unwrap();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let tail = authoring::account_chain_tail(&tx, account, founder.fingerprint(), CONTROL_LOG)
            .unwrap()
            .expect("control chain is non-empty post-genesis");
        let header = AccountEntryHeader {
            account_id: account,
            log_id: CONTROL_LOG,
            device_fingerprint: founder.fingerprint(),
            seq: tail.0 + 1,
            prev_hash: Some(tail.1),
            parent_ref: Some(genesis_hash),
            entry_type: control_ops::entry_type_of(op),
            op_version: 1,
            crypto_suite: 0,
            auth_len: storage::account_effective_count(&tx, account).unwrap(),
            key_id: None,
            authority_ref: Some(genesis_hash),
        };
        let payload = control_ops::encode(op).unwrap();
        let signed = sign_account_entry(founder.secret(), &header, &payload).unwrap();
        let verified = VerifiedAccountEntry {
            header: signed.header,
            payload: signed.payload,
            entry_hash: signed.entry_hash,
        };
        storage::insert_candidate(&tx, &verified, &signed.signed_bytes, NOW).unwrap();
        storage::refold_in_tx(&tx, account).unwrap();
        tx.commit().unwrap();
        verified.entry_hash
    }

    /// Add a member device to the roster (folds effective) and return its fingerprint.
    fn add_member_device(
        conn: &Connection,
        account: AccountId,
        member_x: &DeviceX25519Secret,
    ) -> crate::op::DeviceFingerprint {
        use crate::device::DeviceSecret;

        let member_pub = DeviceSecret::from_seed(&[0x2c; 32]).public();
        let member_fp = member_pub.fingerprint();
        author_control_op(conn, account, &AccountOp::DeviceAdd {
            device_fingerprint: member_fp,
            ed25519_pubkey: member_pub.to_bytes(),
            x25519_pubkey: member_x.public().to_bytes(),
            role: DeviceRole::Member,
            label: None,
        });
        member_fp
    }

    /// Raw-insert a rejected/forked sibling `DeviceAdd` for the member fingerprint (seed `0x2c`)
    /// carrying `x_bad` — present in `account_entries` but NOT effective (a stranded control entry
    /// with a bogus predecessor, so the refold parks it). Its payload differs (different x25519),
    /// so its `entry_hash` differs from the accepted enrollment's — the poison a
    /// fingerprint-keyed reader would wrongly select.
    fn insert_forked_member_device_add(
        conn: &Connection,
        account: AccountId,
        x_bad: &DeviceX25519Secret,
    ) {
        use crate::account::ops as control_ops;
        use crate::device::DeviceSecret;

        let member_ed = DeviceSecret::from_seed(&[0x2c; 32]);
        let member_fp = member_ed.public().fingerprint();
        let founder = local_device(conn, NOW).unwrap();
        let LocalAccountRef { genesis_hash, .. } =
            bootstrap::local_account_ref(conn).unwrap().unwrap();
        let add_bad = AccountOp::DeviceAdd {
            device_fingerprint: member_fp,
            ed25519_pubkey: member_ed.public().to_bytes(),
            x25519_pubkey: x_bad.public().to_bytes(),
            role: DeviceRole::Member,
            label: None,
        };
        let header = AccountEntryHeader {
            account_id: account,
            log_id: CONTROL_LOG,
            device_fingerprint: founder.fingerprint(),
            seq: 99, // stranded slot with a bogus predecessor ⇒ parked, never effective
            prev_hash: Some([0xee; 32]),
            parent_ref: Some(genesis_hash),
            entry_type: control_ops::entry_type_of(&add_bad),
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: Some(genesis_hash),
        };
        let payload = control_ops::encode(&add_bad).unwrap();
        let signed = sign_account_entry(founder.secret(), &header, &payload).unwrap();
        // Insert AFTER the accepted enrollment (higher rowid), with accepted = 0: a
        // fingerprint-keyed reader iterating in rowid order would end with this K_bad,
        // overwriting K_good.
        conn.execute(
            "INSERT INTO account_entries(
                 entry_hash, account_id, log_id, device_fingerprint, seq, prev_hash, parent_ref,
                 authority_ref, entry_type, accepted, signed_bytes, received_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11)",
            rusqlite::params![
                signed.entry_hash.as_slice(),
                account.to_bytes().as_slice(),
                CONTROL_LOG,
                founder.fingerprint().to_bytes().as_slice(),
                99_i64,
                [0xee_u8; 32].as_slice(),
                genesis_hash.as_slice(),
                genesis_hash.as_slice(),
                control_ops::entry_type::DEVICE_ADD,
                signed.signed_bytes,
                NOW,
            ],
        )
        .unwrap();
    }
}
