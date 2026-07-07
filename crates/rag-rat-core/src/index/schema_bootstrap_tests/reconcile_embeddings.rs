use super::*;

#[cfg(unix)]
#[test]
fn indexing_skips_symlink_loops() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn loop_safe_symbol() {}\n").unwrap();
    std::os::unix::fs::symlink(&root, root.join("src/loop")).unwrap();

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_eq!(db.symbols("loop_safe_symbol", Some(Language::Rust), 10).unwrap().len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dirty_git_files_are_indexed_as_worktree_overlay() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("search.md"), "# Title\nbase token\n").unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &[
        "-c",
        "user.name=Rag Rat Test",
        "-c",
        "user.email=rag-rat@example.invalid",
        "commit",
        "-m",
        "initial",
    ]);

    let config = markdown_config_for_root(root.clone());
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.search("base", 10, false).unwrap().len(), 1);

    fs::write(docs.join("search.md"), "# Title\noverlay token\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    // Scope the raw file-scope enumeration to the ACTIVE repo: the poison-sibling harness seeds a
    // same-path (committed-shaped) `main.files` row at this fixture's path, which would otherwise
    // appear as an extra `(true, false)`. Reading through the scope view is wrong here (the test
    // needs both the raw committed row AND the overlay row), so scope by `repo_id` instead.
    let conn = db.storage.connection();
    let repo_id = crate::index::schema::active_repo_id(conn).unwrap();
    let scopes = conn
        .prepare(
            "
                SELECT commit_sha != '', worktree_id != ''
                FROM main.files
                WHERE path = 'docs/search.md' AND repo_id = ?1
                ORDER BY commit_sha != '' DESC, worktree_id != '' DESC
                ",
        )
        .unwrap()
        .query_map([&repo_id], |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(scopes, vec![(true, false), (false, true)]);
    assert!(db.search("base", 10, false).unwrap().is_empty());
    let overlay_hits = db.search("overlay", 10, false).unwrap();
    assert_eq!(overlay_hits.len(), 1);
    assert!(overlay_hits[0].summary.contains("overlay token"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn rebuild_populates_revision_metadata_and_fresh_fts_state() {
    // `content_revision` is a deliberately-GLOBAL digest over the whole `files` table (round-5 kept
    // it in `index_meta`, not per-repo), and `fts_source_revision` tracks it. This test asserts the
    // FRESH digest equals the value stored at rebuild time — but the poison-sibling harness seeds a
    // file AFTER the rebuild commits, so the fresh digest legitimately includes it while the stored
    // value does not. A whole-DB-digest assertion; opt out.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let (root, config) = markdown_config("alpha token");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let status = db.status(&config.database).unwrap();

    assert!(!status.content_revision.is_empty());
    assert_eq!(status.fts_source_revision.as_deref(), Some(status.content_revision.as_str()));
    assert_eq!(
        db.meta("content_revision").unwrap().as_deref(),
        Some(status.content_revision.as_str())
    );
    assert!(!status.fts_dirty);
    assert!(status.fts_fresh);
    assert!(!status.git_history.available);
    assert_eq!(status.git_history.commit_count, 0);
    assert_eq!(status.llm.embedding.state, "MissingModel");
    assert_eq!(status.llm.fastembed.backend, "fastembed");
    assert_eq!(status.llm.fastembed.model, FASTEMBED_DISPLAY_MODEL);
    assert_eq!(status.llm.fastembed.dim, FASTEMBED_EMBEDDING_DIM);
    assert!(!status.llm.fastembed.cache.is_empty());
    assert_eq!(status.llm.fastembed.build_feature_enabled, cfg!(feature = "fastembed"));
    assert_eq!(status.llm.artifacts.total_chunks, 1);
    assert_eq!(
        status.llm.artifacts.eligible_chunks + status.llm.artifacts.skipped_chunks,
        status.llm.artifacts.total_chunks
    );
    assert_eq!(
        status.llm.fastembed.eligible_embeddings + status.llm.fastembed.skipped_embeddings,
        status.llm.artifacts.total_chunks
    );
    assert_eq!(indexed_revision_count(&db), 1);
    assert_eq!(chunk_source_revision_count(&db), 1);

    let _ = fs::remove_dir_all(root);
}

#[cfg(not(feature = "fastembed"))]
#[test]
fn fastembed_missing_feature_reports_rebuild_command() {
    let (root, config) = markdown_config("alpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();

    let err = db.install_model(FASTEMBED_MODEL_ID, None).unwrap_err();
    assert!(err.to_string().contains(ai::FASTEMBED_MISSING_FEATURE_MESSAGE));

    let status = db.llm_status().unwrap();
    assert!(!status.fastembed.build_feature_enabled);
    assert_eq!(status.fastembed.status, "MissingRuntime");
    assert_eq!(status.fastembed.message.as_deref(), Some(ai::FASTEMBED_MISSING_FEATURE_MESSAGE));
    assert_eq!(status.fastembed.next.as_deref(), Some("cargo install rag-rat"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_requires_explicit_model_install_and_ignores_stale_artifacts() {
    let (root, mut config) = markdown_config(
        "alpha token\nsecond line with enough detail for the semantic embedding policy to keep \
         this chunk\nthird line with runtime context\n",
    );
    // Select the deterministic hash embedder explicitly — this test exercises the reconcile flow
    // with the no-download test embedder. A fresh index adopts the CONFIGURED model (#394), so
    // without this it would adopt the default all-MiniLM and the HASH_MODEL_ID assertions below
    // would not hold.
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let chunk_id = first_chunk_id(&db);

    let models = db.list_models().unwrap();
    let embedding = models.iter().find(|model| model.model_id == HASH_MODEL_ID).unwrap();
    assert!(!embedding.installed);
    assert_eq!(embedding.status, "MissingModel");

    let hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].summary.contains("alpha token"));

    let blocked = db.reconcile(Some(1), Some(8)).unwrap();
    assert_eq!(blocked.processed_chunks, 0);
    assert_eq!(blocked.embeddings_written, 0);
    assert_eq!(blocked.blocked_chunks, 0);
    assert_eq!(blocked.model_id, HASH_MODEL_ID);
    assert_eq!(blocked.batch_size, 8);
    assert_eq!(blocked.status, "Blocked");

    let status = db.llm_status().unwrap();
    assert_eq!(status.embedding.state, "MissingModel");
    assert_eq!(status.embedding.blocked_artifacts, 0);

    db.install_model(HASH_MODEL_ID, None).unwrap();
    let plan = db.reconcile_plan().unwrap();
    assert_eq!(plan.embeddings.missing, 1);
    assert_eq!(plan.embeddings.current, 0);
    let current = db.reconcile(Some(1), Some(8)).unwrap();
    assert_eq!(current.embeddings_written, 1);
    assert_eq!(current.model_id, HASH_MODEL_ID);
    assert_eq!(current.model_version, "hash-v1");
    assert_eq!(current.embedding_dim, HASH_EMBEDDING_DIM);
    assert_eq!(current.status, "Current");
    assert_eq!(current.work_reasons.get("Missing"), Some(&1));
    let noop = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(noop.processed_chunks, 0);
    assert_eq!(noop.embeddings_written, 0);
    let status = db.llm_status().unwrap();
    assert_eq!(status.embedding.state, "Ready");
    assert_eq!(status.embedding.current_artifacts, 1);
    let embedding_bytes: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT length(vector_blob) FROM chunk_embeddings WHERE chunk_id = ?1 AND status = \
             'Current'",
            [chunk_id],
            |row| row.get(0),
        )
        .unwrap();
    // int8-quantized blob (#112): a 4-byte f32 scale + one signed byte per dim — ~4x smaller than
    // the old 4*dim f32 blob.
    assert_eq!(embedding_bytes, (4 + HASH_EMBEDDING_DIM) as i64);

    let hits = db.search("alpha", 10, false).unwrap();
    assert!(hits[0].summary.contains("alpha token"));

    db.storage.connection().execute("DELETE FROM chunk_fts", []).unwrap();
    let vector_hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(vector_hits.len(), 1);
    assert_eq!(vector_hits[0].chunk_id, chunk_id);

    db.storage
        .connection()
        .execute("UPDATE chunk_embeddings SET source_text_hash = 'old-hash' WHERE chunk_id = ?1", [
            chunk_id,
        ])
        .unwrap();
    let plan = db.reconcile_plan().unwrap();
    assert_eq!(plan.embeddings.current, 0);
    assert_eq!(plan.embeddings.stale, 1);
    let refreshed = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(refreshed.processed_chunks, 1);
    assert_eq!(refreshed.work_reasons.get("SourceChanged"), Some(&1));
    assert_eq!(db.current_embedding_count(HASH_MODEL_ID).unwrap(), 1);
    let stale_embedding_hits = db.search("alpha", 10, false).unwrap();
    assert_eq!(stale_embedding_hits.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_confirms_a_provisional_model_against_a_config_change() {
    // #394 review: a PROVISIONAL active model (a config seed or a fastembed-cache recovery) yields
    // to a differing config — but once a reconcile COMMITS embeddings under it, the model is
    // confirmed (provisional flag cleared) and a later config-model edit must NOT silently switch
    // it (that would strand the vectors and force a re-embed).
    let (root, mut config) = markdown_config(
        "alpha token\nsecond line with enough detail for the semantic embedding policy to keep \
         this chunk\nthird line with runtime context\n",
    );
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    // Simulate a PROVISIONAL-but-Ready active model, as a fastembed-cache recovery produces: Ready
    // to embed, but not yet user-confirmed. (Mirrors ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META.)
    db.storage
        .connection()
        .execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES \
             ('active_embedding_model_provisional', '1')",
            [],
        )
        .unwrap();
    // A reconcile that commits embeddings CONFIRMS the model — it clears the provisional flag.
    assert!(
        db.reconcile(None, Some(8)).unwrap().embeddings_written >= 1,
        "hash embeddings are committed"
    );
    drop(db);

    // Edit the config to a DIFFERENT model, then reopen: the active model stays hash because the
    // reconcile confirmed it — the seed only reseeds a still-provisional model.
    config.llm.embedding.backend = "sentence-transformers/all-MiniLM-L6-v2".parse().unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        ai::active_embedding_model_id(db.storage.connection()).unwrap(),
        HASH_MODEL_ID,
        "a reconcile-confirmed model is not switched by a config change"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_incremental_pass_heals_a_missing_active_model_atomically() {
    // A pre-#394 index (active model unset) opened on the config-blind incremental / maintenance /
    // watch path must re-seed the active model from config — and, because the seed lives INSIDE the
    // incremental transaction and counts as a mutation, the heal is COMMITTED rather than rolled
    // back as an idle no-write pass (#394 review).
    let (root, mut config) = markdown_config("alpha token\nenough detail for a chunk to survive\n");
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Simulate a pre-#394 index: drop the seeded active-model meta.
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'active_embedding_model'", [])
        .unwrap();
    drop(db);

    // An incremental discover pass re-seeds inside its transaction and commits the heal (had it
    // been treated as an idle pass, the ROLLBACK would leave the active model unset).
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_eq!(
        ai::active_embedding_model_id(db.storage.connection()).unwrap(),
        HASH_MODEL_ID,
        "the incremental pass healed the active model and committed it"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(feature = "fastembed")]
#[test]
fn cached_fastembed_model_recovers_ready_state() {
    let (root, config) = markdown_config("alpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let cache_dir = root.join("models");
    let revision = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
    let repo = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::create_dir_all(repo.join("snapshots").join(revision)).unwrap();
    fs::write(repo.join("refs").join("main"), revision).unwrap();

    // R3b regression: simulate a pre-#317 upgrade where a STALE legacy freshness-version meta
    // lingers and no model is active. Recovery must ACTIVATE the recovered model AND stamp its
    // version — not leave the stale key (which would bake new embeddings under the wrong
    // `model_version`).
    {
        let conn = db.storage.connection();
        conn.execute("DELETE FROM repo_meta WHERE key = 'active_embedding_model'", []).unwrap();
        ai::set_repo_meta(conn, "embedding_active_model_version", "legacy-stale-key").unwrap();
    }

    ai::recover_cached_fastembed_model_at(db.storage.connection(), &cache_dir).unwrap();

    let models = db.list_models().unwrap();
    let fastembed = models.iter().find(|model| model.model_id == FASTEMBED_MODEL_ID).unwrap();
    assert!(fastembed.installed);
    assert_eq!(fastembed.status, "Ready");
    let status = db.llm_status().unwrap();
    assert_eq!(status.fastembed.status, "Ready");
    assert!(status.fastembed.active);
    // The recovered model's version meta is its OWN static spec.version — not the stale legacy key.
    assert_eq!(
        ai::active_embedding_model_version(db.storage.connection(), FASTEMBED_MODEL_ID).unwrap(),
        crate::embedding_models::spec(FASTEMBED_MODEL_ID).unwrap().version,
        "recovery stamps the recovered model's version (R3b)",
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(feature = "fastembed")]
#[test]
fn compatible_migrate_recovers_cached_fastembed_model() {
    let (root, config) = markdown_config("alpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let cache_dir = root.join("models");
    let revision = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
    let repo = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::create_dir_all(repo.join("snapshots").join(revision)).unwrap();
    fs::write(repo.join("refs").join("main"), revision).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE ai_models
                 SET installed = 0, status = 'MissingModel', installed_at_ms = NULL
                 WHERE model_id = ?1",
            [FASTEMBED_MODEL_ID],
        )
        .unwrap();

    IndexDatabase::migrate_with_fastembed_cache(&config.database, Some(&cache_dir)).unwrap();

    let db = IndexDatabase::open(&config.database).unwrap();
    let status = db.llm_status().unwrap();
    assert_eq!(status.fastembed.status, "Ready");
    assert!(status.fastembed.active);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_without_limit_processes_all_chunks() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();

    let report = db.reconcile(None, Some(2)).unwrap();

    assert_eq!(report.processed_chunks, 2);
    assert_eq!(report.embeddings_written, 2);
    assert_eq!(report.batch_size, 2);
    assert_eq!(db.current_embedding_count(HASH_MODEL_ID).unwrap(), 2);
    let second = db.reconcile(None, Some(2)).unwrap();
    assert_eq!(second.processed_chunks, 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn force_reconcile_processes_each_chunk_once_and_terminates() {
    // Regression: --force skipped the needs_embedding filter, so select_reconcile_batch
    // never returned an empty batch and the loop re-embedded the active set forever when
    // no --limit/--max-seconds was set. A generous finite limit lets this test terminate
    // either way; the processed/written counts distinguish fixed (==2) from buggy (==50).
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();

    // Two eligible chunks; force with a limit far above the chunk count.
    let report = db.reconcile_with_progress(Some(50), Some(2), true, |_| {}).unwrap();

    assert_eq!(report.embeddings_written, 2, "force re-embedded chunks: {report:?}");
    assert_eq!(report.processed_chunks, 2, "force re-processed chunks: {report:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn force_reconcile_progress_is_honest_and_terminates_without_limit() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();

    // No --limit. max_seconds is only a safety net: if the force loop regressed to
    // re-embedding forever it would trip max_seconds and report "Partial" rather than
    // terminating naturally, which this test asserts against (no CI hang on regression).
    let mut events = Vec::new();
    let report = db
        .reconcile_with_options_progress(
            ai::ReconcileOptions {
                force: true,
                batch_size: Some(1),
                max_seconds: Some(30),
                ..ai::ReconcileOptions::default()
            },
            |event| events.push(event),
        )
        .unwrap();

    assert_eq!(report.status, "Current", "did not terminate naturally: {report:?}");
    assert_eq!(report.processed_chunks, 2);

    let started_total = events.iter().find_map(|event| match event {
        ai::ReconcileProgress::Started { total_chunks, .. } => Some(*total_chunks),
        _ => None,
    });
    assert_eq!(started_total, Some(2), "denominator should equal the eligible set");

    for event in &events {
        if let ai::ReconcileProgress::Batch { processed_chunks, total_chunks, .. } = event {
            assert!(
                processed_chunks <= total_chunks,
                "progress exceeded 100%: {processed_chunks}/{total_chunks}",
            );
        }
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn status_counts_only_active_context_chunks() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();

    let active = db.llm_status().unwrap().artifacts.total_chunks;
    assert!(active > 0, "expected active chunks, got {active}");

    // Point the connection at a context that matches no indexed rows. The active set
    // (temp.files) is now empty, so status must report 0 chunks. Pre-fix the counts ran
    // over main.chunks (every indexed commit) and ignored the active context entirely.
    db.set_context("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "ghost-worktree").unwrap();
    let scoped = db.llm_status().unwrap().artifacts;
    assert_eq!(scoped.total_chunks, 0, "status ignored active context scope");
    assert_eq!(scoped.current, 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn watch_maintenance_pass_indexes_new_files() {
    // A watcher pass must pick up a brand-new (uncommitted) file, not just refresh known ones.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/one.rs"), "pub fn one() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();

    // New file appears after the initial index; a maintenance pass should index it.
    fs::write(root.join("src/two.rs"), "pub fn newly_added_symbol() {}\n").unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();

    let db = IndexDatabase::open_config(&config).unwrap();
    let hits = db.symbols("newly_added_symbol", Some(Language::Rust), 10).unwrap();
    assert!(!hits.is_empty(), "watcher pass did not index the new file");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn watch_maintenance_pass_defers_a_first_time_empty_config() {
    // #427 review: a maintenance/watch pass on a brand-new config with NO discoverable files (an
    // empty target tree — the `rag-rat mcp` / misconfigured-repo case) must NOT first-time-register
    // an empty repo. It creates no database at all; a later pass registers once real content
    // appears. (The one-shot `index` command surfaces this as an error; the watcher just waits.)
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap(); // configured target `src/`, but it is empty
    let config = source_config(root.clone(), Language::Rust);

    crate::watch::maintenance_pass(&config, false).unwrap();
    assert!(
        !config.database.exists(),
        "an empty first-time config must not create/register an index"
    );

    // Content appears → the next pass registers + indexes it.
    fs::write(root.join("src/one.rs"), "pub fn appeared() {}\n").unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    let hits = db.symbols("appeared", Some(Language::Rust), 10).unwrap();
    assert!(!hits.is_empty(), "a pass after content appears must register + index it");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn discover_deletion_is_worktree_scoped() {
    // Invariant (watcher spec, review item 1): a discover pass run from worktree A must remove
    // only A's own rows for files missing from A's disk — never another worktree's overlay
    // rows. Otherwise two watchers on one shared DB delete each other's live overlays.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A row owned by a *different* worktree, for a path that does not exist on this disk.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                     indexed_at_ms, indexed_revision, commit_sha, worktree_id)
                 VALUES ('src/only_in_other.rs','rust','source','h',0,0,0,'rev','',
                     'other-worktree')",
            [],
        )
        .unwrap();
    drop(db);

    // This worktree loses a.rs; re-discover as this worktree.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    let db = IndexDatabase::index_discover(&config).unwrap();
    let conn = db.storage.connection();

    // The other worktree's overlay row survives untouched.
    let other: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE worktree_id = 'other-worktree' AND kind != \
             'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(other, 1, "this worktree's pass deleted another worktree's row");

    // Deletion still works within this worktree's own scope: a.rs gone from the active view,
    // b.rs retained.
    let active = |path: &str| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM files WHERE path = ?1", [path], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(active("src/a.rs"), 0, "deleted file still active in own worktree");
    assert_eq!(active("src/b.rs"), 1, "live file dropped from own worktree");

    // Post-condition: a worktree-scoped discover-deletion must not delete a sibling repo's rows
    // (round-6 harness) — the same "delete only my scope" invariant, widened to the repo axis.
    crate::index::poison_sibling::assert_sibling_intact(conn);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn gc_prunes_dead_context_rows_and_keeps_live_ones() {
    let (root, config) = markdown_config(
        "# One\nalpha token with enough surrounding detail for embedding eligibility and useful \
         semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding \
         eligibility and useful semantic context\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    db.reconcile(None, Some(8)).unwrap();

    let live_files = table_row_count(db.storage.connection(), "files").unwrap();
    let live_chunks = table_row_count(db.storage.connection(), "chunks").unwrap();
    assert!(live_files > 0 && live_chunks > 0);

    // A ghost file from a commit/worktree that is not live.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                     indexed_at_ms, indexed_revision, commit_sha, worktree_id)
                 VALUES ('ghost.md','markdown','source','deadhash',0,0,0,'deadrev',
                     'deadcommit','dead-worktree')",
            [],
        )
        .unwrap();
    assert_eq!(table_row_count(db.storage.connection(), "files").unwrap(), live_files + 1);

    // Keep only the active worktree. The ghost's commit and worktree are not live.
    let live_worktree = db.active_worktree_id.clone();
    let report = db.prune_to_live(&[], &[live_worktree]).unwrap();

    assert!(!report.skipped);
    assert_eq!(report.files_pruned, 1, "ghost not pruned: {report:?}");
    assert_eq!(
        table_row_count(db.storage.connection(), "files").unwrap(),
        live_files,
        "live files were pruned",
    );
    assert_eq!(
        table_row_count(db.storage.connection(), "chunks").unwrap(),
        live_chunks,
        "live chunks were pruned",
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn gc_refuses_to_prune_with_no_live_context() {
    let (root, config) = markdown_config("# Only\nsome content with enough detail for a chunk\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = table_row_count(db.storage.connection(), "files").unwrap();
    assert!(before > 0);

    // Empty live sets must never wipe the index.
    let report = db.prune_to_live(&[], &[]).unwrap();
    assert!(report.skipped);
    assert_eq!(report.files_pruned, 0);
    assert_eq!(table_row_count(db.storage.connection(), "files").unwrap(), before);

    // Post-condition: the refused prune must leave the poison sibling untouched (round-6 harness).
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_treats_c_chunks_as_embedding_eligible() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.c"),
        r#"
static int read_sensor_value(int baseline)
{
    int adjusted = baseline + 42;
    return adjusted;
}

int main(void)
{
    int sample = read_sensor_value(7);
    return sample == 49 ? 0 : 1;
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::C);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();

    let plan = db.reconcile_plan().unwrap();

    assert_eq!(plan.embeddings.skipped_by_policy.get("SkipLanguageUnsupported"), None);
    assert!(plan.embeddings.missing > 0, "plan: {:?}", plan.embeddings);

    let report = db.reconcile(None, Some(8)).unwrap();
    assert!(report.embeddings_written > 0, "report: {report:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_policy_skips_tiny_chunks_before_embedding() {
    let (root, config) = markdown_config("tiny\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();

    let plan = db.reconcile_plan().unwrap();
    assert_eq!(plan.embeddings.missing, 0);
    assert_eq!(plan.embeddings.skipped_by_policy.get("SkipTooSmall"), Some(&1));

    let report = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(report.embeddings_written, 0);
    assert_eq!(report.skipped_by_policy.get("SkipTooSmall"), Some(&1));
    assert_eq!(db.current_embedding_count(HASH_MODEL_ID).unwrap(), 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_plan_reports_policy_skips_for_fastembed_model() {
    let (root, config) = markdown_config("tiny\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE ai_models
                 SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2
                 WHERE model_id = ?1",
            params![FASTEMBED_MODEL_ID, i64::try_from(FASTEMBED_EMBEDDING_DIM).unwrap()],
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value)
                 VALUES ('__unassigned__', 'active_embedding_model', ?1)
                 ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
            [FASTEMBED_MODEL_ID],
        )
        .unwrap();

    let plan = db.reconcile_plan().unwrap();

    assert_eq!(plan.embeddings.model_id, FASTEMBED_MODEL_ID);
    assert_eq!(plan.embeddings.missing, 0);
    assert_eq!(plan.embeddings.skipped_by_policy.get("SkipTooSmall"), Some(&1));

    let _ = fs::remove_dir_all(root);
}

#[cfg(not(feature = "fastembed"))]
#[test]
fn blocked_fastembed_reconcile_still_reports_policy_skips() {
    let (root, config) = markdown_config("tiny\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value)
                 VALUES ('__unassigned__', 'active_embedding_model', ?1)
                 ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
            [FASTEMBED_MODEL_ID],
        )
        .unwrap();

    let report = db.reconcile(None, Some(8)).unwrap();

    assert_eq!(report.status, "Blocked");
    assert_eq!(report.skipped_by_policy.get("SkipTooSmall"), Some(&1));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_explain_reports_weighted_score_components() {
    let (root, config) = markdown_config(
        "alpha runtime shutdown\nsecond line with enough detail for embedding eligibility and \
         semantic vector scoring\nthird line\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    db.reconcile(None, Some(8)).unwrap();

    let hits = db.search_explain("runtime shutdown", 10, false).unwrap();

    assert_eq!(hits.len(), 1);
    let components = hits[0].score_components.as_ref().unwrap();
    let component_sum = components.bm25
        + components.vector
        + components.symbol
        + components.graph
        + components.git
        + components.github;
    // `score` is rounded to 4dp for display, so compare against the rounded component sum.
    assert!((hits[0].score - crate::query::round_score(component_sum)).abs() < 1e-9);
    assert!(components.bm25 > 0.0);
    assert!(components.vector > 0.0);
    assert!(components.vector_note.is_none());
    assert!(components.bm25 <= 0.45);
    assert!(components.vector <= 0.35);
    assert!(components.symbol <= 0.10);
    assert!(components.graph <= 0.05);
    assert!(components.git <= 0.03);
    assert!(components.github <= 0.02);
    assert!(db.search("runtime shutdown", 10, false).unwrap()[0].score_components.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_explain_labels_missing_vector_runtime() {
    let (root, config) = markdown_config(
        "alpha runtime shutdown\nsecond line with enough detail for lexical search without \
         embeddings\nthird line\n",
    );
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hits = db.search_explain("runtime shutdown", 10, false).unwrap();

    assert_eq!(hits.len(), 1);
    let components = hits[0].score_components.as_ref().unwrap();
    assert!(components.bm25 > 0.0);
    assert_eq!(components.vector, 0.0);
    assert_eq!(
        components.vector_note.as_deref(),
        Some("vector search unavailable: no current embedding model")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_indexes_commits_paths_queries_and_blame() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);

    fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add alpha docs"]);

    fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Refresh beta docs"]);

    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            },
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
        ],
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
    let status = db.status(&config.database).unwrap();
    assert!(status.git_history.available);
    assert!(status.git_history.head.is_some());
    assert_eq!(status.git_history.indexed_head, status.git_history.head);
    assert_eq!(status.git_history.commit_count, 2);
    assert_eq!(status.git_history.file_change_count, 3);

    let commit_hits = db.commit_search("beta", 10).unwrap();
    assert_eq!(commit_hits.len(), 1);
    assert_eq!(commit_hits[0].subject, "Refresh beta docs");
    assert_eq!(commit_hits[0].evidence_kind, "historical");
    assert!(commit_hits[0].score > 0.0);

    let path_history = db.git_history_for_path("docs/search.md", 10).unwrap();
    assert_eq!(path_history.len(), 2);
    assert!(path_history.iter().all(|item| item.evidence_kind == "historical"));

    let symbol_history =
        db.git_history_for_symbol("tracked_symbol", Some(Language::Rust), 10).unwrap();
    assert_eq!(symbol_history.len(), 1);
    assert_eq!(symbol_history[0].path, "src/lib.rs");
    assert_eq!(symbol_history[0].evidence_kind, "historical");
    let impact = db.impact_surface("tracked_symbol", 10).unwrap();
    assert!(impact.iter().any(|item| {
        item.category == "Direct structural impact" && item.reason == "exact_symbol_definition"
    }));
    assert!(impact.iter().any(|item| {
        item.category == "Historical/papertrail evidence"
            && item.reason == "git_commit_touched_file"
    }));

    let query_commits = db.commits_touching_query("beta", 10).unwrap();
    let beta_commit = query_commits.iter().find(|hit| hit.subject == "Refresh beta docs").unwrap();
    assert!(beta_commit.evidence.iter().any(|value| value == "commit_message"));
    assert!(beta_commit.evidence.iter().any(|value| value == "file_change"));
    assert_eq!(beta_commit.evidence_kind, "historical");

    let chunk_id = first_chunk_id(&db);
    let blame = db.git_blame_chunk(chunk_id).unwrap().unwrap();
    assert_eq!(blame.source_text_hash, hex_sha256("# Title\nbeta token\n".as_bytes()));
    assert_eq!(blame.line_count, 2);
    assert_eq!(blame.commit_counts.values().sum::<i64>(), 2);
    assert!(blame.dominant_commit_lines >= 1);
    assert!(blame.dominant_commit.is_some());
    assert_eq!(blame.evidence_kind, "historical");
    let cached = db.git_blame_chunk(chunk_id).unwrap().unwrap();
    assert_eq!(cached.source_text_hash, blame.source_text_hash);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reload_is_skipped_when_head_is_unchanged() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    insert_sentinel_commit(&db);
    drop(db);

    // No file edit and no HEAD movement: the gate must skip the reload, so the sentinel survives.
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 1, "reload should be skipped when HEAD is unchanged");
    // Real history is left intact (the 2 real commits are untouched by the skip).
    assert_eq!(db.status(&config.database).unwrap().git_history.commit_count, 2);

    let _ = fs::remove_dir_all(root);
}
