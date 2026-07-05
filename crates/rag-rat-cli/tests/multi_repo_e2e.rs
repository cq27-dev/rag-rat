//! Multi-repo global-DB integration matrix (memory-sync phase A8, issue #403 — the phase-A exit
//! gate). Two (or three) REAL git fixture repos share ONE consolidated global database, driven
//! end-to-end through the built `rag-rat` binary (the `assert_cmd`-style subprocess pattern the
//! other CLI integration tests use). Every contract the A1–A7 workstreams landed — `repo_id`
//! scoping on reads/sweeps, per-repo write locks, the generation axis, the consolidate importer,
//! the governing-config seam + linked-worktree discovery, the identity gate — is exercised across
//! the shared file, so the matrix goes RED if any of them regresses.
//!
//! ASSERTION-STRENGTH INVARIANT: every scoping assertion pins POSITIVE ACTIVE-REPO IDENTITY — the
//! returned row's owning `repo_id`, a repo-distinct token, or the expected id — never merely a
//! cardinality (`len() == 1`) or the sibling's absence. A count or an emptiness check would still
//! pass if an unscoped `LIMIT 1` returned the SIBLING's row, or if a scoped read leaked and then
//! happened to return the wrong single row; pinning identity is what closes that hole.
//!
//! FIXTURE IDENTITY: commits route through `common::git_commit` (shared helper, #435), which pins
//! `GIT_*_DATE` to a fixed epoch + per-commit counter, so two fixtures get DISTINCT, reproducible
//! `repo_id`s even when their content is byte-identical — the matrix deliberately reuses identical
//! files (same `src/common.rs`, same symbol) to test scoping, and without distinct root commits the
//! two repos would collide into one `repo_id` (#434) and make the whole matrix meaningless.
//!
//! TEST HYGIENE (non-negotiable): every case pins `RAG_RAT_DATA_DIR` (and `RAG_RAT_MODEL_CACHE`)
//! to per-test temp dirs. A keyless config resolves its database through the data-dir cascade —
//! with the env unset that cascade lands on the developer's REAL `~/.local/share/rag-rat` global
//! store, so an unpinned test would read/write a live machine store. The env is pushed to each
//! subprocess via `Command::env` (per-process — never the process-global `std::env::set_var`, which
//! races the thread-based `cargo test`/coverage runner). For the query surfaces the CLI does not
//! expose (memory create/search, symbol lookup, impact) the matrix opens the SAME shared global DB
//! IN-PROCESS through the library, using a config whose `database` is pinned EXPLICITLY at the temp
//! global path — so those opens never touch the env at all and stay race-free under `taskset`.
//!
//! DREAM/RECONCILE SCOPING VERDICT (A8's open question, plan Task A8 item 8): both surfaces are
//! ALREADY fully query-predicate scoped in production — no leak reproduces, so no production
//! fallout fix is warranted. `dream_findings`: `crate::dream::{coverage_gap, stale_reference,
//! sync, dream_run}` every read/supersede/resolve/insert filters `dream_findings.repo_id` (the
//! lifecycle scoping landed with the periphery work; the finding id folds `repo_id` so two repos
//! minting the same `(kind, subject, claim_hash)` cannot collide on the global `id` PK).
//! `reconcile_attempts`: the only scan is `ai/reconcile/status.rs::last_reconcile_status`, which
//! filters `reconcile_attempts.repo_id` before its `ORDER BY … LIMIT 1`; the insert stamps
//! `repo_id` and the finish-update is a point lookup by the global rowid. The A5 memory's
//! "deferred to A8" note is therefore stale — the deferral was closed by the periphery scoping.
//! `dream_worklist_is_repo_scoped_end_to_end` below is the regression pin that keeps it closed.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{git, git_commit, unique_dir};
use rag_rat_core::language::Language;
use rag_rat_core::query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
use rag_rat_core::{Config, IndexDatabase};

/// A body with > `MIN_TOKENS` (20) normalized tokens so it fingerprints as a clone candidate and
/// surfaces a real symbol row. Interpolates the fn name; the arithmetic is identical across names
/// so two of these form a coherent clone class (overlap/max_len == 1.0 ≥ θ).
fn chunky_fn(name: &str) -> String {
    format!(
        "pub fn {name}(x: i64, y: i64) -> i64 {{\n    let a = x + y;\n    let b = a * 2;\n    let \
         c = b - x;\n    let d = c + y;\n    let e = d * 3;\n    let f = e - a;\n    let g = f + \
         b;\n  let h = g - c;\n    h + d + e + f + g\n}}\n"
    )
}

/// Build a committed git repo with a KEYLESS `rag-rat.toml` (no `database` key → the consolidated
/// global store) plus the given `src/<name>` files. `.rag-rat/`, `rag-rat-lib.toml`, and
/// `target/` are gitignored so the working tree stays clean (untracked helper files must not flip
/// the indexer's dirty accounting).
fn keyless_repo(tag: &str, files: &[(&str, String)]) -> PathBuf {
    build_repo(tag, files, "[index]\nroot = \".\"\n\n[target_bindings]\nrust = [\"src\"]\n")
}

/// A committed git repo whose `rag-rat.toml` PINS the legacy per-repo `.rag-rat/index.sqlite` — the
/// pre-flip shape `consolidate` imports from.
fn legacy_pinned_repo(tag: &str, files: &[(&str, String)]) -> PathBuf {
    build_repo(
        tag,
        files,
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
}

fn build_repo(tag: &str, files: &[(&str, String)], config: &str) -> PathBuf {
    let root = unique_dir(tag);
    std::fs::create_dir_all(root.join("src")).unwrap();
    for (name, body) in files {
        let path = root.join("src").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    std::fs::write(root.join("rag-rat.toml"), config).unwrap();
    std::fs::write(root.join(".gitignore"), ".rag-rat/\nrag-rat-lib.toml\ntarget/\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    // Route the commit through the shared helper so a fixed-epoch, per-commit date makes this
    // repo's root commit — and thus its portable repo_id — distinct even from a byte-identical
    // sibling (#434).
    git_commit(&root, &["-q", "-m", "seed"]);
    root
}

/// Run the built binary from `root` (config DISCOVERED — no `--config`) with the store env pinned
/// to the per-test temp dirs.
fn run(root: &Path, data_dir: &Path, model_cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .current_dir(root)
        .env("RAG_RAT_DATA_DIR", data_dir)
        .env("RAG_RAT_MODEL_CACHE", model_cache)
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .args(args)
        .output()
        .unwrap()
}

/// `run`, asserting success, returning stdout.
fn run_ok(root: &Path, data_dir: &Path, model_cache: &Path, args: &[&str]) -> String {
    let out = run(root, data_dir, model_cache, args);
    assert!(
        out.status.success(),
        "`{args:?}` from {} failed: {}",
        root.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn global_db(data_dir: &Path) -> PathBuf {
    data_dir.join("rag-rat.sqlite")
}

/// A config scoped to `root`'s repo but pinned EXPLICITLY at the temp global DB, so `Config::load`
/// never consults `RAG_RAT_DATA_DIR` (race-free under the threaded runner). `root` is an
/// identity-bearing git repo, so the pin-at-a-temp-path never trips the identity gate. The
/// `rag-rat-lib.toml` filename is gitignored and distinct from the CLI's discovered `rag-rat.toml`.
fn scoped_config(root: &Path, data_dir: &Path) -> Config {
    let db = global_db(data_dir);
    let lib_toml = root.join("rag-rat-lib.toml");
    std::fs::write(
        &lib_toml,
        format!(
            "[index]\nroot = \".\"\ndatabase = \"{}\"\n\n[target_bindings]\nrust = [\"src\"]\n",
            common::toml_path(&db)
        ),
    )
    .unwrap();
    Config::load(&lib_toml).unwrap()
}

/// Open the SHARED global DB IN-PROCESS, scoped to `root`'s repo (for the surfaces the CLI does not
/// expose). `open_config` resolves the repo's git identity → registers/adopts it → scopes the
/// connection to it.
fn open_scoped(root: &Path, data_dir: &Path) -> IndexDatabase {
    IndexDatabase::open_config(&scoped_config(root, data_dir)).unwrap()
}

fn conn(data_dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(global_db(data_dir)).unwrap()
}

/// Every registered, non-placeholder repo_id in the shared DB, sorted.
fn real_repo_ids(data_dir: &Path) -> Vec<String> {
    let c = conn(data_dir);
    let mut ids: Vec<String> = c
        .prepare("SELECT repo_id FROM repos WHERE repo_id != '__unassigned__'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    ids.sort();
    ids
}

/// The one registered repo_id that is not in `exclude` (used to name repo B after A is known).
fn repo_id_other_than(data_dir: &Path, exclude: &str) -> String {
    real_repo_ids(data_dir)
        .into_iter()
        .find(|id| id != exclude)
        .expect("a second repo is registered")
}

/// The `repo_id` that OWNS a given `files.id` — the positive-identity pin for a returned row.
fn file_repo_id(data_dir: &Path, file_id: i64) -> String {
    conn(data_dir)
        .query_row("SELECT repo_id FROM files WHERE id = ?1", [file_id], |r| r.get(0))
        .unwrap()
}

/// Per-repo row-count snapshot across the direct- and transitive-scoped tables a small repo
/// populates — INCLUDING the child tables (`repo_memory_bindings`, `clone_edges`) reachable only
/// through their scoped parents, so a gc regression that deleted a sibling's CHILDREN while sparing
/// its parents cannot slip past the parity check. Compared before/after a sibling's gc.
fn repo_snapshot(data_dir: &Path, repo_id: &str) -> Vec<(&'static str, i64)> {
    let c = conn(data_dir);
    let direct = [
        "files",
        "logical_symbols",
        "git_commits",
        "repo_memories",
        "repo_memory_bindings",
        "clone_graph_generations",
        "dream_findings",
        "packages",
        "docs",
    ];
    let mut snap = Vec::new();
    for t in direct {
        let n: i64 = c
            .query_row(&format!("SELECT COUNT(*) FROM {t} WHERE repo_id = ?1"), [repo_id], |r| {
                r.get(0)
            })
            .unwrap();
        snap.push((t, n));
    }
    // Transitive via files.repo_id.
    for (t, alias) in [("symbols", "s"), ("chunks", "ch")] {
        let n: i64 = c
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {t} {alias} JOIN files f ON f.id = {alias}.file_id \
                     WHERE f.repo_id = ?1"
                ),
                [repo_id],
                |r| r.get(0),
            )
            .unwrap();
        snap.push((t, n));
    }
    // clone_edges scope transitively through their generation's repo_id.
    let clone_edges: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM clone_edges e JOIN clone_graph_generations g ON g.generation = \
             e.build_generation WHERE g.repo_id = ?1",
            [repo_id],
            |r| r.get(0),
        )
        .unwrap();
    snap.push(("clone_edges", clone_edges));
    snap
}

fn scoped_file_count(data_dir: &Path, repo_id: &str) -> i64 {
    conn(data_dir)
        .query_row("SELECT COUNT(*) FROM files WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

/// Live-generation file count: after a full rebuild, the live rows sit at the repo's MAX
/// `files.generation` (a prior generation's rows linger until gc). This counts the published set.
fn live_file_count(data_dir: &Path, repo_id: &str) -> i64 {
    let c = conn(data_dir);
    let max_gen: i64 = c
        .query_row(
            "SELECT COALESCE(MAX(generation), 0) FROM files WHERE repo_id = ?1",
            [repo_id],
            |r| r.get(0),
        )
        .unwrap();
    c.query_row(
        "SELECT COUNT(*) FROM files WHERE repo_id = ?1 AND generation = ?2",
        rusqlite::params![repo_id, max_gen],
        |r| r.get(0),
    )
    .unwrap()
}

/// A memory whose body references a `.rs` path → `stale_reference` in any repo whose index does not
/// resolve that path. `gone_path` is the referenced path; `bind_path` is the path binding.
fn ref_memory(title: &str, gone_path: &str, bind_path: &str) -> RepoMemoryCreate {
    RepoMemoryCreate {
        kind: "Invariant".into(),
        title: title.into(),
        body: format!("references the module {gone_path} for the stale-reference check"),
        confidence: "high".into(),
        created_by: Some("a8-matrix".into()),
        source: Some("agent".into()),
        tags: vec![],
        bind: RepoMemoryBindTarget { path: Some(bind_path.into()), ..Default::default() },
    }
}

fn cleanup(dirs: &[&Path]) {
    for d in dirs {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ---------------------------------------------------------------------------------------------
// Case 0: the fixture-identity guarantee the whole matrix rests on — two BYTE-IDENTICAL repos
// still register as two DISTINCT repo_ids in one global DB (the #435 helper closes #434).
// ---------------------------------------------------------------------------------------------
#[test]
fn identical_content_fixtures_get_distinct_repo_ids() {
    let data_dir = unique_dir("c0-data");
    let cache = unique_dir("c0-cache");
    // Byte-identical content in both repos: only common::git_commit's per-commit date makes their
    // root commits — and thus their portable repo_ids — distinct.
    let files = [("common.rs", chunky_fn("twin_symbol"))];
    let repo_a = keyless_repo("c0-a", &files);
    let repo_b = keyless_repo("c0-b", &files);

    let root_commit = |root: &Path| -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    assert_ne!(
        root_commit(&repo_a),
        root_commit(&repo_b),
        "identical-content fixtures must still get distinct root commits"
    );

    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);
    assert_eq!(
        real_repo_ids(&data_dir).len(),
        2,
        "two identical-content repos register as TWO distinct repo_ids in one global DB"
    );

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 1: search / symbol / impact are repo-scoped — each repo's connection sees only its rows,
// and the returned rows are pinned to the ACTIVE repo's identity.
// ---------------------------------------------------------------------------------------------
#[test]
fn search_symbol_and_impact_are_repo_scoped() {
    let data_dir = unique_dir("c1-data");
    let cache = unique_dir("c1-cache");

    // Both repos carry a file at the SAME path (`src/common.rs`) exporting the SAME symbol name
    // (`shared_named_symbol`) — a symbol lookup that returns the sibling's row, or 2 rows, is a
    // repo_id leak. Each repo also has a repo-UNIQUE symbol/file (CLI `query`) and call graph.
    let repo_a = keyless_repo("c1-a", &[
        ("common.rs", chunky_fn("shared_named_symbol")),
        ("a_feature.rs", chunky_fn("alpha_unique_symbol")),
        (
            "a_graph.rs",
            format!(
                "{}pub fn alpha_caller() {{ let _ = alpha_target(1, 2); }}\n",
                chunky_fn("alpha_target")
            ),
        ),
    ]);
    let repo_b = keyless_repo("c1-b", &[
        ("common.rs", chunky_fn("shared_named_symbol")),
        ("b_feature.rs", chunky_fn("beta_unique_symbol")),
        (
            "b_graph.rs",
            format!(
                "{}pub fn beta_caller() {{ let _ = beta_target(1, 2); }}\n",
                chunky_fn("beta_target")
            ),
        ),
    ]);

    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    let a_id = real_repo_ids(&data_dir)[0].clone();
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);
    let b_id = repo_id_other_than(&data_dir, &a_id);
    assert_eq!(real_repo_ids(&data_dir).len(), 2, "one global DB now holds both repos");

    // SEARCH (CLI `query`): repo A's unique file surfaces from A, never from B.
    let a_hits = run_ok(&repo_a, &data_dir, &cache, &["query", "alpha_unique_symbol"]);
    assert!(a_hits.contains("a_feature.rs"), "A's own search finds its file:\n{a_hits}");
    let b_hits = run_ok(&repo_b, &data_dir, &cache, &["query", "alpha_unique_symbol"]);
    assert!(
        !b_hits.contains("a_feature.rs") && !b_hits.contains("alpha_unique_symbol"),
        "B's search must never surface A's rows:\n{b_hits}"
    );

    // SYMBOL (library `symbols`, same-name): each scope returns exactly its one row, and that row
    // is OWNED by the active repo — not the sibling's identically-named row.
    let a_syms = open_scoped(&repo_a, &data_dir)
        .symbols("shared_named_symbol", Some(Language::Rust), 10)
        .unwrap();
    assert_eq!(a_syms.len(), 1, "repo A sees exactly one shared_named_symbol");
    assert_eq!(
        file_repo_id(&data_dir, a_syms[0].file_id),
        a_id,
        "the symbol A resolves is OWNED by repo A, not the sibling's same-named row"
    );
    let b_syms = open_scoped(&repo_b, &data_dir)
        .symbols("shared_named_symbol", Some(Language::Rust), 10)
        .unwrap();
    assert_eq!(b_syms.len(), 1, "repo B sees exactly one shared_named_symbol");
    assert_eq!(
        file_repo_id(&data_dir, b_syms[0].file_id),
        b_id,
        "the symbol B resolves is OWNED by repo B"
    );

    // IMPACT (library `impact_surface`): A's query is non-empty; the SAME query scoped to B — which
    // has no `alpha_target` and whose chunk_fts is repo-scoped — returns nothing.
    let a_impact = open_scoped(&repo_a, &data_dir).impact_surface("alpha_target", 30).unwrap();
    assert!(!a_impact.is_empty(), "impact for A's own symbol is non-vacuous");
    let a_blob = a_impact
        .iter()
        .flat_map(|i| {
            std::iter::once(i.path.clone())
                .chain(i.symbol.clone())
                .chain(std::iter::once(i.reason.clone()))
                .chain(i.evidence.clone())
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !a_blob.contains("b_graph.rs") && !a_blob.contains("beta_caller"),
        "A's impact never leaks B:\n{a_blob}"
    );
    let b_impact = open_scoped(&repo_b, &data_dir).impact_surface("alpha_target", 30).unwrap();
    assert!(b_impact.is_empty(), "impact for A's symbol, scoped to B, sees nothing: {b_impact:?}");

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 2: memories are isolated — identical titles coexist; search/list/doctor pin the ACTIVE
// repo's own memory id, never a count or the sibling's absence.
// ---------------------------------------------------------------------------------------------
#[test]
fn memories_are_isolated_across_repos() {
    let data_dir = unique_dir("c2-data");
    let cache = unique_dir("c2-cache");
    let repo_a = keyless_repo("c2-a", &[("common.rs", chunky_fn("a_anchor"))]);
    let repo_b = keyless_repo("c2-b", &[("common.rs", chunky_fn("b_anchor"))]);
    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);

    // The SAME title in both repos: dedupe must not cross (spec §4.4) and each stays live.
    let common_title = "Shared invariant title";
    let mk = |body: &str| RepoMemoryCreate {
        kind: "Invariant".into(),
        title: common_title.into(),
        body: body.into(),
        confidence: "high".into(),
        created_by: Some("a8".into()),
        source: Some("agent".into()),
        tags: vec![],
        bind: RepoMemoryBindTarget { path: Some("src/common.rs".into()), ..Default::default() },
    };
    let a_mem = open_scoped(&repo_a, &data_dir)
        .memory_create(mk("repo A body carrying the quokkamarker token"))
        .unwrap()
        .memory
        .memory_id;
    let b_mem = open_scoped(&repo_b, &data_dir)
        .memory_create(mk("repo B body carrying the lorikeetmarker token"))
        .unwrap()
        .memory
        .memory_id;
    assert_ne!(a_mem, b_mem, "same title in two repos yields two distinct, non-deduped memories");

    // Both memories exist under distinct repo_ids in the shared DB.
    let live: i64 = conn(&data_dir)
        .query_row("SELECT COUNT(*) FROM repo_memories WHERE title = ?1", [common_title], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(live, 2, "both same-titled memories survive under their own repo_id");

    // memory_search (library, FTS): each scope returns its OWN memory id, never the sibling's.
    let a_found = open_scoped(&repo_a, &data_dir).memory_search("quokkamarker", 10).unwrap();
    assert!(
        a_found.iter().any(|m| m.memory_id == a_mem)
            && a_found.iter().all(|m| m.memory_id != b_mem),
        "A's memory_search returns A's memory id only: {a_found:?}"
    );
    let b_found = open_scoped(&repo_b, &data_dir).memory_search("lorikeetmarker", 10).unwrap();
    assert!(
        b_found.iter().any(|m| m.memory_id == b_mem)
            && b_found.iter().all(|m| m.memory_id != a_mem),
        "B's memory_search returns B's memory id only: {b_found:?}"
    );
    // B's distinctive token — present in no A memory — is unreachable from A's scope.
    let a_cross = open_scoped(&repo_a, &data_dir).memory_search("lorikeetmarker", 10).unwrap();
    assert!(a_cross.is_empty(), "A's FTS scope never matches B's body: {a_cross:?}");

    // memory list (CLI): each repo lists exactly its OWN memory id (not just "some one memory").
    let list_ids = |root: &Path| -> Vec<String> {
        let out = run_ok(root, &data_dir, &cache, &["--json", "memory", "list"]);
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["memory_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(list_ids(&repo_a), vec![a_mem.clone()], "A lists exactly its own memory id");
    assert_eq!(list_ids(&repo_b), vec![b_mem.clone()], "B lists exactly its own memory id");

    // memory doctor: give A a memory bound to a path absent from the index so it validates `gone`;
    // doctor A reports it, doctor B is clean — a sibling repo's gone anchor never surfaces here.
    open_scoped(&repo_a, &data_dir)
        .memory_create(RepoMemoryCreate {
            kind: "Invariant".into(),
            title: "A gone-anchored".into(),
            body: "bound to a vanished path".into(),
            confidence: "high".into(),
            created_by: Some("a8".into()),
            source: Some("agent".into()),
            tags: vec![],
            bind: RepoMemoryBindTarget {
                path: Some("src/vanished_a.rs".into()),
                ..Default::default()
            },
        })
        .unwrap();
    let db_a = open_scoped(&repo_a, &data_dir);
    db_a.memory_validate().unwrap();
    let a_doc = db_a.memory_doctor().unwrap();
    assert!(a_doc.iter().any(|e| e.anchor_status == "gone"), "doctor A flags A's gone anchor");
    let b_doc = open_scoped(&repo_b, &data_dir).memory_doctor().unwrap();
    assert!(
        b_doc.iter().all(|e| e.anchor_status != "gone"),
        "doctor B never touches or flags A's gone anchor: {b_doc:?}"
    );

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 3: clones are per-repo — each repo's listing contains its OWN pair and never the sibling's,
// and precompute of B leaves A's clone generation byte-identical (A5 per-repo generation contract).
// ---------------------------------------------------------------------------------------------
#[test]
fn clones_are_repo_scoped() {
    let data_dir = unique_dir("c3-data");
    let cache = unique_dir("c3-cache");
    // Each repo owns a duplicate pair in repo-distinct files/symbol-names → its own clone class.
    let repo_a = keyless_repo("c3-a", &[
        ("dup_a1.rs", chunky_fn("clone_alpha_one")),
        ("dup_a2.rs", chunky_fn("clone_alpha_two")),
    ]);
    let repo_b = keyless_repo("c3-b", &[
        ("dup_b1.rs", chunky_fn("clone_beta_one")),
        ("dup_b2.rs", chunky_fn("clone_beta_two")),
    ]);
    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    let a_id = real_repo_ids(&data_dir)[0].clone();
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);
    let b_id = repo_id_other_than(&data_dir, &a_id);

    // Precompute A's clone graph, then snapshot A's clone rows; precompute B; A's rows unchanged.
    run_ok(&repo_a, &data_dir, &cache, &["clones", "--precompute"]);
    let clone_snapshot = |repo_id: &str| -> (i64, i64) {
        let c = conn(&data_dir);
        let gens: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM clone_graph_generations WHERE repo_id = ?1",
                [repo_id],
                |r| r.get(0),
            )
            .unwrap();
        let edges: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM clone_edges e JOIN clone_graph_generations g ON \
                 g.generation = e.build_generation WHERE g.repo_id = ?1",
                [repo_id],
                |r| r.get(0),
            )
            .unwrap();
        (gens, edges)
    };
    let a_before = clone_snapshot(&a_id);
    run_ok(&repo_b, &data_dir, &cache, &["clones", "--precompute"]);
    let a_after = clone_snapshot(&a_id);
    assert_eq!(a_before, a_after, "precompute of B must not disturb A's clone generation/edges");
    assert!(a_before.0 >= 1, "A allocated its own clone generation");
    assert!(clone_snapshot(&b_id).0 >= 1, "B allocated its own clone generation");

    // `clones --recall-symbols` (symbol-level recall): each repo lists its OWN pair (present) and
    // never the sibling's (absent) — the symmetric presence + absence assertion.
    let a_recall = run_ok(&repo_a, &data_dir, &cache, &["clones", "--recall-symbols"]);
    assert!(
        a_recall.contains("dup_a1.rs") || a_recall.contains("clone_alpha"),
        "A's own clone pair is present in A's listing:\n{a_recall}"
    );
    assert!(
        !a_recall.contains("dup_b1.rs") && !a_recall.contains("clone_beta"),
        "A's clone listing never contains B's members:\n{a_recall}"
    );
    let b_recall = run_ok(&repo_b, &data_dir, &cache, &["clones", "--recall-symbols"]);
    assert!(
        b_recall.contains("dup_b1.rs") || b_recall.contains("clone_beta"),
        "B's own clone pair is present in B's listing:\n{b_recall}"
    );
    assert!(
        !b_recall.contains("dup_a1.rs") && !b_recall.contains("clone_alpha"),
        "B's clone listing never contains A's members:\n{b_recall}"
    );

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 4: gc of one repo leaves the other byte-identical (parents AND children), and the REPORTED
// counts are per-repo (A7 gc-report scoping — the report counts the active repo, not the union).
// ---------------------------------------------------------------------------------------------
#[test]
fn gc_of_one_repo_leaves_the_other_intact_and_reports_per_repo() {
    let data_dir = unique_dir("c4-data");
    let cache = unique_dir("c4-cache");
    let repo_a =
        keyless_repo("c4-a", &[("a1.rs", chunky_fn("a_one")), ("a2.rs", chunky_fn("a_two"))]);
    let repo_b = keyless_repo("c4-b", &[
        ("b1.rs", chunky_fn("b_one")),
        ("b2.rs", chunky_fn("b_two")),
        ("b3.rs", chunky_fn("b_three")),
    ]);
    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    let a_id = real_repo_ids(&data_dir)[0].clone();
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);
    let b_id = repo_id_other_than(&data_dir, &a_id);

    // Populate B's scoped periphery AND its child tables: clone_edges (precompute) + a memory with
    // a path binding (repo_memory_bindings).
    run_ok(&repo_b, &data_dir, &cache, &["clones", "--precompute"]);
    open_scoped(&repo_b, &data_dir)
        .memory_create(RepoMemoryCreate {
            kind: "Decision".into(),
            title: "B keeps this".into(),
            body: "a durable B decision".into(),
            confidence: "high".into(),
            created_by: Some("a8".into()),
            source: Some("agent".into()),
            tags: vec![],
            bind: RepoMemoryBindTarget { path: Some("src/b1.rs".into()), ..Default::default() },
        })
        .unwrap();

    let b_before = repo_snapshot(&data_dir, &b_id);
    let count = |snap: &[(&str, i64)], t: &str| snap.iter().find(|(k, _)| *k == t).unwrap().1;
    assert_eq!(count(&b_before, "files"), 3, "B has its 3 files: {b_before:?}");
    // Non-vacuous child coverage: the parity check is only meaningful if the children EXIST.
    assert!(count(&b_before, "repo_memory_bindings") >= 1, "B's memory produced a binding row");
    assert!(count(&b_before, "clone_edges") >= 1, "B's precompute produced clone_edge rows");

    // gc repo A (from A's checkout) — parse the report and assert it counts A only.
    let gc_json = run_ok(&repo_a, &data_dir, &cache, &["--json", "gc"]);
    let report: serde_json::Value = serde_json::from_str(&gc_json).unwrap();
    let files_remaining = report["files_remaining"].as_i64().unwrap();
    let a_files = scoped_file_count(&data_dir, &a_id);
    let b_files = scoped_file_count(&data_dir, &b_id);
    assert_eq!(
        files_remaining, a_files,
        "gc REPORTS A's live file count, not the union:\n{gc_json}"
    );
    assert!(
        files_remaining < a_files + b_files,
        "the reported count is strictly below the two-repo union"
    );

    // Repo B is byte-identical — parents AND children (row-count parity across every scoped table).
    let b_after = repo_snapshot(&data_dir, &b_id);
    assert_eq!(b_before, b_after, "gc of A left every one of B's scoped tables untouched");

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 5: `consolidate` of a third LEGACY repo lands its memories into the shared global DB,
// FTS-searchable immediately (the A7 import rebuilds the repo_memory_fts mirror) — and scoped: the
// imported token is reachable from C, hidden from BOTH pre-existing repos.
// ---------------------------------------------------------------------------------------------
#[test]
fn consolidate_lands_a_third_legacy_repos_memories_fts_searchable() {
    let data_dir = unique_dir("c5-data");
    let cache = unique_dir("c5-cache");

    // Two consolidated repos already share the global store.
    let repo_a = keyless_repo("c5-a", &[("common.rs", chunky_fn("a_anchor"))]);
    let repo_b = keyless_repo("c5-b", &[("common.rs", chunky_fn("b_anchor"))]);
    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);

    // A third repo starts LEGACY (pinned per-repo DB), indexed into `.rag-rat/index.sqlite`.
    let repo_c = legacy_pinned_repo("c5-c", &[("common.rs", chunky_fn("c_anchor"))]);
    let legacy = repo_c.join(".rag-rat/index.sqlite");
    run_ok(&repo_c, &data_dir, &cache, &["index", "--full"]);
    assert!(legacy.exists(), "the legacy per-repo index exists");

    // Author a memory into the legacy DB (in-process, pinned at the legacy path — race-free).
    let unique_token = "consolidated_lorikeet_marker";
    let legacy_lib = repo_c.join("rag-rat-lib.toml");
    std::fs::write(
        &legacy_lib,
        format!(
            "[index]\nroot = \".\"\ndatabase = \"{}\"\n\n[target_bindings]\nrust = [\"src\"]\n",
            common::toml_path(&legacy)
        ),
    )
    .unwrap();
    let legacy_mem = {
        let db = IndexDatabase::open_config(&Config::load(&legacy_lib).unwrap()).unwrap();
        db.memory_create(RepoMemoryCreate {
            kind: "Invariant".into(),
            title: "Legacy memory to import".into(),
            body: format!("this body carries the {unique_token} token for FTS"),
            confidence: "high".into(),
            created_by: Some("a8".into()),
            source: Some("agent".into()),
            tags: vec![],
            bind: RepoMemoryBindTarget { path: Some("src/common.rs".into()), ..Default::default() },
        })
        .unwrap()
        .memory
        .memory_id
    };

    // Drop the pin (go keyless) and consolidate: the legacy memory imports into the global store.
    std::fs::write(
        repo_c.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let out = run_ok(&repo_c, &data_dir, &cache, &["consolidate"]);
    assert!(out.contains("imported"), "consolidate reports the import:\n{out}");
    assert!(repo_c.join(".rag-rat/index.sqlite.imported").exists(), ".imported latch is in place");

    // The imported memory is in the GLOBAL DB under repo C's id, and FTS-searchable IMMEDIATELY
    // (the import re-derived the mirror — no reindex needed).
    let global_hit: i64 = conn(&data_dir)
        .query_row("SELECT COUNT(*) FROM repo_memories WHERE id = ?1", [&legacy_mem], |r| r.get(0))
        .unwrap();
    assert_eq!(global_hit, 1, "the legacy memory landed in the global store");
    let found = open_scoped(&repo_c, &data_dir).memory_search(unique_token, 10).unwrap();
    assert!(
        found.iter().any(|m| m.memory_id == legacy_mem),
        "the imported memory is FTS-searchable immediately from C's scope"
    );
    // The import stayed scoped: NEITHER pre-existing repo can reach C's token.
    assert!(
        open_scoped(&repo_a, &data_dir).memory_search(unique_token, 10).unwrap().is_empty(),
        "repo A cannot see C's imported memory"
    );
    assert!(
        open_scoped(&repo_b, &data_dir).memory_search(unique_token, 10).unwrap().is_empty(),
        "repo B cannot see C's imported memory"
    );

    cleanup(&[&repo_a, &repo_b, &repo_c, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 6: concurrency — the per-repo write locks are DISJOINT. The test HOLDS repo B's WriteLock
// explicitly (a genuinely lock-taking write path, not the lockless disjoint-table memory write),
// then runs a full `index --full` of repo A: A acquires ITS OWN flock and completes while B's is
// held. Were the two repos' locks not disjoint, A would block on the held lock and never finish.
// Memory writes on B land under the held lock too.
// ---------------------------------------------------------------------------------------------
#[test]
fn concurrent_index_of_a_and_write_on_b_are_lock_disjoint() {
    let data_dir = unique_dir("c6-data");
    let cache = unique_dir("c6-cache");
    // Give A several files so its `index --full` is a real rebuild.
    let names: Vec<String> = (0..8).map(|i| format!("a{i}.rs")).collect();
    let a_files: Vec<(&str, String)> =
        names.iter().map(|n| (n.as_str(), chunky_fn(&n.replace(['.'], "_")))).collect();
    let repo_a = keyless_repo("c6-a", &a_files);
    let repo_b = keyless_repo("c6-b", &[("common.rs", chunky_fn("b_anchor"))]);
    // Pre-register both repos so the registry lock (which only serializes registration) is out of
    // the picture for the concurrent phase.
    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    let a_id = real_repo_ids(&data_dir)[0].clone();
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);
    let b_id = repo_id_other_than(&data_dir, &a_id);

    // HOLD repo B's per-repo write flock explicitly — the exact lock a B-side writer takes.
    let b_lock_id = rag_rat_core::locks::write_lock_repo_id(&scoped_config(&repo_b, &data_dir));
    let b_lock =
        rag_rat_core::locks::WriteLock::acquire_blocking(&global_db(&data_dir), &b_lock_id)
            .unwrap();

    // With B's lock held, run a full rebuild of A as a subprocess. A takes A's OWN flock.
    let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .current_dir(&repo_a)
        .env("RAG_RAT_DATA_DIR", &data_dir)
        .env("RAG_RAT_MODEL_CACHE", &cache)
        .env("RAG_RAT_NO_WATCH", "1")
        .env("RAG_RAT_HOOK_DISABLE", "1")
        .args(["index", "--full"])
        .spawn()
        .unwrap();

    // Concurrently write memories on B (disjoint-table writes, under the held B flock).
    let db_b = open_scoped(&repo_b, &data_dir);
    for i in 0..6 {
        db_b.memory_create(RepoMemoryCreate {
            kind: "Decision".into(),
            title: format!("B concurrent memory {i}"),
            body: format!("written while repo A was reindexing, iteration {i}"),
            confidence: "high".into(),
            created_by: Some("a8".into()),
            source: Some("agent".into()),
            tags: vec![],
            bind: RepoMemoryBindTarget { path: Some("src/common.rs".into()), ..Default::default() },
        })
        .expect("B's memory write completes while its own flock is held and A is reindexing");
    }
    drop(db_b);

    // A's rebuild must COMPLETE while B's lock is held — the disjointness proof. A bounded poll
    // (rather than a blind `wait`) so a regression that made A contend on B's lock surfaces as a
    // clear failure instead of an indefinite hang.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "A's index --full did not finish while B's per-repo write lock was held — the two \
             repos' locks are NOT disjoint (A is contending on B's lock)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.success(), "A's concurrent full rebuild succeeded");
    drop(b_lock);

    // Both sides landed: B has its 6 memories, A republished all its files.
    let b_mems: i64 = conn(&data_dir)
        .query_row("SELECT COUNT(*) FROM repo_memories WHERE repo_id = ?1", [&b_id], |r| r.get(0))
        .unwrap();
    assert_eq!(b_mems, 6, "all of B's concurrent memory writes completed");
    assert_eq!(live_file_count(&data_dir, &a_id), 8, "A's rebuild republished all its files");

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 7: linked-worktree journey — with NO branch-local config, index/query/memory from the
// linked checkout resolve THROUGH main's config to main's repo in the same global DB (proven by
// listing a MAIN-seeded memory); a divergent branch-local config is ignored with a loud warning;
// `init` from the linked checkout refuses and names main.
// ---------------------------------------------------------------------------------------------
#[test]
fn linked_worktree_resolves_through_main_config() {
    let data_dir = unique_dir("c7-data");
    let cache = unique_dir("c7-cache");

    // Build main with rag-rat.toml GITIGNORED (uncommitted): a linked `--detach` checkout mirrors
    // HEAD, so a committed config would travel to the linked worktree — the point of this case is
    // the NO-branch-config state, exactly what init's linked-worktree refusal leaves behind.
    let main = unique_dir("c7-main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/common.rs"), chunky_fn("main_anchor")).unwrap();
    std::fs::write(main.join(".gitignore"), ".rag-rat/\nrag-rat.toml\nrag-rat-lib.toml\ntarget/\n")
        .unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "."]);
    git_commit(&main, &["-q", "-m", "seed"]);
    // main's config lives on disk only (gitignored → uncommitted).
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();

    run_ok(&main, &data_dir, &cache, &["index", "--full"]);
    let main_id = real_repo_ids(&data_dir)[0].clone();

    // A linked worktree with NO branch-local rag-rat.toml (the post-refusal state).
    let linked = unique_dir("c7-linked");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    assert!(!linked.join("rag-rat.toml").exists(), "the linked checkout carries no branch config");

    // index / query from the linked checkout resolve through main's config to the SAME global DB —
    // no new repo is registered, nothing lands under the linked checkout.
    run_ok(&linked, &data_dir, &cache, &["index"]);
    assert_eq!(
        real_repo_ids(&data_dir),
        vec![main_id.clone()],
        "the linked checkout registered no new repo"
    );
    let q = run_ok(&linked, &data_dir, &cache, &["query", "main_anchor"]);
    assert!(q.contains("common.rs"), "query from the linked checkout reaches main's index:\n{q}");

    // Seed a memory in MAIN's repo; the linked checkout's `memory list` must return THAT id —
    // proving it resolves through the governing main config to main's repo (positive identity),
    // not that it lists some empty repo.
    let main_mem = open_scoped(&main, &data_dir)
        .memory_create(RepoMemoryCreate {
            kind: "Invariant".into(),
            title: "main-only memory".into(),
            body: "lives in the main worktree's repo".into(),
            confidence: "high".into(),
            created_by: Some("a8".into()),
            source: Some("agent".into()),
            tags: vec![],
            bind: RepoMemoryBindTarget { path: Some("src/common.rs".into()), ..Default::default() },
        })
        .unwrap()
        .memory
        .memory_id;
    let linked_list = run_ok(&linked, &data_dir, &cache, &["--json", "memory", "list"]);
    let listed: Vec<String> = serde_json::from_str::<serde_json::Value>(&linked_list)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["memory_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        listed,
        vec![main_mem],
        "the linked checkout resolves main's repo and lists main's memory:\n{linked_list}"
    );
    assert!(
        !linked.join(".rag-rat").exists(),
        "nothing resolved against the linked checkout's own dir"
    );

    // A DIVERGENT branch-local config (a different pinned database) is ignored with a loud warning
    // naming the ignored file and the governing main config — main still governs wholesale.
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"branch-only.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&linked, &data_dir, &cache, &["memory", "list"]);
    assert!(
        out.status.success(),
        "the command still succeeds via main's config: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ignoring") && stderr.contains("main worktree"),
        "the divergent branch config is ignored LOUDLY: {stderr}"
    );
    assert!(
        !linked.join("branch-only.sqlite").exists(),
        "the branch-pinned database was never created"
    );

    // `init` from the linked checkout REFUSES and names the main worktree.
    let init_out = run(&linked, &data_dir, &cache, &["init", "-y", "--force"]);
    assert!(!init_out.status.success(), "init must refuse in a linked worktree");
    let init_err = String::from_utf8_lossy(&init_out.stderr);
    assert!(
        init_err.to_lowercase().contains("linked") || init_err.contains("main"),
        "init's refusal points at the main worktree: {init_err}"
    );

    cleanup(&[&main, &linked, &data_dir, &cache]);
}

// ---------------------------------------------------------------------------------------------
// Case 8: dream_findings query surfaces are repo-scoped end-to-end (the A8 scoping regression pin;
// see the module doc for the reconcile_attempts verdict). stale_reference resolution consults ONLY
// the active repo's files: each repo references a path that exists ONLY in the SIBLING — so a
// union-of-files leak would silently RESOLVE it and suppress the finding.
// ---------------------------------------------------------------------------------------------
#[test]
fn dream_worklist_is_repo_scoped_end_to_end() {
    let data_dir = unique_dir("c8-data");
    let cache = unique_dir("c8-cache");
    // `only_in_a.rs` exists in A only; `only_in_b.rs` in B only.
    let repo_a = keyless_repo("c8-a", &[
        ("common.rs", chunky_fn("a_anchor")),
        ("only_in_a.rs", chunky_fn("only_a_fn")),
    ]);
    let repo_b = keyless_repo("c8-b", &[
        ("common.rs", chunky_fn("b_anchor")),
        ("only_in_b.rs", chunky_fn("only_b_fn")),
    ]);
    run_ok(&repo_a, &data_dir, &cache, &["index", "--full"]);
    let a_id = real_repo_ids(&data_dir)[0].clone();
    run_ok(&repo_b, &data_dir, &cache, &["index", "--full"]);
    let b_id = repo_id_other_than(&data_dir, &a_id);

    // A references a path that resolves ONLY in B; B references one that resolves ONLY in A. A
    // union-of-files stale-reference check would resolve these and emit NO finding — so a present
    // finding proves each dream consults its OWN files only.
    let a_mem = open_scoped(&repo_a, &data_dir)
        .memory_create(ref_memory("A stale", "src/only_in_b.rs", "src/common.rs"))
        .unwrap()
        .memory
        .memory_id;
    let b_mem = open_scoped(&repo_b, &data_dir)
        .memory_create(ref_memory("B stale", "src/only_in_a.rs", "src/common.rs"))
        .unwrap()
        .memory
        .memory_id;

    // Cover the IndexDatabase `dream_model_work_pending` wrapper — the ephemeral zero-work-guard
    // seam the `dream` command consults before cold-starting a paid GPU box. A has an active,
    // never-verified memory, so the model pass has pending work (repo-scoped to A).
    let work_opts = rag_rat_core::dream::DreamOptions {
        now_ms: 0,
        limit: 20,
        verify: true,
        include_reviewed: false,
    };
    assert!(
        open_scoped(&repo_a, &data_dir)
            .dream_model_work_pending(work_opts, 20, true, true)
            .unwrap(),
        "A's model pass has pending work (an un-verified memory) — the guard would provision"
    );

    // dream from A: FLAGS only_in_b.rs (absent from A's files) as stale, and never surfaces B's
    // memory or B's own-resolving path.
    let a_dream = run_ok(&repo_a, &data_dir, &cache, &["--json", "dream"]);
    assert!(
        a_dream.contains("only_in_b.rs"),
        "A's dream flags the sibling-only path as stale — it does NOT consult B's \
         files:\n{a_dream}"
    );
    assert!(
        !a_dream.contains(&b_mem) && !a_dream.contains("only_in_a.rs"),
        "A's dream never surfaces B's finding (nor a stale mark on A's own resolvable \
         file):\n{a_dream}"
    );
    // dream from B: symmetric.
    let b_dream = run_ok(&repo_b, &data_dir, &cache, &["--json", "dream"]);
    assert!(
        b_dream.contains("only_in_a.rs"),
        "B's dream flags the sibling-only path as stale:\n{b_dream}"
    );
    assert!(
        !b_dream.contains(&a_mem) && !b_dream.contains("only_in_b.rs"),
        "B's dream never surfaces A's finding:\n{b_dream}"
    );

    // The persisted worklist rows are partitioned by repo_id, and A's finding is keyed on A's
    // memory subject (the positive-identity pin, not just a count).
    let c = conn(&data_dir);
    let subject_for = |repo_id: &str| -> Vec<String> {
        c.prepare(
            "SELECT subject FROM dream_findings WHERE repo_id = ?1 AND kind = 'stale_reference'",
        )
        .unwrap()
        .query_map([repo_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert!(subject_for(&a_id).contains(&a_mem), "A's stale finding is keyed on A's own memory id");
    assert!(subject_for(&b_id).contains(&b_mem), "B's stale finding is keyed on B's own memory id");
    // No finding id is shared across repos (repo-folded id).
    let ids_for = |repo_id: &str| -> Vec<String> {
        c.prepare("SELECT id FROM dream_findings WHERE repo_id = ?1")
            .unwrap()
            .query_map([repo_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let (a_ids, b_ids) = (ids_for(&a_id), ids_for(&b_id));
    assert!(
        a_ids.iter().all(|id| !b_ids.contains(id)),
        "no dream finding id is shared across repos"
    );

    cleanup(&[&repo_a, &repo_b, &data_dir, &cache]);
}
