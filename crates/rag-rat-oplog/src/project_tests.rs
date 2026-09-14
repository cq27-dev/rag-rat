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

/// A memory's hash and anchor set are published together by one writer, and the hash is taken
/// from the device whose set won, so two writers' full pairs never split, at any clock offset
/// and on a tie.
#[test]
fn concurrent_full_pairs_never_split_the_anchor_and_hash_registers() {
    let hash = |value: &str| MemoryOp::NodeSourceHash {
        node_id: NodeId::from("mem_1"),
        source_text_hash: value.to_string(),
    };
    for (x, y) in [(5, 5), (5, 6), (6, 5), (5, 9), (9, 5)] {
        let state = project(&[
            at(1, 1, create("mem_1", "t")),
            at(x, 1, hash("hx")),
            at(x + 1, 1, anchors_op("mem_1", &["x"])),
            at(y, 2, hash("hy")),
            at(y + 1, 2, anchors_op("mem_1", &["y"])),
        ]);
        let node = &state.nodes[&NodeId::from("mem_1")];
        let anchors_from_x = node.anchors.as_ref().unwrap()[0].binding_id == "x";
        let hash_from_x = node.source_text_hash.as_deref() == Some("hx");
        assert_eq!(anchors_from_x, hash_from_x, "writers at {x}/{y} split the pair");
    }
}

/// A binary that publishes anchors then hash and one that publishes hash then anchors can meet
/// on one memory — an older device, or offline entries arriving after an upgrade. The hash
/// comes from the device whose set won, so the pair holds whichever order each wrote.
#[test]
fn pairs_written_in_opposite_orders_never_split() {
    let hash = |value: &str| MemoryOp::NodeSourceHash {
        node_id: NodeId::from("mem_1"),
        source_text_hash: value.to_string(),
    };
    for (x, y) in [(10, 10), (10, 11), (11, 10), (5, 9), (9, 5)] {
        let state = project(&[
            at(1, 1, create("mem_1", "t")),
            at(x, 1, anchors_op("mem_1", &["x"])),
            at(x + 1, 1, hash("hx")),
            at(y, 2, hash("hy")),
            at(y + 1, 2, anchors_op("mem_1", &["y"])),
        ]);
        let node = &state.nodes[&NodeId::from("mem_1")];
        let anchors_from_x = node.anchors.as_ref().unwrap()[0].binding_id == "x";
        let hash_from_x = node.source_text_hash.as_deref() == Some("hx");
        assert_eq!(anchors_from_x, hash_from_x, "writers at {x}/{y} split the pair");
    }
}

/// A device's LATEST hash pairs with its set: a device that republished describes its newest
/// set with its newest hash, never the one it published beside an earlier set.
#[test]
fn the_winning_device_s_latest_hash_pairs_with_its_set() {
    let hash = |value: &str| MemoryOp::NodeSourceHash {
        node_id: NodeId::from("mem_1"),
        source_text_hash: value.to_string(),
    };
    let state = project(&[
        at(1, 1, create("mem_1", "t")),
        at(2, 1, hash("first")),
        at(3, 1, anchors_op("mem_1", &["a"])),
        at(4, 1, hash("second")),
        at(5, 1, anchors_op("mem_1", &["b"])),
    ]);
    let node = &state.nodes[&NodeId::from("mem_1")];
    assert_eq!(node.anchors.as_ref().unwrap()[0].binding_id, "b");
    assert_eq!(node.source_text_hash.as_deref(), Some("second"));
}

/// A device's anchor scopes pair with ITS set exactly as its hash does: the winning set's
/// device supplies them, a device that published none supplies an empty map, and a later
/// empty set from the winning device retracts.
#[test]
fn anchor_scopes_pair_with_the_winning_set_s_device() {
    let scopes = |dev: u8, hash: &str| MemoryOp::NodeAnchorScopes {
        node_id: NodeId::from("mem_1"),
        scopes: vec![AnchorScope {
            binding_kind: "symbol".to_string(),
            binding_id: dev.to_string(),
            scope_hash: hash.to_string(),
        }],
    };
    let key = |dev: u8| ("symbol".to_string(), dev.to_string());
    // Device 2 publishes scopes beside its set; device 1 (an older binary) publishes none.
    for (x, y) in [(5, 6), (6, 5), (5, 9), (9, 5)] {
        let state = project(&[
            at(1, 1, create("mem_1", "t")),
            at(x, 1, anchors_op("mem_1", &["1"])),
            at(y, 2, scopes(2, "s2")),
            at(y + 1, 2, anchors_op("mem_1", &["2"])),
        ]);
        let node = &state.nodes[&NodeId::from("mem_1")];
        let set_from_2 = node.anchors.as_ref().unwrap()[0].binding_id == "2";
        if set_from_2 {
            assert_eq!(node.anchor_scopes.get(&key(2)).map(String::as_str), Some("s2"));
        } else {
            assert!(node.anchor_scopes.is_empty(), "writers at {x}/{y}: device 1 has none");
        }
    }
    let retracted = project(&[
        at(1, 2, create("mem_1", "t")),
        at(2, 2, scopes(2, "s2")),
        at(3, 2, anchors_op("mem_1", &["2"])),
        at(4, 2, MemoryOp::NodeAnchorScopes { node_id: NodeId::from("mem_1"), scopes: vec![] }),
        at(5, 2, anchors_op("mem_1", &["2"])),
    ]);
    assert!(retracted.nodes[&NodeId::from("mem_1")].anchor_scopes.is_empty());
}

/// An author publishes its scopes AHEAD of its set on one chain, and a pull can stop between
/// them. The set pairs with the latest scopes its device wrote since its preceding set, so a
/// prefix torn after the next publication's scopes still describes the set it holds — the
/// device's latest would pair set A with B's scopes, and set B arriving next would read as no
/// change at all — and a set the device later publishes without scopes (an older binary on the
/// same device) takes none, not an earlier publication's.
#[test]
fn a_set_pairs_with_the_scopes_published_at_or_before_it_not_the_device_s_latest() {
    let scopes = |hash: &str| MemoryOp::NodeAnchorScopes {
        node_id: NodeId::from("mem_1"),
        scopes: vec![AnchorScope {
            binding_kind: "symbol".to_string(),
            binding_id: "twin".to_string(),
            scope_hash: hash.to_string(),
        }],
    };
    let key = ("symbol".to_string(), "twin".to_string());
    let torn = [
        at(1, 1, create("mem_1", "t")),
        at(2, 1, scopes("alpha")),
        at(3, 1, anchors_op("mem_1", &["twin"])),
        at(4, 1, scopes("beta")),
    ];
    let node = &project(&torn).nodes[&NodeId::from("mem_1")];
    assert_eq!(node.anchor_scopes.get(&key).map(String::as_str), Some("alpha"));

    let mut complete = torn.to_vec();
    complete.push(at(5, 1, anchors_op("mem_1", &["twin"])));
    let node = &project(&complete).nodes[&NodeId::from("mem_1")];
    assert_eq!(node.anchor_scopes.get(&key).map(String::as_str), Some("beta"));

    let mut without_scopes = complete.clone();
    without_scopes.push(at(6, 1, anchors_op("mem_1", &["twin"])));
    let node = &project(&without_scopes).nodes[&NodeId::from("mem_1")];
    assert!(node.anchor_scopes.is_empty(), "a set published without scopes takes none");
}

/// The anchors register remembers WHICH entry won it — the content projection maps that entry
/// to its author account — by the same `(lamport, device)` order as the set itself, the device
/// breaking a tie.
#[test]
fn the_anchors_register_records_its_winning_entry() {
    let state = project(&[
        at(1, 1, create("mem_1", "t")),
        at(4, 2, anchors_op("mem_1", &["b"])),
        at(4, 3, anchors_op("mem_1", &["c"])),
        at(3, 9, anchors_op("mem_1", &["a"])),
    ]);
    let node = &state.nodes[&NodeId::from("mem_1")];
    assert_eq!(node.anchors.as_ref().unwrap()[0].binding_id, "c");
    assert_eq!(node.anchors_meta, Some(OpMeta { lamport: 4, device: device(3) }));

    let unanchored = project(&[at(1, 1, create("mem_2", "t"))]);
    assert_eq!(unanchored.nodes[&NodeId::from("mem_2")].anchors_meta, None);
}

fn anchors_op(id: &str, binding_ids: &[&str]) -> MemoryOp {
    MemoryOp::NodeAnchors {
        node_id: NodeId::from(id),
        anchors: binding_ids.iter().map(|binding_id| anchor(binding_id)).collect(),
    }
}

/// The sets a node's register held before the winner are kept, oldest first and capped, so a
/// consumer can recognise a publication it has already superseded. A republish of identical
/// bytes is a publication too. The first set supersedes nothing.
#[test]
fn superseded_anchor_sets_are_kept_oldest_first_and_capped() {
    let mut entries = vec![at(1, 1, create("mem_1", "t"))];
    for i in 0..(SUPERSEDED_ANCHOR_SETS_KEPT as u64 + 3) {
        entries.push(at(2 + i, 1, anchors_op("mem_1", &[&format!("b{i}")])));
    }
    let state = project(&entries);
    let node = &state.nodes[&NodeId::from("mem_1")];
    assert_eq!(node.anchors.as_ref().unwrap()[0].binding_id, "b34");
    let kept: Vec<&str> =
        node.superseded_anchors.iter().map(|(set, _)| set[0].binding_id.as_str()).collect();
    assert_eq!(
        node.superseded_anchors.last().map(|(_, meta)| meta.lamport),
        Some(2 + SUPERSEDED_ANCHOR_SETS_KEPT as u64 + 1),
        "each set keeps the meta of the entry that published it",
    );
    assert_eq!(kept.len(), SUPERSEDED_ANCHOR_SETS_KEPT, "capped");
    assert_eq!(kept.first(), Some(&"b2"), "the oldest kept set follows the ones dropped");
    assert_eq!(kept.last(), Some(&"b33"), "the newest kept set is the one the winner replaced");

    let first = project(&[at(1, 1, create("mem_2", "t")), at(2, 1, anchors_op("mem_2", &["a"]))]);
    assert!(first.nodes[&NodeId::from("mem_2")].superseded_anchors.is_empty());
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

/// The fold orders the set by identity, so two devices that assembled the same bindings in
/// different orders project byte-identical state. Deleting the sort must fail here.
#[test]
fn the_fold_orders_an_anchor_set_by_identity() {
    let state =
        project(&[at(1, 1, create("mem_1", "t")), at(2, 1, anchors_op("mem_1", &["c", "a", "b"]))]);
    let anchors = state.nodes[&NodeId::from("mem_1")].anchors.as_ref().unwrap();
    assert_eq!(anchors.iter().map(|anchor| anchor.binding_id.as_str()).collect::<Vec<_>>(), vec![
        "a", "b", "c"
    ],);
}

/// `None` and `Some(vec![])` are different facts downstream — nobody has published this
/// memory's bindings, versus its author saying it has none — so the fold must not collapse
/// them.
#[test]
fn an_unanchored_node_is_none_and_an_empty_set_is_some_empty() {
    let untouched = project(&[at(1, 1, create("mem_1", "t"))]);
    assert_eq!(untouched.nodes[&NodeId::from("mem_1")].anchors, None);

    let emptied = project(&[at(1, 1, create("mem_1", "t")), at(2, 1, anchors_op("mem_1", &[]))]);
    assert_eq!(emptied.nodes[&NodeId::from("mem_1")].anchors, Some(Vec::new()));
}

/// Anchors ride the node's EXISTENCE register like content does: a set folded for a node no
/// create ever established projects no row at all, so an orphan snapshot cannot conjure a
/// memory. Order-independence is the same property the other registers have.
#[test]
fn anchors_for_a_node_that_was_never_created_project_no_row() {
    let state = project(&[at(1, 1, anchors_op("mem_ghost", &["a"]))]);
    assert!(state.nodes.is_empty(), "an orphan anchor set establishes nothing");

    // ...and a create landing AFTER the set in the total order does not reset it: existence
    // and anchors are independent registers, so the create establishes the node without
    // touching what the earlier op already published.
    let late_create =
        project(&[at(1, 1, anchors_op("mem_1", &["a"])), at(2, 1, create("mem_1", "t"))]);
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
    assert_eq!(node(&state, "mem_b").status, NodeStatus::Active, "no status op → default active");
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
    assert_eq!(node(&state, "mem_a").status, NodeStatus::Obsolete, "status survives the update");
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
        assert_eq!(project(&shuffled), baseline, "rotation {rotation} must not change the fold");
    }
    // The @9 remove is the newest edge op → absent; content is the @4 update; status the @6 op.
    assert!(!baseline.edges.contains_key(&key));
    assert_eq!(node(&baseline, "mem_a").content.title, "v2");
    assert_eq!(node(&baseline, "mem_a").status, NodeStatus::Obsolete);

    // Idempotent: re-folding a single-create restatement of the converged state is stable, and
    // re-running `project` on the same input never drifts.
    assert_eq!(project(&entries), baseline);
}
