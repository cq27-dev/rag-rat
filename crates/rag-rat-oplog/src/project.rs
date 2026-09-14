//! The deterministic projection fold (phase B op-log, §5.4).
//!
//! [`project`] folds a slice of [`Entry`] into a [`ProjectedState`] — the converged node/edge view
//! — as a set of **last-writer-wins registers over one total order** `(lamport, device)`. It is
//! pure, deterministic, order-independent (it sorts internally), and idempotent (re-folding its own
//! inputs is stable). No IO, no clock, no crypto.
//!
//! Each dimension is an INDEPENDENT register keyed by node id / edge key:
//! - node **existence** — established by any `NodeCreate`, never revoked (a "deletion" is a status
//!   flip to `obsolete`, so a tombstoned node is still a projected row).
//! - node **content** — the last-in-order `NodeCreate`/`NodeUpdate` (full replacement).
//! - node **status** — the last-in-order `NodeStatus`; default `active`.
//! - node **anchors** — the last-in-order `NodeAnchors`, a FULL-SET replacement; `None` until one
//!   is folded, which is distinct from an empty set (nobody has said, versus said "no bindings").
//! - node **source hash** — the text an author anchored to, so a receiver can tell its own checkout
//!   has drifted from what that author meant. Not an independent register: it is the latest
//!   `NodeSourceHash` from the device that wrote the winning anchor set, `None` when that device
//!   published none. A device publishes its hash beside its anchors, so its latest hash describes
//!   its latest set — whichever order its binary wrote the pair in — while two independent
//!   registers split pairs written in opposite orders: `anchors A @10, hash A @11` from one device
//!   and `hash B @10, anchors B @11` from another would settle on anchors B beside hash A.
//! - node **anchor scopes** — the scope of each symbol anchor's target, paired with the anchor set
//!   by device like the source hash, but positionally: the latest `NodeAnchorScopes` the winning
//!   set's device wrote SINCE ITS PRECEDING SET and at or before this one, empty when it wrote none
//!   there. An author publishes its scopes ahead of its set, so a pull torn after a later scopes op
//!   (`scopes A, anchors A, scopes B`) still pairs set A with scopes A — the device's latest would
//!   pair it with B's, and the set B that follows would then read as no change — and a set the
//!   device published without scopes takes none rather than an earlier publication's.
//! - edge **presence** — the last-in-order `EdgeAdd`/`EdgeRemove`; present iff the winner is an
//!   add.
//! - edge **resolved anchor** — the last-in-order `Rebind`; rides along iff the edge is present,
//!   and never affects presence or the key.
//!
//! "Tombstones never resurrect" is EMERGENT, not an absorbing flag: an out-of-order or duplicated
//! older `EdgeAdd` sorts before a newer `EdgeRemove` and loses. `Snapshot` is inert this increment.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use super::op::{
    self, AnchorScope, EdgeKey, EdgeSpec, Entry, MemoryOp, NodeContent, NodeId, NodeStatus, OpMeta,
    PortableAnchor, ResolvedAnchor,
};

/// A node's anchor scopes, keyed by the anchor identity `(binding_kind, binding_id)` each
/// describes: the value is the target's scope hash (see [`AnchorScope`]).
pub type AnchorScopes = BTreeMap<(String, String), String>;

/// The converged projection: existing nodes (content + status) and present edges (spec + resolved
/// anchor), each keyed for a stable, sorted, byte-reproducible ordering.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProjectedState {
    pub nodes: BTreeMap<NodeId, ProjectedNode>,
    pub edges: BTreeMap<EdgeKey, ProjectedEdge>,
    /// Edges that were ADDED and then REMOVED — the tombstones. Retained (not dropped like the
    /// fold used to) so a projection consumer honors the remove instead of re-authoring the
    /// edge as a ghost: the reconcile treats a tombstoned edge as authored (never re-adds it),
    /// and the memory projection deletes the corresponding read-table row. An edge that was
    /// never added (a remove-only or rebind-only key) is absent from BOTH maps. The
    /// spec/resolved carried here are the last-known values, kept only so the projection row's
    /// non-null columns can be written.
    pub removed_edges: BTreeMap<EdgeKey, ProjectedEdge>,
}

/// A projected node: its winning content and status. Presence in `ProjectedState::nodes` IS its
/// existence.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedNode {
    pub content: NodeContent,
    pub status: NodeStatus,
    /// The node's portable anchor set, or `None` when no `NodeAnchors` op has been folded. The
    /// distinction is load-bearing downstream: `None` means nobody has published this memory's
    /// bindings, while `Some(vec![])` means its author said it has none.
    pub anchors: Option<Vec<PortableAnchor>>,
    /// The hash of the source text the author anchored to, or `None` when none was published.
    /// `None` surfaces UNMARKED downstream — an absent hash is not evidence of drift.
    pub source_text_hash: Option<String>,
    /// The scope of each symbol anchor's target in the winning set, from the device that wrote it;
    /// empty when it published none. Absence is not evidence — a drain compares scopes only where
    /// both the previous and the new set carry one.
    pub anchor_scopes: AnchorScopes,
    /// The `(lamport, device)` of the entry whose `NodeAnchors` won the anchors register, or
    /// `None` with no set folded. The content projection maps it to that entry's author
    /// account, which is how the drain tells a set its own account's `anchors/1` already
    /// carries from one only the snapshot can deliver.
    pub anchors_meta: Option<OpMeta>,
    /// The anchor sets this node's register held before the winning one, each with the
    /// `(lamport, device)` of the entry that published it, oldest first, at most
    /// [`SUPERSEDED_ANCHOR_SETS_KEPT`]. A device that receives a memory's binding rows from a
    /// sibling before it holds the memory reads them against these: rows that are the image of a
    /// superseded publication another account made are known to be stale and are converged on the
    /// winner; rows matching no such publication may be one still in flight, and are left alone.
    /// The meta names the publication's author, which the content projection resolves.
    pub superseded_anchors: Vec<(Vec<PortableAnchor>, OpMeta)>,
}

/// How many superseded anchor sets a node keeps (see [`ProjectedNode::superseded_anchors`]): enough
/// to recognise the image any live sibling can still hold, bounded so a memory rebound daily for
/// years does not grow its projection row without limit.
pub const SUPERSEDED_ANCHOR_SETS_KEPT: usize = 32;

/// A projected edge: its winning spec (from the last add) and its last resolved anchor, if any.
/// Presence in `ProjectedState::edges` IS its presence.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedEdge {
    pub spec: EdgeSpec,
    pub resolved: Option<ResolvedAnchor>,
}

/// Per-node LWW accumulators, resolved into a [`ProjectedNode`] only if the node exists.
#[derive(Default)]
struct NodeAccum {
    exists: bool,
    content: Option<NodeContent>,
    status: Option<NodeStatus>,
    anchors: Option<(Vec<PortableAnchor>, OpMeta)>,
    /// The sets the register held before the current one, with their publishing entry's meta,
    /// oldest first, capped.
    superseded_anchors: Vec<(Vec<PortableAnchor>, OpMeta)>,
    /// Each device's latest `NodeSourceHash`, to pair with that device's anchor set.
    source_text_hash_by_device: BTreeMap<op::DeviceFingerprint, String>,
    /// Each device's `NodeAnchorScopes` by Lamport, so a set pairs with the latest scopes its
    /// device wrote within its own publication (see the module docs).
    anchor_scopes_by_device: BTreeMap<op::DeviceFingerprint, BTreeMap<u64, AnchorScopes>>,
    /// The Lamport of every `NodeAnchors` each device wrote — the publication boundaries the
    /// scopes lookup is bounded by.
    anchor_set_lamports_by_device: BTreeMap<op::DeviceFingerprint, BTreeSet<u64>>,
}

/// Per-edge LWW accumulators, resolved into a [`ProjectedEdge`] only if the edge is present.
#[derive(Default)]
struct EdgeAccum {
    present: bool,
    spec: Option<EdgeSpec>,
    resolved: Option<ResolvedAnchor>,
}

/// Clone `content` with its tag set canonicalized (sorted + deduped), so the projected state is
/// deterministic regardless of an op's in-memory tag order — the wire encoder canonicalizes
/// identically, so a directly-folded op and the same op round-tripped through the wire agree.
fn canonical_content(content: &NodeContent) -> NodeContent {
    let mut content = content.clone();
    content.canonicalize();
    content
}

/// Clone the anchor set in the SAME identity order the wire encoder writes, so a directly-folded op
/// and one round-tripped through the wire project identically. Duplicates are left alone: `decode`
/// refuses them and `within_wire_limits` keeps them from being authored, so a set reaching the fold
/// with two anchors for one binding was built in-process and is a caller bug, not a state to
/// silently repair here.
fn canonical_anchors(anchors: &[PortableAnchor]) -> Vec<PortableAnchor> {
    let mut anchors = anchors.to_vec();
    // Delegates to the op type's own identity, the way `canonical_content` delegates to
    // `NodeContent::canonicalize`: re-spelling the key here would let the fold and the wire encoder
    // drift apart silently.
    anchors.sort_by(|a, b| a.identity().cmp(&b.identity()));
    anchors
}

/// Fold entries into the converged [`ProjectedState`]. Pure, deterministic, idempotent.
pub fn project(entries: &[Entry]) -> ProjectedState {
    let ordered = in_total_order(entries);

    let mut nodes: BTreeMap<NodeId, NodeAccum> = BTreeMap::new();
    let mut edges: BTreeMap<EdgeKey, EdgeAccum> = BTreeMap::new();

    // Dimensions are INDEPENDENT: a status op never touches content, an edge op never touches its
    // endpoints' nodes.
    for entry in ordered {
        match &entry.op {
            MemoryOp::NodeCreate { node_id, content } => {
                let node = nodes.entry(node_id.clone()).or_default();
                node.exists = true; // established by a create, never revoked
                node.content = Some(canonical_content(content));
            },
            MemoryOp::NodeUpdate { node_id, content } => {
                // Full content replacement. A node only SURFACES once created, so an update with no
                // create anywhere in the log leaves the register set but the node absent (filtered
                // out below).
                nodes.entry(node_id.clone()).or_default().content =
                    Some(canonical_content(content));
            },
            MemoryOp::NodeStatus { node_id, status } => {
                nodes.entry(node_id.clone()).or_default().status = Some(*status);
            },
            MemoryOp::EdgeAdd { edge } => {
                let acc = edges.entry(edge.edge_key()).or_default();
                acc.present = true;
                acc.spec = Some(edge.clone());
            },
            MemoryOp::EdgeRemove { edge_key } => {
                // Tombstone. "Never resurrect" is emergent: an older add sorts before this remove
                // and loses; only a NEWER add re-adds the edge.
                edges.entry(edge_key.clone()).or_default().present = false;
            },
            MemoryOp::Rebind { edge_key, resolved } => {
                // Re-resolves the local anchor only — never presence, never the key.
                edges.entry(edge_key.clone()).or_default().resolved = Some(resolved.clone());
            },
            MemoryOp::NodeSourceHash { node_id, source_text_hash } => {
                nodes
                    .entry(node_id.clone())
                    .or_default()
                    .source_text_hash_by_device
                    .insert(entry.meta.device, source_text_hash.clone());
            },
            MemoryOp::NodeAnchors { node_id, anchors } => {
                // Full-set replacement, like content — an anchor set is one register, not a
                // per-binding merge, so a later op saying "these two" retires a binding the
                // earlier one named.
                let node = nodes.entry(node_id.clone()).or_default();
                if let Some(superseded) = node.anchors.take() {
                    node.superseded_anchors.push(superseded);
                    if node.superseded_anchors.len() > SUPERSEDED_ANCHOR_SETS_KEPT {
                        node.superseded_anchors.remove(0);
                    }
                }
                node.anchors = Some((canonical_anchors(anchors), entry.meta));
                node.anchor_set_lamports_by_device
                    .entry(entry.meta.device)
                    .or_default()
                    .insert(entry.meta.lamport);
            },
            MemoryOp::NodeAnchorScopes { node_id, scopes } => {
                // A full set per publication: the set that follows on the same chain takes the
                // latest one at or before it, and an empty set retracts.
                let scopes: AnchorScopes = scopes
                    .iter()
                    .map(|scope| {
                        let AnchorScope { binding_kind, binding_id, scope_hash } = scope.clone();
                        ((binding_kind, binding_id), scope_hash)
                    })
                    .collect();
                nodes
                    .entry(node_id.clone())
                    .or_default()
                    .anchor_scopes_by_device
                    .entry(entry.meta.device)
                    .or_default()
                    .insert(entry.meta.lamport, scopes);
            },
            // Inert boundary marker this increment (§5.4/C4).
            MemoryOp::Snapshot => {},
        }
    }

    let (live_edges, removed_edges) = split_edges(edges);

    ProjectedState {
        nodes: nodes.into_iter().filter_map(|(id, acc)| finish_node(id, acc)).collect(),
        edges: live_edges,
        removed_edges,
    }
}

fn in_total_order(entries: &[Entry]) -> Vec<&Entry> {
    // One total order for every dimension: `(lamport, device)` ascending, then the canonical op
    // bytes as a final tie-break so a shuffled input — even one carrying a (malformed) duplicate
    // `(lamport, device)` — folds to byte-identical output. Walking this order ascending and
    // overwriting each register makes the highest key win with no explicit comparison.
    let mut ordered: Vec<(&Entry, Vec<u8>)> =
        entries.iter().map(|entry| (entry, op::encode(&entry.op))).collect();
    ordered.sort_by(|(a, a_bytes), (b, b_bytes)| {
        (a.meta.lamport, a.meta.device, a_bytes).cmp(&(b.meta.lamport, b.meta.device, b_bytes))
    });

    ordered.into_iter().map(|(entry, _)| entry).collect()
}

fn split_edges(
    edges: BTreeMap<EdgeKey, EdgeAccum>,
) -> (BTreeMap<EdgeKey, ProjectedEdge>, BTreeMap<EdgeKey, ProjectedEdge>) {
    // Split edges into live and tombstoned. `spec` is set only by an `EdgeAdd`, so `spec.is_some()`
    // means the edge was added at some point: `(present, added)` → live, `(removed, added)` →
    // tombstone, `(_, never-added)` → no row at all.
    let mut live_edges = BTreeMap::new();
    let mut removed_edges = BTreeMap::new();
    for (key, acc) in edges {
        match acc.spec {
            Some(spec) if acc.present => {
                live_edges.insert(key, ProjectedEdge { spec, resolved: acc.resolved });
            },
            Some(spec) => {
                removed_edges.insert(key, ProjectedEdge { spec, resolved: acc.resolved });
            },
            None => {},
        }
    }

    (live_edges, removed_edges)
}

fn finish_node(id: NodeId, acc: NodeAccum) -> Option<(NodeId, ProjectedNode)> {
    // Exists iff a create was seen; existence guarantees a content register.
    let content = acc.exists.then_some(acc.content).flatten()?;
    let (anchors, anchors_meta) = match acc.anchors {
        Some((anchors, meta)) => (Some(anchors), Some(meta)),
        None => (None, None),
    };
    let source_text_hash =
        anchors_meta.and_then(|meta| acc.source_text_hash_by_device.get(&meta.device).cloned());
    let anchor_scopes = anchors_meta
        .and_then(|meta| {
            let by_lamport = acc.anchor_scopes_by_device.get(&meta.device)?;
            // This publication's companion only: reaching back past the device's
            // preceding set would attach an earlier publication's scopes to a set
            // published without any.
            let preceding = acc
                .anchor_set_lamports_by_device
                .get(&meta.device)
                .and_then(|sets| sets.range(..meta.lamport).next_back().copied());
            let from = preceding.map_or(Bound::Unbounded, Bound::Excluded);
            by_lamport
                .range((from, Bound::Included(meta.lamport)))
                .next_back()
                .map(|(_, scopes)| scopes.clone())
        })
        .unwrap_or_default();
    Some((id, ProjectedNode {
        content,
        status: acc.status.unwrap_or_default(),
        anchors,
        source_text_hash,
        anchor_scopes,
        anchors_meta,
        superseded_anchors: acc.superseded_anchors,
    }))
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
