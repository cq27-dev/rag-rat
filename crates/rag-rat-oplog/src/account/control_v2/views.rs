//! Bounded, iterative planning of exact pre-cut replay inputs.
//!
//! The checkpoint's complete legacy evidence is implicit in every view; the manifest lists the
//! exact additional v2 candidates, including losing branches. Dependency edges come from those
//! candidates' signed payloads, not a peer-supplied dependency list. Each distinct view appears
//! once, dependencies first, so an executor can memoize one fold result per view.
//!
//! This layer checks commitments and grammar, NOT signatures or authority. A plan must never be
//! used as a credit/effectiveness proof. The eventual executor must authenticate candidates and
//! enforce frozen legacy policy before deriving the accepted set and citation count.

use std::collections::{BTreeMap, BTreeSet};

use minicbor::{Decoder, Encoder};

use super::super::checkpoint::VerifiedCheckpoint;
use super::super::envelope::{self, SignedAccountEntry};
use super::super::id::{self, AccountEntryHash};
use super::ops;
use crate::cbor::{self, VecEncoderExt};

const DOMAIN: &str = "rag-rat/control-view/2";
pub(in crate::account) const MAX_VIEWS: usize = 128;
pub(in crate::account) const MAX_DEPTH: usize = 32;
pub(in crate::account) const MAX_ENTRIES: usize = 4096;
pub(in crate::account) const MAX_REFERENCES: usize = 16_384;
pub(in crate::account) const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct ViewManifest {
    pub checkpoint: [u8; 32],
    pub entries: Vec<AccountEntryHash>,
}

impl ViewManifest {
    pub(in crate::account) fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(self.entries.len() <= MAX_ENTRIES, "view entries exceed limit");
        let mut entries = self.entries.clone();
        entries.sort_unstable();
        anyhow::ensure!(entries.windows(2).all(|p| p[0] != p[1]), "duplicate view entry");
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes);
        e.put_array(3);
        e.put_str(DOMAIN);
        e.put_bytes(&self.checkpoint);
        e.put_array(entries.len() as u64);
        for entry in entries {
            e.put_bytes(entry.as_slice());
        }
        Ok(bytes)
    }

    pub(in crate::account) fn digest(&self) -> anyhow::Result<[u8; 32]> {
        Ok(cbor::sha256(&self.encode()?))
    }
}

fn decode(bytes: &[u8]) -> anyhow::Result<ViewManifest> {
    // Check before recursively validating the CBOR item or allocating its arrays.
    anyhow::ensure!(bytes.len() <= MAX_ENTRIES * 34 + 128, "view manifest too large");
    cbor::require_canonical_cbor(bytes)?;
    let mut d = Decoder::new(bytes);
    anyhow::ensure!(d.array()? == Some(3) && d.str()? == DOMAIN, "view grammar");
    let checkpoint = id::fixed(d.bytes()?)?;
    let n = d.array()?.ok_or_else(|| anyhow::anyhow!("indefinite view entries"))?;
    anyhow::ensure!(n <= MAX_ENTRIES as u64, "view entries exceed limit");
    let mut entries = Vec::with_capacity(n as usize);
    for _ in 0..n {
        entries.push(id::fixed::<32>(d.bytes()?)?.into());
    }
    let manifest = ViewManifest { checkpoint, entries };
    anyhow::ensure!(
        d.position() == bytes.len() && manifest.encode()? == bytes,
        "noncanonical view"
    );
    Ok(manifest)
}

#[derive(Debug)]
pub(in crate::account) enum PlanError {
    MissingView([u8; 32]),
    MissingEntry(AccountEntryHash),
    Invalid(anyhow::Error),
}

impl From<anyhow::Error> for PlanError {
    fn from(error: anyhow::Error) -> Self {
        Self::Invalid(error)
    }
}

/// Exact, structurally resolved inputs. No caller-supplied effective sets are accepted here.
pub(in crate::account) struct ReplayPlan {
    order: Vec<[u8; 32]>,
    manifests: BTreeMap<[u8; 32], ViewManifest>,
    candidates: BTreeMap<AccountEntryHash, SignedAccountEntry>,
}

impl ReplayPlan {
    pub(in crate::account) fn views(&self) -> impl Iterator<Item = ([u8; 32], &ViewManifest)> {
        self.order.iter().map(|hash| (*hash, &self.manifests[hash]))
    }

    pub(in crate::account) fn candidate(&self, hash: &AccountEntryHash) -> &SignedAccountEntry {
        &self.candidates[hash]
    }
}

/// Resolve a bounded proof bundle. All supplied objects count toward limits, including extraneous
/// ones; senders can strip unneeded objects before calling. Missing evidence returns no plan, so
/// an executor cannot accidentally install a cut's registers from a partial dependency closure.
/// The root citation and excluded consumer hash are decoded from the consuming operation itself;
/// callers cannot accidentally ask for a different root or allow the cut to count itself.
pub(in crate::account) fn plan_replay(
    checkpoint: &VerifiedCheckpoint,
    consuming_operation: &[u8],
    manifests: &[Vec<u8>],
    evidence: &[Vec<u8>],
) -> Result<ReplayPlan, PlanError> {
    require(manifests.len() <= MAX_VIEWS, "too many views")?;
    require(evidence.len() <= MAX_ENTRIES, "too many v2 entries")?;
    let bytes = manifests
        .iter()
        .chain(evidence)
        .try_fold(consuming_operation.len(), |n, b| n.checked_add(b.len()));
    require(bytes.is_some_and(|n| n <= MAX_BYTES), "view evidence byte limit")?;
    let pin = checkpoint.pin();
    let (consumer, consumer_op) = decode_candidate(&pin, consuming_operation)?;
    let root = consumer_op.pre_cut_view;
    let mut views = BTreeMap::new();
    let mut references = 0usize;
    for bytes in manifests {
        let view = decode(bytes)?;
        require(view.checkpoint == pin.checkpoint_digest, "view checkpoint mismatch")?;
        references += view.entries.len();
        require(references <= MAX_REFERENCES, "aggregate view reference limit")?;
        require(views.insert(cbor::sha256(bytes), view).is_none(), "duplicate view")?;
    }
    let mut candidates = BTreeMap::new();
    let mut citations = BTreeMap::new();
    for bytes in evidence {
        let (entry, op) = decode_candidate(&pin, bytes)?;
        require(entry.entry_hash != consumer.entry_hash, "pre-cut evidence contains its consumer")?;
        citations.insert(entry.entry_hash, op.pre_cut_view);
        require(candidates.insert(entry.entry_hash, entry).is_none(), "duplicate v2 candidate")?;
    }
    let order = order_views(root, consumer.entry_hash, &views, &citations)?;
    Ok(ReplayPlan { order, manifests: views, candidates })
}

fn decode_candidate(
    pin: &super::super::checkpoint::TrustedCheckpointPin,
    bytes: &[u8],
) -> anyhow::Result<(SignedAccountEntry, ops::ControlOp)> {
    let entry = envelope::decode_account_signed(bytes)?;
    require(
        entry.header.account_id == pin.account_id
            && entry.header.log_id == 0
            && entry.header.op_version == ops::CONTROL_VERSION
            && entry.header.crypto_suite == 0
            && i64::try_from(entry.header.seq).is_ok(),
        "not a v2 control candidate for this account",
    )?;
    let op = ops::decode(entry.header.entry_type, &entry.payload)?;
    require(op.checkpoint == pin.checkpoint_digest, "candidate checkpoint mismatch")?;
    Ok((entry, op))
}

fn order_views(
    root: [u8; 32],
    consumer: AccountEntryHash,
    views: &BTreeMap<[u8; 32], ViewManifest>,
    citations: &BTreeMap<AccountEntryHash, [u8; 32]>,
) -> Result<Vec<[u8; 32]>, PlanError> {
    let mut order = Vec::new();
    let mut active = BTreeSet::new();
    let mut depths: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    let mut stack = vec![(root, false)];
    while let Some((hash, exiting)) = stack.pop() {
        if depths.contains_key(&hash) {
            continue;
        }
        let view = views.get(&hash).ok_or(PlanError::MissingView(hash))?;
        let mut dependencies = BTreeSet::new();
        for entry in &view.entries {
            require(*entry != consumer, "pre-cut view contains its consumer")?;
            let dependency = citations.get(entry).ok_or(PlanError::MissingEntry(*entry))?;
            dependencies.insert(*dependency);
        }
        if exiting {
            let depth = dependencies.iter().map(|dep| depths[dep]).max().unwrap_or(0) + 1;
            require(depth <= MAX_DEPTH, "view dependency depth limit")?;
            active.remove(&hash);
            depths.insert(hash, depth);
            order.push(hash);
        } else {
            require(active.insert(hash), "cyclic view dependencies")?;
            require(active.len() <= MAX_DEPTH, "view dependency depth limit")?;
            stack.push((hash, true));
            for dep in dependencies.into_iter().rev() {
                if !depths.contains_key(&dep) {
                    stack.push((dep, false));
                }
            }
        }
    }
    Ok(order)
}

fn require(condition: bool, message: &str) -> anyhow::Result<()> {
    anyhow::ensure!(condition, "{message}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercise defensive graph checks with synthetic digests: producing an actual cyclic hash
    // commitment would require a hash fixed point, but the scheduler must still reject cycles.
    #[test]
    fn cyclic_dependency_graph_is_rejected_without_recursive_replay() {
        let a = [1; 32];
        let b = [2; 32];
        let entry_a = AccountEntryHash::from_bytes([3; 32]);
        let entry_b = AccountEntryHash::from_bytes([4; 32]);
        let views = BTreeMap::from([
            (a, ViewManifest { checkpoint: [0; 32], entries: vec![entry_a] }),
            (b, ViewManifest { checkpoint: [0; 32], entries: vec![entry_b] }),
        ]);
        let citations = BTreeMap::from([(entry_a, b), (entry_b, a)]);
        assert!(
            matches!(order_views(a, [99; 32].into(), &views, &citations), Err(PlanError::Invalid(error)) if error.to_string().contains("cyclic"))
        );
    }

    #[test]
    fn consumer_is_excluded_even_when_it_appears_in_a_dependency_view() {
        let a = [1; 32];
        let b = [2; 32];
        let entry = AccountEntryHash::from_bytes([3; 32]);
        let consumer = AccountEntryHash::from_bytes([4; 32]);
        let views = BTreeMap::from([
            (a, ViewManifest { checkpoint: [0; 32], entries: vec![entry] }),
            (b, ViewManifest { checkpoint: [0; 32], entries: vec![consumer] }),
        ]);
        let citations = BTreeMap::from([(entry, b)]);
        assert!(
            matches!(order_views(a, consumer, &views, &citations), Err(PlanError::Invalid(error)) if error.to_string().contains("consumer"))
        );
    }
}
