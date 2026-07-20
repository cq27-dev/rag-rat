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

use anyhow::Context;
use rusqlite::{Connection, params};

use super::super::keywrap::{self, ContentKey, KeyId, WrapContext};
use super::super::{AccountId, bootstrap, content, envelope, fold, storage};
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

/// One exact live content-key group that a newly enrolled device may need. Epoch is part of the
/// identity because it is authenticated by [`WrapContext`], even when one key id was reused at
/// multiple epochs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveKeyEpoch {
    pub stream_id: StreamId,
    pub key_epoch: u64,
    pub key_id: KeyId,
}

/// Snapshot-derived catch-up targets for one effective device. `required` needs a new same-key
/// sibling; `already_covered` has at least one accepted sibling naming the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveKeyTargets {
    pub required: Vec<LiveKeyEpoch>,
    pub already_covered: Vec<LiveKeyEpoch>,
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

/// Every historical content key this device can recover for one stream, indexed by the exact
/// signed `key_id`. Keys remain process-local, are never persisted, and zeroize on drop through
/// [`ContentKey`].
pub struct ContentKeyring(Vec<(KeyId, ContentKey)>);

impl ContentKeyring {
    /// Resolve exactly `key_id`; never substitute another key from the same epoch.
    pub fn get(&self, key_id: KeyId) -> Option<&ContentKey> {
        self.0.iter().find(|(candidate, _)| *candidate == key_id).map(|(_, key)| key)
    }
}

enum KeyRecovery {
    Ready(ContentKey),
    NotRecipient,
    Failed(Vec<WrapRecoveryFailure>),
}

struct WrapRecoveryFailure {
    entry_hash: [u8; 32],
    observed_key_id: Option<KeyId>,
}

/// One EFFECTIVE accepted `StreamKeyWrap` op for a stream, decoded from its stored bytes.
struct AcceptedStreamWrap {
    entry_hash: [u8; 32],
    wrap: StreamKeyWrap,
}

#[derive(Clone, Copy)]
enum AcceptedWrapDecodeMode {
    Tolerant,
    StrictEvidence,
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

/// Derive every live exact key group for streams currently owned by the local account and classify
/// whether accepted siblings already cover `target`. The whole read uses the caller's connection
/// snapshot; callers needing atomic authoring pass their IMMEDIATE transaction.
///
/// Live means either referenced by currently accepted suite-1 content, or selected for current
/// sealing on an owned stream. This deliberately excludes plaintext content, condemned-only wraps,
/// and accepted unused loser keys. Target enrollment is resolved through the current authority
/// projection's exact accepted `roster_ref`; a removed or never-effective target fails.
pub fn live_stream_key_targets_for_device(
    conn: &Connection,
    target: crate::op::DeviceFingerprint,
) -> anyhow::Result<LiveKeyTargets> {
    let account_id = bootstrap::local_account_ref(conn)?
        .context("cannot derive stream-key catch-up targets before the local account is minted")?
        .account_id;
    storage::effective_roster_x25519_pubkey(conn, account_id, target)?.context(
        "stream-key catch-up target is not currently roster-effective in the local account",
    )?;

    let mut stmt = conn.prepare(
        "SELECT stream_id FROM account_stream_ownership
         WHERE account_id = ?1 ORDER BY stream_id",
    )?;
    let owned_streams = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut live = Vec::new();
    for raw_stream_id in owned_streams {
        let stream_id = StreamId::from_bytes(fixed::<32>(&raw_stream_id)?);
        let wraps = list_accepted_stream_key_wraps(conn, account_id, stream_id)?;

        let mut content_stmt = conn.prepare(
            "SELECT signed_bytes FROM content_entries
             WHERE stream_id = ?1 AND accepted = 1 ORDER BY entry_hash",
        )?;
        let content_rows = content_stmt
            .query_map([stream_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for signed_bytes in content_rows {
            let signed = content::decode_content_signed(&signed_bytes)
                .context("stored accepted /3 entry failed to decode while deriving live keys")?;
            if signed.header.crypto_suite != 1 {
                continue;
            }
            let key_id = KeyId::from_bytes(
                signed
                    .header
                    .key_id
                    .context("accepted suite-1 /3 entry has no key_id while deriving live keys")?,
            );
            live.extend(
                wraps
                    .iter()
                    .filter(|accepted| KeyId::from_bytes(accepted.wrap.key_id) == key_id)
                    .map(|accepted| LiveKeyEpoch {
                        stream_id,
                        key_epoch: accepted.wrap.key_epoch,
                        key_id,
                    }),
            );
        }

        if let Some(selected) = select_from_wraps(&wraps) {
            live.push(LiveKeyEpoch {
                stream_id,
                key_epoch: selected.key_epoch,
                key_id: selected.key_id,
            });
        }
    }

    live.sort_by_key(|key| (key.stream_id.to_bytes(), key.key_epoch, key.key_id.to_bytes()));
    live.dedup();

    let mut required = Vec::new();
    let mut already_covered = Vec::new();
    for key in live {
        let covered = list_accepted_stream_key_wraps(conn, account_id, key.stream_id)?
            .iter()
            .filter(|accepted| {
                accepted.wrap.key_epoch == key.key_epoch
                    && KeyId::from_bytes(accepted.wrap.key_id) == key.key_id
            })
            .flat_map(|accepted| &accepted.wrap.wraps)
            .any(|wrap| wrap.recipient_fp == target);
        if covered {
            already_covered.push(key);
        } else {
            required.push(key);
        }
    }
    Ok(LiveKeyTargets { required, already_covered })
}

fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("stored blob is {} bytes, not {N}", bytes.len()))
}

/// Whether an accepted `StreamKeyWrap` exists for `stream_id`. Unlike local key recovery, this is
/// downgrade evidence: corruption of any accepted secrets row must fail closed rather than make a
/// previously keyed stream appear eligible for plaintext. Presence does not require a local
/// recipient wrap or a successful unwrap.
pub(in crate::account) fn accepted_stream_key_wrap_exists_strict(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    Ok(!list_accepted_stream_key_wraps_with_mode(
        conn,
        account_id,
        stream_id,
        AcceptedWrapDecodeMode::StrictEvidence,
    )?
    .is_empty())
}

/// Whether `stream_id`'s content key must be ROTATED (sync phase C4.4, #607): TRUE iff some
/// recipient of the CURRENT sealing wrap is no longer roster-effective — i.e. a removed device
/// still holds a wrap for the key this stream seals under right now. DEVICE-INDEPENDENT (it
/// compares the wrap's recipients against the roster, never the local device), so every peer
/// computes the same answer; the owner-only authoring gate lives in
/// [`super::ensure_stream_key_current_in_tx`], not here.
///
/// Unions recipients across ALL accepted sibling ops at the SELECTED `(epoch, key_id)`, not just
/// the tiebreak-winner op: same-`(epoch, key_id)` fan-out siblings can each name a different
/// recipient subset (the C4.3b BLOCKER-1 class), so a removed device named only by a non-winner
/// sibling must still trigger. Keying on `(epoch, key_id)` mirrors [`current_sealing_key`]'s
/// my-wrap lookup.
///
/// SOUND ONLY because of wrap-to-self: the predicate sees a wrap's RECIPIENTS, never its author, so
/// a removed minting owner is caught only because it sealed to itself and thus appears as a
/// recipient of its own surviving wraps. ONE-DIRECTIONAL: a newly-effective device that is ABSENT
/// from the current wrap does NOT trigger (that is deferred new-device catch-up, not rotation).
///
/// `false` when the stream has no accepted wrap (nothing to rotate — the seal path mints an initial
/// key instead). A contested account needs no special case: the fold keeps contested wraps out of
/// `accepted`, so the selection is simply empty.
pub fn stream_key_rotation_needed(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    let wraps = list_accepted_stream_key_wraps(conn, account_id, stream_id)?;
    let Some(selected) = select_from_wraps(&wraps) else {
        return Ok(false);
    };
    let effective = storage::list_effective_roster_fingerprints(conn, account_id)?;
    let stale_recipient = wraps
        .iter()
        .filter(|w| {
            w.wrap.key_epoch == selected.key_epoch
                && KeyId::from_bytes(w.wrap.key_id) == selected.key_id
        })
        .flat_map(|w| w.wrap.wraps.iter())
        .any(|entry| !effective.contains(&entry.recipient_fp));
    Ok(stale_recipient)
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

    match recover_key(&wraps, account_id, stream_id, selected.key_epoch, selected.key_id, device) {
        KeyRecovery::Ready(key) => Ok(SealingKeyOutcome::Ready(key)),
        KeyRecovery::NotRecipient => Ok(SealingKeyOutcome::NotRecipient),
        KeyRecovery::Failed(failures) => {
            for failure in failures {
                security_event::record_sync_security_event(conn, &SyncSecurityEvent {
                    kind: if failure.observed_key_id.is_some() {
                        SyncSecurityEventKind::WrapKeyIdMismatch
                    } else {
                        SyncSecurityEventKind::WrapUnwrapFailed
                    },
                    account_id,
                    stream_id,
                    key_epoch: selected.key_epoch,
                    entry_hash: failure.entry_hash,
                    expected_key_id: Some(selected.key_id),
                    observed_key_id: failure.observed_key_id,
                    observed_at_ms: now_ms,
                })?;
            }
            Ok(SealingKeyOutcome::FailedClosed)
        },
    }
}

/// Recover every accepted historical stream key addressed to `device`. Each distinct
/// `(key_epoch, key_id)` reconstructs its own wrap context, while all same-pair fan-out siblings
/// are unioned before recovery. Different key IDs at one epoch are never mixed. Unopenable,
/// mismatched, and other-recipient groups are omitted; their accepted log entries remain untouched.
pub fn historical_content_keyring(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
    device: &LocalDevice,
) -> anyhow::Result<ContentKeyring> {
    let wraps = list_accepted_stream_key_wraps(conn, account_id, stream_id)?;
    let mut groups = Vec::new();
    for accepted in &wraps {
        let group = (accepted.wrap.key_epoch, KeyId::from_bytes(accepted.wrap.key_id));
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    let mut keys = Vec::new();
    for (key_epoch, key_id) in groups {
        if keys.iter().any(|(recovered_id, _)| *recovered_id == key_id) {
            continue;
        }
        if let KeyRecovery::Ready(key) =
            recover_key(&wraps, account_id, stream_id, key_epoch, key_id, device)
        {
            keys.push((key_id, key));
        }
    }
    Ok(ContentKeyring(keys))
}

/// Recover one exact historical `(stream, epoch, key_id)` group for same-key fan-out authoring.
/// Unlike [`ContentKeyring`], epoch remains part of the lookup because the wrap context
/// authenticates it even when a key id appears at more than one epoch.
pub(super) fn recover_exact_historical_content_key(
    conn: &Connection,
    account_id: AccountId,
    live: LiveKeyEpoch,
    device: &LocalDevice,
) -> anyhow::Result<Option<ContentKey>> {
    let wraps = list_accepted_stream_key_wraps(conn, account_id, live.stream_id)?;
    Ok(match recover_key(&wraps, account_id, live.stream_id, live.key_epoch, live.key_id, device) {
        KeyRecovery::Ready(key) => Some(key),
        KeyRecovery::NotRecipient | KeyRecovery::Failed(_) => None,
    })
}

/// Shared cryptographic recovery for current sealing and historical projection reads.
fn recover_key(
    wraps: &[AcceptedStreamWrap],
    account_id: AccountId,
    stream_id: StreamId,
    key_epoch: u64,
    key_id: KeyId,
    device: &LocalDevice,
) -> KeyRecovery {
    let my_fingerprint = device.fingerprint();
    let my_wraps: Vec<_> = wraps
        .iter()
        .filter(|accepted| {
            accepted.wrap.key_epoch == key_epoch
                && KeyId::from_bytes(accepted.wrap.key_id) == key_id
        })
        .flat_map(|accepted| {
            accepted
                .wrap
                .wraps
                .iter()
                .filter(move |entry| entry.recipient_fp == my_fingerprint)
                .map(move |entry| (accepted.entry_hash, &entry.sealed))
        })
        .collect();
    if my_wraps.is_empty() {
        return KeyRecovery::NotRecipient;
    }
    let ctx = WrapContext {
        account_id: account_id.to_bytes(),
        stream_id: stream_id.to_bytes(),
        key_epoch,
        recipient_pub: device.x25519_public().to_bytes(),
    };
    let mut failures = Vec::new();
    for (entry_hash, sealed) in my_wraps {
        let Ok(recovered) = keywrap::unwrap_content_key(sealed, device.x25519_secret(), &ctx)
        else {
            failures.push(WrapRecoveryFailure { entry_hash, observed_key_id: None });
            continue;
        };
        let observed_key_id = recovered.key_id();
        if observed_key_id == key_id {
            return KeyRecovery::Ready(recovered);
        }
        failures.push(WrapRecoveryFailure { entry_hash, observed_key_id: Some(observed_key_id) });
    }
    KeyRecovery::Failed(failures)
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
/// corruption. Local key selection/recovery skips such rows (fail-safe: they cannot contribute a
/// key), while downgrade evidence uses strict mode and fails closed.
fn list_accepted_stream_key_wraps(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<Vec<AcceptedStreamWrap>> {
    list_accepted_stream_key_wraps_with_mode(
        conn,
        account_id,
        stream_id,
        AcceptedWrapDecodeMode::Tolerant,
    )
}

fn list_accepted_stream_key_wraps_with_mode(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
    mode: AcceptedWrapDecodeMode,
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
        let entry_hash = match <[u8; 32]>::try_from(entry_hash.as_slice()) {
            Ok(entry_hash) => entry_hash,
            Err(_) if matches!(mode, AcceptedWrapDecodeMode::Tolerant) => continue,
            Err(_) => anyhow::bail!(
                "stored accepted secrets entry_hash is not 32 bytes while checking sealed-ratchet \
                 wrap evidence"
            ),
        };
        let signed = match envelope::decode_account_signed(&signed_bytes) {
            Ok(signed) => signed,
            Err(_) if matches!(mode, AcceptedWrapDecodeMode::Tolerant) => continue,
            Err(err) =>
                return Err(err).context(
                    "stored accepted secrets envelope failed to decode while checking \
                     sealed-ratchet wrap evidence",
                ),
        };
        if signed.entry_hash != entry_hash {
            if matches!(mode, AcceptedWrapDecodeMode::Tolerant) {
                continue;
            }
            anyhow::bail!(
                "stored accepted secrets envelope does not match its entry_hash row while \
                 checking sealed-ratchet wrap evidence"
            );
        }
        match ops::decode(signed.header.entry_type, &signed.payload) {
            Ok(DecodedSecretsOp::Known(wrap)) if wrap.stream_id == stream_id => {
                out.push(AcceptedStreamWrap { entry_hash, wrap });
            },
            Ok(_) => {},
            Err(_) if matches!(mode, AcceptedWrapDecodeMode::Tolerant) => continue,
            Err(err) =>
                return Err(err).context(
                    "stored accepted secrets payload failed to decode while checking \
                     sealed-ratchet wrap evidence",
                ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rusqlite::Connection;

    use super::super::ops::{
        DecodedSecretsOp, StreamKeyWrap, WrapEntry, decode, entry_type as secrets_entry_type,
    };
    use super::{
        LiveKeyEpoch, SealingKeyOutcome, SelectedWrap, current_sealing_key,
        historical_content_keyring, live_stream_key_targets_for_device,
        select_current_sealing_wrap, stream_key_rotation_needed,
    };
    use crate::account::content::{
        ContentEntryHeader, sign_content_entry, sign_sealed_content_entry,
    };
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
    use crate::op::{self, DeviceFingerprint, MemoryOp};
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

    fn mark_as_local_account(conn: &Connection, genesis_hash: [u8; 32]) {
        conn.execute(
            "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
             VALUES(0, ?1, ?2)",
            rusqlite::params![genesis_hash.as_slice(), NOW],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_accepted_content(
        conn: &Connection,
        account: AccountId,
        stream: StreamId,
        author: &Dev,
        roster_ref: [u8; 32],
        seq: u64,
        key: Option<&ContentKey>,
    ) {
        let header = ContentEntryHeader {
            stream_id: stream,
            author_account_id: account,
            device_fingerprint: author.fp,
            seq,
            lamport: seq,
            prev_hash: (seq != 0).then_some([seq as u8; 32]),
            grant_id: None,
            roster_ref,
            owner_auth_len: 0,
            author_auth_len: 0,
            crypto_suite: 0,
            key_id: None,
        };
        let payload = op::encode(&MemoryOp::Snapshot);
        let signed = match key {
            Some(key) =>
                sign_sealed_content_entry(&author.secret, &header, &payload, key, [seq as u8; 24])
                    .unwrap(),
            None => sign_content_entry(&author.secret, &header, &payload).unwrap(),
        };
        conn.execute(
            "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 1, ?8, ?9)",
            rusqlite::params![
                signed.entry_hash.as_slice(),
                stream.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                author.fp.to_bytes().as_slice(),
                seq.to_be_bytes().as_slice(),
                roster_ref.as_slice(),
                0u64.to_be_bytes().as_slice(),
                signed.signed_bytes,
                NOW,
            ],
        )
        .unwrap();
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

    /// The SHARED fold verdict for one entry: its `accepted` flag plus the persisted §16.3
    /// `(status, detail)` taxonomy. This is exactly the device-independent state the read-time
    /// cross-check must NEVER touch (the fold firewall).
    fn fold_verdict(conn: &Connection, entry_hash: [u8; 32]) -> (i64, String, Option<String>) {
        conn.query_row(
            "SELECT e.accepted, s.status, s.detail
             FROM account_entries e JOIN account_entry_status s ON s.entry_hash = e.entry_hash
             WHERE e.entry_hash = ?1",
            [entry_hash.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
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
    fn live_key_targets_include_referenced_history_and_no_content_current_only() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        mark_as_local_account(&conn, genesis_hash);
        let historical = ContentKey::from_seed(&[0x30; 32]);
        let unused = ContentKey::from_seed(&[0x31; 32]);
        let current = ContentKey::from_seed(&[0x32; 32]);
        let same_epoch_other = ContentKey::from_seed(&[0x33; 32]);
        let condemned_only = ContentKey::from_seed(&[0x34; 32]);
        let keys = [(&historical, 0), (&unused, 1), (&same_epoch_other, 0), (&current, 2)];
        let mut prev = None;
        for (seq, (key, epoch)) in keys.into_iter().enumerate() {
            let wrap = honest_wrap(
                account,
                stream_id,
                founder.fp,
                &DeviceX25519Public::from_bytes(&founder.x).unwrap(),
                key,
                epoch,
            );
            let (bytes, hash) =
                wrap_entry(account, &founder, seq as u64, prev, Some(genesis_hash), &wrap);
            ingest(&conn, &bytes);
            prev = Some(hash);
        }
        insert_accepted_content(
            &conn,
            account,
            stream_id,
            &founder,
            genesis_hash,
            0,
            Some(&historical),
        );
        insert_accepted_content(
            &conn,
            account,
            stream_id,
            &founder,
            genesis_hash,
            1,
            Some(&same_epoch_other),
        );
        insert_accepted_content(&conn, account, stream_id, &founder, genesis_hash, 2, None);
        let condemned_wrap = honest_wrap(
            account,
            stream_id,
            founder.fp,
            &DeviceX25519Public::from_bytes(&founder.x).unwrap(),
            &condemned_only,
            9,
        );
        let (condemned_bytes, condemned_hash) =
            wrap_entry(account, &founder, 99, prev, Some(genesis_hash), &condemned_wrap);
        conn.execute(
            "INSERT INTO account_entries(entry_hash, account_id, log_id, device_fingerprint, seq,
                 prev_hash, parent_ref, authority_ref, entry_type, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, 1, ?3, 99, ?4, NULL, NULL, ?5, 0, ?6, ?7)",
            rusqlite::params![
                condemned_hash.as_slice(),
                account.to_bytes().as_slice(),
                founder.fp.to_bytes().as_slice(),
                prev.unwrap().as_slice(),
                secrets_entry_type::STREAM_KEY_WRAP,
                condemned_bytes,
                NOW,
            ],
        )
        .unwrap();
        insert_accepted_content(
            &conn,
            account,
            stream_id,
            &founder,
            genesis_hash,
            3,
            Some(&condemned_only),
        );

        let targets = live_stream_key_targets_for_device(&conn, founder.fp).unwrap();
        assert!(targets.required.is_empty());
        assert_eq!(
            targets.already_covered,
            vec![
                LiveKeyEpoch { stream_id, key_epoch: 0, key_id: historical.key_id() },
                LiveKeyEpoch { stream_id, key_epoch: 0, key_id: same_epoch_other.key_id() },
                LiveKeyEpoch { stream_id, key_epoch: 2, key_id: current.key_id() },
            ],
            "referenced same-epoch ids and the no-content current key are live; plaintext and the \
             unused loser are not",
        );
    }

    #[test]
    fn live_key_targets_require_an_effective_target_and_union_sibling_coverage() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        mark_as_local_account(&conn, genesis_hash);
        let target = Dev::new(9);
        let mut shadow = Dev::new(9);
        shadow.x = Dev::new(10).x;
        let key = ContentKey::from_seed(&[0x40; 32]);
        let founder_x = DeviceX25519Public::from_bytes(&founder.x).unwrap();
        let first = honest_wrap(account, stream_id, founder.fp, &founder_x, &key, 0);
        let (first_bytes, first_hash) =
            wrap_entry(account, &founder, 0, None, Some(genesis_hash), &first);
        ingest(&conn, &first_bytes);

        let (add_bytes, add_hash) = control_op(
            account,
            &founder,
            2,
            Some(first_hash),
            Some(genesis_hash),
            &device_add(&target, DeviceRole::Member),
        );
        // The read follows the effective projection's exact enrollment ref. A same-fingerprint
        // decoy candidate is irrelevant because it is not the projected roster_ref.
        conn.execute(
            "INSERT INTO account_entries(entry_hash, account_id, log_id, device_fingerprint, seq,
                 prev_hash, parent_ref, authority_ref, entry_type, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, 0, ?3, 2, NULL, NULL, NULL, 2, 0, ?4, ?5)",
            rusqlite::params![
                add_hash.as_slice(),
                account.to_bytes().as_slice(),
                founder.fp.to_bytes().as_slice(),
                add_bytes,
                NOW,
            ],
        )
        .unwrap();
        let (shadow_bytes, shadow_hash) = control_op(
            account,
            &founder,
            3,
            Some(add_hash),
            Some(genesis_hash),
            &device_add(&shadow, DeviceRole::Member),
        );
        conn.execute(
            "INSERT INTO account_entries(entry_hash, account_id, log_id, device_fingerprint, seq,
                 prev_hash, parent_ref, authority_ref, entry_type, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, 0, ?3, 3, NULL, NULL, NULL, 2, 0, ?4, ?5)",
            rusqlite::params![
                shadow_hash.as_slice(),
                account.to_bytes().as_slice(),
                founder.fp.to_bytes().as_slice(),
                shadow_bytes,
                NOW,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_roster_history(
                 account_id, device_fingerprint, role, roster_ref, effective_at, closed_at)
             VALUES(?1, ?2, 'member', ?3, 3, NULL)",
            rusqlite::params![
                account.to_bytes().as_slice(),
                target.fp.to_bytes().as_slice(),
                add_hash.as_slice(),
            ],
        )
        .unwrap();

        let targets = live_stream_key_targets_for_device(&conn, target.fp).unwrap();
        assert_eq!(targets.required, vec![LiveKeyEpoch {
            stream_id,
            key_epoch: 0,
            key_id: key.key_id()
        }]);
        assert!(targets.already_covered.is_empty());

        conn.execute(
            "UPDATE account_roster_history SET closed_at = 4
             WHERE account_id = ?1 AND device_fingerprint = ?2",
            rusqlite::params![account.to_bytes().as_slice(), target.fp.to_bytes().as_slice()],
        )
        .unwrap();
        assert!(live_stream_key_targets_for_device(&conn, target.fp).is_err());
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

    #[test]
    fn historical_keyring_recovers_all_exact_keys_and_unions_fanout_siblings() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        let other = Dev::new(9);
        let other_x = DeviceX25519Public::from_bytes(&other.x).unwrap();
        let local_x = device.x25519_public();

        let prior = ContentKey::from_seed(&[0x20; 32]);
        let fanned = ContentKey::from_seed(&[0x21; 32]);
        let same_epoch_other_key = ContentKey::from_seed(&[0x22; 32]);
        let other_device_only = ContentKey::from_seed(&[0x23; 32]);
        let mismatched_plaintext = ContentKey::from_seed(&[0x24; 32]);
        let claimed_id = ContentKey::from_seed(&[0x25; 32]).key_id();

        let ops = [
            honest_wrap(account, stream_id, device.fingerprint(), &local_x, &prior, 0),
            // First fan-out sibling does not name this device.
            honest_wrap(account, stream_id, other.fp, &other_x, &fanned, 1),
            // A later sibling for the same (epoch, key_id) does.
            honest_wrap(account, stream_id, device.fingerprint(), &local_x, &fanned, 1),
            // A distinct key at the same epoch must remain independently addressable.
            honest_wrap(
                account,
                stream_id,
                device.fingerprint(),
                &local_x,
                &same_epoch_other_key,
                1,
            ),
            honest_wrap(account, stream_id, other.fp, &other_x, &other_device_only, 2),
            build_wrap(
                account,
                stream_id,
                device.fingerprint(),
                &local_x,
                &mismatched_plaintext,
                3,
                claimed_id.to_bytes(),
            ),
        ];
        let mut prev = None;
        for (seq, op) in ops.iter().enumerate() {
            let (bytes, hash) =
                wrap_entry(account, &founder, seq as u64, prev, Some(genesis_hash), op);
            ingest(&conn, &bytes);
            prev = Some(hash);
        }

        let keyring = historical_content_keyring(&conn, account, stream_id, &device).unwrap();
        assert_eq!(keyring.get(prior.key_id()).unwrap().as_slice(), prior.as_slice());
        assert_eq!(keyring.get(fanned.key_id()).unwrap().as_slice(), fanned.as_slice());
        assert_eq!(
            keyring.get(same_epoch_other_key.key_id()).unwrap().as_slice(),
            same_epoch_other_key.as_slice(),
        );
        assert!(
            keyring.get(other_device_only.key_id()).is_none(),
            "another device's wrap is not a key"
        );
        assert!(keyring.get(claimed_id).is_none(), "unwrap/key-id mismatch fails closed");
        assert_eq!(security_event_count(&conn), 0, "projection recovery does not mutate evidence");
    }

    #[test]
    fn historical_keyring_tolerates_a_corrupt_accepted_wrap() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let device = local_device(&conn, NOW).unwrap();
        let key = ContentKey::from_seed(&[0x20; 32]);
        let wrap =
            honest_wrap(account, stream_id, device.fingerprint(), &device.x25519_public(), &key, 0);
        let (bytes, entry_hash) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &bytes);
        conn.execute("UPDATE account_entries SET signed_bytes = X'00' WHERE entry_hash = ?1", [
            entry_hash.as_slice(),
        ])
        .unwrap();

        let keyring = historical_content_keyring(&conn, account, stream_id, &device)
            .expect("projection key recovery remains tolerant of an unusable local wrap");
        assert!(keyring.get(key.key_id()).is_none(), "a corrupt wrap recovers no key");
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

        // FOLD FIREWALL: the wrap is validly signed by an owner, so the shared fold ACCEPTS it
        // (device-independent). The read-time cross-check below must leave that verdict untouched.
        let verdict_before = fold_verdict(&conn, entry_hash);
        assert_eq!(
            verdict_before,
            (1, "accepted".to_string(), None),
            "the offending wrap folds accepted before the cross-check",
        );

        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::FailedClosed
        ));
        assert_eq!(security_event_kinds(&conn), vec!["wrap_key_id_mismatch".to_string()]);
        // The cross-check wrote ONLY sync_security_events — the fold verdict (accepted flag +
        // persisted status/detail) is byte-for-byte unchanged. A regression that let the read path
        // condemn/unaccept the offending wrap would break fold device-independence and fail here.
        assert_eq!(
            fold_verdict(&conn, entry_hash),
            verdict_before,
            "a key_id mismatch must NOT mutate the shared fold verdict",
        );
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

        // FOLD FIREWALL: the corrupted-ciphertext wrap is still a validly-signed owner op, so the
        // shared fold ACCEPTS it (the AEAD failure is a device-LOCAL read-time fact, not a fold
        // input). Capture the accepted verdict so the cross-check below can be proven inert on it.
        let verdict_before = fold_verdict(&conn, entry_hash);
        assert_eq!(
            verdict_before,
            (1, "accepted".to_string(), None),
            "the offending wrap folds accepted before the cross-check",
        );

        assert!(matches!(
            current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap(),
            SealingKeyOutcome::FailedClosed
        ));
        assert_eq!(security_event_kinds(&conn), vec!["wrap_unwrap_failed".to_string()]);
        // The unwrap failure recorded LOCAL evidence only; the shared fold verdict is unchanged.
        assert_eq!(
            fold_verdict(&conn, entry_hash),
            verdict_before,
            "an unwrap failure must NOT mutate the shared fold verdict",
        );
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

    // ── C4.4: the rotation-needed predicate ──

    /// Decode the stored `StreamKeyWrap` op for one entry hash.
    fn stored_wrap_by_hash(conn: &Connection, hash: [u8; 32]) -> StreamKeyWrap {
        let signed_bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
                [hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let signed = crate::account::envelope::decode_account_signed(&signed_bytes).unwrap();
        match decode(signed.header.entry_type, &signed.payload).unwrap() {
            DecodedSecretsOp::Known(wrap) => wrap,
            _ => panic!("stored entry is a known StreamKeyWrap"),
        }
    }

    #[test]
    fn rotation_needed_unions_recipients_across_fanout_siblings() {
        // S1: the predicate unions recipients across ALL accepted sibling ops at the selected
        // (epoch, key_id), not just the tiebreak-winner op. Two epoch-0 siblings share ONE key_id,
        // each naming a DIFFERENT member; removing the member named ONLY by the LOSER op must still
        // trigger — a winner-only predicate would miss it.
        let conn = db();
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(&conn, &genesis_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, own_hash) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        ingest(&conn, &own_bytes);

        let m1 = Dev::new(4);
        let m2 = Dev::new(5);
        let (add1_bytes, add1) = control_op(
            account,
            &founder,
            2,
            Some(own_hash),
            Some(genesis_hash),
            &device_add(&m1, DeviceRole::Member),
        );
        ingest(&conn, &add1_bytes);
        let (add2_bytes, add2) = control_op(
            account,
            &founder,
            3,
            Some(add1),
            Some(genesis_hash),
            &device_add(&m2, DeviceRole::Member),
        );
        ingest(&conn, &add2_bytes);

        // Two ops share ONE key (⇒ one key_id) at epoch 0 — a fan-out across ops (SET, never LWW);
        // each names a different member.
        let key = ContentKey::from_seed(&[0x40; 32]);
        let m1_x = DeviceX25519Public::from_bytes(&m1.x).unwrap();
        let m2_x = DeviceX25519Public::from_bytes(&m2.x).unwrap();
        let (w1_bytes, h1) = wrap_entry(
            account,
            &founder,
            0,
            None,
            Some(genesis_hash),
            &honest_wrap(account, stream_id, m1.fp, &m1_x, &key, 0),
        );
        ingest(&conn, &w1_bytes);
        let (w2_bytes, h2) = wrap_entry(
            account,
            &founder,
            1,
            Some(h1),
            Some(genesis_hash),
            &honest_wrap(account, stream_id, m2.fp, &m2_x, &key, 0),
        );
        ingest(&conn, &w2_bytes);

        // The winner op is min entry_hash; remove the member named by the LOSER op so the winner
        // names only a still-effective member.
        let (winner_hash, loser_member) = if h1 < h2 { (h1, &m2) } else { (h2, &m1) };
        let remove = AccountOp::DeviceRemove {
            device_fingerprint: loser_member.fp,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        };
        let (rem_bytes, _) =
            control_op(account, &founder, 4, Some(add2), Some(genesis_hash), &remove);
        ingest(&conn, &rem_bytes);

        assert!(
            stream_key_rotation_needed(&conn, account, stream_id).unwrap(),
            "a removed recipient named only by a non-winner sibling still triggers rotation",
        );
        // Prove the UNION is doing the work: the winning op does NOT name the removed member.
        let winner = stored_wrap_by_hash(&conn, winner_hash);
        assert!(
            !winner.wraps.iter().any(|w| w.recipient_fp == loser_member.fp),
            "the tiebreak-winner op names only the still-effective member",
        );
    }

    #[test]
    fn a_current_wrap_with_only_effective_recipients_needs_no_rotation() {
        // The negative case + ONE-DIRECTIONAL: a wrap whose recipients are all still effective is
        // not stale, and a newly-added member ABSENT from the wrap is deferred catch-up,
        // never a rotation trigger. Built inline so the StreamOwn tail hash is in hand to
        // chain the later DeviceAdd.
        let conn = db();
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(&conn, &genesis_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, own_hash) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        ingest(&conn, &own_bytes);

        // A wrap sealed to the founder only (an effective device).
        let founder_x = DeviceX25519Public::from_bytes(&founder.x).unwrap();
        let key = ContentKey::from_seed(&[0x41; 32]);
        let (bytes, _) = wrap_entry(
            account,
            &founder,
            0,
            None,
            Some(genesis_hash),
            &honest_wrap(account, stream_id, founder.fp, &founder_x, &key, 0),
        );
        ingest(&conn, &bytes);
        assert!(
            !stream_key_rotation_needed(&conn, account, stream_id).unwrap(),
            "the sole recipient is effective — no rotation needed",
        );

        // A newly-added member is absent from the existing wrap; deferred catch-up, NOT rotation.
        let newcomer = Dev::new(7);
        let (add_bytes, _) = control_op(
            account,
            &founder,
            2,
            Some(own_hash),
            Some(genesis_hash),
            &device_add(&newcomer, DeviceRole::Member),
        );
        ingest(&conn, &add_bytes);
        assert!(
            !stream_key_rotation_needed(&conn, account, stream_id).unwrap(),
            "a newly-effective device absent from the wrap does NOT trigger rotation \
             (one-directional)",
        );
    }

    #[test]
    fn concurrent_epoch_one_rotations_both_accept_and_select_deterministically() {
        // Two owners independently rotate epoch 0 → epoch 1 with DIFFERENT fresh keys. Both epoch-1
        // wraps accept (SET, never LWW); the selection converges on the min-entry_hash op — the
        // loser wrap is harmless. This is the shape two concurrent
        // `rotate_stream_key_in_tx` calls produce.
        let conn = db();
        let (account, founder, owner_b, owner_id_b, stream_id, _own_hash, genesis_hash) =
            account_with_second_owner(&conn);
        let device = local_device(&conn, NOW).unwrap();
        let fp = device.fingerprint();
        let x = device.x25519_public();

        // Baseline epoch-0 wrap by the founder.
        let key0 = ContentKey::from_seed(&[0x50; 32]);
        let (w0_bytes, w0) = wrap_entry(
            account,
            &founder,
            0,
            None,
            Some(genesis_hash),
            &honest_wrap(account, stream_id, fp, &x, &key0, 0),
        );
        ingest(&conn, &w0_bytes);

        // Two epoch-1 rotations, one per owner, different fresh keys.
        let key1_founder = ContentKey::from_seed(&[0x51; 32]);
        let key1_owner_b = ContentKey::from_seed(&[0x52; 32]);
        let (wf_bytes, wf) = wrap_entry(
            account,
            &founder,
            1,
            Some(w0),
            Some(genesis_hash),
            &honest_wrap(account, stream_id, fp, &x, &key1_founder, 1),
        );
        ingest(&conn, &wf_bytes);
        let (wb_bytes, wb) = wrap_entry(
            account,
            &owner_b,
            0,
            None,
            Some(owner_id_b),
            &honest_wrap(account, stream_id, fp, &x, &key1_owner_b, 1),
        );
        ingest(&conn, &wb_bytes);

        // Both epoch-1 wraps accepted; selection = epoch 1, tiebreak min entry_hash.
        let selected =
            select_current_sealing_wrap(&conn, account, stream_id).unwrap().expect("a current key");
        assert_eq!(selected.key_epoch, 1, "the max accepted epoch is the rotated one");
        assert_eq!(
            selected.minting_entry_hash,
            wf.min(wb),
            "min entry_hash tiebreaks the two concurrent rotations",
        );

        // The recovered key is the winning op's key — deterministic across peers.
        let winner_key = if wf < wb { &key1_founder } else { &key1_owner_b };
        let recovered =
            expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
        assert_eq!(
            recovered.as_slice(),
            winner_key.as_slice(),
            "seals under the deterministic winner",
        );
        assert_eq!(security_event_count(&conn), 0, "honest concurrent rotations record no events");
    }

    // ── Convergence: derive-on-read selection is a total order, independent of ingest order ──

    /// The persisted single-row local-device identity, as opaque column bytes. Copying it verbatim
    /// makes two fresh stores share ONE device (`local_device` mints from OS entropy, so a plain
    /// re-mint would give each store a different key and no shared wrap could open in both).
    struct IdentityRow {
        seed: Vec<u8>,
        public_key: Vec<u8>,
        fingerprint: Vec<u8>,
        created_at_ms: i64,
        x25519_secret: Vec<u8>,
        x25519_public: Vec<u8>,
    }

    fn read_identity_row(conn: &Connection) -> IdentityRow {
        conn.query_row(
            "SELECT seed, public_key, fingerprint, created_at_ms, x25519_secret, x25519_public
             FROM oplog_device_identity WHERE id = 0",
            [],
            |row| {
                Ok(IdentityRow {
                    seed: row.get(0)?,
                    public_key: row.get(1)?,
                    fingerprint: row.get(2)?,
                    created_at_ms: row.get(3)?,
                    x25519_secret: row.get(4)?,
                    x25519_public: row.get(5)?,
                })
            },
        )
        .unwrap()
    }

    fn write_identity_row(conn: &Connection, row: &IdentityRow) {
        conn.execute(
            "INSERT INTO oplog_device_identity(
                 id, seed, public_key, fingerprint, created_at_ms, x25519_secret, x25519_public)
             VALUES(0, ?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                row.seed.as_slice(),
                row.public_key.as_slice(),
                row.fingerprint.as_slice(),
                row.created_at_ms,
                row.x25519_secret.as_slice(),
                row.x25519_public.as_slice(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn selection_converges_across_ingest_orders() {
        // `select_current_sealing_wrap` / `current_sealing_key` are DERIVE-ON-READ over the
        // accepted wrap set: MAX key_epoch, tiebreak MIN entry_hash — a TOTAL order. The
        // accepted set itself is order-independent (the fold + fixpoint pre-verify
        // promotion converge regardless of arrival order), so the selection must be
        // byte-identical no matter what order the entries were ingested. Build ONE entry
        // byte-set that exercises both tricky cases at once — two concurrent epoch-1 wraps
        // with DIFFERENT key_ids (the min-entry_hash tiebreak decides) AND
        // an S-1 retro-condemn that drops the epoch-2 max so selection must fall back to epoch 1 —
        // then replay that SAME set in natural and reversed order into fresh stores. A regression
        // to an ingest-order-dependent (non-total) selection would make the two stores
        // diverge here.

        // One deterministic local device, shared byte-for-byte across the two stores, so a wrap
        // sealed to it opens in either. Its keys also address the wraps built below.
        let (device_row, recipient_fp, recipient_x) = {
            let seed_store = db();
            let device = local_device(&seed_store, NOW).unwrap();
            (read_identity_row(&seed_store), device.fingerprint(), device.x25519_public())
        };

        // Build the entry set ONCE — the random ephemeral inside each wrap is then fixed into the
        // bytes and replayed identically into both stores.
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        let owner_b = Dev::new(2);
        let (add_bytes, owner_id_b) = control_op(
            account,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&owner_b, DeviceRole::Owner),
        );
        let (stream_id, own) = stream_own(account);
        let (own_bytes, own_hash) =
            control_op(account, &founder, 2, Some(owner_id_b), Some(genesis_hash), &own);

        let key1_founder = ContentKey::from_seed(&[0x51; 32]);
        let key1_owner_b = ContentKey::from_seed(&[0x52; 32]);
        let key2 = ContentKey::from_seed(&[0x53; 32]);
        assert_ne!(
            key1_founder.key_id(),
            key1_owner_b.key_id(),
            "the two concurrent epoch-1 mints have distinct key_ids",
        );

        // Two concurrent epoch-1 wraps (founder + owner_b), both naming the local device.
        let (wf_bytes, wf) = wrap_entry(
            account,
            &founder,
            0,
            None,
            Some(genesis_hash),
            &honest_wrap(account, stream_id, recipient_fp, &recipient_x, &key1_founder, 1),
        );
        let (wb0_bytes, wb0) = wrap_entry(
            account,
            &owner_b,
            0,
            None,
            Some(owner_id_b),
            &honest_wrap(account, stream_id, recipient_fp, &recipient_x, &key1_owner_b, 1),
        );
        // owner_b's epoch-2 wrap (seq 1) — the eventual max epoch that gets retro-condemned.
        let (wb1_bytes, _wb1) = wrap_entry(
            account,
            &owner_b,
            1,
            Some(wb0),
            Some(owner_id_b),
            &honest_wrap(account, stream_id, recipient_fp, &recipient_x, &key2, 2),
        );
        // Demote owner_b with a secrets cut at seq 0: its epoch-1 wrap (seq 0) survives, its
        // epoch-2 wrap (seq 1) is retro-condemned — dropping the max accepted epoch from 2
        // back to 1.
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: owner_b.fp,
            owner_id: owner_id_b,
            control_cut: Cut::Empty,
            secrets_cut: Cut::At { seq: 0, hash: wb0 },
            reason: "demote".to_string(),
        };
        let (demote_bytes, _) =
            control_op(account, &founder, 3, Some(own_hash), Some(genesis_hash), &demote);

        let entries =
            [genesis_bytes, add_bytes, own_bytes, wf_bytes, wb0_bytes, wb1_bytes, demote_bytes];

        // The deterministic expected outcome: epoch 1, tiebreak = min entry_hash of the two epoch-1
        // wraps, recovering that op's key.
        let (winner_hash, winner_key) =
            if wf < wb0 { (wf, &key1_founder) } else { (wb0, &key1_owner_b) };

        let fold_and_select = |order: &[&Vec<u8>]| -> (SelectedWrap, Vec<u8>) {
            let conn = db();
            write_identity_row(&conn, &device_row);
            let device = local_device(&conn, NOW).unwrap();
            for bytes in order {
                account_ingest(&conn, bytes, NOW).unwrap();
            }
            let selected = select_current_sealing_wrap(&conn, account, stream_id)
                .unwrap()
                .expect("a current key");
            let key =
                expect_ready(current_sealing_key(&conn, account, stream_id, &device, NOW).unwrap());
            (selected, key.as_slice().to_vec())
        };

        let natural: Vec<&Vec<u8>> = entries.iter().collect();
        let reversed: Vec<&Vec<u8>> = entries.iter().rev().collect();
        let (sel_natural, key_natural) = fold_and_select(&natural);
        let (sel_reversed, key_reversed) = fold_and_select(&reversed);

        // Both ingest orders converge on the identical selection…
        assert_eq!(
            sel_natural, sel_reversed,
            "the derive-on-read selection is identical regardless of ingest order",
        );
        assert_eq!(sel_natural.key_epoch, 1, "the epoch-2 wrap was condemned; epoch 1 is the max");
        assert_eq!(
            sel_natural.minting_entry_hash, winner_hash,
            "min entry_hash tiebreaks the two concurrent epoch-1 wraps",
        );
        assert_eq!(
            sel_natural.key_id,
            winner_key.key_id(),
            "the selected key_id is the tiebreak winner's",
        );
        // …and recover the identical content key, matching the deterministic winner.
        assert_eq!(
            key_natural, key_reversed,
            "the recovered content key is identical across orders"
        );
        assert_eq!(
            key_natural.as_slice(),
            winner_key.as_slice(),
            "the recovered key is the tiebreak winner's",
        );
    }
}
