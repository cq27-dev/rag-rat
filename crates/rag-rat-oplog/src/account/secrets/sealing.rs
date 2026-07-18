//! Sealing-key selection + the key_id adoption cross-check (sync phase C4.3b, #607).
//!
//! The READ side of the secrets log: which content key a device would seal a stream under RIGHT
//! NOW, and the device-local cross-check that guards it. Both are DERIVE-ON-READ — pure functions
//! of the current accepted `StreamKeyWrap` set, recomputed on every call (no cached table, no
//! refold pass). That is what makes eviction automatic and convergent: a refold that condemns the
//! current wrap simply drops it from `accepted = 1`, and the next read selects the surviving max
//! (possibly a LOWER epoch — by design; see [`current_sealing_key`]).
//!
//! FOLD FIREWALL (load-bearing): the adoption cross-check runs here, at read time, and NEVER in the
//! fold. Its unwrap step is only runnable by a recipient, so it can never be a fold input without
//! breaking convergence — the shared fold verdict (`accepted` / `account_entry_status`) must stay
//! device-independent. A local mismatch / unwrap failure is therefore LOCAL evidence only, written
//! to `sync_security_events` and nothing else.

use rusqlite::{Connection, params};

use super::super::keywrap::{self, ContentKey, KeyId, WrapContext};
use super::super::{AccountId, envelope, fold};
use super::ops::{self, DecodedSecretsOp, StreamKeyWrap};
use super::security_event::{self, SyncSecurityEvent, SyncSecurityEventKind};
use crate::identity::LocalDevice;
use crate::stream::StreamId;

/// The current sealing selection for a stream — the winning `(epoch, key_id)` over the accepted
/// wrap set, plus the entry hash that decided the tiebreak. Carries the CLAIMED `key_id` (the op
/// payload field); the cross-check against the actually-UNWRAPPED key is [`current_sealing_key`]'s
/// job. Also the CLI "what key is current for this stream" surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedWrap {
    pub key_id: KeyId,
    pub key_epoch: u64,
    pub minting_entry_hash: [u8; 32],
}

/// The outcome of resolving the content key a device would seal a stream under right now. Every
/// non-`Ready` is a do-not-seal signal (fail-closed); a `Result::Err` from [`current_sealing_key`]
/// is reserved for infra/DB failures, never a crypto/authority outcome.
pub enum SealingKeyOutcome {
    /// The recovered content key, cross-checked to match the selected op's signed `key_id`.
    Ready(ContentKey),
    /// No accepted wrap exists for the stream — nothing to seal with. Also the contested-account
    /// case (the fold keeps contested wraps out of `accepted`, so the selection is simply empty —
    /// no special case here).
    NoCurrentKey,
    /// The stream has a current key, but this device is not a recipient of any wrap at the current
    /// `(epoch, key_id)`. C4.4's added-after-mint catch-up / rotation trigger; C5 surfaces a
    /// cross-account granted writer (no wrap in the owning account's roster) this way too.
    NotRecipient,
    /// This device IS a recipient, but no wrap naming it opened to the selected `key_id` — every
    /// candidate failed the AEAD tag or the key_id cross-check. Fail closed; the failures were
    /// recorded in `sync_security_events`.
    FailedClosed,
}

/// One EFFECTIVE accepted `StreamKeyWrap` op for a stream, decoded from its stored bytes.
struct AcceptedStreamWrap {
    entry_hash: [u8; 32],
    wrap: StreamKeyWrap,
}

/// The stream's current sealing selection, derived on read from the accepted wrap set — no cached
/// table, no refold pass, convergent by construction. `None` when no accepted wrap exists. This is
/// the CLI "what key is current" surface; the secret-recovering counterpart is
/// [`current_sealing_key`].
pub fn select_current_sealing_wrap(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<Option<SelectedWrap>> {
    Ok(select_from_wraps(&list_accepted_stream_key_wraps(conn, account_id, stream_id)?))
}

/// Resolve the content key THIS device would seal `stream_id` under right now, running the C4.3b
/// adoption cross-check.
///
/// Selects the current `(epoch, key_id)` (derive-on-read), then collects every wrap naming this
/// device at that `(epoch, key_id)` — INCLUDING same-`(epoch, key_id)` fan-out siblings across
/// multiple ops (the tiebreak-winning op may not be one that names this device) — and tries each:
/// - an unwrap FAILURE (AEAD tag / blocklisted epk / non-contributory DH — the primary
///   manifestation of a substituted wrap) records `wrap_unwrap_failed` and moves on;
/// - a clean unwrap whose `key_id` disagrees records `wrap_key_id_mismatch` and moves on;
/// - the first wrap that unwraps to the selected `key_id` is the key (two distinct keys sharing a
///   `key_id` is an HKDF-SHA256 second-preimage — excluded — so try-until-pass is
///   deterministic-in-result).
///
/// Fails closed (`FailedClosed`) only when no candidate passes. NEVER mutates a fold verdict.
///
/// Takes `&LocalDevice`, NOT a bare secret: the my-wrap lookup needs the device fingerprint
/// (`sha256(ed25519 pk)`), which is not derivable from the x25519 secret. Does NOT call
/// `local_device` (which MINTS an identity on first call) — a read API must not mint, so the caller
/// supplies the device.
///
/// DURABILITY: the `sync_security_events` INSERT autocommits on `&Connection`. C5 must call this
/// PRE-txn (acquire the key, THEN open the authoring txn) or a caller that bails would roll the
/// evidence back with it. `now_ms` stamps `observed_at_ms` (injected clock).
pub fn current_sealing_key(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
    device: &LocalDevice,
    now_ms: i64,
) -> anyhow::Result<SealingKeyOutcome> {
    let wraps = list_accepted_stream_key_wraps(conn, account_id, stream_id)?;
    let Some(selected) = select_from_wraps(&wraps) else {
        return Ok(SealingKeyOutcome::NoCurrentKey);
    };

    // Every wrap naming THIS device at the selected (epoch, key_id) — across fan-out sibling ops.
    // Keying on the selected key_id (not the epoch alone) is load-bearing: two accepted wraps can
    // share an epoch with DIFFERENT key_ids (concurrent owner mints), and trying the other key_id's
    // wrap would either diverge or raise a spurious mismatch on an honest state.
    let my_fingerprint = device.fingerprint();
    let my_wraps: Vec<_> = wraps
        .iter()
        .filter(|w| {
            w.wrap.key_epoch == selected.key_epoch
                && KeyId::from_bytes(w.wrap.key_id) == selected.key_id
        })
        .filter_map(|w| {
            w.wrap
                .wraps
                .iter()
                .find(|entry| entry.recipient_fp == my_fingerprint)
                .map(|entry| (w.entry_hash, &entry.sealed))
        })
        .collect();
    if my_wraps.is_empty() {
        return Ok(SealingKeyOutcome::NotRecipient);
    }

    // The AAD binds the exact sealing context byte-for-byte; the recipient_pub is this device's own
    // roster x25519 bytes (wrap-to-self at mint sealed to exactly these).
    let ctx = WrapContext {
        account_id: account_id.to_bytes(),
        stream_id: stream_id.to_bytes(),
        key_epoch: selected.key_epoch,
        recipient_pub: device.x25519_public().to_bytes(),
    };
    for (entry_hash, sealed) in my_wraps {
        // An unwrap failure is fail-closed evidence, NOT `?`-propagated: `Err` is reserved for
        // infra/DB failures, and propagating would record no evidence and return `Err` in violation
        // of the `Ok(SealingKeyOutcome)` fail-closed contract.
        let recovered = match keywrap::unwrap_content_key(sealed, device.x25519_secret(), &ctx) {
            Ok(key) => key,
            Err(_) => {
                security_event::record_sync_security_event(conn, &SyncSecurityEvent {
                    kind: SyncSecurityEventKind::WrapUnwrapFailed,
                    account_id,
                    stream_id,
                    key_epoch: selected.key_epoch,
                    entry_hash,
                    expected_key_id: Some(selected.key_id),
                    observed_key_id: None,
                    observed_at_ms: now_ms,
                })?;
                continue;
            },
        };
        let observed = recovered.key_id();
        if observed == selected.key_id {
            return Ok(SealingKeyOutcome::Ready(recovered));
        }
        // The residual the cross-check exists for: a wrong key inside a validly-signed accepted op
        // (impl bug / a compromised-owner substitution the authority gate can't catch).
        security_event::record_sync_security_event(conn, &SyncSecurityEvent {
            kind: SyncSecurityEventKind::WrapKeyIdMismatch,
            account_id,
            stream_id,
            key_epoch: selected.key_epoch,
            entry_hash,
            expected_key_id: Some(selected.key_id),
            observed_key_id: Some(observed),
            observed_at_ms: now_ms,
        })?;
    }
    Ok(SealingKeyOutcome::FailedClosed)
}

/// The current sealing selection over an accepted-wrap set: MAX `key_epoch`, tiebreak MIN
/// `entry_hash` (a total order — SET resolution, never LWW). `None` for an empty set. Pure so both
/// public entry points share one selection rule.
fn select_from_wraps(wraps: &[AcceptedStreamWrap]) -> Option<SelectedWrap> {
    wraps
        .iter()
        .map(|w| SelectedWrap {
            key_id: KeyId::from_bytes(w.wrap.key_id),
            key_epoch: w.wrap.key_epoch,
            minting_entry_hash: w.entry_hash,
        })
        .max_by(|a, b| {
            // Max key_epoch wins; tiebreak MIN entry_hash (reverse the hash compare so the smaller
            // hash sorts as the greater element for `max_by`).
            a.key_epoch
                .cmp(&b.key_epoch)
                .then_with(|| b.minting_entry_hash.cmp(&a.minting_entry_hash))
        })
}

/// Read every EFFECTIVE accepted (`accepted = 1`) `StreamKeyWrap` op on `account_id`'s secrets log
/// naming `stream_id`, decoding each from the stored, already-signature-verified bytes. `accepted =
/// 1` IS the effective marker (the C4.2b evaluator set it); B-2 slot-eligibility guarantees an
/// accepted log-1 row decodes as a Known `StreamKeyWrap`, so an undecodable/unknown accepted row is
/// corruption — skipped (fail-safe: it just can't contribute a key) rather than aborting the read,
/// mirroring `storage::load_secrets_headers`.
fn list_accepted_stream_key_wraps(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<Vec<AcceptedStreamWrap>> {
    let mut stmt = conn.prepare(
        "SELECT entry_hash, signed_bytes FROM account_entries
         WHERE account_id = ?1 AND log_id = ?2 AND accepted = 1
         ORDER BY entry_hash", /* deterministic order; selection + try-until-pass are order-free
                                * in RESULT */
    )?;
    let rows = stmt
        .query_map(params![account_id.to_bytes().as_slice(), fold::SECRETS_LOG], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for (entry_hash, signed_bytes) in rows {
        let Ok(entry_hash) = <[u8; 32]>::try_from(entry_hash.as_slice()) else {
            continue;
        };
        let Ok(signed) = envelope::decode_account_signed(&signed_bytes) else {
            continue;
        };
        if signed.entry_hash != entry_hash {
            continue;
        }
        match ops::decode(signed.header.entry_type, &signed.payload) {
            Ok(DecodedSecretsOp::Known(wrap)) if wrap.stream_id == stream_id => {
                out.push(AcceptedStreamWrap { entry_hash, wrap });
            },
            _ => continue,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rusqlite::Connection;

    use super::super::ops::{StreamKeyWrap, WrapEntry, entry_type as secrets_entry_type};
    use super::{SealingKeyOutcome, current_sealing_key, select_current_sealing_wrap};
    use crate::account::cut::Cut;
    use crate::account::envelope::{AccountEntryHeader, sign_account_entry};
    use crate::account::id::account_id_from_genesis_payload;
    use crate::account::keywrap::{ContentKey, WrapContext, seal_content_key};
    use crate::account::ops::{self as control_ops, AccountOp, DeviceRole};
    use crate::account::storage::{IngestOutcome, account_ingest, account_is_contested};
    use crate::account::{
        AccountId, ensure_owned_stream_v2_in_tx, local_account,
        mint_and_author_stream_key_wrap_in_tx,
    };
    use crate::device::{DeviceSecret, DeviceX25519Public, DeviceX25519Secret};
    use crate::identity::local_device;
    use crate::op::DeviceFingerprint;
    use crate::stream::{self, StreamId, StreamSpec, StreamSpecV2};

    const NOW: i64 = 1_700_000_000_000;
    const CONTROL_LOG: u8 = 0;
    const SECRETS_LOG: u8 = 1;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    /// A test device (an ed25519 signer + a matching x25519 encryption key).
    struct Dev {
        secret: DeviceSecret,
        fp: DeviceFingerprint,
        ed: [u8; 32],
        x: [u8; 32],
    }

    impl Dev {
        fn new(seed: u8) -> Self {
            let secret = DeviceSecret::from_seed(&[seed; 32]);
            let public = secret.public();
            let x =
                DeviceX25519Secret::from_seed(&[seed.wrapping_add(0x80); 32]).public().to_bytes();
            Dev { fp: public.fingerprint(), ed: public.to_bytes(), x, secret }
        }
    }

    fn genesis(founder: &Dev) -> (AccountId, Vec<u8>, [u8; 32]) {
        let op = AccountOp::AccountGenesis {
            ed25519_pubkey: founder.ed,
            x25519_pubkey: founder.x,
            nonce16: [0u8; 16],
            created_at_ms: NOW as u64,
            label: None,
        };
        let payload = control_ops::encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
        let header = AccountEntryHeader {
            account_id,
            log_id: CONTROL_LOG,
            device_fingerprint: founder.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: control_ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        (account_id, signed.signed_bytes, signed.entry_hash)
    }

    #[allow(clippy::too_many_arguments)]
    fn control_op(
        account: AccountId,
        signer: &Dev,
        seq: u64,
        prev: Option<[u8; 32]>,
        authority_ref: Option<[u8; 32]>,
        op: &AccountOp,
    ) -> (Vec<u8>, [u8; 32]) {
        let payload = control_ops::encode(op).unwrap();
        let header = AccountEntryHeader {
            account_id: account,
            log_id: CONTROL_LOG,
            device_fingerprint: signer.fp,
            seq,
            prev_hash: prev,
            parent_ref: None,
            entry_type: control_ops::entry_type_of(op),
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref,
        };
        let signed = sign_account_entry(&signer.secret, &header, &payload).unwrap();
        (signed.signed_bytes, signed.entry_hash)
    }

    fn stream_own(account: AccountId) -> (StreamId, AccountOp) {
        let spec = StreamSpecV2 {
            owner_account_id: account,
            policy: StreamSpec {
                repo_set: vec!["repo-a".to_string()],
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            },
        };
        let stream_id = stream::derive_v2(&spec).unwrap();
        let stream_spec_bytes = stream::canonical_spec_v2_bytes(&spec).unwrap();
        (stream_id, AccountOp::StreamOwn { stream_id, stream_spec_bytes })
    }

    fn device_add(dev: &Dev, role: DeviceRole) -> AccountOp {
        AccountOp::DeviceAdd {
            device_fingerprint: dev.fp,
            ed25519_pubkey: dev.ed,
            x25519_pubkey: dev.x,
            role,
            label: None,
        }
    }

    /// Build a `StreamKeyWrap` sealing `key` to `recipient` (its x25519 + fingerprint) under the
    /// exact adoption `WrapContext`, with an explicit CLAIMED `key_id` (so a test can drive a
    /// mismatch by claiming a `key_id` other than `key.key_id()`).
    fn build_wrap(
        account: AccountId,
        stream_id: StreamId,
        recipient_fp: DeviceFingerprint,
        recipient_x: &DeviceX25519Public,
        key: &ContentKey,
        key_epoch: u64,
        claimed_key_id: [u8; 32],
    ) -> StreamKeyWrap {
        let ctx = WrapContext {
            account_id: account.to_bytes(),
            stream_id: stream_id.to_bytes(),
            key_epoch,
            recipient_pub: recipient_x.to_bytes(),
        };
        let sealed = seal_content_key(key, &ctx, recipient_x).unwrap();
        StreamKeyWrap {
            stream_id,
            key_id: claimed_key_id,
            key_epoch,
            wraps: vec![WrapEntry { recipient_fp, sealed }],
        }
    }

    /// A wrap sealing a seed-derived key HONESTLY (claimed key_id == the key's key_id) to
    /// `recipient`, at `key_epoch`.
    fn honest_wrap(
        account: AccountId,
        stream_id: StreamId,
        recipient_fp: DeviceFingerprint,
        recipient_x: &DeviceX25519Public,
        key: &ContentKey,
        key_epoch: u64,
    ) -> StreamKeyWrap {
        build_wrap(
            account,
            stream_id,
            recipient_fp,
            recipient_x,
            key,
            key_epoch,
            key.key_id().to_bytes(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn wrap_entry(
        account: AccountId,
        signer: &Dev,
        seq: u64,
        prev: Option<[u8; 32]>,
        authority_ref: Option<[u8; 32]>,
        wrap: &StreamKeyWrap,
    ) -> (Vec<u8>, [u8; 32]) {
        let payload = super::super::ops::encode(wrap).unwrap();
        let header = AccountEntryHeader {
            account_id: account,
            log_id: SECRETS_LOG,
            device_fingerprint: signer.fp,
            seq,
            prev_hash: prev,
            parent_ref: Some([0u8; 32]),
            entry_type: secrets_entry_type::STREAM_KEY_WRAP,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref,
        };
        let signed = sign_account_entry(&signer.secret, &header, &payload).unwrap();
        (signed.signed_bytes, signed.entry_hash)
    }

    fn ingest(conn: &Connection, bytes: &[u8]) -> IngestOutcome {
        account_ingest(conn, bytes, NOW).unwrap()
    }

    fn security_event_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM sync_security_events", [], |row| row.get(0)).unwrap()
    }

    fn security_event_kinds(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT kind FROM sync_security_events ORDER BY kind")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    fn expect_ready(outcome: SealingKeyOutcome) -> ContentKey {
        match outcome {
            SealingKeyOutcome::Ready(key) => key,
            _ => panic!("expected SealingKeyOutcome::Ready"),
        }
    }

    /// genesis(founder) + StreamOwn(founder) → account, founder, its owner-incarnation id (=
    /// genesis hash), and the owned stream. `founder` is an owner, so wraps it authors accept.
    fn account_with_owned_stream(conn: &Connection) -> (AccountId, Dev, [u8; 32], StreamId) {
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(conn, &genesis_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, _) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        ingest(conn, &own_bytes);
        (account, founder, genesis_hash, stream_id)
    }

    // ── Happy path: the REAL C4.3a mint seam round-trips through the C4.3b reader ──

    #[test]
    fn a_freshly_minted_key_is_ready_and_matches_the_selection() {
        let conn = db();
        // local_account founds a genesis account whose owner IS the local device, so
        // mint_and_author seals to it (wrap-to-self) and the wrap folds accepted.
        let account = local_account(&conn, NOW).unwrap();
        let device = local_device(&conn, NOW).unwrap();
        let stream_id = {
            let tx = conn.unchecked_transaction().unwrap();
            let sid = ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).unwrap();
            mint_and_author_stream_key_wrap_in_tx(&tx, sid, NOW).unwrap();
            tx.commit().unwrap();
            sid
        };

        let selected =
            select_current_sealing_wrap(&conn, account, stream_id).unwrap().expect("a current key");
        let key =
            expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
        // The recovered key is the one the header claims (the cross-check passed).
        assert_eq!(key.key_id(), selected.key_id, "the recovered key matches the selected key_id");
        assert_eq!(selected.key_epoch, 0, "the initial mint is epoch 0");
        assert_eq!(security_event_count(&conn), 0, "an honest mint records no security events");
    }

    #[test]
    fn no_accepted_wrap_is_no_current_key() {
        let conn = db();
        let (account, _founder, _genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        // The account owns the stream but no wrap was authored.
        assert!(select_current_sealing_wrap(&conn, account, stream_id).unwrap().is_none());
        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::NoCurrentKey
        ));
    }

    #[test]
    fn a_device_with_no_wrap_is_not_a_recipient() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        // The owner authors a wrap naming SOME OTHER device (not the local device).
        let other = Dev::new(9);
        let other_x = DeviceX25519Public::from_bytes(&other.x).unwrap();
        let key = ContentKey::from_seed(&[0x20; 32]);
        let wrap = honest_wrap(account, stream_id, other.fp, &other_x, &key, 0);
        let (bytes, _) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &bytes);

        // A current key exists (selection is Some) but this device is not a recipient.
        assert!(select_current_sealing_wrap(&conn, account, stream_id).unwrap().is_some());
        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::NotRecipient
        ));
        assert_eq!(security_event_count(&conn), 0, "not-a-recipient records no security event");
    }

    #[test]
    fn a_hand_authored_wrap_to_this_device_is_ready() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        let key = ContentKey::from_seed(&[0x20; 32]);
        let wrap =
            honest_wrap(account, stream_id, device.fingerprint(), &device.x25519_public(), &key, 0);
        let (bytes, _) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &bytes);

        let recovered =
            expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
        assert_eq!(recovered.as_slice(), key.as_slice(), "the recovered key is the sealed key");
        assert_eq!(recovered.key_id(), key.key_id());
        assert_eq!(security_event_count(&conn), 0);
    }

    // ── The adoption cross-check ──

    #[test]
    fn a_key_id_mismatch_fails_closed_and_records_one_event() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        // Seal key K but CLAIM a different key_id in the op payload. The wrap unwraps cleanly (to
        // K), but K.key_id() disagrees with the claimed key_id → mismatch.
        let key = ContentKey::from_seed(&[0x20; 32]);
        let lie = [0xcd; 32];
        let wrap = build_wrap(
            account,
            stream_id,
            device.fingerprint(),
            &device.x25519_public(),
            &key,
            0,
            lie,
        );
        let (bytes, entry_hash) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &bytes);

        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::FailedClosed
        ));
        assert_eq!(security_event_kinds(&conn), vec!["wrap_key_id_mismatch".to_string()]);
        // The recorded event names the offending op + both key_ids.
        let (recorded_hash, expected, observed): (Vec<u8>, Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT entry_hash, expected_key_id, observed_key_id FROM sync_security_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(recorded_hash, entry_hash.to_vec());
        assert_eq!(expected, lie.to_vec(), "expected = the op's claimed key_id");
        assert_eq!(observed, key.key_id().to_bytes().to_vec(), "observed = the unwrapped key's id");

        // A second call re-inserts NO duplicate event (INSERT OR IGNORE on (kind, entry_hash)).
        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::FailedClosed
        ));
        assert_eq!(security_event_count(&conn), 1, "a retry does not re-append the same evidence");
    }

    #[test]
    fn an_unwrap_failure_fails_closed_and_records_one_event() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        // An HONEST wrap (claimed key_id == key.key_id()), but the ciphertext is corrupted after
        // sealing — a substituted/garbage wrap presents as an AEAD TAG failure at unwrap, not a
        // clean-unwrap-wrong-key_id.
        let key = ContentKey::from_seed(&[0x20; 32]);
        let mut wrap =
            honest_wrap(account, stream_id, device.fingerprint(), &device.x25519_public(), &key, 0);
        wrap.wraps[0].sealed.ciphertext[0] ^= 1; // still a structurally-valid SealedKeyWrap
        let (bytes, entry_hash) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &bytes);

        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::FailedClosed
        ));
        assert_eq!(security_event_kinds(&conn), vec!["wrap_unwrap_failed".to_string()]);
        // No observed key_id (no key was recovered).
        let observed: Option<Vec<u8>> = conn
            .query_row(
                "SELECT observed_key_id FROM sync_security_events WHERE entry_hash = ?1",
                [entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(observed, None, "an unwrap failure records no observed key_id");
        // Idempotent on retry.
        current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap();
        assert_eq!(security_event_count(&conn), 1);
    }

    #[test]
    fn two_key_ids_at_one_epoch_selects_deterministically_with_no_spurious_event() {
        // BLOCKER-1: two accepted wraps at epoch 0 with DIFFERENT key_ids, BOTH naming this device.
        // Selection is deterministic (min entry_hash); the my-wrap lookup keys on the SELECTED
        // key_id, so only the selected op's wrap is tried → Ready with no spurious mismatch event.
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();

        let key_a = ContentKey::from_seed(&[0x20; 32]);
        let key_b = ContentKey::from_seed(&[0x21; 32]);
        assert_ne!(key_a.key_id(), key_b.key_id(), "the two mints have distinct key_ids");
        let wrap_a = honest_wrap(
            account,
            stream_id,
            device.fingerprint(),
            &device.x25519_public(),
            &key_a,
            0,
        );
        let wrap_b = honest_wrap(
            account,
            stream_id,
            device.fingerprint(),
            &device.x25519_public(),
            &key_b,
            0,
        );
        // A dense chain: two ops at epoch 0, same (stream) — both accept (SET, never LWW).
        let (a_bytes, ha) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap_a);
        ingest(&conn, &a_bytes);
        let (b_bytes, hb) = wrap_entry(account, &founder, 1, Some(ha), Some(genesis_hash), &wrap_b);
        ingest(&conn, &b_bytes);

        // The tiebreak is MIN entry_hash; the recovered key must be that op's key.
        let expected_key = if ha < hb { &key_a } else { &key_b };
        let selected =
            select_current_sealing_wrap(&conn, account, stream_id).unwrap().expect("a current key");
        assert_eq!(selected.key_id, expected_key.key_id(), "min entry_hash decides the key_id");
        assert_eq!(selected.minting_entry_hash, ha.min(hb));

        let recovered =
            expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
        assert_eq!(recovered.as_slice(), expected_key.as_slice(), "unwraps the SELECTED key_id");
        assert_eq!(
            security_event_count(&conn),
            0,
            "the other key_id's wrap is filtered out — no spurious mismatch event",
        );
    }

    // ── Eviction (S-1): a retro-condemn can drop the selection to a LOWER epoch ──

    /// genesis(founder) + DeviceAdd(owner_b, Owner) + StreamOwn(founder) → account, founder,
    /// owner_b, owner_b's incarnation id, the owned stream, and the StreamOwn control-tail hash.
    fn account_with_second_owner(
        conn: &Connection,
    ) -> (AccountId, Dev, Dev, [u8; 32], StreamId, [u8; 32], [u8; 32]) {
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(conn, &genesis_bytes);
        let owner_b = Dev::new(2);
        let (add_bytes, owner_id_b) = control_op(
            account,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&owner_b, DeviceRole::Owner),
        );
        ingest(conn, &add_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, own_hash) =
            control_op(account, &founder, 2, Some(owner_id_b), Some(genesis_hash), &own);
        ingest(conn, &own_bytes);
        (account, founder, owner_b, owner_id_b, stream_id, own_hash, genesis_hash)
    }

    #[test]
    fn retro_condemning_the_max_epoch_wrap_falls_back_to_the_lower_epoch_key() {
        let conn = db();
        let (account, founder, owner_b, owner_id_b, stream_id, own_hash, genesis_hash) =
            account_with_second_owner(&conn);
        let device = local_device(&conn, NOW).unwrap();
        let fp = device.fingerprint();
        let x = device.x25519_public();

        // owner_b authors epoch-0 (seq0) and epoch-1 (seq1) wraps, both naming this device.
        let key0 = ContentKey::from_seed(&[0x30; 32]);
        let key1 = ContentKey::from_seed(&[0x31; 32]);
        let (w0_bytes, w0) = wrap_entry(
            account,
            &owner_b,
            0,
            None,
            Some(owner_id_b),
            &honest_wrap(account, stream_id, fp, &x, &key0, 0),
        );
        ingest(&conn, &w0_bytes);
        let (w1_bytes, _w1) = wrap_entry(
            account,
            &owner_b,
            1,
            Some(w0),
            Some(owner_id_b),
            &honest_wrap(account, stream_id, fp, &x, &key1, 1),
        );
        ingest(&conn, &w1_bytes);

        // Before condemnation: the max epoch (1) is current.
        let recovered =
            expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
        assert_eq!(recovered.as_slice(), key1.as_slice(), "epoch 1 is current pre-condemn");

        // A demotes owner_b with a secrets cut at seq 0 → the epoch-1 wrap (seq 1) is
        // retro-condemned, the epoch-0 wrap (seq 0, within the cut) survives. The selection
        // reverts to the LOWER epoch.
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: owner_b.fp,
            owner_id: owner_id_b,
            control_cut: Cut::Empty,
            secrets_cut: Cut::At { seq: 0, hash: w0 },
            reason: "demote".to_string(),
        };
        let (demote_bytes, _) =
            control_op(account, &founder, 3, Some(own_hash), Some(genesis_hash), &demote);
        ingest(&conn, &demote_bytes);

        let recovered =
            expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
        assert_eq!(
            recovered.as_slice(),
            key0.as_slice(),
            "selection falls back to the epoch-0 key"
        );
        assert_eq!(security_event_count(&conn), 0, "an honest eviction records no event");
    }

    #[test]
    fn condemning_every_accepted_wrap_yields_no_current_key() {
        let conn = db();
        let (account, founder, owner_b, owner_id_b, stream_id, own_hash, genesis_hash) =
            account_with_second_owner(&conn);
        let device = local_device(&conn, NOW).unwrap();
        let fp = device.fingerprint();
        let x = device.x25519_public();

        let key0 = ContentKey::from_seed(&[0x30; 32]);
        let key1 = ContentKey::from_seed(&[0x31; 32]);
        let (w0_bytes, w0) = wrap_entry(
            account,
            &owner_b,
            0,
            None,
            Some(owner_id_b),
            &honest_wrap(account, stream_id, fp, &x, &key0, 0),
        );
        ingest(&conn, &w0_bytes);
        let (w1_bytes, _w1) = wrap_entry(
            account,
            &owner_b,
            1,
            Some(w0),
            Some(owner_id_b),
            &honest_wrap(account, stream_id, fp, &x, &key1, 1),
        );
        ingest(&conn, &w1_bytes);
        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::Ready(_)
        ));

        // An Empty secrets cut condemns owner_b's WHOLE secrets chain → no accepted wrap remains.
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: owner_b.fp,
            owner_id: owner_id_b,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            reason: "demote".to_string(),
        };
        let (demote_bytes, _) =
            control_op(account, &founder, 3, Some(own_hash), Some(genesis_hash), &demote);
        ingest(&conn, &demote_bytes);

        assert!(select_current_sealing_wrap(&conn, account, stream_id).unwrap().is_none());
        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::NoCurrentKey
        ));
    }

    // ── S-2: a contested account needs no special case ──

    #[test]
    fn a_contested_account_has_no_current_key() {
        let conn = db();
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(&conn, &genesis_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, own_hash) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        ingest(&conn, &own_bytes);
        // Two owner devices, added so the mutual removal below is a genuine owner-vs-owner contest.
        let a = Dev::new(2);
        let b = Dev::new(3);
        let (add_a_bytes, add_a) = control_op(
            account,
            &founder,
            2,
            Some(own_hash),
            Some(genesis_hash),
            &device_add(&a, DeviceRole::Owner),
        );
        ingest(&conn, &add_a_bytes);
        let (add_b_bytes, add_b) = control_op(
            account,
            &founder,
            3,
            Some(add_a),
            Some(genesis_hash),
            &device_add(&b, DeviceRole::Owner),
        );
        ingest(&conn, &add_b_bytes);

        // The founder authors a wrap naming the local device — accepted while the account is LIVE.
        let device = local_device(&conn, NOW).unwrap();
        let key = ContentKey::from_seed(&[0x20; 32]);
        let wrap =
            honest_wrap(account, stream_id, device.fingerprint(), &device.x25519_public(), &key, 0);
        let (wrap_bytes, _) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &wrap_bytes);
        assert!(
            matches!(
                current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
                SealingKeyOutcome::Ready(_)
            ),
            "the wrap is current while the account is live",
        );

        // a removes b and b removes a (incomparable) → the account folds contested; the contested
        // hold keeps the wrap out of `accepted`, so the selection is empty. No special case here.
        let remove_b = AccountOp::DeviceRemove {
            device_fingerprint: b.fp,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        };
        let remove_a = AccountOp::DeviceRemove {
            device_fingerprint: a.fp,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        };
        let (remove_b_bytes, _) = control_op(account, &a, 0, None, Some(add_a), &remove_b);
        ingest(&conn, &remove_b_bytes);
        let (remove_a_bytes, _) = control_op(account, &b, 0, None, Some(add_b), &remove_a);
        ingest(&conn, &remove_a_bytes);

        assert!(account_is_contested(&conn, account).unwrap(), "the account is contested");
        assert!(select_current_sealing_wrap(&conn, account, stream_id).unwrap().is_none());
        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::NoCurrentKey
        ));
    }
}
