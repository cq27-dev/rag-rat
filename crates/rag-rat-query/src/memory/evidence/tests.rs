use crate::memory::fixtures::{self, MemorySeed};

/// A fresh in-memory index at the current schema. `MigrationHooks::noop()` is the
/// documented-sound choice on a fresh scratch DB, keeping these tests engine-free.
fn mem_db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &rag_rat_db::MigrationHooks::noop()).unwrap();
    c
}

/// Point the connection's periphery scope at `repo_id` — mirrors the scope-context write the
/// production open installs.
fn set_repo(c: &Connection, repo_id: &str) {
    c.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    c.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [repo_id],
    )
    .unwrap();
}
use super::*;

/// Seed an active memory under the connection's active repo. Returns its id.
fn seed_memory(c: &Connection, id: &str, title: &str, body: &str, repo_id: &str) {
    fixtures::seed_memory(c, MemorySeed { id, title, body, repo_id, ..MemorySeed::default() });
}

/// Seed a file + one chunk carrying `text`, under `repo_id`. Returns the file id.
fn seed_file(c: &Connection, path: &str, text: &str, repo_id: &str) -> i64 {
    let file_id = fixtures::seed_file(c, path, repo_id);
    let line_count = text.split('\n').count() as i64;
    c.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text_hash) VALUES (?1,'code',0,0,1,?2,'th')",
        rusqlite::params![file_id, line_count],
    )
    .unwrap();
    let chunk_id = c.last_insert_rowid();
    rag_rat_db::chunk_text_store::seed_chunk_text(c, chunk_id, text).unwrap();
    // Mirror production: the chunk is FTS-searchable, so `resolve_identifier`'s verbatim-text
    // tier can narrow to it (`seed_chunk_text` alone populates only `chunk_text`).
    c.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", rusqlite::params![
        chunk_id, text
    ])
    .unwrap();
    file_id
}

fn content_hash(title: &str, body: &str) -> String {
    note_content_hash(title, body)
}

#[test]
fn checked_inputs_hash_folds_child_files_of_a_directory_binding() {
    // Regression (PR #428): a `--dir` binding stores a directory in
    // repo_memory_bindings.path that never equals a files.path, so the inputs hash must fold in
    // the directory's CHILD files — else it stays the empty sentinel and a dir-scoped memory
    // churn-skips with a stale verdict as its files change.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note about the whole module", "r");
    seed_file(&c, "src/dir/a.rs", "fn a() {}\n", "r");
    seed_file(&c, "src/dir/b.rs", "fn b() {}\n", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','dir','src/dir','src/dir','current',0,'r')",
        [],
    )
    .unwrap();
    let scope = Some("r".to_string());
    let before = checked_inputs_hash(&c, "m1", &scope).unwrap();

    // A child file's sha changing must change the directory binding's inputs hash.
    c.execute("UPDATE main.files SET sha256 = 'sha-changed' WHERE path = 'src/dir/b.rs'", [])
        .unwrap();
    let after = checked_inputs_hash(&c, "m1", &scope).unwrap();
    assert_ne!(before, after, "a child file change moves the directory binding's inputs hash");

    // And it is NOT the empty sentinel (children were actually folded in).
    let empty = {
        let d = mem_db();
        set_repo(&d, "r");
        seed_memory(&d, "m2", "t", "note with no bindings", "r");
        checked_inputs_hash(&d, "m2", &scope).unwrap()
    };
    assert_ne!(before, empty, "the directory binding hashed real child files, not the sentinel");
}

#[test]
fn checked_inputs_hash_folds_all_files_for_a_repo_root_binding() {
    // Regression (PR #428): a `--dir .` binding normalizes to an empty path, which
    // matches no `files.path` under the file-or-child pattern — it must instead fold in EVERY
    // indexed file (a root-scoped note is invalidated by any repo change).
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note about the whole repo", "r");
    seed_file(&c, "src/a.rs", "fn a() {}\n", "r");
    seed_file(&c, "docs/b.md", "# b\n", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES ('m1','dir','','','current',0,'r')",
        [],
    )
    .unwrap();
    let scope = Some("r".to_string());
    let before = checked_inputs_hash(&c, "m1", &scope).unwrap();
    // A change to ANY file moves a root binding's inputs hash.
    c.execute("UPDATE main.files SET sha256 = 'sha-x' WHERE path = 'docs/b.md'", []).unwrap();
    assert_ne!(before, checked_inputs_hash(&c, "m1", &scope).unwrap(), "any file change counts");
    let empty = {
        let d = mem_db();
        set_repo(&d, "r");
        seed_memory(&d, "m2", "t", "no bindings", "r");
        checked_inputs_hash(&d, "m2", &scope).unwrap()
    };
    assert_ne!(before, empty, "the root binding folded real files, not the empty sentinel");
}

#[test]
fn directory_binding_does_not_fold_a_like_wildcard_sibling() {
    // Regression (PR #428): a bound dir path with a SQLite LIKE wildcard (`_`) must be
    // escaped, or `src/foo_bar` also matches an unrelated `src/fooXbar/…`, folding a sibling's
    // files into this memory's hash.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note", "r");
    seed_file(&c, "src/foo_bar/a.rs", "fn a() {}\n", "r");
    seed_file(&c, "src/fooXbar/b.rs", "fn b() {}\n", "r"); // the wildcard-collision sibling
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','dir','src/foo_bar','src/foo_bar','current',0,'r')",
        [],
    )
    .unwrap();
    let scope = Some("r".to_string());
    let before = checked_inputs_hash(&c, "m1", &scope).unwrap();
    // Changing the SIBLING (fooXbar) must NOT move the hash — it isn't under the bound dir.
    c.execute("UPDATE main.files SET sha256 = 'sha-x' WHERE path = 'src/fooXbar/b.rs'", [])
        .unwrap();
    assert_eq!(before, checked_inputs_hash(&c, "m1", &scope).unwrap(), "sibling is excluded");
    // Changing the real child DOES move it.
    c.execute("UPDATE main.files SET sha256 = 'sha-y' WHERE path = 'src/foo_bar/a.rs'", [])
        .unwrap();
    assert_ne!(before, checked_inputs_hash(&c, "m1", &scope).unwrap(), "real child is included");
}

#[test]
fn checked_inputs_hash_reflects_the_bound_path_not_just_content() {
    // Regression (PR #428): hashing only unique shas is blind to a rebind that keeps
    // identical content — the (path, sha) multiset must change when the bound path changes.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note", "r");
    seed_file(&c, "src/a.rs", "same\n", "r");
    seed_file(&c, "src/b.rs", "same\n", "r"); // identical content → identical sha-{...}? no: sha-{path}
    // seed_file stamps sha = sha-{path}, so force identical shas to isolate the path axis.
    c.execute("UPDATE main.files SET sha256 = 'same-sha' WHERE path IN ('src/a.rs','src/b.rs')", [
    ])
    .unwrap();
    let bind = |path: &str| {
        c.execute("DELETE FROM repo_memory_bindings WHERE memory_id='m1'", []).unwrap();
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES ('m1','path',?1,?1,'current',0,'r')",
            [path],
        )
        .unwrap();
    };
    let scope = Some("r".to_string());
    bind("src/a.rs");
    let hash_a = checked_inputs_hash(&c, "m1", &scope).unwrap();
    bind("src/b.rs");
    let hash_b = checked_inputs_hash(&c, "m1", &scope).unwrap();
    assert_ne!(
        hash_a, hash_b,
        "rebinding to a same-content file at a different path changes the hash"
    );
}

#[test]
fn checked_inputs_hash_tracks_identifier_resolution_with_no_bound_file() {
    // Regression (PR #428): the churn key must fingerprint the WHOLE evidence pack,
    // not just bound files. A memory with no binding whose identifier flips from
    // NOT_FOUND to a real symbol (the index gained it) must change hash, so an
    // all-NOT_FOUND uncitable memory re-queues once the code adds the symbol.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note about `resolve_marker_token`", "r");
    let scope = Some("r".to_string());
    let before = checked_inputs_hash(&c, "m1", &scope).unwrap(); // identifier resolves NOT_FOUND
    let fid = seed_file(&c, "src/x.rs", "fn f() {}\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','resolve_marker_token','function',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let after = checked_inputs_hash(&c, "m1", &scope).unwrap(); // now resolves to a symbol
    assert_ne!(before, after, "the identifier's resolution flip changes the evidence fingerprint");
}

#[test]
fn evidence_pack_excerpts_respect_the_line_cap_across_a_merged_window() {
    // Regression (PR #428): `identifier_windows` merges adjacent hits into ONE range,
    // so an identifier repeated on hundreds of lines yields a single huge window. The
    // cap must be enforced per-append (clamping the range), not only checked before it,
    // or one push blows past MAX_EXCERPT_LINES and overflows the model prompt.
    let c = mem_db();
    set_repo(&c, "r");
    // 400 lines each mentioning the identifier → one merged window far larger than the cap.
    let body_text = (0..400).map(|_| "let shared_marker_token = 1;").collect::<Vec<_>>().join("\n");
    seed_file(&c, "src/big.rs", &body_text, "r");
    seed_memory(&c, "m1", "t", "a note about `shared_marker_token`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/big.rs','src/big.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let pack = evidence_pack(&c, "m1").unwrap();
    let total: i64 = pack.excerpts.iter().map(|e| e.end_line - e.start_line + 1).sum();
    assert!(total <= MAX_EXCERPT_LINES as i64, "excerpt lines {total} exceed the cap");
}

#[test]
fn a_memory_with_no_live_binding_absent_evidence_is_not_citable() {
    // A note whose every identifier resolves to NOT_FOUND, has no bound-file excerpts,
    // and has no live binding is the pass-0 unverifiable case and must NOT be citable.
    let c = mem_db();
    set_repo(&c, "r");
    // One unrelated indexed file, bound: the exact file declares the note's source domain.
    seed_file(&c, "src/lib.rs", "fn unrelated() {}\n", "r");
    seed_memory(&c, "m1", "t", "a note about `never_defined_symbol` and nothing else", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/lib.rs','src/lib.rs','gone',0,'r')",
        [],
    )
    .unwrap();
    let pack = evidence_pack(&c, "m1").unwrap();
    assert!(!pack.has_live_binding, "a gone binding is not live");
    assert!(!pack.identifiers.is_empty(), "the identifier was extracted");
    assert!(pack.identifiers.iter().all(|id| id.resolution == NOT_FOUND));
    assert!(!pack.is_citable(), "an all-NOT_FOUND, no-excerpt pack is uncitable");
}

#[test]
fn evidence_pack_is_citable_truth_table_for_live_binding_absence_and_evidence_anchors() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn unrelated() {}\n", "r");

    seed_memory(&c, "m-live-absent", "t", "a note about `never_defined_symbol`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m-live-absent','path','src/lib.rs','src/lib.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let live_absent = evidence_pack(&c, "m-live-absent").unwrap();
    assert!(live_absent.has_live_binding, "the current binding is live");
    assert!(
        live_absent.identifiers.iter().all(|id| id.kind == ResolutionKind::Absent),
        "the live-bound regression case must resolve every identifier as absent"
    );
    assert!(live_absent.excerpts.is_empty(), "the unrelated bound file has no identifier hit");
    assert!(
        live_absent.is_citable(),
        "a live-bound all-absent identifier pack is citable for note_ahead"
    );

    let id = |kind| IdentifierResolution {
        identifier: "i".to_string(),
        resolution: "r".to_string(),
        kind,
    };
    let pack = |identifiers, excerpts| EvidencePack {
        memory_id: "m".to_string(),
        identifiers,
        excerpts,
        has_live_binding: false,
    };
    assert!(
        !pack(vec![id(ResolutionKind::Absent)], Vec::new()).is_citable(),
        "an unbound all-absent pack remains uncitable"
    );
    seed_memory(&c, "m-prose-only", "t", "a prose-only note with no code identifiers", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m-prose-only','path','src/lib.rs','src/lib.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let prose_only = evidence_pack(&c, "m-prose-only").unwrap();
    assert!(prose_only.has_live_binding, "the prose-only note still has a live binding");
    assert!(!prose_only.is_citable(), "a prose-only live-bound pack remains uncitable");
    assert!(
        pack(vec![id(ResolutionKind::Symbol)], Vec::new()).is_citable(),
        "a present identifier remains citable"
    );
    assert!(
        pack(Vec::new(), vec![FileExcerpt {
            path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            text: "fn unrelated() {}".to_string(),
        }])
        .is_citable(),
        "a non-empty excerpt remains citable"
    );
}

#[test]
fn checked_inputs_hash_changes_with_binding_state_for_absent_only_pack_without_excerpts() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn unrelated() {}\n", "r");
    seed_memory(&c, "m1", "t", "a note about `never_defined_symbol`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/lib.rs','src/lib.rs','current',0,'r')",
        [],
    )
    .unwrap();

    let live = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    c.execute("UPDATE repo_memory_bindings SET anchor_status = 'gone' WHERE memory_id = 'm1'", [])
        .unwrap();
    let gone = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    assert_ne!(live, gone, "live-binding state changes citability and must move the hash");
}

#[test]
fn checked_inputs_hash_stays_legacy_when_capped_probe_has_bound_excerpt() {
    let c = mem_db();
    set_repo(&c, "r");
    for index in 0..2000 {
        seed_file(&c, &format!("src/decoy-{index:04}.rs"), "foo bar\n", "r");
    }
    seed_file(&c, "src/bound.rs", "fn foo::bar() {}\n", "r");
    seed_memory(&c, "m1", "t", "a note about `never_defined_symbol` and `foo::bar()`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/bound.rs','src/bound.rs','current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    assert!(pack.is_citable(), "the bound-file excerpt makes the pack citable");
    assert!(
        pack.excerpts.iter().any(|excerpt| excerpt.path == "src/bound.rs"),
        "the bound file contains an excerpt for the capped probe target"
    );
    assert!(
        pack.identifiers
            .iter()
            .any(|id| id.identifier == "foo::bar()" && id.kind == ResolutionKind::Unresolvable),
        "the probe must be capped rather than resolving the lower-ranked exact target"
    );

    assert_eq!(
        checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap(),
        "f19579362b7c6c90ecb6942590d4ebd76eb3b592b4a0a3d67f02ea23730894c5",
        "binding state cannot change this already-citable pack's legacy hash"
    );
}

#[test]
fn legacy_hashes_for_unrelated_packs_do_not_requeue() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/present.rs", "fn present_symbol() {}\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','present_symbol','function',0,0)",
        [file_id],
    )
    .unwrap();
    seed_memory(&c, "m-present", "t", "a note about `present_symbol`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m-present','path','src/present.rs','src/present.rs','current',0,'r')",
        [],
    )
    .unwrap();
    seed_memory(&c, "m-prose", "t", "a plain prose note with no code identifiers", "r");
    let scope = Some("r".to_string());
    // Historical preimage fixtures: do not derive these through the production resolver, or
    // a future rendered-resolution drift would move both sides of the assertion together.
    let legacy_hashes = [
        (
            "m-present",
            "a note about `present_symbol`",
            "cdc9c457ac8c6757535f121cc577afcbf15bc8bc21f0450199890a4a3ffe4d1e",
        ),
        (
            "m-prose",
            "a plain prose note with no code identifiers",
            "1f18d650d205d71d934c3646ff5fac1c096ba52eba4cf758b865364f4167d3cd",
        ),
    ];
    for &(memory_id, body, legacy_hash) in &legacy_hashes {
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
             prompt_version, checked_at_ms) VALUES (?1,'r',?2,?3,?4,1000)",
            rusqlite::params![
                memory_id,
                content_hash("t", body),
                legacy_hash,
                VERDICT_PROMPT_VERSION
            ],
        )
        .unwrap();
    }

    let queue = verification_queue(&c, 2000).unwrap();
    assert!(
        queue.is_empty(),
        "unrelated legacy rows requeued (m-present, m-prose): {:?}",
        queue.iter().map(|entry| (&entry.memory_id, entry.reason)).collect::<Vec<_>>()
    );
    for &(memory_id, _, legacy_hash) in &legacy_hashes {
        assert_eq!(
            checked_inputs_hash(&c, memory_id, &scope).unwrap(),
            legacy_hash,
            "{memory_id} keeps the byte-identical legacy hash"
        );
    }
}

#[test]
fn queue_re_enqueues_when_a_gone_binding_becomes_live_for_absent_evidence() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn unrelated() {}\n", "r");
    seed_memory(&c, "m1", "t", "a note about `never_defined_symbol`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/lib.rs','src/lib.rs','gone',0,'r')",
        [],
    )
    .unwrap();
    let gone_hash = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    c.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
         prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,?3,1000)",
        rusqlite::params![
            content_hash("t", "a note about `never_defined_symbol`"),
            gone_hash,
            VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();
    assert!(verification_queue(&c, 2000).unwrap().is_empty(), "baseline row is current");

    c.execute(
        "UPDATE repo_memory_bindings SET anchor_status = 'current' WHERE memory_id = 'm1'",
        [],
    )
    .unwrap();
    let queue = verification_queue(&c, 2000).unwrap();
    assert_eq!(queue.len(), 1, "a live-bound absence pack must be re-evaluated");
    assert_eq!(queue[0].reason, VerificationReason::InputsChanged);
}

#[test]
fn queue_re_enqueues_when_the_verdict_prompt_version_changes() {
    // Regression (PR #428): a stored verdict from an older PROMPT_VERSION is not
    // comparable, so an unchanged memory must re-queue on a prompt bump instead of skipping.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a plain note", "r");
    let inputs = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    c.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
         prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,'an-old-prompt-version',1000)",
        rusqlite::params![content_hash("t", "a plain note"), inputs],
    )
    .unwrap();
    let q = verification_queue(&c, 2000).unwrap();
    assert_eq!(q.len(), 1, "a stale-prompt-version row re-queues");
    assert_eq!(q[0].reason, VerificationReason::PromptChanged);
}

#[test]
fn resolve_bound_files_is_path_sorted_not_rowid_order() {
    // Regression (PR #428): the excerpt-budget cap consumes files in THIS order, so it
    // must be path-sorted (deterministic across reindex), not the volatile `files.id` rowid
    // order the expansion query returns.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a dir note", "r");
    // Insert under src/ in NON-alphabetical order so rowid order != path order.
    for p in ["src/zeta.rs", "src/alpha.rs", "src/mid.rs"] {
        seed_file(&c, p, "fn f() {}\n", "r");
    }
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES ('m1','dir','src','src','current',0,'r')",
        [],
    )
    .unwrap();
    let paths: Vec<String> = resolve_bound_files(&c, "m1", &Some("r".to_string()))
        .unwrap()
        .into_iter()
        .map(|(p, _, _)| p)
        .collect();
    assert_eq!(paths, vec!["src/alpha.rs", "src/mid.rs", "src/zeta.rs"], "path-sorted");
}

#[test]
fn resolve_symbol_returns_all_same_named_matches_as_ambiguous() {
    // Regression (PR #428): a common bare name must surface as AMBIGUOUS (all
    // matches), not silently pinned to the first path-ordered definition — else the
    // model audits against, and the churn key pins to, an unrelated namesake.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note about `shared_name`", "r");
    let mk = |path: &str| {
        let fid = seed_file(&c, path, "fn f() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','shared_name','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
    };
    mk("src/b.rs");
    mk("src/a.rs");
    let locs = resolve_symbol(&c, "shared_name", None).unwrap();
    assert_eq!(locs, vec!["src/a.rs::shared_name", "src/b.rs::shared_name"], "all, path-sorted");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "shared_name", &files).unwrap();
    assert!(res.starts_with("symbols (2):"), "renders ambiguous: {res}");
    assert_eq!(kind, ResolutionKind::Symbol);
}

#[test]
fn resolve_file_segment_returns_all_same_suffix_matches_as_ambiguous() {
    // Regression (PR #428): a shorthand path (`lib.rs`) suffix-matching more than one
    // indexed file must surface as AMBIGUOUS, not pinned to the first — same class as ambiguous
    // symbols, so deleting the file the note meant re-verifies even when a same-suffix file
    // survives.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "a note about `lib.rs`", "r");
    seed_file(&c, "crates/b/lib.rs", "fn f() {}\n", "r");
    seed_file(&c, "crates/a/lib.rs", "fn f() {}\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "lib.rs", &files).unwrap();
    assert!(res.starts_with("files (2):"), "renders ambiguous: {res}");
    assert!(
        res.contains("crates/a/lib.rs") && res.contains("crates/b/lib.rs"),
        "lists both matches: {res}"
    );
    assert_eq!(kind, ResolutionKind::File);
}

#[test]
fn resolve_identifier_ladder_classifies_symbol_file_text_absent_and_unresolvable() {
    // The divergence-false-positive fix: a span that is present in source as a NON-symbol (a DB
    // table name, a local var) resolves to `TextPresent`, NOT the authoritative NOT_FOUND that
    // the model over-read as divergence; a non-code-shaped span that matches nothing is
    // `Unresolvable` (uninformative), never NOT_FOUND; only a name-shaped genuine absence keeps
    // NOT_FOUND.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(
        &c,
        "src/db.rs",
        "fn real_fn() {\n    let local_root_named = 1;\n    conn.execute(\"CREATE TABLE \
         commit_fts(x)\");\n}\n",
        "r",
    );
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','real_fn','function',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    let resolve = |id: &str| resolve_identifier(&c, id, &files).unwrap();

    assert_eq!(resolve("real_fn").1, ResolutionKind::Symbol, "1: a defined symbol");
    // A BARE call to a defined function arg-strips to its name and resolves as a Symbol — NOT a
    // false Absent / mislabeled TextPresent.
    assert_eq!(resolve("real_fn(&db, 1)").1, ResolutionKind::Symbol, "1b: a bare call");
    assert_eq!(resolve("src/db.rs").1, ResolutionKind::File, "2: an indexed file");

    // 3: a DB table name and a local variable — present as literal text, not defined symbols.
    // The resolution is file-INDEPENDENT (names no files) so the pack/churn key stay stable.
    let (res, kind) = resolve("commit_fts");
    assert_eq!(kind, ResolutionKind::TextPresent, "table name in DDL text: {res}");
    assert!(res.contains("appears verbatim"), "labeled present-but-not-a-symbol: {res}");
    assert_eq!(resolve("local_root_named").1, ResolutionKind::TextPresent, "a local variable");

    // 4a: a name-shaped identifier that exists nowhere → the genuine divergence signal.
    let (res, kind) = resolve("no_such_symbol_anywhere");
    assert_eq!(kind, ResolutionKind::Absent);
    assert_eq!(res, NOT_FOUND);

    // A qualified name may denote a field/variant/member the symbol index does not cover.
    // Without a call or a resolvable trailing symbol, its miss is not authoritative absence.
    let (res, kind) = resolve("ConfigSnapshot::trusted_path");
    assert_eq!(kind, ResolutionKind::Unresolvable, "qualified member: {res}");
    assert_ne!(res, NOT_FOUND);

    // 4b: an attribute span, absent from text and not name-shaped → uninformative, NOT
    // NOT_FOUND.
    let (res, kind) = resolve("#[cfg(never_seen_flag)]");
    assert_eq!(kind, ResolutionKind::Unresolvable, "attribute span: {res}");
    assert_ne!(res, NOT_FOUND);
}

#[test]
fn resolve_identifier_treats_memory_id_cross_references_as_non_code() {
    // #678: a memory body that cross-references ANOTHER memory by id (`mem_<hex>_<hex>`, or the
    // common shorthand PREFIX) is not a CODE entity. It resolves `Unresolvable` —
    // uninformative, hidden from the pack — so it is NEITHER the NOT_FOUND
    // code-divergence signal NOR source presence. The presence point matters: a note
    // whose ONLY resolving token is a cross-ref has no code evidence, so it must stay
    // `unverifiable`, not be promoted to the verdict model on the strength of another
    // note existing.
    let c = mem_db();
    set_repo(&c, "r");
    // The referenced memory exists here, but existence does not change the classification — a
    // cross-ref is never code evidence, dangling or not.
    seed_memory(&c, "mem_19f2ad6cf90_2feb75f29ff8", "title", "a cross-referenced decision", "r");
    let files = indexed_file_paths(&c).unwrap();
    let resolve = |id: &str| resolve_identifier(&c, id, &files).unwrap();

    for id in [
        "mem_19f2ad6cf90_2feb75f29ff8",  // an existing memory, full id
        "mem_19f2ad6cf90",               // the timestamp-prefix form agents usually cite
        "mem_deadbeefdead_cafebabecafe", // a dangling reference
    ] {
        let (res, kind) = resolve(id);
        assert_eq!(kind, ResolutionKind::Unresolvable, "{id}: a cross-ref is uninformative: {res}");
        assert!(!kind.is_present(), "{id}: a cross-ref is not source presence: {res}");
        assert_ne!(res, NOT_FOUND, "{id}: a cross-ref is never the code-absence signal: {res}");
    }
}

#[test]
fn resolve_identifier_prefers_a_real_symbol_over_the_memory_id_heuristic() {
    // #678 review: `is_memory_id_shaped` is a SHAPE heuristic — a real code symbol whose name
    // happens to be all-hex (`mem_deadbeefdead`) must still resolve as that SYMBOL, not be
    // hidden as a memory cross-reference. Code resolution runs FIRST; the memory-ref heuristic
    // only reclassifies a span that would otherwise be a genuine NOT_FOUND absence.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(&c, "src/m.rs", "fn mem_deadbeefdead() {}\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','mem_deadbeefdead','function',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "mem_deadbeefdead", &files).unwrap();
    assert_eq!(kind, ResolutionKind::Symbol, "a real hex-named symbol resolves as code: {res}");
}

#[test]
fn resolve_identifier_treats_a_source_mentioned_memory_id_as_a_cross_ref_not_text() {
    // #678 review: even when a `mem_<hex>` id appears VERBATIM in indexed source (a doc comment
    // or a test fixture), it is still a cross-reference to another memory, not code evidence —
    // so it classifies `Unresolvable`, NOT `TextPresent`. The memory-id check runs
    // after symbol/file resolution (real symbols win) but BEFORE the verbatim-text
    // probe.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(
        &c,
        "src/x.rs",
        "// see mem_19f2ad6cf90_2feb75f29ff8 for the rationale\nfn f() {}\n",
        "r",
    );
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "mem_19f2ad6cf90_2feb75f29ff8", &files).unwrap();
    assert_eq!(
        kind,
        ResolutionKind::Unresolvable,
        "a source-mentioned mem-id is a cross-ref, not verbatim text: {res}"
    );
    assert!(!kind.is_present(), "a cross-ref is not source presence even in source: {res}");
}

#[test]
fn resolve_identifier_does_not_mistake_a_segmented_hex_word_for_a_memory_id() {
    // #678 review: `is_memory_id_shaped` keys on the FIRST underscore-delimited segment being a
    // long contiguous hex run (the minted `mem_<hex-timestamp>_<suffix>` shape), NOT the
    // aggregate hex count across underscores. Otherwise an ordinary identifier built from short
    // hex-word chunks — `mem_dead_beef_ca` (4+4+2 = 10 hex) — is misread as a memory cross-ref,
    // and if it appears as source text its legitimate `TextPresent` evidence gets suppressed.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/y.rs", "let mem_dead_beef_ca = 1;\n", "r");
    let files = indexed_file_paths(&c).unwrap();

    // A segmented hex-word local is NOT a memory id: its verbatim source presence survives.
    let (res, kind) = resolve_identifier(&c, "mem_dead_beef_ca", &files).unwrap();
    assert_eq!(
        kind,
        ResolutionKind::TextPresent,
        "a segmented hex word keeps its source presence: {res}"
    );
    assert!(kind.is_present(), "a segmented hex word is not suppressed as a cross-ref: {res}");

    // A genuinely minted id (long first-segment hex run) is still classified as a cross-ref.
    let (_, mem_kind) = resolve_identifier(&c, "mem_19f2ad6cf90_2feb75f29ff8", &files).unwrap();
    assert_eq!(
        mem_kind,
        ResolutionKind::Unresolvable,
        "a real mem-id (long first-segment hex run) still classifies as a cross-ref"
    );
}

#[test]
fn memory_id_shape_splits_full_prefix_and_non_ids() {
    use MemIdShape::{Full, NotAnId, Prefix};
    // Full — two hex segments: both mint sites (`mem_{now:x}_{suffix}`, consolidate `13+12`),
    // plus a trailing-underscore edge. Decisive by shape, no record lookup.
    for id in
        ["mem_19f2ad6cf90_2feb75f29ff8", "mem_deadbeefdead0_cafebabecafe", "mem_deadbeefdead_"]
    {
        assert_eq!(memory_id_shape(id), Full, "{id} is a full two-segment id");
    }
    // Prefix — one long hex segment, no suffix: the shorthand agents cite; ambiguous with a
    // local.
    for id in ["mem_19f2ad6cf90", "mem_deadbeefdead"] {
        assert_eq!(memory_id_shape(id), Prefix, "{id} is a bare timestamp prefix");
    }
    // NotAnId — short first segment, non-hex tail, or no `mem_` prefix.
    for id in ["mem_dead_beef_ca", "mem_19f2ad6cf90_lookup", "mem_copy", "memcpy", "other"] {
        assert_eq!(memory_id_shape(id), NotAnId, "{id} is not a memory id");
    }
}

#[test]
fn resolve_identifier_lets_a_coincidental_hex_local_keep_its_text_presence() {
    // #678 review (Codex P2): a PREFIX-shaped token that is neither a defined symbol nor a
    // recorded memory, but appears verbatim in source, is a coincidental code identifier — a
    // memory-shaped LOCAL misses the symbol tier, so ONLY its source presence carries it. Its
    // `TextPresent` evidence must survive, not be suppressed as a memory cross-reference.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/y.rs", "let mem_deadbeefdead = 1;\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "mem_deadbeefdead", &files).unwrap();
    assert_eq!(
        kind,
        ResolutionKind::TextPresent,
        "a coincidental hex local keeps its source presence: {res}"
    );
    assert!(kind.is_present(), "a coincidental hex local is not suppressed as a cross-ref: {res}");
}

#[test]
fn resolve_identifier_keeps_a_recorded_memorys_prefix_a_cross_ref_even_when_source_mentions_it() {
    // #678 review: a prefix that IS the timestamp of a RECORDED memory stays a cross-reference
    // even when the bare prefix also appears verbatim in indexed text (an ADR/plan doc citing
    // the id) — a cross-ref is never code evidence. Record-confirmation is what
    // distinguishes this from a coincidental hex local (no matching memory), which
    // keeps its text presence above.
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "mem_19f2ad6cf90_2feb75f29ff8", "title", "a decision", "r");
    seed_file(&c, "docs/adr.md", "// relates to mem_19f2ad6cf90\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "mem_19f2ad6cf90", &files).unwrap();
    assert_eq!(
        kind,
        ResolutionKind::Unresolvable,
        "a recorded memory's prefix is a cross-ref, not source text: {res}"
    );
    assert!(!kind.is_present(), "a cross-ref is not source presence: {res}");
    assert_ne!(res, NOT_FOUND, "a cross-ref is never the code-absence signal: {res}");
}

#[test]
fn resolve_identifier_treats_a_dangling_memory_prefix_as_a_cross_ref_not_an_absence() {
    // #678: a prefix matching NO recorded memory, appearing in source only as a SUBSTRING of a
    // longer full id (not at a token boundary), is a dangling cross-reference — `Unresolvable`,
    // and NEVER the NOT_FOUND code-absence signal (a bare code-shaped prefix would otherwise
    // reach the `Absent` terminal). The Prefix arm owns this terminal so the property
    // can't regress.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/z.rs", "// see mem_19f2ad6cf90_2feb75f29ff8\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "mem_19f2ad6cf90", &files).unwrap();
    assert_eq!(kind, ResolutionKind::Unresolvable, "a dangling prefix is a cross-ref: {res}");
    assert_ne!(res, NOT_FOUND, "a dangling prefix is never a code absence: {res}");
}

#[test]
fn resolve_identifier_never_masks_a_deleted_qualified_method_via_a_namesake() {
    // A qualified call is NOT resolved by bare method name (which would match an unrelated
    // NAMESAKE and hide the method's deletion). A PRESENT qualified call resolves through the
    // verbatim-text tier on its arg-stripped NAME. A qualified call is NEVER ruled Absent (a
    // dot-called / external / paraphrased method is too ambiguous to convict) — an absent one
    // is Unresolvable, so it can't false-diverge.
    let c = mem_db();
    set_repo(&c, "r");
    // `from_config` exists as a bare free function (the potential namesake), and the qualified
    // `Present::from_config` appears verbatim in source; `Gone::from_config` does not.
    let fid = seed_file(
        &c,
        "src/m.rs",
        "fn from_config() {}\nlet x = Present::from_config(&cfg);\n",
        "r",
    );
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','from_config','function',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();

    // Present qualified call → verbatim text of the qualified NAME (args paraphrased away).
    let (res, kind) =
        resolve_identifier(&c, "Present::from_config(&config.dream.model)", &files).unwrap();
    assert_eq!(kind, ResolutionKind::TextPresent, "present qualified call resolves: {res}");

    // A qualified call whose qualified NAME is not written verbatim is NEVER false-Absented,
    // whether its bare method survives elsewhere (`Elsewhere::from_config` — `from_config` is a
    // symbol) or vanished entirely (`Gone::vanished_method_xyz`): both are Unresolvable, so a
    // dot-called / external / paraphrased qualified reference can't produce a false divergence.
    // This drops the speculative deleted-qualified-method true-positive to kill a common false
    // positive.
    for call in ["Elsewhere::from_config(x)", "Gone::vanished_method_xyz(y)"] {
        let (res, kind) = resolve_identifier(&c, call, &files).unwrap();
        assert_eq!(kind, ResolutionKind::Unresolvable, "{call} is not a false absence: {res}");
    }
}

#[test]
fn resolver_is_language_neutral_and_never_false_absents_dotted_references() {
    // The resolver must not be Rust-centric in a way that manufactures false divergences for
    // other languages. `::`-qualified handling is Rust/C++-specific, but a `.`-qualified
    // reference (TS/Kotlin/Python/Java `Class.method`, `module.func`) is NEVER ruled Absent:
    // present → verbatim TextPresent, absent → Unresolvable. Bare names (any language,
    // camelCase included) still resolve as Symbol / Absent correctly. So no language
    // gets a false divergence; the only cross-language gap is precision (a
    // `.`-qualified name isn't resolved to a Symbol), which costs at most a missed
    // divergence — the safe direction.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(
        &c,
        "src/svc.ts",
        "class UserService { getUserById(id) { return this.repo.findById(id); } }\n",
        "r",
    );
    // A camelCase symbol (as any indexed language yields) resolves by bare name.
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'typescript','getUserById','method',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    let kind = |id: &str| resolve_identifier(&c, id, &files).unwrap().1;

    assert_eq!(kind("getUserById"), ResolutionKind::Symbol, "camelCase symbol resolves");
    assert_eq!(kind("getUserDeleted"), ResolutionKind::Absent, "a gone bare name is absent");
    // `.`-qualified references: present verbatim → TextPresent; absent → Unresolvable. NEVER
    // Absent — the FP-averse guarantee holds for non-`::` languages.
    assert_eq!(
        kind("UserService.getUserById(id)"),
        ResolutionKind::Unresolvable,
        "a dotted method call is never a false absence"
    );
    assert_eq!(
        kind("this.repo.findById"),
        ResolutionKind::TextPresent,
        "a dotted reference present verbatim is text-present, not absent"
    );
}

#[test]
fn text_presence_is_sound_past_the_rank_cap() {
    // Soundness regression: the old AND-of-tokens + `LIMIT 256` narrowing was UNSOUND — the
    // chunk that literally contains the identifier could rank below the cap among many
    // token-co-occurring chunks (empirically `clone_edges`: 1025 AND-matches ≫ 256) and
    // be dropped → a false `Absent` → a false `memory_divergence` on the very token
    // class the fix targets. The phrase narrowing + scan guard must find the verbatim
    // chunk even when it ranks last behind a flood of higher-ranked phrase-matches that
    // do NOT contain it.
    let c = mem_db();
    set_repo(&c, "r");
    // 300 decoys that phrase-match `"poison token"` twice each (higher bm25) but never contain
    // the underscored identifier; one chunk with the verbatim `poison_token`, ranked last.
    for i in 0..300 {
        seed_file(&c, &format!("src/decoy_{i}.rs"), "poison token poison token\n", "r");
    }
    seed_file(&c, "src/real.rs", "let poison_token = 1;\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "poison_token", &files).unwrap();
    assert_eq!(
        kind,
        ResolutionKind::TextPresent,
        "found the verbatim chunk past the 256 cap: {res}"
    );
}

#[test]
fn checked_inputs_hash_is_stable_against_unrelated_text_presence_churn() {
    // Churn-stability regression: the churn hash must NOT fold the TextPresent path
    // enumeration, or adding an UNRELATED file that happens to carry a cited common
    // token re-keys the memory and re-runs the paid verdict for no reason. A
    // `TextPresent` token contributes a KIND marker only.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/a.rs", "let commit_fts_seen = 1;\n", "r");
    seed_memory(&c, "m1", "t", "note about the `commit_fts_seen` local", "r");
    let before = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    // Another file carrying the same token — unrelated to this memory. Hash must not move.
    seed_file(&c, "src/b.rs", "let commit_fts_seen = 2;\n", "r");
    let after = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    assert_eq!(before, after, "unrelated text-presence file must not re-key the churn hash");
}

#[test]
fn dotted_field_access_is_unresolvable_not_a_false_file_absence() {
    // Regression: field access reads as NOT a file — interior dots
    // (`config.dream.model`) OR a single dot whose extension no indexed file uses
    // (`DreamOptions.verify` — `.verify` is not a source extension) → `Unresolvable`, never
    // `Absent`/NOT_FOUND (a false divergence). A single-extension filename whose extension IS
    // indexed (`.rs`) stays code-shaped, so a genuinely gone one is a real absence.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/only.rs", "fn f() {}\n", "r"); // an indexed `.rs`; nothing else below
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "config.dream.model", &files).unwrap().1,
        ResolutionKind::Unresolvable,
        "interior-dot field access is not a file"
    );
    assert_eq!(
        resolve_identifier(&c, "DreamOptions.verify", &files).unwrap().1,
        ResolutionKind::Unresolvable,
        "single-dot field access with a non-source extension is not a file"
    );
    assert_eq!(
        resolve_identifier(&c, "deleted_module.rs", &files).unwrap().1,
        ResolutionKind::Absent,
        "a bare filename with an indexed extension is a genuine file absence"
    );
}

#[test]
fn deleted_bare_call_is_a_genuine_absence_not_unresolvable() {
    // A note citing a function in CALL form whose function was REMOVED must surface the
    // genuine-absence signal (the deleted-function / note-ahead case), not be silently dropped
    // as Unresolvable. The arg-stripped bare name is name-shaped, so its whole-tree absence is
    // a divergence signal — consistent with tiers 1 and 3, which both arg-strip a bare
    // call. The text tier still diverts a PRESENT expression head (`Ok(None)`) to
    // TextPresent first, so this does not reintroduce a false NOT_FOUND on illustrative
    // expressions.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/x.rs", "fn other() {}\n", "r"); // does not mention `new_helper` at all
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "new_helper()", &files).unwrap();
    assert_eq!(kind, ResolutionKind::Absent, "a removed bare call is a genuine absence: {res}");
    assert_eq!(res, NOT_FOUND);
    assert_eq!(
        resolve_identifier(&c, "build_index(cfg)", &files).unwrap().1,
        ResolutionKind::Absent,
        "a removed bare call carrying args is still a genuine absence"
    );
    // GUARD: an enum-constructor / expression whose head is PRESENT as text stays TextPresent
    // (the text tier resolves it before the terminal), so bare-call absence does not
    // manufacture a false NOT_FOUND on an illustrative expression.
    seed_file(&c, "src/y.rs", "let v = Ok(None);\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "Ok(None)", &files).unwrap().1,
        ResolutionKind::TextPresent,
        "a present expression head is text-present, not a false absence"
    );
}

#[test]
fn renamed_identifier_superstring_does_not_mask_absence() {
    // Token-boundary soundness: when a NON-symbol identifier was renamed to a longer token
    // (`commit_fts` -> `commit_fts_v2`), the old name survives only as a SUBSTRING of the new
    // one. A raw substring match would mask the rename as TextPresent and suppress the absence
    // signal; the verbatim probe must confirm the match at token boundaries, so the gone name
    // resolves as a genuine absence while the renamed-TO token is still found.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/db.rs", "let commit_fts_v2 = open();\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "commit_fts", &files).unwrap();
    assert_eq!(kind, ResolutionKind::Absent, "a rename to a superstring is not masked: {res}");
    assert_eq!(res, NOT_FOUND);
    assert_eq!(
        resolve_identifier(&c, "commit_fts_v2", &files).unwrap().1,
        ResolutionKind::TextPresent,
        "the bounded token itself is still found (boundaries don't reject real matches)"
    );
}

#[test]
fn deleted_file_present_only_as_text_gets_a_file_specific_label() {
    // A path-shaped span that is NOT an indexed file but appears verbatim (in a comment/string)
    // must carry a FILE-specific text-present label, so a memory claiming the file exists can
    // still diverge — the symbol-oriented label only contradicts SYMBOL claims. Symmetry with
    // the "not a defined symbol" case for names.
    let c = mem_db();
    set_repo(&c, "r");
    // An indexed `.md` file establishes `.md` as a COVERED extension (so a `.md` miss is
    // index-authoritative); a comment mentions a DELETED `.md` file verbatim.
    seed_file(&c, "docs/current.md", "# current docs\n", "r");
    seed_file(&c, "src/note.rs", "// see docs/old.md for the legacy format\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "docs/old.md", &files).unwrap();
    assert_eq!(kind, ResolutionKind::TextPresent, "present as text: {res}");
    assert!(res.contains("not an indexed file"), "file-specific label: {res}");
    // A NON-path present token still gets the symbol-oriented label.
    seed_file(&c, "src/t.rs", "let commit_fts = 1;\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, _) = resolve_identifier(&c, "commit_fts", &files).unwrap();
    assert!(res.contains("not a defined symbol"), "a name keeps the symbol label: {res}");
}

#[test]
fn scoped_package_path_with_at_sign_is_recognized_as_a_path() {
    // A scoped package directory (`@scope/`) uses `@`, a valid path char — a deleted but
    // index-covered `.ts` file under it is a genuine file absence, not Unresolvable.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "packages/@scope/app/src/index.ts", "export const x = 1;\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "packages/@scope/app/src/gone.ts", &files).unwrap().1,
        ResolutionKind::Absent,
        "a deleted scoped-package path is a genuine file absence"
    );
}

#[test]
fn unindexed_path_reference_is_unresolvable_not_a_false_absence() {
    // A tier-2 file MISS is a genuine absence only where the index has AUTHORITY over the path:
    // a path whose extension no indexed file uses (`.yml` when only `.rs`/`.md` are indexed), a
    // DIRECTORY path (no file extension), or an out-of-root path is a coverage artifact — the
    // file may well exist on disk, just outside the index — so its miss is Unresolvable, never
    // the NOT_FOUND that reads as a genuine deletion / divergence.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "crates/x/src/lib.rs", "fn f() {}\n", "r"); // `.rs` indexed
    seed_file(&c, "docs/guide.md", "# guide\n", "r"); // `.md` indexed (for the field-access case)
    let files = indexed_file_paths(&c).unwrap();
    for p in [
        ".github/workflows/release.yml", // unindexed extension, out of root
        "crates/x/src/oplog/",           // a directory (trailing slash, no file extension)
        "crates/x/src/oplog",            // a directory (no file extension)
        "/workspace/rag-rat.toml",       // out-of-root, unindexed extension
        "config.docs.md",                /* a bare MULTI-DOT dotted ref (field access), not
                                          * a file */
    ] {
        let (res, kind) = resolve_identifier(&c, p, &files).unwrap();
        assert_eq!(kind, ResolutionKind::Unresolvable, "{p} is not a false file absence: {res}");
        assert_ne!(res, NOT_FOUND);
    }
    // CONTRAST: paths with an INDEXED extension that are genuinely gone stay real absences,
    // including a slashed MULTI-DOT filename — the slash disambiguates a path from the bare
    // multi-dot field access above.
    for gone in ["crates/x/src/deleted.rs", "crates/x/src/schema.test.rs", "gone_module.rs"] {
        assert_eq!(
            resolve_identifier(&c, gone, &files).unwrap().1,
            ResolutionKind::Absent,
            "a gone path with an indexed extension is still a genuine file absence: {gone}"
        );
    }
}

#[test]
fn turbofish_generic_call_resolves_by_callee_name_not_its_generic_args() {
    // A Rust turbofish (`build_index::<Cfg>(cfg)`) must resolve by its CALLEE name — its
    // `::<...>` is a generic-argument marker, not a qualified-path separator. A present
    // one resolves to its symbol; a removed one is a genuine absence (NOT_FOUND), not
    // hidden as Unresolvable.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(&c, "src/x.rs", "fn build_index() {}\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','build_index','function',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "build_index::<Cfg>(cfg)", &files).unwrap().1,
        ResolutionKind::Symbol,
        "a present generic call resolves to its callee symbol"
    );
    assert_eq!(
        resolve_identifier(&c, "gone_index::<Cfg>(cfg)", &files).unwrap(),
        (NOT_FOUND.to_string(), ResolutionKind::Absent),
        "a removed generic call is a genuine absence, not hidden"
    );
    // A turbofish argument carrying a `->` (fn-pointer return type) or a const-generic
    // comparison (`{ N > 0 }`) must not have its inner `>` read as the end of the generic list.
    for gone in ["gone_index::<fn() -> u8>(x)", "gone_index::<{ N > 0 }>()"] {
        assert_eq!(
            resolve_identifier(&c, gone, &files).unwrap(),
            (NOT_FOUND.to_string(), ResolutionKind::Absent),
            "a turbofish arg with `->`/const-generic `>` is stripped cleanly: {gone}"
        );
    }
    // A PRESENT bare turbofish call whose callee appears with different args/generics must not
    // be a false absence — the normalized callee `from_str` is found by its contiguous
    // name.
    seed_file(&c, "src/y.rs", "let v = from_str::<Real>(&body);\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    assert_ne!(
        resolve_identifier(&c, "from_str::<Cfg>(payload)", &files).unwrap().1,
        ResolutionKind::Absent,
        "a present bare turbofish callee (different args) is not a false absence"
    );
}

#[test]
fn qualified_macros_are_conservative_never_a_false_absence() {
    // A `::`-qualified macro can't be disambiguated from an external / imported / moved /
    // namesake macro (the index tracks macros by BARE name only), so it stays CONSERVATIVE
    // (Unresolvable), never NOT_FOUND: a live `tracing::info!()` must not be falsely marked
    // absent, a possibly-removed `crate::gone_macro!()` is uninformative rather than a
    // fabricated divergence, and `old_mod::foo!` is not silently masked by an unrelated
    // same-named macro.
    let c = mem_db();
    set_repo(&c, "r");
    // The macro is external: the source imports and invokes it BARE (`info!`), with no local
    // macro symbol and no verbatim `tracing::info!` spelling.
    seed_file(&c, "src/x.rs", "use tracing::info;\nfn f() { info!(\"hi\"); }\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    for cited in
        ["tracing::info!()", "crate::gone_macro!(x)", "crate::gone_macro![x]", "old_mod::foo!"]
    {
        assert_eq!(
            resolve_identifier(&c, cited, &files).unwrap().1,
            ResolutionKind::Unresolvable,
            "a qualified macro is conservative, never a false absence: {cited}"
        );
    }
}

#[test]
fn present_qualified_turbofish_call_is_text_present_not_hidden() {
    // A PRESENT qualified turbofish call must stay citable. The source carries the generic args
    // verbatim, so the TEXT tier probes the ORIGINAL span — a normalized `HashMap::new` is NOT
    // contiguous in `HashMap::<T>::new()` and would falsely miss (Unresolvable/uncitable).
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/x.rs", "let m = HashMap::<String, Vec<u8>>::new();\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "HashMap::<String, Vec<u8>>::new()", &files).unwrap().1,
        ResolutionKind::TextPresent,
        "a present qualified turbofish call is found verbatim, not hidden"
    );
}

#[test]
fn removed_macro_invocation_is_a_genuine_absence() {
    // A macro is a named code entity: a removed `my_macro!` / `my_macro!(x)` must surface
    // NOT_FOUND, not be hidden as Unresolvable because of the `!`. A present one resolves.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(&c, "src/x.rs", "macro_rules! live_macro { () => {}; }\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','live_macro','macro',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "live_macro!", &files).unwrap().1,
        ResolutionKind::Symbol,
        "a present macro resolves to its symbol"
    );
    for gone in ["gone_macro!", "gone_macro!(x)"] {
        assert_eq!(
            resolve_identifier(&c, gone, &files).unwrap(),
            (NOT_FOUND.to_string(), ResolutionKind::Absent),
            "a removed macro invocation is a genuine absence: {gone}"
        );
    }
}

#[test]
fn removed_macro_is_not_masked_by_a_same_named_non_macro_symbol() {
    // A removed macro `gone_macro!` with a surviving same-named NON-macro (`fn gone_macro`)
    // must surface as a genuine absence, not be masked: the symbol lookup is
    // macro-kind-constrained (no false Symbol), and the text probe searches the
    // INVOCATION form `gone_macro!` (with the bang), which the bare `fn gone_macro`
    // does not satisfy — so it resolves NOT_FOUND.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(&c, "src/x.rs", "fn gone_macro() {}\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','gone_macro','function',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "gone_macro!", &files).unwrap(),
        (NOT_FOUND.to_string(), ResolutionKind::Absent),
        "a removed macro surfaces as absent, not masked by a non-macro namesake"
    );
}

#[test]
fn macro_invocations_with_bracket_or_brace_delimiters_resolve() {
    // Rust macros invoke with `()`, `[]`, or `{}` delimiters. A present `live_vec![x]` resolves
    // to its macro symbol; a removed `gone_vec![x]` / `gone_tl! { .. }` surfaces NOT_FOUND
    // rather than falling through as Unresolvable.
    let c = mem_db();
    set_repo(&c, "r");
    let fid = seed_file(&c, "src/x.rs", "macro_rules! live_vec { () => {}; }\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
         (?1,'rust','live_vec','macro',0,0)",
        rusqlite::params![fid],
    )
    .unwrap();
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "live_vec![x]", &files).unwrap().1,
        ResolutionKind::Symbol,
        "a present macro invoked with [] resolves to its symbol"
    );
    for gone in ["gone_vec![x]", "gone_tl! { a }"] {
        assert_eq!(
            resolve_identifier(&c, gone, &files).unwrap(),
            (NOT_FOUND.to_string(), ResolutionKind::Absent),
            "a removed bracket/brace macro is a genuine absence: {gone}"
        );
    }
}

#[test]
fn present_invoked_macro_without_a_symbol_is_text_present() {
    // A macro invoked in source but not indexed as a symbol (an external macro) resolves via
    // its INVOCATION form in text — present, not a false absence.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/x.rs", "fn f() { ext_macro!(1); }\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    assert_eq!(
        resolve_identifier(&c, "ext_macro!", &files).unwrap().1,
        ResolutionKind::TextPresent,
        "a present invoked macro is found via its invocation form"
    );
}

#[test]
fn flag_prefix_rename_is_not_masked_as_verbatim_presence() {
    // A cited CLI flag whose only source occurrence is a LONGER flag (`--config` vs
    // `--config-file`) must NOT be reported present — `-` is a token char for a flag needle, so
    // the prefix is not a boundary match. The exact longer flag itself is still found.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/cli.rs", "let f = value_of(\"--config-file\");\n", "r");
    let files = indexed_file_paths(&c).unwrap();
    let (res, kind) = resolve_identifier(&c, "--config", &files).unwrap();
    assert_ne!(
        kind,
        ResolutionKind::TextPresent,
        "a flag prefix must not be masked as verbatim presence: {res}"
    );
    assert_eq!(
        resolve_identifier(&c, "--config-file", &files).unwrap().1,
        ResolutionKind::TextPresent,
        "the exact flag is present at a boundary"
    );
}

#[test]
fn is_citable_requires_presence_evidence_not_bare_absence_or_unresolvable_without_live_binding() {
    let id = |kind| IdentifierResolution {
        identifier: "i".to_string(),
        resolution: "r".to_string(),
        kind,
    };
    let pack = |ids: Vec<IdentifierResolution>| EvidencePack {
        memory_id: "m".to_string(),
        identifiers: ids,
        excerpts: Vec::new(),
        has_live_binding: false,
    };
    assert!(
        !pack(vec![id(ResolutionKind::Absent), id(ResolutionKind::Unresolvable)]).is_citable(),
        "a pack of only genuine-absence + uninformative rows carries no evidence — uncitable"
    );
    assert!(
        pack(vec![id(ResolutionKind::Absent), id(ResolutionKind::TextPresent)]).is_citable(),
        "verbatim-text presence is citable evidence"
    );
    assert!(pack(vec![id(ResolutionKind::Symbol)]).is_citable(), "a symbol is citable");
}

#[test]
fn checked_inputs_hash_flips_when_a_name_gains_verbatim_text_presence() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "t", "note about the `commit_fts` table", "r");
    let before = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    // The name now appears as source text → its resolution flips NOT_FOUND → verbatim-text,
    // which the churn key must reflect so a re-verification is queued.
    seed_file(&c, "src/schema.rs", "conn.execute(\"CREATE TABLE commit_fts(id)\");\n", "r");
    let after = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    assert_ne!(before, after, "the resolution-tier flip re-keys the churn hash");
}

#[test]
fn queue_is_in_deterministic_order() {
    let c = mem_db();
    set_repo(&c, "r");
    // Four never-checked memories (same reason/rank) → ordered by memory_id.
    for id in ["m4", "m1", "m3", "m2"] {
        seed_memory(&c, id, "t", "note", "r");
    }
    let q = verification_queue(&c, 1).unwrap();
    assert_eq!(q.iter().map(|e| e.memory_id.as_str()).collect::<Vec<_>>(), vec![
        "m1", "m2", "m3", "m4"
    ]);
}

#[test]
fn evidence_pack_is_byte_identical_across_runs_and_reports_not_found() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "crates/x/src/thing.rs", "fn real_symbol() {}\n", "r");
    c.execute(
        "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) SELECT id, \
         'rust', 'real_symbol', 'function', 0, 0 FROM main.files WHERE path = \
         'crates/x/src/thing.rs'",
        [],
    )
    .unwrap();
    seed_memory(&c, "m1", "t", "refs `real_symbol` and `ghost_symbol`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','crates/x/src/thing.rs','crates/x/src/thing.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let a = evidence_pack(&c, "m1").unwrap();
    let b = evidence_pack(&c, "m1").unwrap();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap(),
        "the pack is byte-identical across runs"
    );
    let resolved = a
        .identifiers
        .iter()
        .find(|i| i.identifier == "real_symbol")
        .expect("real_symbol identifier present");
    assert!(resolved.resolution.starts_with("symbol "), "known symbol resolves");
    let missing = a
        .identifiers
        .iter()
        .find(|i| i.identifier == "ghost_symbol")
        .expect("ghost_symbol identifier present");
    assert_eq!(missing.resolution, NOT_FOUND, "an exact-file-domain miss is authoritative");
}

#[test]
fn evidence_pack_downgrades_absence_when_the_binding_is_outside_index_coverage() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "release config", "the workflow uses `git_release_enable`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','release-plz.toml','release-plz.toml','current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution = pack
        .identifiers
        .iter()
        .find(|identifier| identifier.identifier == "git_release_enable")
        .unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_when_only_some_bindings_are_index_covered() {
    // A note bound to BOTH an indexed source file and an excluded config file: the covered
    // binding must not lend absence authority to an identifier that lives only in the
    // uncovered one — a whole-tree miss for `git_release_enable` stays indeterminate.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "release config", "the workflow uses `git_release_enable`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/lib.rs','src/lib.rs','current',0,'r'), \
         ('m1','path','release-plz.toml','release-plz.toml','current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution = pack
        .identifiers
        .iter()
        .find(|identifier| identifier.identifier == "git_release_enable")
        .unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_for_a_pathless_commit_binding() {
    // A commit/tracker binding stores `path = NULL`: the note is anchored to history or an
    // issue, not to the indexed tree, so a whole-tree miss must not read as an authoritative
    // NOT FOUND just because the pack is otherwise citable.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "historical", "the refactor removed `gone_helper`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','commit','deadbeef',NULL,'current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_keeps_absence_indeterminate_for_an_unbound_note() {
    // An intentionally unbound note is checked against the whole index, but the index is not
    // the whole TREE on a partial index — its misses stay indeterminate whether or not the
    // index is empty (here: non-empty).
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "t", "the note describes `ghost_symbol`", "r");
    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "ghost_symbol").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_for_an_unpersisted_call_path_binding() {
    // A client-supplied call-path hash has no persisted edges to re-resolve against the live
    // graph — unverifiable, so absence stays indeterminate.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','call_path','client-hash',NULL,'current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_for_a_call_path_binding_with_dead_edges() {
    // Persisted edges that no longer resolve against the live graph (fingerprint AND loose
    // identity both miss) mean the path's domain drifted out of the index — indeterminate.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','call_path','hash1',NULL,'current',0,'r')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name) VALUES \
         ('m1','hash1',0,'fp-unknown','deleted_caller','deleted_callee','calls_name',NULL)",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_for_a_call_path_with_a_missing_duplicate_edge() {
    // Two persisted edges share one loose identity, but only ONE live call site remains: the
    // multiset match must not let the survivor vouch for both — the path is incomplete and
    // absence stays indeterminate.
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "fn caller_fn() {}\n", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, target_qualified_name, \
         source_file_id, source_start_line, source_end_line) VALUES \
         ('caller_fn','gone_helper','calls_name','exact',NULL,?1,1,1)",
        [file_id],
    )
    .unwrap();
    seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','call_path','hash1',NULL,'current',0,'r')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
         callee_identity_known) VALUES \
         ('m1','hash1',0,'fp-a','caller_fn','gone_helper','calls_name',NULL,1), \
         ('m1','hash1',1,'fp-b','caller_fn','gone_helper','calls_name',NULL,1)",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_call_path_binding_with_live_edges_keeps_absence_authoritative() {
    // A server-derived call path whose persisted edges still resolve against the live graph
    // (here by loose name/kind/target identity — the fingerprint is unknown) IS index-covered:
    // its identifiers were resolved from this index, so a whole-tree miss is a real absence.
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "fn caller_fn() {}\n", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, target_qualified_name, \
         source_file_id, source_start_line, source_end_line) VALUES \
         ('caller_fn','gone_helper','calls_name','exact',NULL,?1,1,1)",
        [file_id],
    )
    .unwrap();
    seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','call_path','hash1',NULL,'current',0,'r')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
         callee_identity_known) VALUES \
         ('m1','hash1',0,'fp-unknown','caller_fn','gone_helper','calls_name',NULL,1)",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(
        resolution.resolution, NOT_FOUND,
        "a live call path keeps whole-tree absence authoritative"
    );
}

#[test]
fn evidence_pack_downgrades_absence_for_a_mixed_file_and_commit_binding() {
    // An indexed file binding PLUS a pathless commit anchor: the file alone would give
    // absence authority, but identifiers belonging to the historical side of the note live
    // outside the indexed tree — any pathless row keeps absence indeterminate.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "mixed", "the refactor removed `gone_helper`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/lib.rs','src/lib.rs','current',0,'r'), \
         ('m1','commit','deadbeef',NULL,'current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_for_a_repo_root_binding_under_a_partial_index() {
    // A `--dir .` binding in a partially indexed repository (only `crates`/`docs` ingested)
    // must not grant absence authority: an identifier living only in an excluded root TOML or
    // workflow would otherwise read as an authoritative NOT FOUND.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
    seed_memory(&c, "m1", "release config", "the workflow uses `git_release_enable`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES ('m1','dir','','','current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution = pack
        .identifiers
        .iter()
        .find(|identifier| identifier.identifier == "git_release_enable")
        .unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_downgrades_absence_for_a_directory_binding() {
    // A directory binding can hold children the index never ingested (excluded by
    // `target_bindings`), and the index cannot enumerate what it does not contain — so a
    // directory NEVER gives absence authority even when every discovered child is indexed.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/dir/a.rs", "fn a() {}\n", "r");
    seed_file(&c, "src/dir/b.rs", "fn b() {}\n", "r");
    seed_memory(&c, "m1", "module note", "the module keeps `gone_helper` available", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','dir','src/dir','src/dir','current',0,'r')",
        [],
    )
    .unwrap();

    let pack = evidence_pack(&c, "m1").unwrap();
    let resolution =
        pack.identifiers.iter().find(|identifier| identifier.identifier == "gone_helper").unwrap();
    assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
    assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
}

#[test]
fn evidence_pack_excerpt_contains_the_identifier_line() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/lib.rs", "fn top() {}\nfn verification_queue() {}\nfn bottom() {}\n", "r");
    seed_memory(&c, "m1", "t", "the note describes `verification_queue`", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/lib.rs','src/lib.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let pack = evidence_pack(&c, "m1").unwrap();
    assert!(
        pack.excerpts.iter().any(|e| e.text.contains("fn verification_queue()")),
        "the bound-file excerpt contains the identifier's line: {:?}",
        pack.excerpts
    );
}

#[test]
fn a_memory_id_in_a_bound_file_is_not_citable_source_evidence() {
    // #678 review: a note whose only identifier is a `mem_<hex>` cross-ref must NOT become
    // citable just because that id appears verbatim in its bound file. The mem-id is excluded
    // from excerpt windowing (it is Unresolvable, never source evidence), so a cross-ref-only
    // note does not leak to the verdict model via a `path:line:` excerpt.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/x.rs", "// rationale: see mem_19f2ad6cf90_2feb75f29ff8\nfn f() {}\n", "r");
    // The note's ONLY identifier is the memory cross-reference.
    seed_memory(&c, "m1", "t", "background: mem_19f2ad6cf90_2feb75f29ff8", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/x.rs','src/x.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let pack = evidence_pack(&c, "m1").unwrap();
    assert!(
        pack.excerpts.is_empty(),
        "a memory cross-ref must not window a bound-file excerpt: {:?}",
        pack.excerpts
    );
    assert!(!pack.is_citable(), "a cross-ref-only note stays non-citable");
}

#[test]
fn a_coincidental_hex_local_is_citable_and_windows_an_excerpt() {
    // #678 review (Codex P2): the mirror of the cross-ref case above — a note whose only
    // identifier is a coincidental hex LOCAL (`mem_deadbeefdead`, a prefix-shaped token that is
    // no recorded memory) present in its bound file IS real source evidence. It
    // resolves `TextPresent`, so it is NOT excluded from excerpt windowing and the note
    // stays citable — otherwise a genuinely-verifiable memory would be wrongly held
    // back as `unverifiable`.
    let c = mem_db();
    set_repo(&c, "r");
    seed_file(&c, "src/x.rs", "let mem_deadbeefdead = compute();\nfn f() {}\n", "r");
    seed_memory(&c, "m1", "t", "the guard reads mem_deadbeefdead each pass", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/x.rs','src/x.rs','current',0,'r')",
        [],
    )
    .unwrap();
    let pack = evidence_pack(&c, "m1").unwrap();
    assert!(
        pack.excerpts.iter().any(|e| e.text.contains("mem_deadbeefdead")),
        "a coincidental hex local windows its bound-file excerpt: {:?}",
        pack.excerpts
    );
    assert!(pack.is_citable(), "a note anchored by a real source token stays citable");
}
