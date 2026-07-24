use super::*;

/// #463: a node created with NO binding target is UNANCHORED — a graph node (a `Concept` /
/// standalone `Task`) with no code anchor. It surfaces in the general `memory list` with blank
/// binding columns, dedupes against another unanchored node of the same text, is excluded by a
/// binding-kind filter, and is never flagged by `memory_validate` (no anchor to go stale/gone).
#[test]
fn unanchored_node_is_created_listed_and_deduped() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let make = || rag_rat_query::memory::RepoMemoryCreate {
        // Only Task/Concept may be created UNANCHORED (#465); other kinds must anchor to code.
        kind: "Concept".to_string(),
        title: "Prefer the event log over polling".to_string(),
        body: "A cross-cutting concept not anchored to any one symbol.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(), // empty → unanchored
    };

    let created = db.memory_create(make()).unwrap();
    assert!(!created.duplicate);
    assert!(created.memory.bindings.is_empty(), "an unanchored node has zero bindings");

    let conn = db.storage.connection();

    // Surfaces in the general list with blank binding columns (LEFT JOIN).
    let all = rag_rat_query::memory::list_memories(conn, None).unwrap();
    let summary = all
        .iter()
        .find(|s| s.memory_id == created.memory.memory_id)
        .expect("unanchored node must appear in `memory list`");
    assert_eq!(summary.kind, "Concept");
    assert_eq!(summary.binding_kind, "");
    assert_eq!(summary.binding_id, "");

    // A binding-kind filter excludes it (it has no binding kind).
    assert!(
        rag_rat_query::memory::list_memories(conn, Some("path")).unwrap().is_empty(),
        "an unanchored node must not surface under a binding-kind filter"
    );

    // A second unanchored node with identical text dedupes to the same id.
    let again = db.memory_create(make()).unwrap();
    assert!(again.duplicate, "a second unanchored node with the same text dedupes");
    assert_eq!(again.memory.memory_id, created.memory.memory_id);

    // Validation never flags an unanchored node (no anchor to go stale/gone).
    assert_eq!(db.memory_validate().unwrap().stale, 0);

    let _ = fs::remove_dir_all(&root);
}

/// #463: `memory_rebind` still REQUIRES an anchor — moving a memory to "no binding" is meaningless.
/// Only `memory_create` accepts the unanchored case.
#[test]
fn rebind_still_requires_a_binding_target() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: "an unanchored node".to_string(),
            body: "created without a binding".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap();

    let err = db
        .memory_rebind(
            &created.memory.memory_id,
            rag_rat_query::memory::RepoMemoryBindTarget::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("requires a binding target"),
        "rebind must reject an empty bind target: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #463 guard: a PARTIALLY populated bind (an intended anchor missing a field) must ERROR, not
/// silently become an unanchored node — otherwise a typo'd anchor yields an invisible memory that
/// no `memory_for_*` lookup surfaces and validation never checks.
#[test]
fn a_partial_binding_is_rejected_not_silently_unanchored() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // tracker+project but NO item_key — an incomplete anchor, not an unanchored node.
    let err = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "partial tracker anchor".to_string(),
            body: "tracker+project without an item_key".to_string(),
            confidence: "low".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                tracker: Some("github".to_string()),
                project: Some("o/r".to_string()),
                ..Default::default()
            },
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("binding is incomplete"),
        "a partial binding must be rejected, got: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #465: a polymorphic node (a `Task`/`Concept` kind) stores and round-trips an opaque JSON payload
/// through create → read → update. A non-object payload is rejected.
#[test]
fn a_polymorphic_node_stores_and_round_trips_its_payload() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A `Task` node (a new #465 kind), unanchored, carrying a structured payload.
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Task".to_string(),
            title: "Wire the payload column".to_string(),
            body: "Track the polymorphic payload work.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some(r#"{"estimate":"1d","priority":2}"#.to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap();
    assert!(!created.duplicate);
    assert_eq!(created.memory.kind, "Task");
    assert!(created.memory.bindings.is_empty(), "a Task node is unanchored");
    assert_eq!(
        created.memory.payload_json.as_deref(),
        Some(r#"{"estimate":"1d","priority":2}"#),
        "the payload round-trips verbatim on create"
    );

    // Read back independently.
    let fetched = db.memory_get(&created.memory.memory_id).unwrap().expect("memory");
    assert_eq!(fetched.payload_json.as_deref(), Some(r#"{"estimate":"1d","priority":2}"#));

    // Update just the payload; `None` on the other fields leaves them unchanged.
    let updated = db
        .memory_update(rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: created.memory.memory_id.clone(),
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: None,
            payload_json: Some(r#"{"priority":1}"#.to_string()),
        })
        .unwrap();
    assert_eq!(updated.payload_json.as_deref(), Some(r#"{"priority":1}"#));
    assert_eq!(updated.title, "Wire the payload column", "other fields unchanged");

    // A non-object payload (an array) is rejected.
    let err = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: "bad payload".to_string(),
            body: "an array is not a valid payload".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some("[1,2,3]".to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("must be a JSON object"),
        "a non-object payload must be rejected, got: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #465: dedup folds the payload — two polymorphic nodes with identical text but DIFFERENT payloads
/// are distinct (neither silently collapses onto the other, dropping its payload); identical text
/// AND payload dedups.
#[test]
fn payload_bearing_nodes_dedupe_on_payload_not_just_text() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let make = |payload: &str| rag_rat_query::memory::RepoMemoryCreate {
        kind: "Task".to_string(),
        title: "same title".to_string(),
        body: "same body".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: Some(payload.to_string()),
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    };

    let a = db.memory_create(make(r#"{"priority":1}"#)).unwrap();
    assert!(!a.duplicate);
    // Same text, DIFFERENT payload → a distinct node, not a duplicate.
    let b = db.memory_create(make(r#"{"priority":2}"#)).unwrap();
    assert!(!b.duplicate, "a different payload must not dedup onto the first node");
    assert_ne!(a.memory.memory_id, b.memory.memory_id);
    // Same text AND payload → a duplicate.
    let c = db.memory_create(make(r#"{"priority":1}"#)).unwrap();
    assert!(c.duplicate, "identical text and payload dedups");
    assert_eq!(c.memory.memory_id, a.memory.memory_id);

    let _ = fs::remove_dir_all(&root);
}

/// #465 (PR #471 review): dedup folds KIND, and the unanchored-create gate is kind-aware. Two
/// distinct graph-node kinds sharing text+payload are NOT duplicates; and only Task/Concept may be
/// created unanchored — an unanchored Decision is rejected (which keeps `create` in lock-step with
/// the dream verifier's kind exemption).
#[test]
fn dedup_and_unanchored_create_are_kind_aware() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let unanchored = |kind: &str| rag_rat_query::memory::RepoMemoryCreate {
        kind: kind.to_string(),
        title: "same text".to_string(),
        body: "same body".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: Some(r#"{"p":1}"#.to_string()),
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    };

    // A Concept and a Task with identical text+payload are DISTINCT (dedup folds kind).
    let concept = db.memory_create(unanchored("Concept")).unwrap();
    assert!(!concept.duplicate);
    let task = db.memory_create(unanchored("Task")).unwrap();
    assert!(!task.duplicate, "a different kind is not a duplicate");
    assert_ne!(concept.memory.memory_id, task.memory.memory_id);
    // Re-creating the same kind+text+payload dedups.
    let again = db.memory_create(unanchored("Concept")).unwrap();
    assert!(again.duplicate, "identical kind+text+payload dedups");
    assert_eq!(again.memory.memory_id, concept.memory.memory_id);

    // A non-Task/Concept kind cannot be created UNANCHORED (no payload here, so the anchor gate is
    // what fires).
    let err = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "unanchored decision".to_string(),
            body: "b".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("must anchor to code"),
        "an unanchored Decision must be rejected, got: {err}"
    );
    // A payload is rejected on a non-polymorphic kind, even when ANCHORED to code.
    let perr = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "invariant with payload".to_string(),
            body: "b".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some(r#"{"p":1}"#.to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/lib.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap_err();
    assert!(
        perr.to_string().contains("only Task/Concept may have a payload"),
        "a payload on a non-polymorphic kind must be rejected, got: {perr}"
    );

    // The invariant holds on UPDATE too: a zero-binding node cannot be retyped to a non-graph kind,
    // but retyping between graph-node kinds (Task -> Concept) is fine.
    let retype = |kind: &str| rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: task.memory.memory_id.clone(),
        kind: Some(kind.to_string()),
        title: None,
        body: None,
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    };
    let bad = db.memory_update(retype("Decision")).unwrap_err();
    assert!(
        bad.to_string().contains("only Task/Concept may be unanchored"),
        "retyping an unanchored node to Decision must be rejected, got: {bad}"
    );
    assert_eq!(
        db.memory_update(retype("Concept")).unwrap().kind,
        "Concept",
        "retyping between graph-node kinds is allowed"
    );

    // Retyping an ANCHORED Task (carrying a payload) to a non-polymorphic kind is allowed (it has a
    // binding) and CLEARS the stranded payload rather than preserving it.
    let anchored = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Task".to_string(),
            title: "anchored task".to_string(),
            body: "b".to_string(),
            confidence: "medium".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some(r#"{"p":9}"#.to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/lib.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    assert_eq!(anchored.memory.payload_json.as_deref(), Some(r#"{"p":9}"#));
    let retyped = db
        .memory_update(rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: anchored.memory.memory_id.clone(),
            kind: Some("Decision".to_string()),
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: None,
            payload_json: None,
        })
        .unwrap();
    assert_eq!(retyped.kind, "Decision");
    assert!(retyped.payload_json.is_none(), "retyping away from Task/Concept clears the payload");

    // A LEGACY zero-binding non-graph memory (a pre-gate Decision, seeded directly under the active
    // repo) stays CLEANABLE: a status-only update (mark_obsolete) does not change the kind, so the
    // gate does not trap it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id)
             SELECT 'mem_legacy', 'Decision', 'legacy orphan', 'b', 'low', 'active', 'agent', 0, \
             0, 'agent', 'v1', repo_id FROM repo_memories WHERE id = ?1",
            [&concept.memory.memory_id],
        )
        .unwrap();
    assert_eq!(
        db.memory_mark_obsolete("mem_legacy").unwrap().status,
        "obsolete",
        "a legacy unanchored non-graph memory can still be cleaned up"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #464: typed edges — a task `depends_on` another (forward `edges_from` + reverse `edges_into`), a
/// task `tracks` a github issue, `edge_key` is stable/idempotent, `remove` works, self-loops are
/// rejected, and an edge into an ABSENT repo is stored `unresolved` (not an error).
#[test]
fn typed_edges_add_traverse_and_resolve() {
    use rag_rat_query::memory::EdgeTarget;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let task = |title: &str| rag_rat_query::memory::RepoMemoryCreate {
        kind: "Task".to_string(),
        title: title.to_string(),
        body: "b".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("t".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    };
    let a = db.memory_create(task("task A")).unwrap().memory.memory_id;
    let b = db.memory_create(task("task B")).unwrap().memory.memory_id;
    let node = |id: &str| EdgeTarget::Node { repo_id: None, node_id: id.to_string() };

    // A depends_on B (same repo).
    let edge = db.memory_edge_add(&a, "depends_on", node(&b)).unwrap();
    assert_eq!(edge.relation, "depends_on");
    assert_eq!(edge.target_node_id.as_deref(), Some(b.as_str()));
    assert_eq!(edge.anchor_status, "current");

    // Forward: edges_from(A) sees it. Reverse: edges_into(B) is the reverse traversal.
    let from = db.memory_edges_from(&a).unwrap();
    assert_eq!(from.len(), 1);
    assert_eq!(from[0].target_node_id.as_deref(), Some(b.as_str()));
    let into = db.memory_edges_into(node(&b)).unwrap();
    assert_eq!(into.len(), 1);
    assert_eq!(into[0].source_node_id, a);

    // Idempotent: re-adding the same logical edge keeps the SAME edge_key (no duplicate row).
    let again = db.memory_edge_add(&a, "depends_on", node(&b)).unwrap();
    assert_eq!(again.edge_key, edge.edge_key);
    assert_eq!(db.memory_edges_from(&a).unwrap().len(), 1);

    // A tracks a github issue — reverse-bindable "issue <- task".
    let gh = || EdgeTarget::Github { owner: "o".to_string(), repo: "r".to_string(), number: 42 };
    db.memory_edge_add(&a, "tracks", gh()).unwrap();
    let tracking = db.memory_edges_into(gh()).unwrap();
    assert_eq!(tracking.len(), 1);
    assert_eq!(tracking[0].source_node_id, a);
    assert_eq!(tracking[0].relation, "tracks");

    // A cross-repo edge into an ABSENT repo is stored `unresolved`, never a hard failure.
    let cross = db
        .memory_edge_add(&a, "relates_to", EdgeTarget::Node {
            repo_id: Some("some-other-repo".to_string()),
            node_id: "mem_absent".to_string(),
        })
        .unwrap();
    assert_eq!(cross.anchor_status, "unresolved");
    assert!(cross.target_node_id.is_none());

    // Re-resolution on READ: once the previously-absent target is indexed, a later read shows the
    // edge `current` with its target self-healed (the stored `unresolved` was only an add-time
    // snapshot). Seed the target node directly under its sibling repo (id copied from `a`, repo
    // overridden) so the id lookup finds it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, input_hash, memory_version, repo_id) SELECT \
             'mem_absent', kind, title, body, confidence, status, created_by, created_at_ms, \
             updated_at_ms, source, 'reresolve-hash', memory_version, 'some-other-repo' FROM \
             repo_memories WHERE id = ?1",
            [&a],
        )
        .unwrap();
    let healed = db.memory_edges_from(&a).unwrap();
    let cross_now = healed.iter().find(|e| e.edge_key == cross.edge_key).unwrap();
    assert_eq!(
        cross_now.anchor_status, "current",
        "an unresolved edge re-resolves once its target is indexed"
    );
    assert_eq!(cross_now.target_node_id.as_deref(), Some("mem_absent"));
    assert_eq!(
        cross_now.target_repo_id, "some-other-repo",
        "target_repo_id self-heals from the node"
    );

    // An IMPLICIT cross-repo target (no explicit repo_id) that resolves to a SIBLING repo is
    // rejected — `mem_absent` now lives in `some-other-repo`, so a bare `node()` edge to it must be
    // made explicit. (The `relates_to` edge above was allowed only because it named the repo.)
    let implicit = db.memory_edge_add(&a, "depends_on", node("mem_absent")).unwrap_err();
    assert!(implicit.to_string().contains("is not a node in this repo"), "{implicit}");

    // EXPLICIT cross-repo whose id resolves to a DIFFERENT repo than named → rejected (`mem_absent`
    // lives in `some-other-repo`, not the named `wrong-repo`).
    let mismatch = db
        .memory_edge_add(&a, "depends_on", rag_rat_query::memory::EdgeTarget::Node {
            repo_id: Some("wrong-repo".to_string()),
            node_id: "mem_absent".to_string(),
        })
        .unwrap_err();
    assert!(mismatch.to_string().contains("not the named `wrong-repo`"), "{mismatch}");

    // EXPLICIT cross-repo into a REGISTERED repo but the node is absent → a typo, rejected. (An
    // UNREGISTERED repo is instead a legitimate deferred `unresolved` reference — the `relates_to`
    // edge above.)
    db.storage
        .connection()
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
             ('some-other-repo', 'other', 0)",
            [],
        )
        .unwrap();
    let typo = db
        .memory_edge_add(&a, "depends_on", rag_rat_query::memory::EdgeTarget::Node {
            repo_id: Some("some-other-repo".to_string()),
            node_id: "mem_ghost".to_string(),
        })
        .unwrap_err();
    assert!(typo.to_string().contains("is not a node in repo `some-other-repo`"), "{typo}");

    // A self-loop is rejected.
    let err = db.memory_edge_add(&a, "relates_to", node(&a)).unwrap_err();
    assert!(err.to_string().contains("cannot point a node at itself"), "{err}");

    // A SAME-repo target that doesn't exist is a typo → rejected (a cross-repo absent target is
    // fine, as the `relates_to` edge above stored `unresolved`).
    let missing = db.memory_edge_add(&a, "depends_on", node("mem_nonexistent")).unwrap_err();
    assert!(missing.to_string().contains("is not a node in this repo"), "{missing}");

    // Remove by edge_key.
    assert!(db.memory_edge_remove(&edge.edge_key).unwrap());
    assert!(db.memory_edges_from(&a).unwrap().iter().all(|e| e.edge_key != edge.edge_key));

    // An obsoleted SOURCE node's edges drop out of both traversals — a hidden node's relationships
    // are dead. `a` still owns the `tracks` and (now-resolved) `relates_to` edges here.
    assert!(
        !db.memory_edges_from(&a).unwrap().is_empty(),
        "sanity: a has live edges before obsolete"
    );
    db.memory_mark_obsolete(&a).unwrap();
    assert!(
        db.memory_edges_from(&a).unwrap().is_empty(),
        "obsolete source has no live outgoing edges"
    );
    assert!(
        db.memory_edges_into(gh()).unwrap().is_empty(),
        "reverse traversal drops an obsolete source's edges"
    );
    // ...and you cannot author a NEW edge FROM an obsolete source (the add-time twin of the
    // filter).
    let from_dead = db.memory_edge_add(&a, "depends_on", node(&b)).unwrap_err();
    assert!(from_dead.to_string().contains("not found or is obsolete"), "{from_dead}");

    let _ = fs::remove_dir_all(&root);
}

/// #492: a single `gone` observation must not persist a downgrade — a validate pass racing a
/// rebuild window (or sweeping from a narrower checkout context) can produce a torn observation,
/// and doctor then hands out destructive mark-obsolete advice for healthy anchors. The persisted
/// `anchor_status` downgrades only on the SECOND consecutive gone observation; the validation
/// report still counts what each pass actually saw.
#[test]
fn a_downgrade_to_gone_needs_two_consecutive_observations() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keeper() {}\n").unwrap();
    fs::write(root.join("src/doomed.rs"), "pub fn doomed() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchored to a file a torn pass will misjudge".to_string(),
            body: "One gone observation arms the marker; only the second downgrades.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/doomed.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    let persisted = |db: &IndexDatabase| -> (String, Option<i64>) {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status, downgrade_pending_at_ms FROM repo_memory_bindings
                 WHERE memory_id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    db.memory_validate().unwrap();
    assert_eq!(persisted(&db), ("current".to_string(), None), "alive file → current, unarmed");

    fs::remove_file(root.join("src/doomed.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/doomed.rs'",
            [],
        )
        .unwrap();

    // First gone observation: the report says what the pass saw, but the persisted status holds
    // and the marker arms.
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "the report counts the computed observation");
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current", "one observation must not persist the downgrade");
    assert!(marker.is_some(), "the first gone observation arms the marker");

    // Second consecutive observation: the downgrade lands and the marker clears.
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1);
    assert_eq!(
        persisted(&db),
        ("gone".to_string(), None),
        "the second consecutive observation persists the downgrade and clears the marker"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #492: a positive observation between two gone observations disarms the pending downgrade — the
/// ping-pong a pair of checkout contexts produces (one sees the anchor, one does not) must never
/// land a persisted `gone`.
#[test]
fn a_recovered_anchor_clears_the_pending_downgrade() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keeper() {}\n").unwrap();
    fs::write(root.join("src/wobbling.rs"), "pub fn wobbling() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchored to a file that wobbles across contexts".to_string(),
            body: "A recovery between gone observations must disarm the downgrade.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/wobbling.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    let persisted = |db: &IndexDatabase| -> (String, Option<i64>) {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status, downgrade_pending_at_ms FROM repo_memory_bindings
                 WHERE memory_id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    fs::remove_file(root.join("src/wobbling.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/wobbling.rs'",
            [],
        )
        .unwrap();
    db.memory_validate().unwrap();
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current");
    assert!(marker.is_some(), "the gone observation arms the marker");

    // The anchor recovers (the other context's view): the next pass re-asserts it and disarms
    // the half-armed downgrade.
    fs::write(root.join("src/wobbling.rs"), "pub fn wobbling() {}\n").unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'source', sha256 = 'restored'
              WHERE path = 'src/wobbling.rs'",
            [],
        )
        .unwrap();
    db.memory_validate().unwrap();
    assert_eq!(
        persisted(&db),
        ("current".to_string(), None),
        "a positive observation stamps current and clears the marker"
    );

    // A later gone observation starts the two-pass rule from scratch.
    fs::remove_file(root.join("src/wobbling.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/wobbling.rs'",
            [],
        )
        .unwrap();
    db.memory_validate().unwrap();
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current", "the disarmed marker means the count restarts at one");
    assert!(marker.is_some());

    let _ = fs::remove_dir_all(&root);
}
