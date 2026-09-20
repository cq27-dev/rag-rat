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

use super::super::id::{self, AccountEntryHash};
use super::super::keywrap::{self, ContentKey, KeyId, WrapContext};
use super::super::{AccountId, bootstrap, content, envelope, fold, storage};
use super::ops::{self, DecodedSecretsOp, StreamKeyWrap};
use super::security_event::{self, SyncSecurityEvent, SyncSecurityEventKind};
use crate::identity::{LocalDevice, load_local_device};
use crate::stream::StreamId;

/// The current sealing selection for a stream — the winning `(epoch, key_id)` over the accepted
/// wrap set, plus the entry hash that decided the tiebreak. Carries the CLAIMED `key_id` (the op
/// payload field); the cross-check against the actually-UNWRAPPED key is [`current_sealing_key`]'s
/// job. Also the CLI "what key is current for this stream" surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedWrap {
    pub key_id: KeyId,
    pub key_epoch: u64,
    pub minting_entry_hash: AccountEntryHash,
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
    entry_hash: AccountEntryHash,
    observed_key_id: Option<KeyId>,
}

/// One EFFECTIVE accepted `StreamKeyWrap` op for a stream, decoded from its stored bytes.
struct AcceptedStreamWrap {
    entry_hash: AccountEntryHash,
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

/// Derive every live exact key group in the requested streams and classify whether accepted
/// siblings already cover `target`. Every requested stream must currently be owned by the local
/// account. The whole read uses the caller's connection snapshot; callers needing atomic authoring
/// pass their IMMEDIATE transaction.
///
/// Live means either referenced by currently accepted suite-1 content, or selected for current
/// sealing on an owned stream. This deliberately excludes plaintext content, condemned-only wraps,
/// and accepted unused loser keys. Target enrollment is resolved through the current authority
/// projection's exact accepted `roster_ref`; a removed or never-effective target fails.
pub fn live_stream_key_targets_for_device(
    conn: &Connection,
    target: crate::op::DeviceFingerprint,
    streams: &[StreamId],
) -> anyhow::Result<LiveKeyTargets> {
    let account_id = bootstrap::local_account_ref(conn)?
        .context("cannot derive stream-key catch-up targets before the local account is minted")?
        .account_id;
    storage::effective_roster_x25519_pubkey(conn, account_id, target)?.context(
        "stream-key catch-up target is not currently roster-effective in the local account",
    )?;
    let live = live_stream_key_epochs(conn, account_id, streams)?;

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

/// Verify the local founder can recover every live exact key a new device needs, then return the
/// catch-up workload. Enrollment minting calls this before issuing a ticket; counting alone would
/// allow a permanently unusable invite when an accepted wrap omitted or cannot decrypt locally.
pub(crate) fn recoverable_live_stream_key_target_count(
    conn: &Connection,
    account_id: AccountId,
    streams: &[StreamId],
) -> anyhow::Result<usize> {
    let live = live_stream_key_epochs(conn, account_id, streams)?;
    let device = load_local_device(conn)?
        .context("cannot preflight enrollment key recovery without a local device")?;
    for target in &live {
        anyhow::ensure!(
            recover_exact_historical_content_key(conn, account_id, *target, &device)?.is_some(),
            "live content key is not recoverable for enrollment catch-up (stream {:?}, epoch {}, \
             key {:?})",
            target.stream_id,
            target.key_epoch,
            target.key_id,
        );
    }
    Ok(live.len())
}

/// Every live exact key group in `streams` (see [`live_stream_key_targets_for_device`] for what
/// "live" means). Each requested stream must currently be owned by `account_id`.
pub(super) fn live_stream_key_epochs(
    conn: &Connection,
    account_id: AccountId,
    streams: &[StreamId],
) -> anyhow::Result<Vec<LiveKeyEpoch>> {
    let mut stmt = conn.prepare(
        "SELECT stream_id FROM account_stream_ownership
         WHERE account_id = ?1 ORDER BY stream_id",
    )?;
    let owned_streams = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| Ok(StreamId::from_bytes(id::fixed::<32>(&raw)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    for stream in streams {
        anyhow::ensure!(
            owned_streams.contains(stream),
            "requested catch-up stream is not currently owned by the local account"
        );
    }

    let mut live = Vec::new();
    for stream_id in streams.iter().copied() {
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
    Ok(live)
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
    let _snapshot = super::super::control_policy::read_snapshot(conn)?;
    super::super::control_policy::require_foldable_account_control(conn, account_id)?;
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
        if signed.entry_hash != AccountEntryHash::from_bytes(entry_hash) {
            if matches!(mode, AcceptedWrapDecodeMode::Tolerant) {
                continue;
            }
            anyhow::bail!(
                "stored accepted secrets envelope does not match its entry_hash row while \
                 checking sealed-ratchet wrap evidence"
            );
        }
        match ops::decode(signed.header.entry_type, &signed.payload) {
            Ok(DecodedSecretsOp::StreamKeyWrap(wrap)) if wrap.stream_id == stream_id => {
                out.push(AcceptedStreamWrap {
                    entry_hash: AccountEntryHash::from_bytes(entry_hash),
                    wrap,
                });
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
#[path = "sealing/tests.rs"]
mod tests;
