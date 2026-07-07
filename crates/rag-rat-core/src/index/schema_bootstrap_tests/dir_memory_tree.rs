use super::*;

#[test]
fn dir_memory_binds_to_a_directory() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn dir_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "src holds the core library".to_string(),
            body: "All Rust source lives under src/.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: Some("src".to_string()),
            },
        })
        .unwrap();

    assert!(!created.duplicate);
    assert_eq!(created.memory.bindings.len(), 1);
    let binding = &created.memory.bindings[0];
    assert_eq!(binding.binding_kind, "dir");
    assert_eq!(binding.binding_id, "src");
    assert_eq!(binding.anchor_status, "current");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dir_memory_validation_current_and_gone() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn dir_validate_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Helper: build a dir bind target with only `dir` set.
    let dir_bind = |dir: Option<String>| crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir,
    };

    // Case 1: memory on a populated directory ("src") -> validates current.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "src dir is the library root".to_string(),
        body: "All source lives under src/.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind(Some("src".to_string())),
    })
    .unwrap();

    // Case 2: memory on a directory with no indexed files -> resolves gone at bind time, and
    // memory_validate leaves it gone.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "nonexistent dir has no files".to_string(),
        body: "This directory does not exist in the index.".to_string(),
        confidence: "low".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind(Some("does/not/exist".to_string())),
    })
    .unwrap();

    // Case 3: root memory (dir:"") -> current whenever any file is indexed.
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "repo root anchors the whole index".to_string(),
        body: "The entire repo is indexed.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        bind: dir_bind(Some("".to_string())),
    })
    .unwrap();

    let report = db.memory_validate().unwrap();
    // "src" + "" both current, "does/not/exist" gone -> current==2, gone==1.
    assert_eq!(report.current, 2, "expected 2 current dir bindings");
    assert_eq!(report.gone, 1, "expected 1 gone dir binding");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn list_memories_returns_summaries_and_filters_by_binding_kind() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn list_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dir_bind = |dir: Option<String>| crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir,
    };
    let path_bind = |path: String| crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: Some(path),
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir: None,
    };

    // Create a dir-scoped memory.
    let dir_result = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "src is the library root".to_string(),
            body: "Core library lives under src/.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: dir_bind(Some("src".to_string())),
        })
        .unwrap();

    // Create a path-scoped memory.
    let path_result = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "lib.rs exports the public surface".to_string(),
            body: "All public symbols are re-exported from lib.rs.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: path_bind("src/lib.rs".to_string()),
        })
        .unwrap();

    let conn = db.storage.connection();

    // list_memories(None) returns both memories.
    let all = crate::query::memory::list_memories(conn, None).unwrap();
    assert_eq!(all.len(), 2, "expected 2 summaries, got: {all:?}");

    // The dir memory is present with correct summary fields.
    let dir_summary = all.iter().find(|s| s.memory_id == dir_result.memory.memory_id).unwrap();
    assert_eq!(dir_summary.kind, "Decision");
    assert_eq!(dir_summary.title, "src is the library root");
    assert_eq!(dir_summary.status, "active");
    assert_eq!(dir_summary.binding_kind, "dir");
    assert_eq!(dir_summary.binding_id, "src");

    // The path memory is present with correct summary fields.
    let path_summary = all.iter().find(|s| s.memory_id == path_result.memory.memory_id).unwrap();
    assert_eq!(path_summary.kind, "Invariant");
    assert_eq!(path_summary.binding_kind, "path");
    assert_eq!(path_summary.binding_id, "src/lib.rs");

    // list_memories(Some("dir")) returns only the dir-scoped memory.
    let dir_only = crate::query::memory::list_memories(conn, Some("dir")).unwrap();
    assert_eq!(dir_only.len(), 1, "expected 1 dir-kind summary, got: {dir_only:?}");
    assert_eq!(dir_only[0].binding_kind, "dir");
    assert_eq!(dir_only[0].memory_id, dir_result.memory.memory_id);

    // list_memories(Some("path")) returns only the path-scoped memory.
    let path_only = crate::query::memory::list_memories(conn, Some("path")).unwrap();
    assert_eq!(path_only.len(), 1, "expected 1 path-kind summary, got: {path_only:?}");
    assert_eq!(path_only[0].binding_kind, "path");

    let _ = fs::remove_dir_all(root);
}

// ─── Fix 1: label/depth contract ─────────────────────────────────────────────

#[test]
fn dir_tree_label_depth_flat_siblings() {
    // Fixture: src/a (3 files), src/b (3 files).
    // Expected display tree (formatter indents by depth, prints label):
    //   src      (depth 0, label "src")
    //     a      (depth 1, label "a")
    //     b      (depth 1, label "b")
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn g() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let find = |p: &str| {
        tree.nodes.iter().find(|n| n.path == p).unwrap_or_else(|| {
            panic!(
                "no node for {p}; nodes: {:?}",
                tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
            )
        })
    };

    let src = find("src");
    assert_eq!(src.depth, 0, "src depth");
    assert_eq!(src.label, "src", "src label");

    let a = find("src/a");
    assert_eq!(a.depth, 1, "src/a depth");
    assert_eq!(a.label, "a", "src/a label");

    let b = find("src/b");
    assert_eq!(b.depth, 1, "src/b depth");
    assert_eq!(b.label, "b", "src/b label");

    assert_eq!(tree.truncated, 0);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn dir_tree_label_depth_collapse_single_child_chain() {
    // Fixture: pkg/inner/deep with 3 files only at `deep` — no files in pkg or inner.
    // pkg → inner (single child, no files, no memory) → deep (3 files).
    // After collapse: one node with path="pkg", label="pkg/inner/deep", depth=0.
    // (The chain anchor is `pkg`; it collapses into `inner` then into `deep`.)
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/pkg/inner/deep")).unwrap();
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("src/pkg/inner/deep").join(name), "pub fn f() {}\n").unwrap();
    }
    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    // max_depth must be deep enough to reach depth 4 (src/pkg/inner/deep).
    let opts = crate::query::tree::TreeOpts { max_depth: 5, min_files: 3, max_nodes: 25 };
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    // The chain src → pkg → inner collapses; the one visible node for the pkg subtree
    // anchors at `src/pkg` (or `src`) and spans through to `deep`.  What matters:
    // (a) exactly one node has path == "src/pkg/inner/deep" OR the chain ends there,
    // (b) that node's label spans the collapsed segments relative to its display parent,
    // (c) its depth reflects only displayed ancestors.
    //
    // With src having only one included child (src/pkg), and src/pkg only one included child
    // (src/pkg/inner), etc., the whole chain from `src` collapses into a single anchor node
    // at `src` with label "src/pkg/inner/deep" (full path, display parent = "").
    let collapsed = tree.nodes.iter().find(|n| n.path == "src");
    assert!(
        collapsed.is_some(),
        "expected a collapsed node anchored at 'src'; nodes: {:?}",
        tree.nodes.iter().map(|n| (&n.path, &n.label, n.depth)).collect::<Vec<_>>()
    );
    let collapsed = collapsed.unwrap();
    assert_eq!(collapsed.label, "src/pkg/inner/deep", "collapsed label must span full chain");
    assert_eq!(collapsed.depth, 0, "collapsed chain anchor must be depth 0");
    assert_eq!(collapsed.file_count, 0, "file_count on chain anchor is 0 (files live at deep)");

    // No other node should appear (the entire tree collapses).
    assert_eq!(
        tree.nodes.len(),
        1,
        "only one node after full collapse; got: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    assert_eq!(tree.truncated, 0);
    let _ = fs::remove_dir_all(root);
}

// ─── Fix 1 + memory-only inclusion ───────────────────────────────────────────

#[test]
fn dir_tree_memory_only_dir_appears_without_min_files() {
    // A dir with a "dir" memory but fewer than min_files direct files still appears.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    // Only 1 file in src/a — below default min_files=3.
    fs::write(root.join("src/a/only.rs"), "pub fn only() {}\n").unwrap();
    // src/b gets 3 files so it qualifies on its own (ensures src is pulled in as ancestor).
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["p.rs", "q.rs", "r.rs"] {
        fs::write(root.join("src/b").join(name), "pub fn f() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Anchor a dir memory on src/a.
    create_dir_memory(&db, "sparse subsystem", Some("src/a".to_string()));

    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let node_a = tree.nodes.iter().find(|n| n.path == "src/a").unwrap_or_else(|| {
        panic!(
            "src/a missing from tree; nodes: {:?}",
            tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
        )
    });
    assert_eq!(node_a.file_count, 1, "src/a file_count");
    assert_eq!(node_a.memory_title.as_deref(), Some("sparse subsystem"), "src/a memory_title");
    assert_eq!(node_a.depth, 1, "src/a depth");
    assert_eq!(node_a.label, "a", "src/a label");

    let _ = fs::remove_dir_all(root);
}

// ─── Fix 2: generated exclusion ──────────────────────────────────────────────

#[test]
fn dir_tree_excludes_generated_files_from_count() {
    // A dir whose only files are generated=1 must not become a qualifying node (file_count
    // must not include generated files).
    //
    // Layout: src/gen (3 generated files), src/real (3 real files), src/also (3 real files).
    // Two real siblings prevent src from collapsing into a single-child chain so that
    // src/real and src/also appear as their own nodes.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/gen")).unwrap();
    fs::create_dir_all(root.join("src/real")).unwrap();
    fs::create_dir_all(root.join("src/also")).unwrap();
    // Real files — indexed with generated=0.
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("src/real").join(name), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/also").join(name), "pub fn g() {}\n").unwrap();
    }
    // Generated files — write them so the indexer picks them up, then flip generated=1.
    for name in &["g1.rs", "g2.rs", "g3.rs"] {
        fs::write(root.join("src/gen").join(name), "// generated\npub fn gen() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Mark all files under src/gen as generated after indexing.
    db.storage
        .connection()
        .execute("UPDATE main.files SET generated = 1 WHERE path LIKE 'src/gen/%'", [])
        .unwrap();

    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    // src/gen must either be absent (did not qualify) or have file_count == 0.
    if let Some(gen_node) = tree.nodes.iter().find(|n| n.path == "src/gen") {
        assert_eq!(
            gen_node.file_count,
            0,
            "generated dir must have file_count=0; got {}: {:?}",
            gen_node.file_count,
            tree.nodes.iter().map(|n| (&n.path, n.file_count)).collect::<Vec<_>>()
        );
    }
    // src/real must appear with file_count == 3 (only non-generated files counted).
    let real_node = tree.nodes.iter().find(|n| n.path == "src/real").unwrap_or_else(|| {
        panic!(
            "src/real missing; nodes: {:?}",
            tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
        )
    });
    assert_eq!(real_node.file_count, 3, "src/real file_count must be 3 (non-generated only)");

    let _ = fs::remove_dir_all(root);
}

// ─── Fix 3: real multi-context scoping ───────────────────────────────────────

#[test]
fn dir_tree_scope_excludes_other_worktree_files() {
    // Two worktree contexts share the same main.files table.  Scoping to one context must
    // not inflate file_count with the other worktree's rows.
    //
    // Arrangement: the primary build indexes src/a/{a,b,c}.rs AND src/b/{p,q,r}.rs.
    // Two sibling dirs prevent src from collapsing so src/a appears as its own node.
    // We then INSERT three extra files under src/a with a different worktree_id.
    // After scoping to the primary context, src/a must report file_count == 3, not 6.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn f() {}\n").unwrap();
    }
    for name in &["p.rs", "q.rs", "r.rs"] {
        fs::write(root.join("src/b").join(name), "pub fn g() {}\n").unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Insert extra files belonging to a different worktree (same path prefix, different
    // worktree_id).
    let conn = db.storage.connection();
    for name in &["x.rs", "y.rs", "z.rs"] {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, 'rust', 'source', 'sha-other', 0, 0, 0, 'rev-other', '', 'other-worktree')",
            [format!("src/a/{name}")],
        )
        .unwrap();
    }

    // Scope to the primary worktree only.
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default();
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let node_a = tree.nodes.iter().find(|n| n.path == "src/a").unwrap_or_else(|| {
        panic!("src/a missing; nodes: {:?}", tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>())
    });
    assert_eq!(
        node_a.file_count, 3,
        "file_count must not be inflated by other-worktree rows; got {}",
        node_a.file_count
    );

    let _ = fs::remove_dir_all(root);
}

// ─── Fix 3: max_nodes cap ────────────────────────────────────────────────────

#[test]
fn dir_tree_truncates_at_max_nodes() {
    // Create enough dirs to exceed max_nodes=3.  We use min_files=1 so every dir with a file
    // qualifies, giving us 5 leaf dirs + ancestor nodes.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    for i in 0..5u8 {
        let dir = root.join(format!("pkg{i}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("lib.rs"), "pub fn f() {}\n").unwrap();
    }
    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(".")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts { max_depth: 2, min_files: 1, max_nodes: 3 };
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    assert!(tree.nodes.len() <= 3, "nodes.len()={} must be <= max_nodes=3", tree.nodes.len());
    assert!(tree.truncated > 0, "truncated must be >0 when nodes were dropped");

    let _ = fs::remove_dir_all(root);
}

// ─── original integration test (extended) ────────────────────────────────────

#[test]
fn dir_tree_builds_annotated_layout() {
    // Index six files: three in src/a/ and three in src/b/.  Both dirs meet min_files (3),
    // so both appear in the tree.  A "dir" memory is anchored to src/a with title "alpha core"
    // and a root memory (dir:"") is anchored to the repo with title "the repo".

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn ax() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn bx() {}\n").unwrap();
    }

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    create_dir_memory(&db, "alpha core", Some("src/a".to_string()));
    create_dir_memory(&db, "the repo", Some("".to_string()));

    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts::default(); // max_depth=6, min_files=3, max_nodes=30
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    // Root memory must be present.
    assert_eq!(
        tree.root_memory_title.as_deref(),
        Some("the repo"),
        "root_memory_title mismatch; got: {:?}",
        tree.root_memory_title
    );

    // src must be an intermediate node (pulled in as ancestor).
    let src = tree.nodes.iter().find(|n| n.path == "src");
    assert!(
        src.is_some(),
        "no node for src; nodes: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    let src = src.unwrap();
    assert_eq!(src.depth, 0, "src depth");
    assert_eq!(src.label, "src", "src label");

    // src/a must appear with correct label/depth, file_count==3 and memory_title.
    let node_a = tree.nodes.iter().find(|n| n.path == "src/a");
    assert!(
        node_a.is_some(),
        "no node for src/a; nodes: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    let node_a = node_a.unwrap();
    assert_eq!(node_a.file_count, 3, "src/a file_count");
    assert_eq!(node_a.depth, 1, "src/a depth");
    assert_eq!(node_a.label, "a", "src/a label");
    assert_eq!(
        node_a.memory_title.as_deref(),
        Some("alpha core"),
        "src/a memory_title mismatch: {:?}",
        node_a.memory_title
    );

    // src/b must appear with correct label/depth and file_count==3.
    let node_b = tree.nodes.iter().find(|n| n.path == "src/b");
    assert!(
        node_b.is_some(),
        "no node for src/b; nodes: {:?}",
        tree.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
    );
    let node_b = node_b.unwrap();
    assert_eq!(node_b.file_count, 3, "src/b file_count");
    assert_eq!(node_b.depth, 1, "src/b depth");
    assert_eq!(node_b.label, "b", "src/b label");

    // No truncation.
    assert_eq!(tree.truncated, 0, "unexpected truncation");

    // Scoping invariant: re-installing the same scope view and re-querying must not change
    // counts (guards against the view accumulating duplicate rows on reinstall).
    install_scope(conn, &root);
    let tree2 = crate::query::tree::dir_tree(conn, &opts).unwrap();
    let node_a2 = tree2.nodes.iter().find(|n| n.path == "src/a").unwrap();
    assert_eq!(node_a2.file_count, 3, "file_count changed after scope reinstall");

    let _ = fs::remove_dir_all(root);
}

// ─── Bug fix: children of collapsed node must use leaf labels ─────────────────

#[test]
fn dir_tree_children_of_collapsed_node_use_leaf_labels() {
    // Fixture:
    //   top/          — single included child (mid), no direct files, no memory → collapses
    //   top/mid/      — has two included children (x, y); files only in x/* and y/*
    //   top/mid/x/    — 3 files (qualifies on its own)
    //   top/mid/y/    — 3 files (qualifies on its own)
    //
    // After collapse: one displayed node anchored at `top` with label "top/mid" (relative to
    // root display parent ""). Its children x and y must be labelled "x" and "y" (relative to
    // the chain-end "top/mid"), NOT "mid/x" / "mid/y" (which would be wrong — relative to the
    // anchor "top").
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("top/mid/x")).unwrap();
    fs::create_dir_all(root.join("top/mid/y")).unwrap();
    for name in &["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join("top/mid/x").join(name), "pub fn fx() {}\n").unwrap();
        fs::write(root.join("top/mid/y").join(name), "pub fn fy() {}\n").unwrap();
    }
    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(".")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    install_scope(conn, &root);

    let opts = crate::query::tree::TreeOpts { max_depth: 6, min_files: 3, max_nodes: 30 };
    let tree = crate::query::tree::dir_tree(conn, &opts).unwrap();

    let node_labels: Vec<(&str, &str, u8)> =
        tree.nodes.iter().map(|n| (n.path.as_str(), n.label.as_str(), n.depth)).collect();

    // The collapsed node: anchor at "top", label "top/mid", depth 0.
    let collapsed = tree
        .nodes
        .iter()
        .find(|n| n.path == "top")
        .unwrap_or_else(|| panic!("no collapsed node at 'top'; nodes: {node_labels:?}"));
    assert_eq!(collapsed.label, "top/mid", "collapsed node label; nodes: {node_labels:?}");
    let collapsed_depth = collapsed.depth;

    // Children must be labelled by leaf segment only (not "mid/x" / "mid/y").
    let x = tree
        .nodes
        .iter()
        .find(|n| n.path == "top/mid/x")
        .unwrap_or_else(|| panic!("no node for top/mid/x; nodes: {node_labels:?}"));
    assert_eq!(x.label, "x", "top/mid/x label must be leaf 'x'; nodes: {node_labels:?}");
    assert_eq!(
        x.depth,
        collapsed_depth + 1,
        "top/mid/x depth must be parent+1; nodes: {node_labels:?}"
    );

    let y = tree
        .nodes
        .iter()
        .find(|n| n.path == "top/mid/y")
        .unwrap_or_else(|| panic!("no node for top/mid/y; nodes: {node_labels:?}"));
    assert_eq!(y.label, "y", "top/mid/y label must be leaf 'y'; nodes: {node_labels:?}");
    assert_eq!(
        y.depth,
        collapsed_depth + 1,
        "top/mid/y depth must be parent+1; nodes: {node_labels:?}"
    );

    assert_eq!(tree.truncated, 0);
    let _ = fs::remove_dir_all(root);
}

/// V022 bootstrap (fresh-applies-all): a brand-new index applies every migration through V022 and
/// ends with the `packages` table and the three DEDICATED edge import-scope columns — and the
/// `edges` compatibility view surfaces them. There is NO `files.package_id` column: the
/// file→package mapping is computed at LOAD time from `packages` (the #106 fix dropped the
/// persisted pointer). The oracle's `callee_*` columns are untouched (the columns are dedicated,
/// not a callee overload).
#[test]
fn v025_creates_chunk_text_compression_tables() {
    // #77 Phase 2: the chunk_text (zstd blob) + chunk_text_dict (shared dictionary) tables exist
    // after a fresh apply (baseline) AND a forward-migrate (V025).
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    for t in ["chunk_text", "chunk_text_dict"] {
        assert!(conn_table_exists(&conn, t), "{t} created on fresh apply");
    }
    let cols = conn_table_columns(&conn, "chunk_text");
    for expected in ["chunk_id", "blob", "raw_len", "dict_version"] {
        assert!(cols.contains(&expected.to_string()), "chunk_text missing {expected}");
    }
    // Forward-migrate path: drop the tables + the V025 ledger row, re-apply → recreated.
    conn.execute_batch(
        "DROP TABLE chunk_text; DROP TABLE chunk_text_dict;
         DELETE FROM schema_version WHERE id = '025_chunk_text_compression_tables';",
    )
    .unwrap();
    schema::apply(&conn).unwrap();
    assert!(conn_table_exists(&conn, "chunk_text"), "V025 recreates chunk_text on forward migrate");
    assert!(conn_table_exists(&conn, "chunk_text_dict"));

    // Dicts are immutable + versioned (#77 Phase 2): MULTIPLE versions coexist (the prior
    // CHECK(id=1) single-row constraint is gone — that was the mutable-global-slot footgun a
    // retrain would hit).
    conn.execute("INSERT INTO chunk_text_dict(version, dict) VALUES (1, x'00')", []).unwrap();
    conn.execute("INSERT INTO chunk_text_dict(version, dict) VALUES (2, x'00')", [])
        .expect("multiple dict versions coexist");
    // raw_len is the decompress capacity; a negative value would cast to a huge usize.
    assert!(
        conn.execute(
            "INSERT INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES (1, x'00', -1, \
             1)",
            [],
        )
        .is_err(),
        "chunk_text rejects negative raw_len"
    );
}
