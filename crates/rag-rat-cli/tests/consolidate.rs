//! Integration: `rag-rat consolidate` imports a repo's legacy per-repo index into the consolidated
//! global database (phase A7). A pinned `database` key (the exact shape pre-flip `init` rendered)
//! is REFUSED with the remove-the-key remedy — no import, no side effects — so there is never a
//! divergence window between an early import and a later rename; the single post-removal run
//! imports and renames atomically. Idempotent afterwards.
//!
//! Runs the built binary in a subprocess with `RAG_RAT_DATA_DIR` set (per-process env — never races
//! the thread-based `cargo test` runner), so the global store lands in a throwaway temp dir and
//! never touches a developer's real `~/.local/share/rag-rat`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

mod common;

use common::{ScratchRoot, git, git_commit, unique_dir};

/// A git fixture repo with one rust file and an EXPLICIT `database` key — the pre-flip `init`
/// shape — so the initial index builds the LEGACY per-repo `.rag-rat/index.sqlite`.
///
/// `model = "none"` (the hash embedder) is DELIBERATE and must stay: it pins the active-model id to
/// a resolution-independent constant, so no test in this file depends on whether the default
/// fastembed model resolves to its legacy id or the post-#317 canonical name (network/cache
/// dependent). Without it `consolidating_a_second_repo_leaves_the_first_repos_heal_owed_meta` was
/// environment-flaky. The seeded heal-owed witness (a legacy model id set directly in the DB) still
/// mismatches this `none` config, so the "consolidate must not heal a sibling's meta" assertion is
/// unchanged — just deterministic offline.
fn fixture_repo() -> ScratchRoot {
    let root = unique_dir("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn consolidate_anchor() {}\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[llm.embedding]\nmodel = \
         \"none\"\n\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    git_commit(&root, &["-q", "-m", "seed"]);
    root
}

fn run(root: &Path, data_dir: &Path, model_cache: &Path, args: &[&str]) -> Output {
    let binary = env!("CARGO_BIN_EXE_rag-rat");
    Command::new(binary)
        .current_dir(root)
        .env("RAG_RAT_DATA_DIR", data_dir)
        .env("RAG_RAT_MODEL_CACHE", model_cache)
        .env("RAG_RAT_NO_WATCH", "1")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn consolidate_refuses_a_pinned_config_then_completes_after_key_removal() {
    let root = fixture_repo();
    let data_dir = unique_dir("data");
    let model_cache = unique_dir("cache");
    let legacy = root.join(".rag-rat/index.sqlite");
    let imported = root.join(".rag-rat/index.sqlite.imported");
    let global = data_dir.join("rag-rat.sqlite");

    // Build the LEGACY per-repo index (the explicit `database` key targets
    // `.rag-rat/index.sqlite`).
    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(legacy.exists(), "the legacy per-repo index was built");
    assert!(!global.exists(), "the global store does not exist before consolidate");

    // A pinned `database` key is REFUSED outright: no import, no global store, no rename — an
    // early import would open a divergence window in which legacy-side memory edits are silently
    // dropped by the idempotent finishing run.
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(!out.status.success(), "a pinned config must be refused, not imported");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Remove the `database` key"), "the refusal names the remedy: {stderr}");
    assert!(!global.exists(), "REFUSED: no global store was created");
    assert!(legacy.exists(), "REFUSED: the legacy file is untouched");
    assert!(!imported.exists(), "REFUSED: no .imported marker");

    // The refused repo is untouched: its next index run still uses the legacy DB.
    let out = run(&root, &data_dir, &model_cache, &["index"]);
    assert!(
        out.status.success(),
        "a refused repo keeps indexing its legacy DB: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(legacy.exists(), "the legacy DB is still the pinned repo's live index");

    // Remove the `database` key (the config goes keyless) and re-run: the SINGLE completing run
    // imports and renames atomically — no window in which legacy-side edits can be lost.
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(
        out.status.success(),
        "the post-removal run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("imported"), "the completing run reports the import: {stdout}");
    assert!(global.exists(), "the global store now exists");
    assert!(!legacy.exists(), "the completing run renamed the legacy file away");
    assert!(imported.exists(), "the .imported marker is in place");
    // No WAL-sidecar litter remains at the legacy path (they travel with the archive or were
    // folded by the checkpoint — either way the directory is clean).
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = legacy.as_os_str().to_os_string();
        sidecar.push(suffix);
        assert!(
            !std::path::Path::new(&sidecar).exists(),
            "no bare {suffix} sidecar remains beside the renamed legacy file"
        );
    }

    // A further re-run is a no-op: the `.imported` marker LATCHES the keyless config onto the
    // global store, so resolution now lands there directly (`already_global`).
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success(), "re-run failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already_global"), "re-run is a no-op via the latch: {stdout}");
}

/// A CUSTOM pinned `database` path gets the path-aware remedy: the refusal prints the literal move
/// to the default legacy location (keyless resolution never consults a custom path, so removing
/// the key alone would strand the index as `no_legacy_index` and its memories would never be
/// imported). Following the printed remedy — move, remove the key, re-run — imports the custom
/// index end-to-end.
#[test]
fn consolidate_custom_pin_remedy_moves_then_imports() {
    let root = unique_dir("custom");
    let data_dir = unique_dir("customdata");
    let model_cache = unique_dir("customcache");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn custom_pin_anchor() {}\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"custom/my-index.sqlite\"\n\n[llm.embedding]\nmodel = \
         \"none\"\n\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    git_commit(&root, &["-q", "-m", "seed"]);

    let custom = root.join("custom/my-index.sqlite");
    let default_legacy = root.join(".rag-rat/index.sqlite");
    let global = data_dir.join("rag-rat.sqlite");

    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(custom.exists(), "the custom-pinned index was built");

    // Refused with the CUSTOM remedy: the literal move to the default location.
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(!out.status.success(), "a custom pin must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The remedy interpolates real paths via `Path::display()` (native `\` on Windows); normalize
    // separators for the suffix checks — the message content, not the separator, is what matters.
    let stderr_norm = stderr.replace('\\', "/");
    assert!(stderr.contains("mv "), "the custom remedy prints the literal move: {stderr}");
    assert!(
        stderr_norm.contains("my-index.sqlite") && stderr_norm.contains(".rag-rat/index.sqlite"),
        "the move names the user's actual paths: {stderr}"
    );
    assert!(custom.exists(), "REFUSED: the custom index is untouched");
    assert!(!global.exists(), "REFUSED: no global store was created");

    // Follow the remedy: move the file to the default location, drop the key, re-run.
    fs::create_dir_all(default_legacy.parent().unwrap()).unwrap();
    fs::rename(&custom, &default_legacy).unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(
        out.status.success(),
        "the post-move run must import: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("imported"), "the moved index imported: {stdout}");
    assert!(global.exists(), "the global store now exists");
    assert!(!default_legacy.exists(), "the moved file was renamed to the marker");
    assert!(root.join(".rag-rat/index.sqlite.imported").exists(), "marker in place");
}

/// Consolidating a SECOND repo into a one-repo global DB must not heal/mutate the FIRST repo's
/// meta: consolidate's pre-registration migration is schema-only, because on that config-less
/// connection the open-time healers' witness would resolve the sole registered repo — the SIBLING
/// of the incoming one — and a heal-owed pass would clear its model meta under only the incoming
/// repo's locks.
#[test]
fn consolidating_a_second_repo_leaves_the_first_repos_heal_owed_meta() {
    let data_dir = unique_dir("seconddata");
    let model_cache = unique_dir("secondcache");
    let global = data_dir.join("rag-rat.sqlite");

    // Repo 1: index under a pin, then complete the keyless consolidate.
    let repo1 = fixture_repo();
    let out = run(&repo1, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "repo1 index: {}", String::from_utf8_lossy(&out.stderr));
    fs::write(
        repo1.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&repo1, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success(), "repo1 consolidate: {}", String::from_utf8_lossy(&out.stderr));

    // Give repo 1 a HEAL-OWED model meta (a legacy model id) directly in the global DB.
    let repo1_id: String = {
        let conn = rusqlite::Connection::open(&global).unwrap();
        let id: String = conn
            .query_row(
                "SELECT repo_id FROM repos WHERE repo_id != '__unassigned__' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO repo_meta(repo_id, key, value) VALUES (?1, \
             'active_embedding_model', 'fastembed-all-minilm-l6-v2')",
            [&id],
        )
        .unwrap();
        id
    };

    // Repo 2: index under a pin, then consolidate INTO the one-repo global DB.
    let repo2 = fixture_repo();
    let out = run(&repo2, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "repo2 index: {}", String::from_utf8_lossy(&out.stderr));
    fs::write(
        repo2.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&repo2, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success(), "repo2 consolidate: {}", String::from_utf8_lossy(&out.stderr));

    // Repo 1's heal-owed meta is UNTOUCHED — consolidate never healed the sibling.
    let conn = rusqlite::Connection::open(&global).unwrap();
    let survived: Option<String> = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = 'active_embedding_model'",
            [&repo1_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        survived.as_deref(),
        Some("fastembed-all-minilm-l6-v2"),
        "consolidating repo 2 healed (cleared) repo 1's model meta through the sibling witness",
    );
}

/// Guard ordering: a config still PINNED at a missing/renamed path must be REFUSED with the
/// remedy — not exit happily as `no_legacy_index` while the repo stays stranded on the pin (the
/// next `rag-rat index` would recreate an empty per-repo DB there). A pin at the global target
/// itself is genuinely `already_global`.
#[test]
fn consolidate_refuses_a_pin_at_a_missing_path_and_accepts_a_pin_at_global() {
    let root = fixture_repo(); // pinned at .rag-rat/index.sqlite, never indexed → path missing
    let data_dir = unique_dir("pinorderdata");
    let model_cache = unique_dir("pinordercache");

    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(!out.status.success(), "a pin at a MISSING path must still be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Remove the `database` key"),
        "the refusal (not no_legacy_index) surfaces with the remedy: {stderr}"
    );

    // A pin at the global target itself is the one genuinely-fine pin: already_global.
    let global = data_dir.join("rag-rat.sqlite");
    fs::write(
        root.join("rag-rat.toml"),
        format!(
            "[index]\nroot = \".\"\ndatabase = \"{}\"\n\n[llm.embedding]\nmodel = \
             \"none\"\n\n[target_bindings]\nrust = [\"src\"]\n",
            common::toml_path(&global)
        ),
    )
    .unwrap();
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success(), "a pin at the global target is already_global");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already_global"), "pin-at-global reports already_global: {stdout}");
}

/// The legacy-side lock drain covers the source DB's OWN registered ids, not just the current
/// identity (Codex batch 5): a legacy index created under a `local:` shallow id and deepened
/// before consolidating derives a PORTABLE identity — a current-identity lock alone would not
/// conflict with a pre-deepen watcher still holding the `local:` lock, letting the snapshot +
/// rename race its writes into the renamed artifact. Consolidate peeks the source's `repos`
/// registry and drains EVERY recorded id's legacy-side lock (canonical order, bounded), so it
/// BLOCKS while the pre-deepen writer runs and completes once the lock is released.
#[test]
fn consolidate_drains_a_pre_deepen_writers_legacy_lock() {
    let root = fixture_repo();
    let data_dir = unique_dir("draindata");
    let model_cache = unique_dir("draincache");
    let legacy = root.join(".rag-rat/index.sqlite");

    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();

    // The legacy DB's registry records the OLD `local:` id it was created under (the pre-deepen
    // identity); the checkout now derives portable.
    {
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
             ('local:predeepen00', 's', 0)",
            [],
        )
        .unwrap();
    }

    // A simulated PRE-DEEPEN writer: holds the `local:` id's legacy-side flock (the exact lock a
    // watcher/memory command started before the unshallow would hold).
    let lock_path = rag_rat_base::locks::write_lock_path(&legacy, "local:predeepen00");
    let lock_file =
        fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).unwrap();
    lock_file.lock().unwrap();

    // Consolidate must BLOCK on the outgoing lock (bounded, 30s) — not race the writer.
    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .current_dir(&root)
        .env("RAG_RAT_DATA_DIR", &*data_dir)
        .env("RAG_RAT_MODEL_CACHE", &*model_cache)
        .env("RAG_RAT_NO_WATCH", "1")
        .arg("consolidate")
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        child.try_wait().unwrap().is_none(),
        "consolidate must block on the pre-deepen writer's local: lock, not race it"
    );

    // Writer finishes → consolidate acquires the drained lock set and completes the import.
    lock_file.unlock().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "consolidate completes once the pre-deepen writer releases");
    assert!(
        root.join(".rag-rat/index.sqlite.imported").exists(),
        "the import + rename completed after the drain"
    );
}

/// OLD-VINTAGE sources (Codex batch 6): a legacy DB this binary never opened keeps its model meta
/// in the pre-V039 `index_meta`/`reconcile_meta` tables, while the import reads `repo_meta` only —
/// consolidate therefore runs the migration LADDER on the source first (under the source-side
/// locks, before the snapshot; the file is about to be renamed `.imported` anyway), letting
/// V039/V040's own migrations relocate the meta instead of the importer re-implementing dual-path
/// reads. The model-state unit must arrive complete on the global side.
#[test]
fn consolidate_migrates_an_old_vintage_source_so_model_meta_arrives() {
    let root = fixture_repo();
    let data_dir = unique_dir("vintagedata");
    let model_cache = unique_dir("vintagecache");
    let legacy = root.join(".rag-rat/index.sqlite");
    let global = data_dir.join("rag-rat.sqlite");

    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));

    // Doctor the legacy file into the pre-V039 vintage: model meta lives in the OLD global
    // `index_meta` table, and the schema ledger stops at V038 so the ladder re-runs the
    // relocation migrations on the next apply.
    {
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'active_embedding_model'", []).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('active_embedding_model', \
             'vintage-model')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM schema_version WHERE CAST(substr(id, 1, 3) AS INTEGER) > 38", [])
            .unwrap();
    }

    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success(), "consolidate failed: {}", String::from_utf8_lossy(&out.stderr));

    // The model identity crossed: the source's ladder run relocated it into repo_meta, and the
    // import carried it into the global store under the repo's id.
    let conn = rusqlite::Connection::open(&global).unwrap();
    let carried: Option<String> = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE key = 'active_embedding_model' AND repo_id != \
             '__unassigned__'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        carried.as_deref(),
        Some("vintage-model"),
        "an old-vintage source's model meta must arrive with the import (ladder-relocated)",
    );
}

/// Batch-7 finding 3: a config explicitly PINNED AT the global path can coexist with a lingering,
/// never-imported legacy `.rag-rat/index.sqlite` (the pin was added by hand — no consolidate run
/// ever renamed the old per-repo DB). `already_global` there would strand the legacy file's
/// authored memories while claiming success, so the arm probes the default legacy path and
/// IMPORTS it — the one pinned shape where proceeding is strictly correct, because the pin
/// already names the target and the rename cannot strand the config. The marker then latches the
/// re-run back to `already_global`.
#[test]
fn consolidate_imports_a_lingering_legacy_under_a_pin_at_global() {
    let root = fixture_repo();
    let data_dir = unique_dir("pinlegacydata");
    let model_cache = unique_dir("pinlegacycache");
    let legacy = root.join(".rag-rat/index.sqlite");
    let imported = root.join(".rag-rat/index.sqlite.imported");
    let global = data_dir.join("rag-rat.sqlite");

    // Build the legacy per-repo index under the default pin, then hand-edit the pin to the
    // GLOBAL path — the "I consolidated by editing the config" mistake.
    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(legacy.exists());
    fs::write(
        root.join("rag-rat.toml"),
        format!(
            "[index]\nroot = \".\"\ndatabase = \"{}\"\n\n[llm.embedding]\nmodel = \
             \"none\"\n\n[target_bindings]\nrust = [\"src\"]\n",
            common::toml_path(&global)
        ),
    )
    .unwrap();

    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(
        out.status.success(),
        "pin-at-global with a lingering legacy imports: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("imported"), "the run reports an import, not already_global: {stdout}");
    assert!(global.exists(), "the global store exists");
    assert!(!legacy.exists(), "the lingering legacy file was renamed away");
    assert!(imported.exists(), "the .imported marker is in place");

    // With the marker present, the arm returns to `already_global` — idempotent.
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success(), "re-run failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already_global"), "the marker latches the re-run: {stdout}");
}

/// The worktree dimension of consolidate (batch-7 directive): run FROM a LINKED worktree whose
/// checked-out `rag-rat.toml` still carries the old pinned key while MAIN's config went keyless.
/// The governing seam resolves the WHOLE config from main — so consolidate finds MAIN's legacy
/// file, imports it, and renames it beside MAIN's checkout; the divergent branch-local file is
/// ignored with a warning naming it. No paths ever resolve against the linked checkout.
#[test]
fn consolidate_from_a_linked_worktree_uses_the_main_config_and_legacy() {
    let root = fixture_repo(); // committed toml pins .rag-rat/index.sqlite
    let data_dir = unique_dir("wtdata");
    let model_cache = unique_dir("wtcache");
    let legacy = root.join(".rag-rat/index.sqlite");
    let imported = root.join(".rag-rat/index.sqlite.imported");
    let global = data_dir.join("rag-rat.sqlite");

    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(legacy.exists());

    // The linked worktree checks out the COMMITTED (pinned) config; main goes keyless.
    let linked = unique_dir("wtlinked");
    git(&root, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();

    let out = run(&linked, &data_dir, &model_cache, &["consolidate"]);
    assert!(
        out.status.success(),
        "consolidate from the linked worktree completes via main's config: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("imported"), "main's legacy index was imported: {stdout}");
    assert!(!legacy.exists(), "MAIN's legacy file was renamed away");
    assert!(imported.exists(), "the marker lands beside MAIN's legacy path");
    assert!(global.exists(), "the global store exists");
    assert!(
        !linked.join(".rag-rat").exists(),
        "nothing resolved against the linked checkout's own .rag-rat"
    );
    assert!(
        stderr.contains("ignoring") && stderr.contains("main worktree"),
        "the divergent branch-local config is ignored LOUDLY: {stderr}"
    );

    // Idempotent from either checkout.
    let out = run(&linked, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("already_global"));
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("already_global"));
}

/// Consolidation opens its target through the schema-only path, so it must explicitly run the
/// store-global `/3` projector upgrade before reconcile's projection anti-join. A retry after the
/// import committed but the legacy rename did not is the dangerous shape: the immutable content
/// op already exists, and stale/missing projection rows would make reconcile append it again.
#[test]
fn consolidate_rebuilds_stale_content_projection_before_reconcile() {
    let root = fixture_repo();
    let data_dir = unique_dir("projectordata");
    let model_cache = unique_dir("projectorcache");
    let legacy = root.join(".rag-rat/index.sqlite");
    let imported = root.join(".rag-rat/index.sqlite.imported");
    let global = data_dir.join("rag-rat.sqlite");

    let out = run(&root, &data_dir, &model_cache, &["index", "--full"]);
    assert!(out.status.success(), "index --full failed: {}", String::from_utf8_lossy(&out.stderr));
    {
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        let repo_id: String = conn
            .query_row(
                "SELECT repo_id FROM repos WHERE repo_id != '__unassigned__' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id) VALUES ('projector-memory', \
             'Invariant', 'projector', 'body', 'high', 'active', 0, 0, 'test', 'v1', ?1)",
            [&repo_id],
        )
        .unwrap();
    }

    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \"none\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(
        out.status.success(),
        "first consolidate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Emulate the import-committed/rename-not-landed retry window, then doctor the target into an
    // old projector's state: accepted content remains authoritative, but its projection and stamp
    // are missing. Without the pre-reconcile rebuild, the anti-join authors a second NodeCreate.
    fs::rename(&imported, &legacy).unwrap();
    {
        let conn = rusqlite::Connection::open(&global).unwrap();
        let before: i64 =
            conn.query_row("SELECT COUNT(*) FROM content_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(before, 1, "the first consolidate authored exactly one content op");
        conn.execute_batch(
            "DELETE FROM content_projected_nodes WHERE node_id = 'projector-memory';
             DELETE FROM oplog_meta WHERE key = 'content_projector_version';",
        )
        .unwrap();
    }

    let out = run(&root, &data_dir, &model_cache, &["consolidate"]);
    assert!(
        out.status.success(),
        "retry consolidate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let conn = rusqlite::Connection::open(&global).unwrap();
    let content_entries: i64 =
        conn.query_row("SELECT COUNT(*) FROM content_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(content_entries, 1, "stale projection must not cause duplicate content authoring");
    let projected: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'projector-memory'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projected, 1, "consolidate rebuilt the missing content projection");
    let stamp: String = conn
        .query_row(
            "SELECT value FROM oplog_meta WHERE key = 'content_projector_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // Pinned as a literal because the constant is oplog-internal and exporting it would make a
    // projector detail a public commitment for one assertion's sake. Move this with the next
    // CONTENT_PROJECTOR_VERSION bump.
    assert_eq!(stamp, "5", "consolidate upgraded the store-global projector stamp");
}
