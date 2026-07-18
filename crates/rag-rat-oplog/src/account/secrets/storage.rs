//! The secrets-log (`log_id = 1`) acceptance refold pass (§13/§15, C4.2b).
//!
//! Mirrors `content::storage::refold_content_stream` — a content-style orchestration loop over the
//! log-generic candidate primitives, NOT `derive_account_projection` (whose `effective` input is
//! `fold_account` outcomes that log-1 entries never get). It is wired into
//! [`super::super::storage::refold_in_tx`] between the authority-projection rewrite and the content
//! trigger, in the SAME IMMEDIATE txn, so a control fold that condemns a device's secrets chain
//! retro-condemns its wraps atomically (the retro-condemn cascade, §16.3).
//!
//! The main refold loop writes `retained_unfolded` for every log-1 row (the declassify baseline);
//! this pass OVERWRITES that for the wraps it classifies (S-b/S4), in both `account_entry_status`
//! and the returned status map. A structurally-valid-but-non-evaluable log-1 entry (unknown tag,
//! future `op_version`, sealed `crypto_suite != 0`) is SLOT-ELIGIBLE (it occupies its dense seq
//! slot and extends the chain for descendants) but its own verdict STAYS `retained_unfolded` —
//! never `accepted` (B-2, the frozen convergence rule).

use std::collections::{HashMap, HashSet, VecDeque};

use rusqlite::{Transaction, params};

use super::super::candidate::{self as account_candidate, Ancestry, HeaderView, UnknownCause};
use super::super::cut::Cut;
use super::super::envelope::{self, AccountEntryHeader};
use super::super::fold::{SECRETS_LOG, SUPPORTED_OP_VERSION};
use super::super::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityQuery, OwnerChainAuthority, storage,
};
use super::acceptance::{
    self, AncestryRelation, CitedFreshness, SecretsAcceptance, SecretsAcceptanceInput,
    SecretsParkReason, UnknownAncestry,
};
use super::candidate::{self, BranchPin, BranchSelection, SecretsCandidate, SecretsCoordinate};
use super::ops::{self, DecodedSecretsOp};

type EntryHash = [u8; 32];

/// One log-1 candidate resolved against the current fold. A non-evaluable entry carries no facts —
/// it is slot-eligible but its verdict is fixed at `retained_unfolded`.
struct ResolvedSecretsEntry {
    entry_hash: EntryHash,
    header: AccountEntryHeader,
    dense_predecessor_reachable: bool,
    facts: Option<WrapFacts>,
}

impl ResolvedSecretsEntry {
    /// A non-evaluable entry (`facts` absent) is slot-eligible but never accepted (B-2).
    fn is_evaluable(&self) -> bool {
        self.facts.is_some()
    }
}

/// The authority facts for one evaluable `StreamKeyWrap`, all read from ONE fold snapshot.
struct WrapFacts {
    authority_ref: Option<EntryHash>,
    owner_authority: AuthorityQuery<OwnerChainAuthority>,
    ownership: AuthorityQuery<EntryHash>,
    freshness: CitedFreshness,
    /// The owning account's contested state (§12) — identical for every entry this refold.
    contested: bool,
}

/// Which pass [`verdict_for`] runs.
#[derive(Clone, Copy)]
enum Phase {
    /// The authority pass only: `Some` = decided pre-DAG (rejected/parked/condemned), `None` =
    /// eligible to contest a slot.
    Eligibility,
    /// The whole predicate, including branch selection + freshness — always decides.
    Finished,
}

/// Re-derive secrets-log acceptance for every candidate on `account_id`'s secrets chain from the
/// CURRENT fold, overwriting `account_entry_status` + `accepted` and the caller's `statuses` map
/// (§13/§15, S4). `accepted` was already cleared for the whole account by [`refold_in_tx`]; this
/// pass re-sets `accepted = 1` for its winners (the `account_accepted_slot` index keys on `log_id`,
/// so log-1 winners never collide with the control winners the main loop set).
pub(in crate::account) fn refold_secrets_log(
    tx: &Transaction<'_>,
    account_id: AccountId,
    statuses: &mut HashMap<EntryHash, String>,
) -> anyhow::Result<()> {
    let headers = load_secrets_headers(tx, account_id)?;
    if headers.is_empty() {
        return Ok(());
    }
    let view: HashMap<EntryHash, AccountEntryHeader> =
        headers.iter().map(|(hash, header, _)| (*hash, header.clone())).collect();
    let reachable = reachable_entries(&view);
    let resolved = resolve_secrets_entries(tx, account_id, &headers, &reachable)?;

    // Phase 1 — eligibility. A wrap the authority pass condemns or rejects must NOT compete for a
    // dense seq slot; a non-evaluable entry is always slot-eligible (B-2), occupying its slot and
    // extending the chain for its descendants.
    let mut eligible = HashSet::new();
    for r in &resolved {
        if !r.is_evaluable() || verdict_for(r, &view, false, Phase::Eligibility)?.is_none() {
            eligible.insert(r.entry_hash);
        }
    }

    // Pins from BOTH secrets boundaries of each wrap's cited owner incarnation (B-1): the register
    // decides which branch is real, not hash order.
    let pins = branch_pins(&resolved);
    let candidates: Vec<SecretsCandidate> = resolved
        .iter()
        .map(|r| SecretsCandidate { entry_hash: r.entry_hash, header: r.header.clone() })
        .collect();
    let selection = candidate::select_accepted_branch(&candidates, &eligible, &pins, &view);

    // Phase 2 — the finished verdict, made prefix-closed, then the write. A non-evaluable winner is
    // prefix-TRANSPARENT: it does not accept itself but does not break the accepted prefix for its
    // descendants (B-2).
    let mut raw: HashMap<EntryHash, SecretsAcceptance> = HashMap::new();
    for r in &resolved {
        if r.is_evaluable() {
            let selected = selection.accepted.contains(&r.entry_hash);
            raw.insert(
                r.entry_hash,
                verdict_for(r, &view, selected, Phase::Finished)?
                    .expect("the finished evaluator always returns a verdict for a wrap"),
            );
        }
    }
    let accepted = prefix_closed_accepted(&resolved, &selection, &raw);
    for r in &resolved {
        // A non-evaluable entry keeps the main loop's `retained_unfolded` baseline (B-2) — its
        // status is not one this pass owns.
        let Some(verdict) = raw.get(&r.entry_hash).copied() else {
            continue;
        };
        let hash = r.entry_hash;
        let verdict = if accepted.contains(&hash) {
            SecretsAcceptance::Accepted
        } else if selection.forked.contains(&hash) {
            SecretsAcceptance::Forked
        } else {
            match verdict {
                // Selected + fresh, but truncated by a parked ancestor — parks on that ancestor
                // catching up (S-a #1).
                SecretsAcceptance::Accepted =>
                    SecretsAcceptance::Parked(SecretsParkReason::AuthLenAhead),
                // Eligible but stranded above an unselected parent — recoverable, not a terminal
                // fork (S-a #2).
                SecretsAcceptance::Forked =>
                    SecretsAcceptance::Parked(SecretsParkReason::MissingPredecessor),
                other => other,
            }
        };
        write_secrets_verdict(tx, &hash, verdict, statuses)?;
    }
    Ok(())
}

/// Build the per-entry evaluator input from resolved facts and run the requested phase. `None`
/// (from the eligibility phase) means the wrap is authorized and eligible to contest a slot. The
/// ancestry closure walks `view`, so the input never outlives it. Only ever called for an evaluable
/// wrap.
fn verdict_for(
    r: &ResolvedSecretsEntry,
    view: &HashMap<EntryHash, AccountEntryHeader>,
    branch_selected: bool,
    phase: Phase,
) -> anyhow::Result<Option<SecretsAcceptance>> {
    let facts = r.facts.as_ref().expect("verdict_for is only called for evaluable wraps");
    let input = SecretsAcceptanceInput {
        account_id: facts.freshness.account_id,
        entry_hash: r.entry_hash,
        seq: r.header.seq,
        authority_ref: facts.authority_ref,
        owner_authority: facts.owner_authority,
        ownership: facts.ownership,
        asserted_auth_len: r.header.auth_len,
        dense_predecessor_reachable: r.dense_predecessor_reachable,
        branch_selected,
        freshness: facts.freshness,
        contested: facts.contested,
        ancestry: |target: EntryHash, watermark: EntryHash| {
            ancestry_relation(target, watermark, view)
        },
    };
    let verdict = match phase {
        Phase::Eligibility => acceptance::authority_verdict(&input),
        Phase::Finished => acceptance::evaluate_secrets_acceptance(&input).map(Some),
    };
    verdict.map_err(|error| {
        anyhow::anyhow!("secrets refold built inconsistent freshness provenance: {error:?}")
    })
}

/// Read every candidate on the account's secrets chain, decoding the header + payload from the
/// stored (already signature-verified) bytes. The refold re-derives authority, never re-verifies.
fn load_secrets_headers(
    tx: &Transaction<'_>,
    account_id: AccountId,
) -> anyhow::Result<Vec<(EntryHash, AccountEntryHeader, Vec<u8>)>> {
    let mut stmt = tx.prepare(
        "SELECT entry_hash, signed_bytes FROM account_entries
         WHERE account_id = ?1 AND log_id = ?2
         ORDER BY entry_hash", // deterministic load order (selection is order-free regardless)
    )?;
    let rows = stmt
        .query_map(params![account_id.to_bytes().as_slice(), SECRETS_LOG], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (entry_hash, signed_bytes) in rows {
        let entry_hash = fixed::<32>(&entry_hash)?;
        // A row stored outside `account_ingest` (or a corrupt blob) that no longer decodes / hashes
        // to its key cannot belong to any valid chain — treat it as absent (it keeps the main
        // loop's `retained_unfolded` baseline), rather than aborting the whole account fold
        // on one bad row.
        let Ok(signed) = envelope::decode_account_signed(&signed_bytes) else {
            continue;
        };
        if signed.entry_hash != entry_hash {
            continue;
        }
        out.push((entry_hash, signed.header, signed.payload));
    }
    Ok(out)
}

/// The entries whose dense secrets chain is fully held back to seq 0, in ONE O(n) forward pass from
/// the chain roots (mirrors `content::storage::reachable_entries`). A secrets chain is dense per
/// `(account, device)` on `log: SECRETS_LOG`.
fn reachable_entries(view: &HashMap<EntryHash, AccountEntryHeader>) -> HashSet<EntryHash> {
    let mut by_prev: HashMap<EntryHash, Vec<&EntryHash>> = HashMap::new();
    let mut queue: VecDeque<&EntryHash> = VecDeque::new();
    for (hash, header) in view {
        match header.prev_hash {
            None if header.seq == 0 => queue.push_back(hash),
            None => {},
            Some(prev) => by_prev.entry(prev).or_default().push(hash),
        }
    }
    let mut reachable = HashSet::new();
    while let Some(hash) = queue.pop_front() {
        if !reachable.insert(*hash) {
            continue;
        }
        let parent = &view[hash];
        let Some(children) = by_prev.get(hash) else {
            continue;
        };
        for child_hash in children {
            let child = &view[*child_hash];
            // Contiguous + same coordinate (a link that skips a slot or jumps chain is not a real
            // predecessor). `seq` is peer-supplied, so guard `+ 1` against a `u64::MAX` row.
            if parent.seq.checked_add(1) == Some(child.seq)
                && child.account_id == parent.account_id
                && child.device_fingerprint == parent.device_fingerprint
            {
                queue.push_back(child_hash);
            }
        }
    }
    reachable
}

/// Resolve every log-1 candidate's authority facts against the current fold. A non-evaluable entry
/// (unknown tag / future version / sealed) resolves NO facts — it is slot-eligible, never
/// evaluated.
fn resolve_secrets_entries(
    tx: &Transaction<'_>,
    account_id: AccountId,
    headers: &[(EntryHash, AccountEntryHeader, Vec<u8>)],
    reachable: &HashSet<EntryHash>,
) -> anyhow::Result<Vec<ResolvedSecretsEntry>> {
    // The account's contested state is the same for every entry — read it once (§12).
    let contested = storage::account_is_contested(tx, account_id)?;
    let mut caches = Caches::default();
    let mut resolved = Vec::with_capacity(headers.len());
    for (entry_hash, header, payload) in headers {
        let facts = wrap_facts(tx, account_id, header, payload, contested, &mut caches)?;
        resolved.push(ResolvedSecretsEntry {
            entry_hash: *entry_hash,
            header: header.clone(),
            dense_predecessor_reachable: reachable.contains(entry_hash),
            facts,
        });
    }
    Ok(resolved)
}

/// Per-refold memoization of the authority facts, scoped to this snapshot — a chain of many wraps
/// sharing one owner incarnation resolves each fact once.
#[derive(Default)]
struct Caches {
    owner: HashMap<(EntryHash, crate::op::DeviceFingerprint), AuthorityQuery<OwnerChainAuthority>>,
    ownership: HashMap<crate::stream::StreamId, AuthorityQuery<EntryHash>>,
    freshness: HashMap<u64, AuthorityFreshness>,
}

/// Resolve one entry to its `WrapFacts` — `None` when the entry is NOT an evaluable `StreamKeyWrap`
/// (unknown tag, future `op_version`, or sealed `crypto_suite != 0`), which makes it slot-eligible.
fn wrap_facts(
    tx: &Transaction<'_>,
    account_id: AccountId,
    header: &AccountEntryHeader,
    payload: &[u8],
    contested: bool,
    caches: &mut Caches,
) -> anyhow::Result<Option<WrapFacts>> {
    // Evaluable ⇔ a KNOWN secrets op at the supported version with a plaintext payload — mirrors
    // the control fold's `foldable` gate. Everything else is slot-eligible (B-2).
    if header.op_version != SUPPORTED_OP_VERSION || header.crypto_suite != 0 {
        return Ok(None);
    }
    let stream_id = match ops::decode(header.entry_type, payload) {
        Ok(DecodedSecretsOp::Known(wrap)) => wrap.stream_id,
        Ok(DecodedSecretsOp::Unknown { .. }) | Err(_) => return Ok(None),
    };
    let owner_authority = match header.authority_ref {
        Some(owner_id) => {
            let key = (owner_id, header.device_fingerprint);
            match caches.owner.get(&key) {
                Some(cached) => *cached,
                None => {
                    let resolved = storage::owner_secrets_authority_in_snapshot(
                        tx,
                        account_id,
                        owner_id,
                        header.device_fingerprint,
                    )?;
                    caches.owner.insert(key, resolved);
                    resolved
                },
            }
        },
        // A null authority_ref is rejected by the evaluator on its own check; the owner value is
        // unused, so a placeholder Unknown is correct.
        None => AuthorityQuery::Unknown,
    };
    let ownership = match caches.ownership.get(&stream_id) {
        Some(cached) => *cached,
        None => {
            let resolved = storage::stream_owner_effective_in_snapshot(tx, account_id, stream_id)?;
            caches.ownership.insert(stream_id, resolved);
            resolved
        },
    };
    let state = match caches.freshness.get(&header.auth_len) {
        Some(cached) => *cached,
        None => {
            let state = storage::auth_len_freshness(tx, account_id, header.auth_len)?;
            caches.freshness.insert(header.auth_len, state);
            state
        },
    };
    Ok(Some(WrapFacts {
        authority_ref: header.authority_ref,
        owner_authority,
        ownership,
        freshness: CitedFreshness { account_id, asserted_auth_len: header.auth_len, state },
        contested,
    }))
}

/// The register watermarks pinning a branch: BOTH secrets boundaries of each wrap's cited owner
/// incarnation (device register + owner-incarnation register), keyed to the wrap's `(account,
/// device)` secrets coordinate. `pinned_branch` re-validates each against its coordinate, so
/// over-collecting is safe.
fn branch_pins(resolved: &[ResolvedSecretsEntry]) -> Vec<BranchPin> {
    let mut pins = Vec::new();
    for r in resolved {
        let Some(facts) = &r.facts else {
            continue;
        };
        let AuthorityQuery::Effective(owner) = facts.owner_authority else {
            continue;
        };
        let coordinate = SecretsCoordinate {
            account_id: r.header.account_id,
            device_fingerprint: r.header.device_fingerprint,
        };
        for boundary in [owner.device_boundary, owner.incarnation_boundary] {
            if let AuthorityBoundary::Cut { seq, hash } = boundary {
                pins.push(BranchPin { coordinate, seq, watermark: hash });
            }
        }
    }
    pins
}

/// Narrow the branch-selected winners to a contiguous accepted prefix per coordinate. A
/// non-evaluable selected winner is prefix-TRANSPARENT (it does not accept itself but does not
/// break the prefix for its descendants — B-2); an evaluable winner whose finished verdict is not
/// `Accepted` breaks it.
fn prefix_closed_accepted(
    resolved: &[ResolvedSecretsEntry],
    selection: &BranchSelection,
    raw: &HashMap<EntryHash, SecretsAcceptance>,
) -> HashSet<EntryHash> {
    let mut chains: HashMap<SecretsCoordinate, Vec<(u64, EntryHash, bool)>> = HashMap::new();
    for r in resolved {
        if selection.accepted.contains(&r.entry_hash) {
            let coordinate = SecretsCoordinate {
                account_id: r.header.account_id,
                device_fingerprint: r.header.device_fingerprint,
            };
            chains.entry(coordinate).or_default().push((
                r.header.seq,
                r.entry_hash,
                r.is_evaluable(),
            ));
        }
    }
    let mut accepted = HashSet::new();
    for mut winners in chains.into_values() {
        winners.sort_by_key(|(seq, _, _)| *seq);
        for (_, hash, evaluable) in winners {
            if !evaluable {
                continue; // a slot-eligible winner is transparent — never accepted, never a break
            }
            if raw.get(&hash) == Some(&SecretsAcceptance::Accepted) {
                accepted.insert(hash);
            } else {
                break; // prefix broken: every later winner on this chain is not accepted
            }
        }
    }
    accepted
}

/// Write one wrap's verdict: the `(status, detail)` pair (§16.3), `accepted = 1` only for
/// `Accepted`, and OVERWRITE the caller's status map (S4).
fn write_secrets_verdict(
    tx: &Transaction<'_>,
    entry_hash: &EntryHash,
    verdict: SecretsAcceptance,
    statuses: &mut HashMap<EntryHash, String>,
) -> rusqlite::Result<()> {
    let (status, detail) = verdict.as_db_pair();
    tx.execute(
        "INSERT INTO account_entry_status(entry_hash, status, detail) VALUES (?1, ?2, ?3)
         ON CONFLICT(entry_hash) DO UPDATE SET status = excluded.status, detail = excluded.detail",
        params![entry_hash.as_slice(), status, detail],
    )?;
    if verdict == SecretsAcceptance::Accepted {
        tx.execute("UPDATE account_entries SET accepted = 1 WHERE entry_hash = ?1", [
            entry_hash.as_slice()
        ])?;
    }
    statuses.insert(*entry_hash, status.to_string());
    Ok(())
}

/// Map the account-log ancestry verdict into the evaluator's `AncestryRelation` (both name a
/// withheld watermark and a missing mid-chain link apart — I11).
fn ancestry_relation(
    target: EntryHash,
    watermark: EntryHash,
    view: &dyn HeaderView,
) -> AncestryRelation {
    // The seq is unused by the ancestry walk (it follows `prev_hash` from the watermark hash), so a
    // placeholder 0 is correct here.
    match account_candidate::ancestry(&target, &Cut::At { seq: 0, hash: watermark }, view) {
        Ancestry::OnBranch => AncestryRelation::OnBranch,
        Ancestry::OffBranch => AncestryRelation::OffBranch,
        Ancestry::Unknown(UnknownCause::UnknownCutTarget) =>
            AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget),
        Ancestry::Unknown(UnknownCause::IncompleteCutAncestry) =>
            AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry),
    }
}

fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("stored blob is {} bytes, not {N}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use minicbor::Encoder;
    use rag_rat_db::schema;
    use rusqlite::Connection;

    use super::super::ops::{StreamKeyWrap, WrapEntry, entry_type as secrets_entry_type};
    use super::*;
    use crate::account::envelope::sign_account_entry;
    use crate::account::id::account_id_from_genesis_payload;
    use crate::account::keywrap::{ContentKey, WrapContext, seal_content_key};
    use crate::account::ops::{self as control_ops, AccountOp, DeviceRole};
    use crate::account::storage::{IngestOutcome, account_ingest, entry_status};
    use crate::device::{DeviceSecret, DeviceX25519Public, DeviceX25519Secret};
    use crate::op::DeviceFingerprint;
    use crate::stream::{self, StreamId, StreamSpec, StreamSpecV2};

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

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
            log_id: super::super::super::fold::CONTROL_LOG,
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
            log_id: super::super::super::fold::CONTROL_LOG,
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

    /// Build a `StreamKeyWrap` op sealing a seed-derived content key to `recipient`.
    fn wrap_op(
        account: AccountId,
        stream_id: StreamId,
        recipient: &Dev,
        seed: u8,
    ) -> StreamKeyWrap {
        let key = ContentKey::from_seed(&[seed; 32]);
        let recipient_pub = DeviceX25519Public::from_bytes(&recipient.x).unwrap();
        let ctx = WrapContext {
            account_id: account.to_bytes(),
            stream_id: stream_id.to_bytes(),
            key_epoch: 0,
            recipient_pub: recipient.x,
        };
        let sealed = seal_content_key(&key, &ctx, &recipient_pub).unwrap();
        StreamKeyWrap {
            stream_id,
            key_id: key.key_id().to_bytes(),
            key_epoch: 0,
            wraps: vec![WrapEntry { recipient_fp: recipient.fp, sealed }],
        }
    }

    /// Author a signed secrets-log (`log_id = 1`) `StreamKeyWrap` entry.
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
        secrets_entry(
            account,
            signer,
            seq,
            prev,
            authority_ref,
            secrets_entry_type::STREAM_KEY_WRAP,
            &payload,
        )
    }

    /// Author a signed secrets-log entry with an arbitrary `entry_type` + payload.
    #[allow(clippy::too_many_arguments)]
    fn secrets_entry(
        account: AccountId,
        signer: &Dev,
        seq: u64,
        prev: Option<[u8; 32]>,
        authority_ref: Option<[u8; 32]>,
        entry_type: u32,
        payload: &[u8],
    ) -> (Vec<u8>, [u8; 32]) {
        let header = AccountEntryHeader {
            account_id: account,
            log_id: SECRETS_LOG,
            device_fingerprint: signer.fp,
            seq,
            prev_hash: prev,
            parent_ref: Some([0u8; 32]),
            entry_type,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref,
        };
        let signed = sign_account_entry(&signer.secret, &header, payload).unwrap();
        (signed.signed_bytes, signed.entry_hash)
    }

    fn ingest(conn: &Connection, bytes: &[u8]) -> IngestOutcome {
        account_ingest(conn, bytes, NOW).unwrap()
    }

    fn status(conn: &Connection, hash: &[u8; 32]) -> (String, Option<String>) {
        entry_status(conn, hash).unwrap().expect("entry is stored")
    }

    fn accepted_flag(conn: &Connection, hash: &[u8; 32]) -> i64 {
        conn.query_row(
            "SELECT accepted FROM account_entries WHERE entry_hash = ?1",
            [hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// genesis(A) + StreamOwn(A) → the account, the founder owner-incarnation id (= genesis hash),
    /// and the owned stream.
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

    #[test]
    fn a_fresh_owner_wrap_accepts_and_ingest_reports_accepted() {
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let wrap = wrap_op(account, stream_id, &founder, 0x20);
        let (bytes, hash) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        // S4: the ingest-returned status is the secrets pass's verdict, not the `retained_unfolded`
        // baseline the main loop wrote.
        let outcome = ingest(&conn, &bytes);
        assert_eq!(outcome, IngestOutcome::Ingested { status: "accepted".to_string() });
        assert_eq!(status(&conn, &hash), ("accepted".to_string(), None));
        assert_eq!(accepted_flag(&conn, &hash), 1);
    }

    #[test]
    fn a_non_owner_wrap_is_rejected_by_the_owner_only_gate() {
        let conn = db();
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(&conn, &genesis_bytes);
        // A member device, added to the roster (so it is signature-resolvable).
        let member = Dev::new(2);
        let add = AccountOp::DeviceAdd {
            device_fingerprint: member.fp,
            ed25519_pubkey: member.ed,
            x25519_pubkey: member.x,
            role: DeviceRole::Member,
            label: None,
        };
        let (add_bytes, _) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &add);
        ingest(&conn, &add_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, _) =
            control_op(account, &founder, 2, Some(genesis_hash), Some(genesis_hash), &own);
        ingest(&conn, &own_bytes);
        // The member signs a wrap citing the founder's owner incarnation — a device mismatch, so
        // the owner-only gate rejects it (WrongSubject → invalid_owner).
        let wrap = wrap_op(account, stream_id, &member, 0x20);
        let (bytes, hash) = wrap_entry(account, &member, 0, None, Some(genesis_hash), &wrap);
        assert_eq!(ingest(&conn, &bytes), IngestOutcome::Ingested {
            status: "rejected".to_string()
        });
        assert_eq!(
            status(&conn, &hash),
            ("rejected".to_string(), Some("invalid_owner".to_string()))
        );
        assert_eq!(accepted_flag(&conn, &hash), 0);
    }

    #[test]
    fn a_null_authority_ref_wrap_is_rejected() {
        let conn = db();
        let (account, founder, _genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let wrap = wrap_op(account, stream_id, &founder, 0x20);
        let (bytes, hash) = wrap_entry(account, &founder, 0, None, None, &wrap);
        ingest(&conn, &bytes);
        assert_eq!(
            status(&conn, &hash),
            ("rejected".to_string(), Some("invalid_owner".to_string()))
        );
    }

    #[test]
    fn a_wrap_before_its_stream_own_parks_then_accepts_when_ownership_folds() {
        let conn = db();
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(&conn, &genesis_bytes);
        let (stream_id, own) = stream_own(account);
        // The wrap arrives BEFORE the StreamOwn: the account does not yet own the stream, so it
        // parks (recoverable), never rejects (§14).
        let wrap = wrap_op(account, stream_id, &founder, 0x20);
        let (bytes, hash) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap);
        ingest(&conn, &bytes);
        assert_eq!(
            status(&conn, &hash),
            ("parked".to_string(), Some("unknown_account".to_string()))
        );
        // The StreamOwn folds in the same account → the secrets pass re-classifies the wrap as
        // accepted in that same refold (the account→content retro-trigger analog).
        let (own_bytes, _) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        ingest(&conn, &own_bytes);
        assert_eq!(status(&conn, &hash), ("accepted".to_string(), None));
        assert_eq!(accepted_flag(&conn, &hash), 1);
    }

    #[test]
    fn an_unsorted_or_undecodable_wrap_payload_is_structurally_rejected_at_ingest() {
        let conn = db();
        let (account, founder, genesis_hash, _stream_id) = account_with_owned_stream(&conn);
        // A hand-built payload whose wrapped_key is not a SealedKeyWrap — the secrets twin decodes
        // it at ingest and rejects, so it is NEVER stored.
        let mut payload = Vec::new();
        {
            let mut enc = Encoder::new(&mut payload);
            enc.array(4).unwrap();
            enc.bytes(&[9; 32]).unwrap();
            enc.bytes(&[8; 32]).unwrap();
            enc.u64(0).unwrap();
            enc.array(1).unwrap();
            enc.array(2).unwrap();
            enc.bytes(&[0x10; 32]).unwrap();
            enc.bytes(&[0xde, 0xad]).unwrap();
        }
        let (bytes, hash) = secrets_entry(
            account,
            &founder,
            0,
            None,
            Some(genesis_hash),
            secrets_entry_type::STREAM_KEY_WRAP,
            &payload,
        );
        assert!(
            matches!(ingest(&conn, &bytes), IngestOutcome::Rejected(_)),
            "twin rejects at ingest"
        );
        assert!(entry_status(&conn, &hash).unwrap().is_none(), "a rejected entry is not stored");
    }

    #[test]
    fn a_slot_eligible_unknown_tag_is_transparent_and_chained_wraps_accept() {
        // B-2: [wrap@0, unknown-tag@1, wrap@2] → both wraps accepted, the middle retained_unfolded.
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let wrap0 = wrap_op(account, stream_id, &founder, 0x20);
        let (w0_bytes, w0) = wrap_entry(account, &founder, 0, None, Some(genesis_hash), &wrap0);
        ingest(&conn, &w0_bytes);
        // An unknown secrets tag with a canonical (empty-array) payload: storable, slot-eligible.
        let mut unknown_payload = Vec::new();
        Encoder::new(&mut unknown_payload).array(0).unwrap();
        let (u_bytes, u1) =
            secrets_entry(account, &founder, 1, Some(w0), Some(genesis_hash), 99, &unknown_payload);
        ingest(&conn, &u_bytes);
        let wrap2 = wrap_op(account, stream_id, &founder, 0x21);
        let (w2_bytes, w2) = wrap_entry(account, &founder, 2, Some(u1), Some(genesis_hash), &wrap2);
        ingest(&conn, &w2_bytes);

        assert_eq!(status(&conn, &w0), ("accepted".to_string(), None), "wrap@0 accepts");
        assert_eq!(
            status(&conn, &u1),
            ("retained_unfolded".to_string(), None),
            "the unknown tag is slot-eligible but never accepted (B-2)",
        );
        assert_eq!(accepted_flag(&conn, &u1), 0, "a slot-eligible entry is never accepted");
        assert_eq!(
            status(&conn, &w2),
            ("accepted".to_string(), None),
            "the wrap chained PAST the unknown tag still accepts (slot-eligible is transparent)",
        );
    }

    #[test]
    fn multiple_wrap_ops_for_one_stream_and_key_all_accept() {
        // A dense chain of wraps for the same (stream, key_id) — the SET resolution, never LWW.
        let conn = db();
        let (account, founder, genesis_hash, stream_id) = account_with_owned_stream(&conn);
        let mut prev = None;
        let mut hashes = Vec::new();
        for (seq, seed) in [(0u64, 0x20u8), (1, 0x21), (2, 0x22)] {
            let wrap = wrap_op(account, stream_id, &founder, seed);
            let (bytes, hash) = wrap_entry(account, &founder, seq, prev, Some(genesis_hash), &wrap);
            ingest(&conn, &bytes);
            prev = Some(hash);
            hashes.push(hash);
        }
        for hash in hashes {
            assert_eq!(
                status(&conn, &hash),
                ("accepted".to_string(), None),
                "every wrap op accepts"
            );
        }
    }

    /// genesis(A owner) + DeviceAdd(B owner) + StreamOwn(A) → the account, both devices, B's owner
    /// incarnation id, the owned stream, and the StreamOwn hash (the control tail a later demote
    /// chains from). A's control chain: genesis(0) ← DeviceAdd(1) ← StreamOwn(2).
    fn account_with_second_owner(
        conn: &Connection,
    ) -> (AccountId, Dev, Dev, [u8; 32], StreamId, [u8; 32]) {
        let founder = Dev::new(1);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        ingest(conn, &genesis_bytes);
        let owner_b = Dev::new(2);
        let add = AccountOp::DeviceAdd {
            device_fingerprint: owner_b.fp,
            ed25519_pubkey: owner_b.ed,
            x25519_pubkey: owner_b.x,
            role: DeviceRole::Owner,
            label: None,
        };
        let (add_bytes, owner_id_b) =
            control_op(account, &founder, 1, Some(genesis_hash), Some(genesis_hash), &add);
        ingest(conn, &add_bytes);
        let (stream_id, own) = stream_own(account);
        let (own_bytes, own_hash) =
            control_op(account, &founder, 2, Some(owner_id_b), Some(genesis_hash), &own);
        ingest(conn, &own_bytes);
        (account, founder, owner_b, owner_id_b, stream_id, own_hash)
    }

    #[test]
    fn a_later_owner_demote_secrets_cut_retro_condemns_a_beyond_cut_wrap() {
        let conn = db();
        let (account, founder, owner_b, owner_id_b, stream_id, own_hash) =
            account_with_second_owner(&conn);
        // B authors two wraps on its secrets chain; both accept while B is a live owner.
        let w0op = wrap_op(account, stream_id, &owner_b, 0x30);
        let (w0_bytes, w0) = wrap_entry(account, &owner_b, 0, None, Some(owner_id_b), &w0op);
        ingest(&conn, &w0_bytes);
        let w1op = wrap_op(account, stream_id, &owner_b, 0x31);
        let (w1_bytes, w1) = wrap_entry(account, &owner_b, 1, Some(w0), Some(owner_id_b), &w1op);
        ingest(&conn, &w1_bytes);
        assert_eq!(status(&conn, &w0), ("accepted".to_string(), None), "wrap@0 accepts pre-cut");
        assert_eq!(status(&conn, &w1), ("accepted".to_string(), None), "wrap@1 accepts pre-cut");

        // A demotes B, bounding B's secrets incarnation at seq 0 (h0). The cut retro-condemns the
        // beyond-cut wrap@1 in the SAME refold — wrap@0 (within the cut, on-branch) stays accepted.
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: owner_b.fp,
            owner_id: owner_id_b,
            control_cut: super::super::super::cut::Cut::Empty,
            secrets_cut: super::super::super::cut::Cut::At { seq: 0, hash: w0 },
            reason: "demote".to_string(),
        };
        let genesis_hash = owner_id_of_founder(&conn, account);
        let (demote_bytes, _) =
            control_op(account, &founder, 3, Some(own_hash), Some(genesis_hash), &demote);
        ingest(&conn, &demote_bytes);
        assert_eq!(
            status(&conn, &w0),
            ("accepted".to_string(), None),
            "the within-cut wrap stays accepted",
        );
        assert_eq!(
            status(&conn, &w1),
            ("condemned".to_string(), Some("beyond_cut".to_string())),
            "the beyond-cut wrap is retro-condemned in the same refold",
        );
        assert_eq!(accepted_flag(&conn, &w1), 0);
    }

    #[test]
    fn equivocation_below_a_secrets_cut_condemns_the_attacker_fork_off_branch() {
        // B-1: B equivocates its secrets chain below a demotion cut; the cut names the honest
        // branch, so the honest on-watermark wraps accept and the attacker fork is
        // condemned off_branch.
        let conn = db();
        let (account, founder, owner_b, owner_id_b, stream_id, own_hash) =
            account_with_second_owner(&conn);
        let w0op = wrap_op(account, stream_id, &owner_b, 0x30);
        let (w0_bytes, w0) = wrap_entry(account, &owner_b, 0, None, Some(owner_id_b), &w0op);
        ingest(&conn, &w0_bytes);
        // Two DIFFERENT seq-1 entries off w0 — an equivocation with distinct payloads (distinct
        // hashes).
        let honest_op = wrap_op(account, stream_id, &owner_b, 0x31);
        let (honest_bytes, honest) =
            wrap_entry(account, &owner_b, 1, Some(w0), Some(owner_id_b), &honest_op);
        ingest(&conn, &honest_bytes);
        let attacker_op = wrap_op(account, stream_id, &owner_b, 0x41);
        let (attacker_bytes, attacker) =
            wrap_entry(account, &owner_b, 1, Some(w0), Some(owner_id_b), &attacker_op);
        ingest(&conn, &attacker_bytes);

        // A demotes B with a secrets cut naming the HONEST seq-1 head. The register pins the honest
        // branch; the attacker fork is off it.
        let genesis_hash = owner_id_of_founder(&conn, account);
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: owner_b.fp,
            owner_id: owner_id_b,
            control_cut: super::super::super::cut::Cut::Empty,
            secrets_cut: super::super::super::cut::Cut::At { seq: 1, hash: honest },
            reason: "demote".to_string(),
        };
        let (demote_bytes, _) =
            control_op(account, &founder, 3, Some(own_hash), Some(genesis_hash), &demote);
        ingest(&conn, &demote_bytes);

        assert_eq!(status(&conn, &w0), ("accepted".to_string(), None), "the shared root accepts");
        assert_eq!(
            status(&conn, &honest),
            ("accepted".to_string(), None),
            "the honest on-watermark wrap accepts",
        );
        assert_eq!(
            status(&conn, &attacker),
            ("condemned".to_string(), Some("off_branch".to_string())),
            "the equivocated-below-cut attacker fork is condemned off_branch",
        );
        assert_eq!(accepted_flag(&conn, &attacker), 0);
    }

    /// The founder's owner incarnation id is the genesis entry hash (§ the founder's origin slot).
    fn owner_id_of_founder(conn: &Connection, account: AccountId) -> [u8; 32] {
        conn.query_row(
            "SELECT entry_hash FROM account_entries
             WHERE account_id = ?1 AND entry_type = ?2 AND log_id = ?3",
            rusqlite::params![
                account.to_bytes().as_slice(),
                control_ops::entry_type::ACCOUNT_GENESIS,
                super::super::super::fold::CONTROL_LOG,
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map(|bytes| fixed::<32>(&bytes).unwrap())
        .unwrap()
    }
}
