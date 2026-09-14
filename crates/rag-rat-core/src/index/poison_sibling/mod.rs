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
//! `repo_memories`) — AND the dream-verification siblings `memory_reality` /
//! `memory_note_summaries` / the retired `memory_summaries` / `memory_model_failures`, each of
//! which carries its own `repo_id` — AND the typed-edge set
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

use rusqlite::Connection;

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

mod assert;
mod seed;
#[cfg(test)]
mod tests;

pub(crate) use assert::assert_sibling_intact;
pub(crate) use seed::seed_sibling;
