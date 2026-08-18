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
//! - edge **presence** — the last-in-order `EdgeAdd`/`EdgeRemove`; present iff the winner is an
//!   add.
//! - edge **resolved anchor** — the last-in-order `Rebind`; rides along iff the edge is present,
//!   and never affects presence or the key.
//!
//! "Tombstones never resurrect" is EMERGENT, not an absorbing flag: an out-of-order or duplicated
//! older `EdgeAdd` sorts before a newer `EdgeRemove` and loses. `Snapshot` is inert this increment.

use std::collections::BTreeMap;

use super::op::{
    self, EdgeKey, EdgeSpec, Entry, MemoryOp, NodeContent, NodeId, NodeStatus, PortableAnchor,
    ResolvedAnchor,
};

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
}

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
    anchors: Option<Vec<PortableAnchor>>,
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
    anchors.sort_by(|a, b| (&a.binding_kind, &a.binding_id).cmp(&(&b.binding_kind, &b.binding_id)));
    anchors
}

/// Fold entries into the converged [`ProjectedState`]. Pure, deterministic, idempotent.
pub fn project(entries: &[Entry]) -> ProjectedState {
    // One total order for every dimension: `(lamport, device)` ascending, then the canonical op
    // bytes as a final tie-break so a shuffled input — even one carrying a (malformed) duplicate
    // `(lamport, device)` — folds to byte-identical output. Walking this order ascending and
    // overwriting each register makes the highest key win with no explicit comparison.
    let mut ordered: Vec<(&Entry, Vec<u8>)> =
        entries.iter().map(|entry| (entry, op::encode(&entry.op))).collect();
    ordered.sort_by(|(a, a_bytes), (b, b_bytes)| {
        (a.meta.lamport, a.meta.device, a_bytes).cmp(&(b.meta.lamport, b.meta.device, b_bytes))
    });

    let mut nodes: BTreeMap<NodeId, NodeAccum> = BTreeMap::new();
    let mut edges: BTreeMap<EdgeKey, EdgeAccum> = BTreeMap::new();

    // Dimensions are INDEPENDENT: a status op never touches content, an edge op never touches its
    // endpoints' nodes.
    for &(entry, _) in &ordered {
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
            MemoryOp::NodeAnchors { node_id, anchors } => {
                // Full-set replacement, like content — an anchor set is one register, not a
                // per-binding merge, so a later op saying "these two" retires a binding the
                // earlier one named.
                nodes.entry(node_id.clone()).or_default().anchors =
                    Some(canonical_anchors(anchors));
            },
            // Inert boundary marker this increment (§5.4/C4).
            MemoryOp::Snapshot => {},
        }
    }

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

    ProjectedState {
        nodes: nodes
            .into_iter()
            .filter_map(|(id, acc)| {
                // Exists iff a create was seen; existence guarantees a content register.
                let content = acc.exists.then_some(acc.content).flatten()?;
                Some((id, ProjectedNode {
                    content,
                    status: acc.status.unwrap_or_default(),
                    anchors: acc.anchors,
                }))
            })
            .collect(),
        edges: live_edges,
        removed_edges,
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_query::memory::EdgeRelation;

    use super::*;

    fn device(byte: u8) -> super::super::op::DeviceFingerprint {
        super::super::op::DeviceFingerprint::from_bytes([byte; 32])
    }

    /// An entry at Lamport `lamport` from device byte `dev`.
    fn at(lamport: u64, dev: u8, op: MemoryOp) -> Entry {
        Entry { meta: super::super::op::OpMeta { lamport, device: device(dev) }, op }
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

    fn create(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeCreate { node_id: NodeId::from(id), content: content(title) }
    }

    fn update(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeUpdate { node_id: NodeId::from(id), content: content(title) }
    }

    fn status(id: &str, status: NodeStatus) -> MemoryOp {
        MemoryOp::NodeStatus { node_id: NodeId::from(id), status }
    }

    fn anchor(binding_id: &str) -> PortableAnchor {
        PortableAnchor {
            binding_kind: "symbol".to_string(),
            binding_id: binding_id.to_string(),
            path: Some("src/lib.rs".to_string()),
            start_line: Some(1),
            end_line: Some(2),
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
            created_at_ms: 7,
            symbol_kind: None,
            signature_hash: None,
            moniker_tool: None,
            moniker_tool_version: None,
        }
    }

    fn anchors_op(id: &str, binding_ids: &[&str]) -> MemoryOp {
        MemoryOp::NodeAnchors {
            node_id: NodeId::from(id),
            anchors: binding_ids.iter().map(|binding_id| anchor(binding_id)).collect(),
        }
    }

    /// The anchor register is LWW and a FULL-SET replacement, exactly like content: the later op
    /// wins outright, so a binding the earlier one named is retired rather than merged forward.
    #[test]
    fn the_anchor_register_is_last_writer_wins_over_the_whole_set() {
        let state = project(&[
            at(1, 1, create("mem_1", "t")),
            at(2, 1, anchors_op("mem_1", &["a", "b"])),
            at(3, 1, anchors_op("mem_1", &["c"])),
        ]);
        let anchors = state.nodes[&NodeId::from("mem_1")].anchors.as_ref().unwrap();
        assert_eq!(
            anchors.iter().map(|a| a.binding_id.as_str()).collect::<Vec<_>>(),
            vec!["c"],
            "the later set replaces the earlier one whole",
        );
    }

    /// `None` and `Some(vec![])` are different facts downstream — nobody has published this
    /// memory's bindings, versus its author saying it has none — so the fold must not collapse
    /// them.
    #[test]
    fn an_unanchored_node_is_none_and_an_empty_set_is_some_empty() {
        let untouched = project(&[at(1, 1, create("mem_1", "t"))]);
        assert_eq!(untouched.nodes[&NodeId::from("mem_1")].anchors, None);

        let emptied =
            project(&[at(1, 1, create("mem_1", "t")), at(2, 1, anchors_op("mem_1", &[]))]);
        assert_eq!(emptied.nodes[&NodeId::from("mem_1")].anchors, Some(Vec::new()));
    }

    /// Anchors ride the node's EXISTENCE register like content does: a set folded for a node no
    /// create ever established projects no row at all, so an orphan snapshot cannot conjure a
    /// memory. Order-independence is the same property the other registers have.
    #[test]
    fn anchors_for_a_node_that_was_never_created_project_no_row() {
        let state = project(&[at(1, 1, anchors_op("mem_ghost", &["a"]))]);
        assert!(state.nodes.is_empty(), "an orphan anchor set establishes nothing");

        // ...and once the create arrives out of order, the earlier set is still there.
        let late_create =
            project(&[at(2, 1, anchors_op("mem_1", &["a"])), at(1, 1, create("mem_1", "t"))]);
        assert!(late_create.nodes[&NodeId::from("mem_1")].anchors.is_some());
    }

    fn spec(source: &str, target: &str) -> EdgeSpec {
        EdgeSpec {
            source_node_id: NodeId::from(source),
            relation: EdgeRelation::DependsOn,
            target_repo_id: "repo".to_string(),
            target_kind: "node".to_string(),
            target_anchor: target.to_string(),
            owner_repo_id: "repo".to_string(),
        }
    }

    fn node<'a>(state: &'a ProjectedState, id: &str) -> &'a ProjectedNode {
        state.nodes.get(&NodeId::from(id)).unwrap_or_else(|| panic!("node `{id}` should exist"))
    }

    #[test]
    fn folds_content_status_and_edges() {
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let state = project(&[
            at(1, 1, create("mem_a", "first")),
            at(2, 1, status("mem_a", NodeStatus::Stale)),
            at(3, 1, MemoryOp::EdgeAdd { edge: edge.clone() }),
            at(1, 1, create("mem_b", "other")),
        ]);
        assert_eq!(state.nodes.len(), 2);
        assert_eq!(node(&state, "mem_a").content.title, "first");
        assert_eq!(node(&state, "mem_a").status, NodeStatus::Stale);
        assert_eq!(
            node(&state, "mem_b").status,
            NodeStatus::Active,
            "no status op → default active"
        );
        let projected = state.edges.get(&key).expect("the added edge is present");
        assert_eq!(projected.spec, edge);
        assert!(projected.resolved.is_none(), "no rebind → no resolved anchor");
    }

    #[test]
    fn cross_author_content_edits_collapse_under_chain_tail_lamport() {
        // The two-writer hazard (#1164). `/3` currently mints `lamport = per-(stream,author)-chain
        // seq`, but this fold orders every register by `(lamport, device)` STREAM-WIDE. So when two
        // identities write one stream, cross-author ordering tracks relative chain LENGTH, not
        // causal order. A contributor with a long chain creates mem_x at a high lamport; the owner
        // later makes the authoritative CONTENT edit, but its short chain lands the edit at a low
        // lamport — and it is silently lost:
        let (contributor, owner) = (0xCC, 0x00);
        let buggy = project(&[
            at(99, contributor, create("mem_x", "contributor-v1")),
            at(5, owner, update("mem_x", "owner-corrected-later")),
        ]);
        assert_eq!(
            node(&buggy, "mem_x").content.title,
            "contributor-v1",
            "chain-tail lamport loses the owner's causally-later content edit",
        );
        // Stream-global lamport (`max over accepted stream entries + 1`) restores causal LWW — the
        // slice-2 fix: the later edit gets a higher lamport and wins.
        let fixed = project(&[
            at(99, contributor, create("mem_x", "contributor-v1")),
            at(100, owner, update("mem_x", "owner-corrected-later")),
        ]);
        assert_eq!(
            node(&fixed, "mem_x").content.title,
            "owner-corrected-later",
            "stream-global lamport lets the causally-later edit win",
        );
    }

    #[test]
    fn owner_obsolete_of_a_contributor_node_is_safe_regardless_of_lamport() {
        // Refinement of the hazard: status and content are INDEPENDENT dimensions, so the owner's
        // dream obsolete (a NodeStatus) never competes with the contributor's create/update
        // (content). Even far below the contributor's lamport, the sole status op wins — so the
        // common maintenance op (owner obsoletes a contributor node) is correct as-is. The lamport
        // fix is needed for same-node cross-author CONTENT edits and status-vs-status races, not
        // this case.
        let (contributor, owner) = (0xCC, 0x00);
        let state = project(&[
            at(380, contributor, create("mem_y", "from-contributor")),
            at(51, owner, status("mem_y", NodeStatus::Obsolete)),
        ]);
        assert_eq!(
            node(&state, "mem_y").status,
            NodeStatus::Obsolete,
            "the owner's obsolete wins as the sole status op despite a lower lamport",
        );
        assert_eq!(node(&state, "mem_y").content.title, "from-contributor");
    }

    #[test]
    fn content_and_status_are_independent_dimensions() {
        // A status op must not disturb content, and an update must not disturb status.
        let state = project(&[
            at(1, 1, create("mem_a", "v1")),
            at(2, 1, status("mem_a", NodeStatus::Obsolete)),
            at(3, 1, update("mem_a", "v2")),
        ]);
        assert_eq!(node(&state, "mem_a").content.title, "v2", "update wins content");
        assert_eq!(
            node(&state, "mem_a").status,
            NodeStatus::Obsolete,
            "status survives the update"
        );
    }

    #[test]
    fn content_is_last_writer_wins_by_lamport() {
        // The higher `(lamport, device)` wins regardless of input position.
        let state = project(&[
            at(5, 1, update("mem_a", "late")),
            at(1, 1, create("mem_a", "early")),
            at(3, 1, update("mem_a", "middle")),
        ]);
        assert_eq!(node(&state, "mem_a").content.title, "late");
    }

    #[test]
    fn equal_lamport_is_tie_broken_by_device() {
        // Same Lamport, different device: the larger device fingerprint wins the register.
        let low = project(&[
            at(7, 9, update("mem_a", "device_9")),
            at(0, 0, create("mem_a", "seed")),
            at(7, 2, update("mem_a", "device_2")),
        ]);
        assert_eq!(low.nodes[&NodeId::from("mem_a")].content.title, "device_9");
    }

    #[test]
    fn tombstone_never_resurrects_under_reordering() {
        // [EdgeAdd@5, EdgeRemove@10, EdgeAdd@3]: the @3 add is OLDER than the @10 remove, so the
        // edge is absent — the classic out-of-order resurrection the total order defeats.
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let state = project(&[
            at(5, 1, MemoryOp::EdgeAdd { edge: edge.clone() }),
            at(10, 1, MemoryOp::EdgeRemove { edge_key: key.clone() }),
            at(3, 1, MemoryOp::EdgeAdd { edge }),
        ]);
        assert!(!state.edges.contains_key(&key), "the newest op is the remove → edge absent");
    }

    #[test]
    fn a_newer_add_re_adds_a_removed_edge() {
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let state = project(&[
            at(1, 1, MemoryOp::EdgeAdd { edge: edge.clone() }),
            at(2, 1, MemoryOp::EdgeRemove { edge_key: key.clone() }),
            at(3, 1, MemoryOp::EdgeAdd { edge }),
        ]);
        assert!(state.edges.contains_key(&key), "the newest op is the add → edge present");
        assert!(!state.removed_edges.contains_key(&key), "re-added → not a tombstone");
    }

    #[test]
    fn an_added_then_removed_edge_is_a_retained_tombstone() {
        // The tombstone is retained in `removed_edges` (not dropped), so a projection consumer can
        // honor the remove instead of re-authoring it as a ghost (#691).
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let state = project(&[
            at(1, 1, MemoryOp::EdgeAdd { edge }),
            at(2, 1, MemoryOp::EdgeRemove { edge_key: key.clone() }),
        ]);
        assert!(!state.edges.contains_key(&key), "removed → absent from live edges");
        assert!(state.removed_edges.contains_key(&key), "removed → retained as a tombstone");
    }

    #[test]
    fn a_remove_only_or_rebind_only_edge_is_not_a_tombstone() {
        // An edge that was never ADDED has no tombstone — nothing to honor.
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let state = project(&[at(1, 1, MemoryOp::EdgeRemove { edge_key: key.clone() })]);
        assert!(!state.edges.contains_key(&key));
        assert!(!state.removed_edges.contains_key(&key), "never added → no tombstone");
    }

    #[test]
    fn rebind_updates_a_present_edges_resolved_anchor_only() {
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let resolved = ResolvedAnchor {
            target_repo_id: "repo".to_string(),
            target_node_id: Some("mem_b".to_string()),
            anchor_status: "current".to_string(),
        };
        let state = project(&[
            at(1, 1, MemoryOp::EdgeAdd { edge }),
            at(2, 1, MemoryOp::Rebind { edge_key: key.clone(), resolved: resolved.clone() }),
        ]);
        let projected = state.edges.get(&key).expect("edge present");
        assert_eq!(projected.resolved.as_ref(), Some(&resolved));
    }

    #[test]
    fn rebind_of_an_absent_edge_is_dropped() {
        // A rebind never establishes presence; with no surviving add the edge is not projected.
        let key = EdgeKey::from("edgekey_never_added");
        let state = project(&[at(1, 1, MemoryOp::Rebind {
            edge_key: key.clone(),
            resolved: ResolvedAnchor {
                target_repo_id: "repo".to_string(),
                target_node_id: None,
                anchor_status: "unresolved".to_string(),
            },
        })]);
        assert!(state.edges.is_empty());
    }

    #[test]
    fn update_without_a_create_is_inert() {
        // No `NodeCreate` establishes existence → the node never surfaces.
        let state = project(&[at(1, 1, update("mem_ghost", "orphan"))]);
        assert!(state.nodes.is_empty());
    }

    #[test]
    fn project_canonicalizes_stored_tags() {
        // An in-memory op with unsorted + duplicate tags projects with a canonical (sorted,
        // deduped) tag set — matching what the wire encoder would produce, so both fold
        // paths agree.
        let mut unsorted = content("t");
        unsorted.tags = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        let state = project(&[at(1, 1, MemoryOp::NodeCreate {
            node_id: NodeId::from("mem_a"),
            content: unsorted,
        })]);
        assert_eq!(node(&state, "mem_a").content.tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn snapshot_is_inert() {
        let state = project(&[at(1, 1, create("mem_a", "v1")), at(2, 1, MemoryOp::Snapshot)]);
        assert_eq!(state.nodes.len(), 1);
        assert_eq!(node(&state, "mem_a").content.title, "v1");
    }

    #[test]
    fn fold_is_deterministic_under_shuffling_and_idempotent() {
        let edge = spec("mem_a", "mem_b");
        let key = edge.edge_key();
        let entries = vec![
            at(1, 1, create("mem_a", "v1")),
            at(4, 1, update("mem_a", "v2")),
            at(2, 3, status("mem_a", NodeStatus::Stale)),
            at(6, 2, status("mem_a", NodeStatus::Obsolete)),
            at(1, 2, create("mem_b", "b")),
            at(3, 1, MemoryOp::EdgeAdd { edge: edge.clone() }),
            at(9, 1, MemoryOp::EdgeRemove { edge_key: key.clone() }),
            at(5, 2, MemoryOp::EdgeAdd { edge }),
        ];
        let baseline = project(&entries);

        // Every rotation of the input yields byte-identical output (the fold sorts internally).
        for rotation in 0..entries.len() {
            let mut shuffled = entries.clone();
            shuffled.rotate_left(rotation);
            assert_eq!(
                project(&shuffled),
                baseline,
                "rotation {rotation} must not change the fold"
            );
        }
        // The @9 remove is the newest edge op → absent; content is the @4 update; status the @6 op.
        assert!(!baseline.edges.contains_key(&key));
        assert_eq!(node(&baseline, "mem_a").content.title, "v2");
        assert_eq!(node(&baseline, "mem_a").status, NodeStatus::Obsolete);

        // Idempotent: re-folding a single-create restatement of the converged state is stable, and
        // re-running `project` on the same input never drifts.
        assert_eq!(project(&entries), baseline);
    }
}
