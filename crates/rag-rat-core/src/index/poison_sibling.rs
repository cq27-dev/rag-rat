//! Poison-sibling test harness (`#[cfg(test)]` only).
//!
//! GOAL: end the review-driven discovery loop for repo-scoping bugs. Every unscoped read / count /
//! delete / resume in the engine should fail an EXISTING test **locally** rather than surface in a
//! reviewer's comment. The mechanism: after a fixture DB reaches its ready state (the tail of
//! [`IndexDatabase::rebuild_with_progress`]), register a SECOND repo (`poison-sibling`) directly
//! via SQL — `register_repo` refuses a second real repo before A7, so the harness seeds it the same
//! way `multi_repo_scope`'s `two_repo_fixture` does — and hang tripwire rows off it in every table
//! that carries a repo dimension at this schema version. A production read that forgets its
//! `repo_id` predicate (or bypasses the scope view) then sees the sibling's rows and the test's own
//! assertion trips; a production DELETE that forgets it silently wipes the sibling, which
//! [`assert_sibling_intact`] catches.
//!
//! TWO TRIPWIRE CLASSES (load-bearing). (1) DISTINCT-PATH rows seed under `zz_poison_`-prefixed
//! paths/keys that can never collide with a primary-repo path — they catch an unscoped
//! read/count/delete that returns the UNION across repos (a missing `repo_id` filter on a
//! whole-table scan). (2) SAME-PATH rows seed under a REAL primary-repo path (resolved at seed time
//! by [`primary_collision_path`]) — they catch the class the distinct-path rows CANNOT: an
//! aggregate that reads a scoped table grouped by a NON-repo key (path / source_path / (path,
//! start_byte)) and then JOINS the result onto the active repo's rows BY THAT KEY instead of
//! flowing repo attribution through the join. The canonical example is the `papertrail_ref_counts`
//! CTE in `query::repo_brief::file_rows` (`SELECT source_path, COUNT(*) FROM papertrail_refs GROUP
//! BY source_path` then `LEFT JOIN … ON papertrail_ref_counts.path = files.path`): a sibling's
//! `papertrail_refs` row at `src/lib.rs` inflates the active repo's file `src/lib.rs` unless the
//! CTE filters `repo_id`. A distinct-path sibling ref at `zz_poison_path.rs` never joins onto a
//! primary file, so ONLY a same-path ref exposes that leak. Same-path rows go in for
//! `papertrail_refs`, `files`, `git_file_changes`, `parser_failures` (V041), and — for the V042
//! periphery whose
//! readers key by path — `repo_memory_bindings` (path-bound memories joined onto files by path) and
//! `edge_oracle` (source_path + source_start_byte joined onto files); each is pinned in
//! [`sibling_tripwires`] by a path-INDEPENDENT sentinel column so the intact check holds regardless
//! of which fixture path was chosen.
//!
//! SCHEMA-VERSION SCOPE (load-bearing): this worktree is **V042** (`LATEST_SCHEMA_VERSION = 42`),
//! which scopes the V040 core tables — `repos`, `repo_roots`, `repo_meta`, `files`, `packages`,
//! `logical_symbols`, `docs`, `parser_failures`, `git_commits`, `git_file_changes` (plus
//! `chunks`/`symbols`/`edges_data` TRANSITIVELY via `files.id` and `logical_symbol_members` via
//! `logical_symbols.id`) — the provider-neutral papertrail tables (`papertrail_refs`,
//! `papertrail_items`, `papertrail_comments`, `papertrail_closing_edges`, `papertrail_sync_cursor`,
//! `papertrail_item_tags`, V060) plus the `papertrail_fts` mirror — AND the V042 periphery tables
//! that each gained their OWN `repo_id`: `repo_memories`, `repo_memory_bindings`,
//! `repo_memory_fts`, `logical_symbol_monikers` (now direct, no longer only transitive),
//! `oracle_runs`, `edge_oracle`, `clone_graph_generations`, `clone_token_df`, `clone_refinements`,
//! `dream_findings`, and `reconcile_attempts` (with `repo_memory_tags` scoped transitively through
//! `repo_memories`) — AND the dream-verification siblings `memory_reality` / `memory_summaries` /
//! `memory_model_failures`, each of which carries its own `repo_id` — AND the typed-edge set
//! `repo_node_edges` (V049), owner-scoped by `repo_id`.
//! [`seed_sibling`] seeds a tripwire row into every one of those. Nothing repo-scoped is left
//! unseeded; a table without a `repo_id` dimension (content-addressed pools like
//! `name_strings` / `embedding_cache`, the FTS-derived `chunk_fts`, `clone_edges`/postings scoped
//! by their globally-unique `build_generation`) is deliberately absent — seeding it would
//! manufacture a FALSE tripwire against a legitimately cross-repo store.
//!
//! REGISTRY REGISTRATION IS CONDITIONAL (A7): the sibling gets a REAL `repos` + `repo_roots` +
//! `repo_meta` registry row **only when the fixture repo is itself a real (adopted) repo** — i.e.
//! when the DB already holds a non-placeholder, non-sibling repo (see [`primary_is_real`]). That is
//! the genuinely multi-repo shape A7 makes the default, and registering the sibling there closes
//! the last tripwire gap: an unscoped `repos`/`repo_roots`/`repo_meta` read/count/delete now trips.
//! The eight direct-scoped DATA tables carry `repo_id` as a plain column with **no foreign key to
//! `repos`**, so the scoped-row tripwires are valid with or without the registry row.
//!
//! For a NON-git fixture (the many bare temp-dir fixtures that stay under the `__unassigned__`
//! placeholder because `adopt_repo_from_config` reads them as `Absent`), the sibling stays
//! registry-LESS: registering it as a second real repo would (a) make `sole_repo_id` — the
//! config-blind fallback those fixtures rely on — return the sibling instead of the placeholder,
//! hijacking their scope, and (b) flip `multiple_real_repos`. So the harness registers the sibling
//! ONLY where a real repo already anchors the DB (a git fixture, resolved by
//! identity/recorded-root, never by `sole_repo_id`), and leaves the registry pristine on
//! placeholder DBs. The registry tripwires are correspondingly conditional — [`sibling_tripwires`]
//! appends them only when `primary_is_real`, keyed on the fixture's own (never mutated) real repo
//! row so a leak that deletes the sibling's registry rows is still caught.
//!
//! OPT-OUT: default-ON per test thread (see [`disable_poison_sibling`]). A test that legitimately
//! asserts a scoped table's UNSCOPED total (a `full_rebuild_preserves_*` cache-total check, a
//! whole-table row count) disables the harness at its start. Each opt-out is a deliberate statement
//! that the test's invariant is single-repo by nature, not a workaround for a real leak.

use std::cell::Cell;

use rusqlite::{Connection, params};

/// The reserved id of the tripwire repo. Distinctive so a stray row is unmistakable in a failure.
pub(crate) const POISON_REPO_ID: &str = "poison-sibling";

/// Sentinel prefix on every text value the harness seeds, so a leaked row is greppable and a
/// value-mutation is detectable by exact match.
const POISON_PREFIX: &str = "zz_poison_";

/// The poison sibling's logical-symbol id. Explicit (the real derivation folds `repo_id` into a
/// content hash) and far outside any real id range so it never collides with a fixture's symbols.
const POISON_LOGICAL_ID: i64 = 9_900_000_777;

/// The poison sibling's git commit hash (a distinctive 40-hex-shaped sentinel).
const POISON_COMMIT: &str = "zzpoison00000000000000000000000000000000";

/// The poison sibling's papertrail item key — a distinctive sentinel far outside any fixture's
/// range, so a seeded papertrail row is unmistakable and never collides on the
/// `(repo_id, tracker, project, item_kind, item_key)` natural keys.
const POISON_ITEM_KEY: &str = "9900077";

/// The poison sibling's tracker project (an `owner/repo` path that can never match a fixture's).
const POISON_PROJECT: &str = "zz_poison_owner/zz_poison_repo";

/// SAME-PATH tripwire sentinels. The DISTINCT-PATH rows above seed under `zz_poison_`-prefixed
/// paths that never collide with a primary-repo path, so a leak that JOINS a scoped table onto the
/// active repo's rows BY PATH (rather than flowing repo attribution through the join) cannot trip —
/// the sibling's path never matches a primary path. These SAME-PATH rows instead seed under a REAL
/// primary-repo path (resolved at seed time by [`primary_collision_path`]), so an unscoped
/// join-by-path aggregate attributes the sibling's rows to the active repo and an existing read
/// assertion trips. Each carries its own path-INDEPENDENT sentinel column value (distinct from the
/// distinct-path rows) so the intact check pins it regardless of which primary path was chosen.
const POISON_SAMEPATH_ITEM_KEY: &str = "9900078";
/// `papertrail_refs.source_text` sentinel on the same-path ref (so it reads distinctly in a
/// dump).
const POISON_SAMEPATH_REFTEXT: &str = "zz_poison_samepath_reftext";
/// `files.sha256` sentinel pinning the same-path `files` row.
const POISON_SAMEPATH_SHA: &str = "zz_poison_samepath_sha";
/// `parser_failures.message` sentinel pinning the same-path `parser_failures` row.
const POISON_SAMEPATH_MSG: &str = "zz_poison_samepath_msg";
/// `git_file_changes.additions` sentinel pinning the same-path `git_file_changes` row (a value no
/// real fixture change produces).
const POISON_SAMEPATH_ADDITIONS: i64 = 7_700_077;
/// `repo_memory_bindings.binding_id` sentinel pinning the same-path memory binding (a SECOND
/// binding off the poison memory whose `path` column is a REAL primary path — the distinct-path
/// binding leaves `path` NULL, so it never reaches the `path IS NOT NULL` path-join readers).
const POISON_SAMEPATH_BIND: &str = "zz_poison_samepath_bind";
/// `edge_oracle.scip_symbol` sentinel pinning the same-path oracle edge (its `source_path` +
/// `file_sha` collide with a real primary file so the `edge_oracle`→`files` path+sha join trips).
const POISON_SAMEPATH_SCIP: &str = "zz_poison_samepath_scip";
/// Fallback collision path when the fixture indexed no files — nothing to collide with, but the row
/// still guards against an unscoped DELETE.
const POISON_SAMEPATH_FALLBACK: &str = "zz_poison_no_primary_file.rs";

/// The poison sibling's repo-memory id — the anchor the memory bindings / tags / FTS mirror hang
/// off (they scope through `memory_id` → `repo_memories.repo_id`, or carry `repo_id` directly).
const POISON_MEMORY_ID: &str = "zz_poison_mem";

/// The poison sibling's clone generation / token-hash sentinel — a distinctive integer far outside
/// any fixture's `MAX(generation)+1` allocation so a seeded clone row never collides.
const POISON_GENERATION: i64 = 9_900_000_042;

/// The poison sibling's recorded working-tree root (A7). A distinctive path that can never collide
/// with a real fixture root, so registering the sibling in `repo_roots` never claims a fixture's
/// root nor lets `real_root_owner` mis-resolve a fixture path to the sibling.
const POISON_REPO_ROOT: &str = "/zz_poison_root";

/// The poison sibling's `repo_meta` sentinel key/value (A7) — the tripwire for an unscoped
/// `repo_meta` read/count/delete once the sibling is a REAL registered repo.
const POISON_META_KEY: &str = "zz_poison_meta_key";
const POISON_META_VALUE: &str = "zz_poison_meta_val";

thread_local! {
    /// Whether [`seed_if_enabled`] seeds on this thread. Default ON. Thread-local (not a global
    /// static) so a `cargo test` run — which executes tests as parallel THREADS in one process —
    /// keeps each test's opt-out isolated; under `nextest` (process-per-test) it is trivially
    /// isolated too.
    static POISON_ENABLED: Cell<bool> = const { Cell::new(true) };
}

/// Restores the previous enabled state on drop, so an opt-out is scoped to the test that took it.
pub(crate) struct PoisonDisabled(bool);

impl Drop for PoisonDisabled {
    fn drop(&mut self) {
        POISON_ENABLED.with(|flag| flag.set(self.0));
    }
}

/// Whether seeding is currently disabled on THIS thread — for test helpers that spawn a WORKER
/// thread (e.g. a paused rebuild) and must propagate the calling test's opt-out onto it (the
/// thread-local default is ON, so a spawned rebuild would otherwise re-seed behind the opt-out).
pub(crate) fn poison_disabled_on_this_thread() -> bool {
    !POISON_ENABLED.with(Cell::get)
}

/// Disable poison-sibling seeding for the remainder of THIS test (until the returned guard drops).
/// Bind it: `let _guard = disable_poison_sibling();`. Use it in tests that need a virgin
/// single-repo DB — registry/adoption/migration-ladder tests, `sole_repo_id` assertions, and any
/// test asserting a scoped table's UNSCOPED total. Every call is a claim that the test's invariant
/// is single-repo by nature.
pub(crate) fn disable_poison_sibling() -> PoisonDisabled {
    POISON_ENABLED.with(|flag| {
        let prev = flag.get();
        flag.set(false);
        PoisonDisabled(prev)
    })
}

/// The rebuild-tail seam: seed the poison sibling on `conn` unless this thread opted out.
/// Idempotent (clears any prior sibling first), so repeated rebuilds on one DB reconverge to the
/// same tripwire set. A seeding failure PROPAGATES — a harness that cannot seed is a bug to
/// surface, never to swallow.
pub(crate) fn seed_if_enabled(conn: &Connection) -> anyhow::Result<()> {
    if POISON_ENABLED.with(Cell::get) {
        seed_sibling(conn)?;
    }
    Ok(())
}

/// Clear then insert the full tripwire row set for the poison sibling. Runs under `foreign_keys =
/// ON` (the live rebuild connection), so inserts are parent→child and clears child→parent.
/// Deliberately touches NO registry table (`repos`/`repo_roots`/`repo_meta`) — see the module docs.
pub(crate) fn seed_sibling(conn: &Connection) -> anyhow::Result<()> {
    clear_sibling(conn)?;

    // Resolve the primary-repo path the SAME-PATH tripwires collide onto BEFORE seeding any sibling
    // rows (so the poison rows never win the `ORDER BY path` pick).
    let collision_path = primary_collision_path(conn)?;

    // --- git history (git_file_changes FKs git_commits(repo_id, hash); git_commits has NO FK to
    // repos, so this needs no registry row) ---
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, committed_at_s, \
         subject, body, changed_file_count, repo_id)
         VALUES (?1, ?2, ?2, 0, 0, ?3, '', 1, ?4)",
        params![
            POISON_COMMIT,
            format!("{POISON_PREFIX}author"),
            format!("{POISON_PREFIX}subject"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES (?1, ?2, 0, 0, 'modified', ?3)",
        params![POISON_COMMIT, format!("{POISON_PREFIX}change.rs"), POISON_REPO_ID],
    )?;

    // --- direct-scoped core tables ---
    // A6: seed the sibling's files at generation 0 — the sibling's OWN live generation (it has no
    // `repo_meta[live_files_generation]`, so its live generation reads 0). This is DISTINCT from
    // the primary repo's post-rebuild live generation (>= 1, since the seed runs at the rebuild
    // tail after the flip), so a repo-UNSCOPED dead-generation sweep — `WHERE generation !=
    // <primary live>` missing the `repo_id` predicate — would delete these rows and trip
    // `assert_sibling_intact`. That is the exact class the generation gc sweep must never regress
    // (and the reason the sibling carries no `repo_meta` live-generation pointer of its own).
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '', ?4, 0)",
        params![
            format!("{POISON_PREFIX}file.rs"),
            format!("{POISON_PREFIX}sha"),
            POISON_COMMIT,
            POISON_REPO_ID
        ],
    )?;
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO packages(manifest_dir, commit_sha, worktree_id, local_roots_json, repo_id)
         VALUES (?1, '', '', '[]', ?2)",
        params![format!("{POISON_PREFIX}pkg"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO parser_failures(repo_id, path, language, message) VALUES (?1, ?2, 'rust', ?3)",
        params![POISON_REPO_ID, format!("{POISON_PREFIX}fail.rs"), format!("{POISON_PREFIX}msg")],
    )?;
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
         variant_count, group_reason, repo_id)
         VALUES (?1, 'rust', ?2, ?3, NULL, 'function', 1, ?4, ?5)",
        params![
            POISON_LOGICAL_ID,
            format!("{POISON_PREFIX}file.rs"),
            format!("{POISON_PREFIX}symbol"),
            format!("{POISON_PREFIX}group"),
            POISON_REPO_ID
        ],
    )?;

    // --- children hung off the poison file (transitively scoped through files.repo_id) ---
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line, is_test)
         VALUES (?1, 'rust', ?2, NULL, 'function', 0, 0, 0, 0, 0)",
        params![file_id, format!("{POISON_PREFIX}symbol")],
    )?;
    let symbol_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text_hash, source_revision, anchor_version, normalized_hash, start_boundary_hash, \
         end_boundary_hash, start_context_hash, end_context_hash, context_radius, \
         embedding_policy, embedding_priority)
         VALUES (?1, 'symbol', 0, 0, 0, 0, ?2, '', 0, ?2, '', '', '', '', 0, 'none', 0)",
        params![file_id, format!("{POISON_PREFIX}chunkhash")],
    )?;
    let chunk_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO docs(chunk_id, source_kind, heading_path, repo_id) VALUES (?1, 'markdown', \
         ?2, ?3)",
        params![chunk_id, format!("{POISON_PREFIX}heading"), POISON_REPO_ID],
    )?;

    // --- one edge whose source file is the poison file (scoped via source_file_id → files) ---
    conn.execute_batch(&format!(
        "INSERT OR IGNORE INTO name_strings(value) VALUES
            ('{POISON_PREFIX}from'), ('{POISON_PREFIX}to'), ('{POISON_PREFIX}calls'),
            ('{POISON_PREFIX}conf'), ('{POISON_PREFIX}res');"
    ))?;
    let name_id = |value: &str| -> rusqlite::Result<i64> {
        conn.query_row("SELECT id FROM name_strings WHERE value = ?1", [value], |row| row.get(0))
    };
    conn.execute(
        "INSERT INTO edges_data(source_file_id, from_name_id, to_name_id, edge_kind_id, \
         confidence_id, resolution_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            file_id,
            name_id(&format!("{POISON_PREFIX}from"))?,
            name_id(&format!("{POISON_PREFIX}to"))?,
            name_id(&format!("{POISON_PREFIX}calls"))?,
            name_id(&format!("{POISON_PREFIX}conf"))?,
            name_id(&format!("{POISON_PREFIX}res"))?,
        ],
    )?;

    // --- children hung off the poison logical symbol (scoped via logical_symbols.repo_id) ---
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line)
         VALUES (?1, ?2, 0, 0)",
        params![POISON_LOGICAL_ID, symbol_id],
    )?;
    // logical_symbol_monikers has NO repo_id and NO FK; it is scoped only by the join to
    // logical_symbols. This row is the tripwire for the oracle moniker clear/count (round-6 P2 #3).
    // Since V042 `logical_symbol_monikers` carries its OWN `repo_id` (the count/clear/write now
    // filter it directly rather than joining `logical_symbols`), stamp it the sibling id.
    conn.execute(
        "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
         computed_at, repo_id)
         VALUES (?1, 'scip-rust', ?2, ?3, 0, ?4)",
        params![
            POISON_LOGICAL_ID,
            format!("{POISON_PREFIX}ver"),
            format!("{POISON_PREFIX}moniker"),
            POISON_REPO_ID
        ],
    )?;

    // --- papertrail (V060): every provider-neutral table + the standalone `papertrail_fts`
    // mirror carries a `repo_id` column, so a sibling row is valid (these caches carry NO FK to
    // `repos`) and any unscoped papertrail read/count/delete trips a tripwire. The two items
    // share `POISON_ITEM_KEY` under DIFFERENT `item_kind`s, so a read that drops the kind from
    // the natural key also trips. ---
    conn.execute(
        "INSERT INTO papertrail_refs(tracker, project, item_key, ref_kind, source_kind, \
         source_path, source_text, discovered_at_ms, repo_id)
         VALUES ('github', ?1, ?2, 'closing', 'file', ?3, ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}path.rs"),
            format!("{POISON_PREFIX}reftext"),
            POISON_REPO_ID
        ],
    )?;
    // V073 (#702): a sibling closing edge for the SAME external pair — an unscoped closing-edge
    // read would adopt the sibling's attested closer.
    conn.execute(
        "INSERT OR IGNORE INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key, source, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'issue', ?2, 'commit', ?3, 'provider', 0, ?4)",
        params![POISON_PROJECT, POISON_ITEM_KEY, format!("{POISON_PREFIX}sha"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'issue', ?2, 'http://x', 'open', ?3, ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}title"),
            format!("{POISON_PREFIX}body"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, merged_at, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'change_request', ?2, 'http://x', 'open', ?3, ?4, NULL, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}prtitle"),
            format!("{POISON_PREFIX}prbody"),
            POISON_REPO_ID
        ],
    )?;
    // The three legacy comment shapes in the unified table: a plain thread comment, a review
    // event (review_state), and a file-anchored review comment (anchor_path).
    conn.execute(
        "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'issue', ?2, ?3, 'http://x', ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}comment_id_1"),
            format!("{POISON_PREFIX}comment"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, review_state, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'change_request', ?2, ?3, NULL, ?4, 'commented', 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}comment_id_2"),
            format!("{POISON_PREFIX}review"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, anchor_path, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'change_request', ?2, ?3, 'http://x', ?4, ?5, 0, ?6)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}comment_id_3"),
            format!("{POISON_PREFIX}revcomment"),
            format!("{POISON_PREFIX}anchored.rs"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_sync_cursor(tracker, project, high_mark_at, repo_id)
         VALUES ('github', ?1, ?2, ?3)",
        params![POISON_PROJECT, format!("{POISON_PREFIX}mark"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO papertrail_item_tags(tracker, project, item_kind, item_key, tag, repo_id)
         VALUES ('github', ?1, 'issue', ?2, ?3, ?4)",
        params![POISON_PROJECT, POISON_ITEM_KEY, format!("{POISON_PREFIX}itemtag"), POISON_REPO_ID],
    )?;
    // papertrail_fts mirror rows, seeded EXACTLY as the INCREMENTAL writers (`store_item` /
    // `store_comment`) and the whole-table `papertrail::rebuild_fts` derive them from the five
    // base rows above: item rows carry the item title, comment rows carry COALESCE(anchor_path,
    // '') in the title slot and COALESCE(url, '') in the url slot. A full mirror rebuild DELETEs
    // everything and re-derives, so seeding anything else would strand the intact check on a
    // vanished row set; `papertrail_fts_tripwires_survive_a_mirror_rebuild` pins this
    // equivalence. `classification` is recomputed by `insert_fts` (`classify_text`) at
    // re-derivation and is deliberately NOT pinned by the tripwires.
    let insert_poison_fts = |item_kind: &str,
                             doc_kind: &str,
                             comment_id: &str,
                             url: &str,
                             title: &str,
                             body: String| {
        conn.execute(
            "INSERT INTO papertrail_fts(tracker, project, item_kind, item_key, doc_kind, \
             comment_id, url, title, body, classification, repo_id)
                 VALUES ('github', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'other', ?9)",
            params![
                POISON_PROJECT,
                item_kind,
                POISON_ITEM_KEY,
                doc_kind,
                comment_id,
                url,
                title,
                body,
                POISON_REPO_ID
            ],
        )
    };
    insert_poison_fts(
        "issue",
        "item",
        "",
        "http://x",
        &format!("{POISON_PREFIX}title"),
        format!("{POISON_PREFIX}body"),
    )?;
    insert_poison_fts(
        "change_request",
        "item",
        "",
        "http://x",
        &format!("{POISON_PREFIX}prtitle"),
        format!("{POISON_PREFIX}prbody"),
    )?;
    insert_poison_fts(
        "issue",
        "comment",
        &format!("{POISON_PREFIX}comment_id_1"),
        "http://x",
        "",
        format!("{POISON_PREFIX}comment"),
    )?;
    insert_poison_fts(
        "change_request",
        "comment",
        &format!("{POISON_PREFIX}comment_id_2"),
        "",
        "",
        format!("{POISON_PREFIX}review"),
    )?;
    insert_poison_fts(
        "change_request",
        "comment",
        &format!("{POISON_PREFIX}comment_id_3"),
        "http://x",
        &format!("{POISON_PREFIX}anchored.rs"),
        format!("{POISON_PREFIX}revcomment"),
    )?;

    // --- A5 periphery (V042): repo memories (+ bindings / tags / FTS mirror), oracle runs, edge
    // oracle, clone generations / token-df / refinements, dream findings, and reconcile attempts
    // each gained a `repo_id` column in V042, so a sibling row is now valid and any unscoped
    // periphery read/count/delete trips a tripwire. `repo_memory_tags` has NO `repo_id` of its own
    // (it scopes transitively via `memory_id` → `repo_memories.repo_id`), so it hangs off the
    // poison memory. ---
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
         updated_at_ms, source, memory_version, repo_id)
         VALUES (?1, 'Invariant', ?2, ?3, 'high', 'active', 0, 0, 'agent', 'v1', ?4)",
        params![
            POISON_MEMORY_ID,
            format!("{POISON_PREFIX}title"),
            format!("{POISON_PREFIX}body"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
         created_at_ms, repo_id)
         VALUES (?1, 'path', ?2, 'current', 0, ?3)",
        params![POISON_MEMORY_ID, format!("{POISON_PREFIX}bind"), POISON_REPO_ID],
    )?;
    conn.execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES (?1, ?2)", params![
        POISON_MEMORY_ID,
        format!("{POISON_PREFIX}tag")
    ])?;
    conn.execute(
        "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
         VALUES (?1, ?2, ?3, ?4, 'Invariant', ?5)",
        params![
            POISON_REPO_ID,
            POISON_MEMORY_ID,
            format!("{POISON_PREFIX}title"),
            format!("{POISON_PREFIX}body"),
            format!("{POISON_PREFIX}tag")
        ],
    )?;
    // A sibling typed edge (#464, V049): owned by the poison repo, authored on the poison memory.
    // Any UNSCOPED `edges_from` / `edges_into` / count would surface it and trip a tripwire.
    conn.execute(
        "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, target_repo_id, \
         target_kind, target_anchor, target_node_id, anchor_status, created_at_ms)
         VALUES (?1, ?2, ?3, 'depends_on', ?2, 'node', ?4, NULL, 'unresolved', 0)",
        params![
            format!("{POISON_PREFIX}edge_key"),
            POISON_REPO_ID,
            POISON_MEMORY_ID,
            format!("{POISON_PREFIX}target"),
        ],
    )?;
    conn.execute(
        "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, status, \
         stats_json, repo_id)
         VALUES ('scip-rust', ?1, ?2, '', 0, 'complete', '{}', ?3)",
        params![format!("{POISON_PREFIX}ver"), POISON_COMMIT, POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
         callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
         scip_symbol, kind, computed_at)
         VALUES (?1, ?2, 0, 0, 0, 0, 'calls_name', ?3, 'scip-rust', ?4, ?5, 'resolved', 0)",
        params![
            POISON_REPO_ID,
            format!("{POISON_PREFIX}edge.rs"),
            format!("{POISON_PREFIX}sha"),
            format!("{POISON_PREFIX}ver"),
            format!("{POISON_PREFIX}scip")
        ],
    )?;
    conn.execute(
        "INSERT INTO clone_graph_generations(generation, status, theta_floor, normalizer_kind, \
         normalizer_version, source_revision, started_at_ms, repo_id)
         VALUES (?1, 'Complete', 0.7, 'baseline', 1, ?2, 0, ?3)",
        params![POISON_GENERATION, format!("{POISON_PREFIX}rev"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
         VALUES (?1, 'baseline', ?2, 1)",
        params![POISON_REPO_ID, POISON_GENERATION],
    )?;
    conn.execute(
        "INSERT INTO clone_refinements(repo_id, class_key, language, refine_mode, template, \
         variation_points_json, proposed_signature_json, confidence, anti_unify_coverage, \
         lcs_ratio, refactorability, norm_version, alignment_version, created_at_ms, lcs_sampled)
         VALUES (?1, ?2, 'rust', 'exact', ?3, '[]', '{}', 'high', 0.0, 0.0, 0.0, 1, 1, 0, 0)",
        params![POISON_REPO_ID, format!("{POISON_PREFIX}class"), format!("{POISON_PREFIX}tmpl")],
    )?;
    conn.execute(
        "INSERT INTO dream_findings(id, kind, subject, claim_hash, evidence, base_rank, \
         first_seen_at_ms, last_seen_at_ms, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, 0.0, 0, 0, ?6)",
        params![
            format!("{POISON_PREFIX}dream"),
            format!("{POISON_PREFIX}kind"),
            format!("{POISON_PREFIX}subj"),
            format!("{POISON_PREFIX}claim"),
            format!("{POISON_PREFIX}ev"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO reconcile_attempts(started_at_ms, status, repo_id) VALUES (0, ?1, ?2)",
        params![format!("{POISON_PREFIX}status"), POISON_REPO_ID],
    )?;

    // --- Dream v2 verification siblings: each carries its own `repo_id`, so a sibling row is now
    // valid and any unscoped verification-queue / evidence / summary / failure read/count/delete
    // trips a tripwire. They all hang off the poison memory id. ---
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms)
         VALUES (?1, ?2, ?3, 0)",
        params![POISON_MEMORY_ID, POISON_REPO_ID, format!("{POISON_PREFIX}bodyhash")],
    )?;
    conn.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms)
         VALUES (?1, ?2, ?3, ?4, 0)",
        params![
            POISON_MEMORY_ID,
            POISON_REPO_ID,
            format!("{POISON_PREFIX}bodyhash"),
            format!("{POISON_PREFIX}summary")
        ],
    )?;
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms)
         VALUES (?1, ?2, 'compact', ?3, ?4, ?5, 'summary_guard_rejected', 0)",
        params![
            POISON_MEMORY_ID,
            POISON_REPO_ID,
            format!("{POISON_PREFIX}bodyhash"),
            format!("{POISON_PREFIX}model"),
            format!("{POISON_PREFIX}prompt")
        ],
    )?;

    // --- SAME-PATH tripwires: sibling rows whose PATH (or path+sha / path+byte) deliberately
    // collides with a real primary row (`collision_path`). A join-by-<key> aggregate that reads a
    // scoped table without a `repo_id` predicate attributes these to the active repo. The
    // DISTINCT-PATH rows above cannot catch that class — their `zz_poison_` paths never match a
    // primary path. Each row is under `POISON_REPO_ID`, so `clear_sibling`'s existing
    // `WHERE repo_id = POISON_REPO_ID` deletes them; the intact check pins each by its own sentinel
    // column (not by path). ---
    // files: caught by any unscoped `main.files` read that groups/joins by path (the scope view
    // would exclude the sibling, so only a view-bypassing path read leaks). No children hung off
    // it.
    // Generation 0 (the sibling's live generation), as for the distinct-path file above.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '', ?4, 0)",
        params![collision_path, POISON_SAMEPATH_SHA, POISON_COMMIT, POISON_REPO_ID],
    )?;
    // git_file_changes at the shared path (off the sibling commit): caught by an unscoped churn /
    // co-change / history aggregate that joins `git_file_changes.path` onto the scoped `files`.
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES (?1, ?2, ?3, 0, 'modified', ?4)",
        params![POISON_COMMIT, collision_path, POISON_SAMEPATH_ADDITIONS, POISON_REPO_ID],
    )?;
    // parser_failures at the shared path (PK is (repo_id, path), so this is distinct from the
    // distinct-path failure): caught by an unscoped parser-failure read joined/grouped by path.
    conn.execute(
        "INSERT INTO parser_failures(repo_id, path, language, message) VALUES (?1, ?2, 'rust', ?3)",
        params![POISON_REPO_ID, collision_path, POISON_SAMEPATH_MSG],
    )?;
    // papertrail_refs at the shared source_path: THE canonical join-by-path leak (the
    // `papertrail_ref_counts` CTE in `repo_brief::file_rows`). Distinct `item_key` from the
    // distinct-path ref, so `idx_papertrail_refs_unique` is satisfied.
    conn.execute(
        "INSERT INTO papertrail_refs(tracker, project, item_key, ref_kind, source_kind, \
         source_path, source_text, discovered_at_ms, repo_id)
         VALUES ('github', ?1, ?2, 'closing', 'file', ?3, ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_SAMEPATH_ITEM_KEY,
            collision_path,
            POISON_SAMEPATH_REFTEXT,
            POISON_REPO_ID
        ],
    )?;
    // repo_memory_bindings at the shared `path` (V042): a SECOND path-binding off the poison memory
    // whose `path` column is a REAL primary path — caught by the memory path-join readers
    // (`repo_brief::memory_counts_by_path`, `orientation` memory titles, `memory::memories_for_*`)
    // if they forget the `repo_id` predicate. The distinct-path binding leaves `path` NULL, so it
    // never reaches those `path IS NOT NULL` readers. Distinct `binding_id` for the PK.
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id)
         VALUES (?1, 'path', ?2, ?3, 'current', 0, ?4)",
        params![POISON_MEMORY_ID, POISON_SAMEPATH_BIND, collision_path, POISON_REPO_ID],
    )?;
    // edge_oracle at the shared (source_path, file_sha) (V042): the `edge_oracle`→`files` metric
    // join keys on `files.path = source_path AND files.sha256 = file_sha`, so it is scoped only
    // transitively through the `files` view — a sibling row whose path AND sha match a real primary
    // file (an identical vendored/shared file across repos) leaks unless the read filters
    // `edge_oracle.repo_id`. `collision_sha` is the primary file's sha at `collision_path` (or the
    // fallback when no primary file exists — then nothing collides).
    let collision_sha: String = conn.query_row(
        "SELECT COALESCE(
             (SELECT sha256 FROM main.files WHERE path = ?1 AND repo_id != ?2 ORDER BY sha256 \
         LIMIT 1),
             ?3)",
        params![collision_path, POISON_REPO_ID, POISON_SAMEPATH_SHA],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
         callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
         scip_symbol, kind, computed_at)
         VALUES (?1, ?2, 0, 0, 0, 0, 'calls_name', ?3, 'scip-rust', ?4, ?5, 'resolved', 0)",
        params![
            POISON_REPO_ID,
            collision_path,
            collision_sha,
            format!("{POISON_PREFIX}ver"),
            POISON_SAMEPATH_SCIP
        ],
    )?;

    // --- registry rows (A7): register the sibling as a REAL repo ONLY on a DB that already holds a
    // real fixture repo (a git fixture). This is the genuinely multi-repo shape A7 makes the
    // default, and it makes an unscoped `repos`/`repo_roots`/`repo_meta` read/count/delete trip a
    // tripwire. On a placeholder-only (non-git) fixture the sibling stays registry-less — a second
    // real repo would hijack `sole_repo_id` for the many fixtures that rely on it (see the module
    // docs). `repo_roots` / `repo_meta` FK `repos(repo_id)` ON DELETE CASCADE, so insert the parent
    // `repos` row FIRST (the connection runs `foreign_keys = ON`). ---
    if primary_is_real(conn)? {
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?2, 0)",
            params![POISON_REPO_ID, format!("{POISON_PREFIX}name")],
        )?;
        conn.execute(
            "INSERT INTO repo_roots(repo_id, root, registered_at_ms) VALUES (?1, ?2, 0)",
            params![POISON_REPO_ID, POISON_REPO_ROOT],
        )?;
        conn.execute("INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)", params![
            POISON_REPO_ID,
            POISON_META_KEY,
            POISON_META_VALUE
        ])?;
    }

    Ok(())
}

/// Whether the DB already holds a REAL fixture repo — a `repos` row that is neither the
/// `__unassigned__` placeholder nor the poison sibling itself. Gates whether [`seed_sibling`]
/// registers the sibling as a real repo and whether [`sibling_tripwires`] appends the registry
/// tripwires (see the module docs). Stable across the mutation under test: the fixture's own real
/// repo row is never the target of the unscoped-read leaks the harness hunts, so a seed-time and an
/// assert-time evaluation agree — a leak that deletes the SIBLING's registry rows is still caught.
fn primary_is_real(conn: &Connection) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM repos WHERE repo_id != ?1 AND repo_id != ?2",
        params![rag_rat_base::repo_identity::LEGACY_REPO_ID, POISON_REPO_ID],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// The lexicographically-first REAL (non-sibling) indexed path — the primary-repo path the
/// SAME-PATH tripwires collide onto. Falls back to [`POISON_SAMEPATH_FALLBACK`] when the fixture
/// indexed no files (an empty repo): there is then nothing to collide with, but the seeded rows
/// still guard against an unscoped DELETE. Deterministic (`ORDER BY path`), so a meta-test can
/// re-resolve the same path. Resolve it BEFORE seeding the sibling's own rows (all under
/// `POISON_REPO_ID`, so `repo_id != POISON_REPO_ID` also excludes them defensively).
fn primary_collision_path(conn: &Connection) -> anyhow::Result<String> {
    let path: String = conn.query_row(
        "SELECT COALESCE(
             (SELECT path FROM main.files WHERE repo_id != ?1 ORDER BY path LIMIT 1),
             ?2)",
        params![POISON_REPO_ID, POISON_SAMEPATH_FALLBACK],
        |row| row.get(0),
    )?;
    Ok(path)
}

/// Remove every poison-sibling row, child→parent, so [`seed_sibling`] is idempotent across repeated
/// rebuilds on one DB. Explicit child-first order works whether or not the FK cascades fire.
fn clear_sibling(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "DELETE FROM logical_symbol_monikers WHERE logical_symbol_id = {POISON_LOGICAL_ID};
         DELETE FROM logical_symbol_members WHERE logical_symbol_id = {POISON_LOGICAL_ID};
         DELETE FROM logical_symbols WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM docs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM edges_data WHERE source_file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM chunks WHERE file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM symbols WHERE file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM parser_failures WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM packages WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM git_file_changes WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM git_commits WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM main.files WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_refs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_items WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_comments WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_sync_cursor WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_item_tags WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_fts WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM reconcile_attempts WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM memory_reality WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM memory_summaries WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM memory_model_failures WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM dream_findings WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM clone_refinements WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM clone_token_df WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM clone_graph_generations WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM edge_oracle WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM oracle_runs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_memory_fts WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_memory_tags WHERE memory_id = '{POISON_MEMORY_ID}';
         DELETE FROM repo_memory_bindings WHERE repo_id = '{POISON_REPO_ID}';
         -- #464: cleared EXPLICITLY (not just via the source FK cascade) so a reseed is idempotent
         -- even with `foreign_keys` off or an orphaned row — else the next INSERT trips the \
         edge_key PK.
         DELETE FROM repo_node_edges WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_memories WHERE repo_id = '{POISON_REPO_ID}';
         -- Registry rows (A7): child-first (repo_meta/repo_roots FK repos ON DELETE CASCADE), so a
         -- re-seed on a git fixture starts from a clean slate. No-op when the sibling was never
         -- registered (a placeholder-only fixture).
         DELETE FROM repo_meta WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_roots WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repos WHERE repo_id = '{POISON_REPO_ID}';"
    ))?;
    Ok(())
}

/// Each seeded tripwire as `(table, full-sentinel WHERE predicate)`. Asserting each still matches
/// EXACTLY one row is a row-count check (catches an unscoped DELETE) and a value checksum (the
/// predicate pins every seeded column, so an in-place UPDATE stops matching) in one. The transitive
/// children are matched through the poison file / logical symbol, exactly how a scoped reader would
/// have to reach them.
fn sibling_tripwires(conn: &Connection) -> anyhow::Result<Vec<(&'static str, String)>> {
    let file_scope =
        format!("file_id IN (SELECT id FROM main.files WHERE repo_id = '{POISON_REPO_ID}')");
    let mut tripwires = vec![
        ("git_commits", format!("repo_id = '{POISON_REPO_ID}' AND hash = '{POISON_COMMIT}'")),
        // Pinned to the distinct-path row's path so the same-path `git_file_changes` tripwire
        // (a second row under `POISON_REPO_ID`) doesn't inflate this count.
        (
            "git_file_changes",
            format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}change.rs'"),
        ),
        ("main.files", format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}file.rs'")),
        ("packages", format!("repo_id = '{POISON_REPO_ID}'")),
        // Pinned to the distinct-path row's path so the same-path `parser_failures` tripwire
        // doesn't inflate this count.
        (
            "parser_failures",
            format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}fail.rs'"),
        ),
        ("docs", format!("repo_id = '{POISON_REPO_ID}'")),
        ("logical_symbols", format!("id = {POISON_LOGICAL_ID} AND repo_id = '{POISON_REPO_ID}'")),
        ("symbols", file_scope.clone()),
        ("chunks", file_scope.clone()),
        (
            "edges_data",
            format!(
                "source_file_id IN (SELECT id FROM main.files WHERE repo_id = '{POISON_REPO_ID}')"
            ),
        ),
        ("logical_symbol_members", format!("logical_symbol_id = {POISON_LOGICAL_ID}")),
        (
            "logical_symbol_monikers",
            format!(
                "logical_symbol_id = {POISON_LOGICAL_ID} AND repo_id = '{POISON_REPO_ID}' AND \
                 moniker = '{POISON_PREFIX}moniker'"
            ),
        ),
        // papertrail (V060): each base table + the fts mirror pinned by the sentinel item key.
        (
            "papertrail_closing_edges",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND issue_key = '{POISON_ITEM_KEY}' AND closer_key \
                 = '{POISON_PREFIX}sha'"
            ),
        ),
        (
            "papertrail_refs",
            format!("repo_id = '{POISON_REPO_ID}' AND item_key = '{POISON_ITEM_KEY}'"),
        ),
        (
            "papertrail_items",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND item_kind = 'issue' AND item_key = \
                 '{POISON_ITEM_KEY}' AND body = '{POISON_PREFIX}body'"
            ),
        ),
        (
            "papertrail_items",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND item_kind = 'change_request' AND item_key = \
                 '{POISON_ITEM_KEY}' AND body = '{POISON_PREFIX}prbody'"
            ),
        ),
        (
            "papertrail_comments",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND comment_id = '{POISON_PREFIX}comment_id_1' AND \
                 body = '{POISON_PREFIX}comment' AND review_state IS NULL AND anchor_path IS NULL"
            ),
        ),
        (
            "papertrail_comments",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND comment_id = '{POISON_PREFIX}comment_id_2' AND \
                 body = '{POISON_PREFIX}review' AND review_state = 'commented'"
            ),
        ),
        (
            "papertrail_comments",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND comment_id = '{POISON_PREFIX}comment_id_3' AND \
                 body = '{POISON_PREFIX}revcomment' AND anchor_path = '{POISON_PREFIX}anchored.rs'"
            ),
        ),
        (
            "papertrail_sync_cursor",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND project = '{POISON_PROJECT}' AND high_mark_at = \
                 '{POISON_PREFIX}mark'"
            ),
        ),
        (
            "papertrail_item_tags",
            format!("repo_id = '{POISON_REPO_ID}' AND tag = '{POISON_PREFIX}itemtag'"),
        ),
        // papertrail_fts: one derived mirror row per base row above, pinned by doc_kind +
        // item_kind/comment_id + body so a full mirror rebuild's re-derivation must reconverge
        // onto exactly this set (`classification` is derivation-owned and deliberately unpinned).
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'item' AND item_kind = 'issue' AND \
                 item_key = '{POISON_ITEM_KEY}' AND body = '{POISON_PREFIX}body'"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'item' AND item_kind = \
                 'change_request' AND item_key = '{POISON_ITEM_KEY}' AND body = \
                 '{POISON_PREFIX}prbody'"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'comment' AND comment_id = \
                 '{POISON_PREFIX}comment_id_1' AND body = '{POISON_PREFIX}comment' AND url = \
                 'http://x' AND title = ''"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'comment' AND comment_id = \
                 '{POISON_PREFIX}comment_id_2' AND body = '{POISON_PREFIX}review' AND url = '' \
                 AND title = ''"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'comment' AND comment_id = \
                 '{POISON_PREFIX}comment_id_3' AND body = '{POISON_PREFIX}revcomment' AND title = \
                 '{POISON_PREFIX}anchored.rs'"
            ),
        ),
        // SAME-PATH tripwires (V041): pinned by a path-INDEPENDENT sentinel column (the collision
        // path is fixture-dependent), so the intact check holds whichever primary path was chosen.
        (
            "main.files",
            format!("repo_id = '{POISON_REPO_ID}' AND sha256 = '{POISON_SAMEPATH_SHA}'"),
        ),
        (
            "git_file_changes",
            format!("repo_id = '{POISON_REPO_ID}' AND additions = {POISON_SAMEPATH_ADDITIONS}"),
        ),
        (
            "parser_failures",
            format!("repo_id = '{POISON_REPO_ID}' AND message = '{POISON_SAMEPATH_MSG}'"),
        ),
        (
            "papertrail_refs",
            format!("repo_id = '{POISON_REPO_ID}' AND item_key = '{POISON_SAMEPATH_ITEM_KEY}'"),
        ),
        // A5 periphery (V042): each directly-scoped table pinned by the sibling repo_id (plus its
        // sentinel key where a table's row is otherwise ambiguous). `repo_memory_tags` scopes
        // transitively through the poison memory.
        ("repo_memories", format!("repo_id = '{POISON_REPO_ID}' AND id = '{POISON_MEMORY_ID}'")),
        // Pinned to the distinct-path binding's `binding_id` so the same-path binding tripwire
        // (a SECOND binding under the same memory) doesn't inflate this count.
        (
            "repo_memory_bindings",
            format!("repo_id = '{POISON_REPO_ID}' AND binding_id = '{POISON_PREFIX}bind'"),
        ),
        ("repo_memory_tags", format!("memory_id = '{POISON_MEMORY_ID}'")),
        (
            "repo_memory_fts",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "repo_node_edges",
            format!("repo_id = '{POISON_REPO_ID}' AND edge_key = '{POISON_PREFIX}edge_key'"),
        ),
        ("oracle_runs", format!("repo_id = '{POISON_REPO_ID}'")),
        // Pinned to the distinct-path edge's `scip_symbol` so the same-path edge tripwire doesn't
        // inflate this count.
        (
            "edge_oracle",
            format!("repo_id = '{POISON_REPO_ID}' AND scip_symbol = '{POISON_PREFIX}scip'"),
        ),
        (
            "clone_graph_generations",
            format!("repo_id = '{POISON_REPO_ID}' AND generation = {POISON_GENERATION}"),
        ),
        ("clone_token_df", format!("repo_id = '{POISON_REPO_ID}'")),
        ("clone_refinements", format!("repo_id = '{POISON_REPO_ID}'")),
        ("dream_findings", format!("repo_id = '{POISON_REPO_ID}'")),
        ("reconcile_attempts", format!("repo_id = '{POISON_REPO_ID}'")),
        // Dream v2 verification siblings: each pinned by the sibling repo_id + poison memory.
        (
            "memory_reality",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "memory_summaries",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "memory_model_failures",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        // SAME-PATH tripwires (V042): the memory binding and oracle edge whose path (and, for the
        // oracle, path+sha) collide with a real primary row, pinned by their own sentinel keys.
        (
            "repo_memory_bindings",
            format!("repo_id = '{POISON_REPO_ID}' AND binding_id = '{POISON_SAMEPATH_BIND}'"),
        ),
        (
            "edge_oracle",
            format!("repo_id = '{POISON_REPO_ID}' AND scip_symbol = '{POISON_SAMEPATH_SCIP}'"),
        ),
    ];
    // Registry tripwires (A7): present ONLY when the sibling is a REAL registered repo (a git
    // fixture — see `primary_is_real`, which gates the matching seed). Each pins the sibling's own
    // registry row, so an unscoped `repos` / `repo_roots` / `repo_meta` read/count/delete trips.
    if primary_is_real(conn)? {
        tripwires.push(("repos", format!("repo_id = '{POISON_REPO_ID}'")));
        tripwires.push((
            "repo_roots",
            format!("repo_id = '{POISON_REPO_ID}' AND root = '{POISON_REPO_ROOT}'"),
        ));
        tripwires.push((
            "repo_meta",
            format!("repo_id = '{POISON_REPO_ID}' AND key = '{POISON_META_KEY}'"),
        ));
    }
    Ok(tripwires)
}

/// Post-condition for a MUTATING test: assert the poison sibling survived intact — every seeded
/// tripwire still present and unmodified. Any unscoped DELETE / UPDATE in the operation under test
/// (a GC that wipes a sibling's rows, an oracle clear that drops a sibling's monikers, an
/// incremental pass that stamps a sibling's file) trips this. Reads `main.*` directly (bypassing
/// the scope view), because the active connection is scoped to the fixture repo and would never see
/// the sibling through the view. Call it on the fixture's connection at the end of the mutating
/// step.
pub(crate) fn assert_sibling_intact(conn: &Connection) {
    for (table, predicate) in sibling_tripwires(conn).expect("read the sibling tripwire set") {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|err| panic!("poison-sibling probe failed on {table}: {err}"));
        assert_eq!(
            count, 1,
            "poison sibling leaked/mutated in `{table}` (WHERE {predicate}): expected exactly 1 \
             row, found {count}. An unscoped read/count/delete in the operation under test \
             touched a sibling repo's rows — scope it by repo_id.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexDatabase;
    use crate::index::schema_bootstrap_tests::poison_test_config;

    /// The harness self-check: after a normal rebuild (poison ON by default), an INTENTIONALLY
    /// unscoped probe of a scoped table MUST see the sentinel — proving the tripwires are live. If
    /// this fails, the harness is asleep and every "sibling intact" downstream assertion is
    /// meaningless.
    #[test]
    fn poison_tripwires_are_live_after_a_default_rebuild() {
        let (_root, config) = poison_test_config("poison_live");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();

        // Unscoped total over a direct-scoped table sees BOTH the fixture repo and the sibling.
        let sibling_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1",
                [POISON_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sibling_files >= 1, "the poison file must be seeded by the default rebuild");

        // And every tripwire is intact right after seeding.
        assert_sibling_intact(conn);
    }

    #[test]
    fn poison_sibling_seeds_memory_model_failure_tripwire() {
        let (_root, config) = poison_test_config("poison_failure");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();

        let (pass, reason): (String, String) = conn
            .query_row(
                "SELECT pass, reason FROM memory_model_failures WHERE repo_id = ?1 AND memory_id \
                 = ?2",
                [POISON_REPO_ID, POISON_MEMORY_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(pass, "compact");
        assert_eq!(reason, "summary_guard_rejected");
    }

    /// SAME-PATH tripwire liveness: the harness must seed a sibling row whose join key COLLIDES
    /// with a real primary-repo path, and an intentionally path-keyed UNSCOPED aggregate must see
    /// it — proving the harness can trip a join-by-path leak (the class the distinct-path rows
    /// cannot). The paired assertion pins the FIX: the SCOPED production `repo_brief` attributes
    /// ZERO of the sibling's refs to the primary path. If the unscoped side stops seeing the
    /// sentinel, the same-path harness is asleep and every join-by-path scoping test is toothless.
    #[test]
    fn same_path_tripwires_expose_a_join_by_path_leak() {
        let (_root, config) = poison_test_config("poison_samepath");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();

        // The real primary path the harness collided onto — re-resolved the same deterministic way
        // `primary_collision_path` picks it (src/lib.rs for this fixture).
        let collision_path: String = conn
            .query_row(
                "SELECT path FROM main.files WHERE repo_id != ?1 ORDER BY path LIMIT 1",
                [POISON_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !collision_path.starts_with(POISON_PREFIX),
            "the collision path must be a REAL primary path, got `{collision_path}`"
        );

        // The UNSCOPED join-by-path shape (the pre-fix `papertrail_ref_counts` CTE) sees the
        // sibling's colliding ref at the primary path — the tripwire is live.
        let unscoped_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM papertrail_refs WHERE source_path = ?1",
                [&collision_path],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            unscoped_refs >= 1,
            "same-path papertrail_refs tripwire is asleep: an unscoped path aggregate saw no \
             sibling ref at the primary path `{collision_path}`",
        );

        // The SCOPED production surface must NOT leak it: the fixture repo has no
        // papertrail_refs of its own, so its `src/lib.rs` candidate reports zero refs.
        let brief = db
            .repo_brief(rag_rat_query::repo_brief::RepoBriefOptions {
                mode: rag_rat_query::repo_brief::RepoBriefMode::Spine,
                limit: 50,
                include_generated: true,
                include_memories: false,
            })
            .unwrap();
        let primary = brief
            .candidates
            .iter()
            .find(|candidate| candidate.path == collision_path)
            .expect("the primary path must appear in the brief");
        assert_eq!(
            primary.metrics.papertrail_ref_count, 0,
            "repo_brief leaked a sibling repo's papertrail_refs across the shared path \
             `{collision_path}` — scope the papertrail_ref_counts CTE by repo_id",
        );

        // The same-path rows are counted by the intact check too.
        assert_sibling_intact(conn);
    }

    /// A full mirror rebuild (the full re-walk / recovery path) DELETEs the whole
    /// `papertrail_fts` mirror and re-derives it from the base tables — every poisoned base row
    /// becomes a derived mirror row. The harness must reconverge: the seeded mirror rows are
    /// derivation-faithful copies (same doc_kind / comment_id / url / title / body slots), so the
    /// rebuilt mirror carries the SAME tripwire set and `assert_sibling_intact` stays meaningful
    /// after a mid-test rebuild. This pins `seed_sibling`'s fts seeding to BOTH derivations (the
    /// incremental writers and `rebuild_fts` share the slot mapping) — if a column mapping in
    /// either drifts, this fails locally instead of surfacing as a phantom sibling leak in
    /// whichever papertrail test rebuilds first.
    #[test]
    fn papertrail_fts_tripwires_survive_a_mirror_rebuild() {
        let (_root, config) = poison_test_config("poison_resync");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();

        // Intact on the seeded mirror…
        assert_sibling_intact(conn);
        // …and byte-equivalently intact on the re-derived mirror.
        rag_rat_papertrail::rebuild_fts(conn).unwrap();
        assert_sibling_intact(conn);
    }

    /// Opt-out honored: with the guard held, a rebuild seeds NO tripwire rows.
    #[test]
    fn disabling_the_harness_seeds_no_tripwires() {
        let _guard = disable_poison_sibling();
        let (_root, config) = poison_test_config("poison_optout");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let sibling_files: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1",
                [POISON_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sibling_files, 0, "opt-out must seed no poison rows");
    }

    /// A7: on a git fixture (a REAL registered repo), the sibling now becomes a SECOND real repo —
    /// `repos` + `repo_roots` + `repo_meta` rows — so the DB is a genuine multi-repo shape and an
    /// unscoped registry read/count/delete trips a tripwire. The fixture resolves by identity /
    /// recorded root (never `sole_repo_id`), so the second real repo does not hijack it.
    #[test]
    fn the_sibling_is_a_real_repo_on_a_git_fixture() {
        let (_root, config) = poison_test_config("poison_registry");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();
        let poison_registered: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos WHERE repo_id = ?1", [POISON_REPO_ID], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            poison_registered, 1,
            "the sibling is registered as a real repo on a git fixture"
        );
        assert!(
            rag_rat_db::schema::multiple_real_repos(conn).unwrap(),
            "the fixture + the sibling make the DB genuinely multi-repo",
        );
        // The sibling's registry rows are counted by the intact check (the registry tripwires are
        // appended because `primary_is_real`).
        assert_sibling_intact(conn);
    }
}
