pub mod ai;
pub mod anchors;
pub mod chunker;
pub mod edges;
pub mod git_history;
pub mod ignore_rules;
pub(crate) mod languages;
pub mod papertrail_autosync;
pub mod parser;
pub mod symbols;
pub mod walker;

pub mod consolidate;
pub mod remove;

mod adoption_hints;
pub(crate) mod change_coupling;
pub(crate) mod chunk_text_store;
mod discovery;
mod file_index;
mod file_rows;
mod fts;
pub(crate) use fts::retry_once_on_fts_corruption;
pub use fts::{FtsHealOutcome, error_is_fts_corruption};
mod git_context;
mod git_meta;
mod graph_index;
mod incremental;
mod lifecycle;
mod mem_diag;
mod meta;
// Crate-internal: a `content_revision()` digest pinned to its connection write-state, threaded
// probe → clone delta by the watcher pass (#821).
pub(crate) use meta::ContentRevisionSnapshot;
mod migration_gate;
mod packages;
mod parser_failures;
mod prep;
mod query_api;
mod rebuild;
mod staleness;
// #77 Phase 2 chunk-text compression. pub(crate) so the read layer (`crate::query`) can decompress
// stored blobs, not just the index write path.
mod util;
mod worktree_overlay;
pub use adoption_hints::{
    EmptyIndexRefused, SameIdentityJoin, is_first_time_empty, is_first_time_empty_conn,
    is_root_already_indexed, same_identity_join_note, would_discover_any_file,
};
pub use discovery::DiscoveryStatus;
pub(crate) use discovery::*;
pub use git_context::resolve_git_context;
pub(crate) use git_context::*;

/// The domain builders the db layer's migrations/adoption may invoke (see
/// `rag_rat_db::hooks::MigrationHooks`) — constructed here because this crate is the one that
/// links every domain the hooks reach into.
pub fn migration_hooks() -> rag_rat_db::MigrationHooks {
    rag_rat_db::MigrationHooks {
        rederive_dream_finding_ids: rag_rat_dream::rederive_finding_ids,
        backfill_authority_projection: rag_rat_oplog::backfill_authority_projection,
        rebuild_papertrail_fts: rag_rat_papertrail::rebuild_fts,
        realign_logical_symbol_ids: graph_index::realign_logical_symbol_ids,
    }
}
// Only tests reach `install_scope_view` directly now: non-test code resolves the repo id
// explicitly (`resolve_scope_repo_id`) and passes it into `install_worktree_scope_view`, which
// writes the scope itself rather than routing through the config-blind `active_repo_id`
// fallback. Re-exported un-gated: the read-layer crate's tests reach it through the
// dev-dependency, and `#[cfg(test)]` does not propagate cross-crate.
pub use lifecycle::{
    GlobalStoreOverview, install_scope_view, install_worktree_scope_view, resolve_scope_repo_id,
};
pub(crate) use mem_diag::{maybe_set_sqlite_soft_heap_limit, mem_trace};
pub use parser_failures::ParserFailure;
pub(crate) use prep::*;
pub use query_api::{
    CLONE_DELTA_MAX_FILES, CandidateCloneClass, CloneCheckInput, CloneCompleteness,
    CloneDeltaReport, CloneEdgeReport, CloneEligibility, CloneFingerprintHealth,
    CloneIneligibilityReason, CloneMember, CloneSymbolSelector, ClonesForSymbolResult,
    DatabaseFileHealth, FindClonesOptions, FindClonesResult, FreelistReclaim,
    FreelistReclaimReport, GcReport, GlobalFtsStatus, GlobalStatus, ImportantSymbolsRequest,
    MemoryCounts, MemoryKindCounts, OracleShaSnapshots, PapertrailCursor, RepoContent,
    RepoFreshness, RepoPapertrail, RepoStatus, RoiFactors, SearchRequest, SyncCatchUpReport,
    TextCloneMatch, WAL_CHECKPOINT_MIN_BYTES, WalCheckpointReport, WorktreeOverlay,
    reclaim_freelist_at,
};
pub use schema::RegisteredRepo;
pub(crate) use util::*;
pub use worktree_overlay::{
    OverlayBasisUpdate, OverlayLogicalRebuild, OverlayRefreshTail, WorktreeOverlayReport,
};

#[cfg(test)]
mod anchor_tests;
#[cfg(test)]
mod chunker_tests;
#[cfg(test)]
mod parser_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;
use std::{fs, thread};

use gix::bstr::{BString, ByteSlice};
use gix::status::{UntrackedFiles, tree_index};
use rag_rat_base::config::{Config, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_db::schema;
use rag_rat_db::storage::{IndexConnection, StorageStatus};
use rag_rat_papertrail as papertrail;
use rag_rat_papertrail::{Papertrail, PapertrailEvidence, PapertrailStatus, PapertrailSyncReport};
use rag_rat_query::graph_meta::{self, GraphMetaMode};
use rag_rat_query::memory::AnchorHealth;
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::index::ai::{LlmStatus, ModelInfo, ReconcilePlan, ReconcileReport};
use crate::index::anchors::{AnchorStatus, ChunkAnchor};
use crate::index::chunker::Chunk;
use crate::index::git_history::{
    ChunkBlameSummary, CommitSearchHit, GitHistoryIndexStatus, PathHistoryItem, QueryCommitHit,
    SymbolHistoryItem,
};
use crate::index::symbols::Symbol;
use crate::search::lexical::SearchHit;

#[derive(Debug)]
pub struct IndexDatabase {
    storage: IndexConnection,
    /// The repo this connection is scoped to — resolved once at open (`register_repo` on a
    /// config-bearing open, the sole registered repo on a bare/read-only open) and stamped onto
    /// every direct-scoped write. Mirrored into `temp.connection_context` by `install_scope_view`
    /// so free-conn helpers resolve the same id via `schema::active_repo_id`. Empty only between
    /// construction and the first repo resolution.
    pub active_repo_id: String,
    pub active_commit_sha: String,
    pub active_worktree_id: String,
    /// The `files.generation` this connection writes at and scopes to (A6): the repo's LIVE
    /// generation on a reader / incremental open, the WRITE generation N+1 while a full rebuild
    /// stages a fresh generation before flipping the live pointer. Resolved by `set_context` /
    /// `set_context_at_generation`; every direct-scoped file INSERT stamps it. `0` until the first
    /// context is installed — the generation a fresh index and every pre-V043 row carries.
    pub active_generation: i64,
    /// Injected tracker repo context. Resolved from `gh` only in `open_config` (real usage);
    /// `rebuild`/`open` leave it offline, and tests set it explicitly — so the library never
    /// shells out to `gh` during tests (#60).
    papertrail: papertrail::PapertrailContext,
    /// The config this index was opened against — present only on the config-bearing opens
    /// (`open_config` / `try_open_config_read_only`), `None` for `rebuild`/`open`/tests. Lets a
    /// read tool classify a working-tree change set against the indexed targets — the lazy
    /// zero-hit heal (`symbol_candidates`, #152) needs it to index a just-added file the watcher
    /// hasn't caught yet. Without it that heal is a graceful no-op.
    config: Option<Config>,
    /// The RESOLVED repo's per-repo write lock, held for this connection's lifetime when identity
    /// resolution lands on a repo whose lock differs from the one the surrounding command took at
    /// entry (A6, batch-5 P2 — the fence gap). Entry locks are keyed by the id DERIVED before
    /// open; an unshallow between derivation and adoption re-points the repo to a portable id, so
    /// without this the writer would keep writing the portable repo's rows under only the stale
    /// `local:` lock while a fresh portable-lock writer runs concurrently. RULE: a writer's held
    /// lock must match the repo id it writes under. `None` on the lockless open paths (they stay
    /// lockless by design) and whenever the entry lock already covers the resolved id.
    _identity_lock: Option<rag_rat_base::locks::WriteLock>,
    /// The lazily captured #493 drift-heal snapshot, tagged with the repo it was captured for.
    /// Populated by the FIRST [`Self::remove_file_in_scope`] of a pass while the key-version
    /// stamp is stale — the symbol rows about to be deleted carry the snapshot's signature
    /// evidence, so it must be memoized before the first deletion — and consumed by the next
    /// [`Self::rebuild_logical_symbols`] (which captures fresh when nothing was removed: the
    /// evidence is still intact then). Idle passes never remove a file, never populate this, and
    /// never pay the snapshot scan. Interior mutability because the deleters take `&self`.
    drift_snapshot:
        std::sync::Mutex<Option<(String, Option<Vec<graph_index::LogicalKeyDriftRow>>)>>,
    /// #827: when armed, the pass captures the source files whose edges a scoped re-resolve must
    /// rewrite — the changed files it (re)writes plus the source files of the in-edges its
    /// removals NULL — into `temp.edge_rewrite_files`, so `resolve_changed_edges` narrows the
    /// write set to those instead of every edge in the active scope. Armed for the duration of
    /// an incremental content-changed pass by `begin_scoped_edge_rewrite`; the capture seams
    /// (`remove_file_in_scope`, the incremental file insert) take `&self`, hence interior
    /// mutability. `Relaxed` is sufficient: capture and resolve run on one connection/thread
    /// within the pass, never a cross-thread handoff.
    edge_rewrite_capture: AtomicBool,
    /// #826: when armed, the pass captures the PATHS whose symbols it rewrote / removed / healed into
    /// `temp.logical_rederive_paths`, so `rederive_changed_logical_symbols` re-derives only those
    /// paths' `logical_symbols` groups instead of the whole repo. Armed by
    /// `begin_scoped_logical_rederive` for the base incremental pass AND the linked-worktree
    /// overlay finalize; the capture seams (`remove_file_in_scope`, the incremental file
    /// insert) take `&self`. A SEPARATE flag from `edge_rewrite_capture` (not a shared one)
    /// because #827's edge narrowing does NOT run in the overlay pass — arming a shared flag
    /// there would pointlessly stage `temp.edge_rewrite_files`. `Relaxed` for the same
    /// single-thread reason as #827.
    logical_rederive_capture: AtomicBool,
    /// Test-only #819 observability: how many times [`Self::rebuild_logical_symbols`] ran on this
    /// connection, so batch tests can assert the once-per-pass rebuild cardinality.
    #[cfg(test)]
    pub(crate) logical_symbol_rebuilds: AtomicUsize,
}

#[derive(Debug, Clone)]
pub enum IndexProgress {
    Started {
        database: PathBuf,
        mode: IndexMode,
    },
    Discovering,
    Discovered {
        files: usize,
    },
    PreparingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
    IndexingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
    IndexingGitHistory,
    RebuildingLogicalSymbols,
    ResolvingGraph,
    SyncingFts,
    RebuildingFts,
    Finished {
        files: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Changed,
    /// Reconcile exactly an explicitly-supplied set of candidate paths (#659) — the substrate for
    /// edit-driven reindex. Like `Changed` (working-tree-scoped, content-hash decides staleness, no
    /// carry), but the change set comes from the caller's path list rather than a git-status walk,
    /// so it also sees committed changes the status walk would not. The path list is threaded
    /// alongside the mode (`explicit_paths`) rather than carried in the variant, so `IndexMode`
    /// stays `Copy` (it is copied at several hot sites in the pass). Unlike `Changed`, it never
    /// promotes to `Discover` on an incomplete base scope — it is deliberately scoped.
    Paths,
    Discover,
    Full,
}

impl IndexMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Changed => "changed files",
            Self::Paths => "explicit paths",
            Self::Discover => "discovery",
            Self::Full => "full rebuild",
        }
    }

    /// Whether a tombstone in this mode's `deleted` set is a pure FILESYSTEM deletion (the path
    /// left the tree) rather than a SEMANTIC one (the path left the target set / became ignored
    /// but still exists). Only fs-deletion modes get the #561 restore recheck at apply time —
    /// see `write_prepared_incremental_files`. `Changed` and `Paths` both derive `deleted`
    /// purely from "the file is not on disk", so both revalidate; `Discover`'s plan also
    /// tombstones semantic deletions that must land regardless of disk existence, so it does
    /// not.
    pub(crate) fn revalidates_fs_deletions(self) -> bool {
        matches!(self, Self::Changed | Self::Paths)
    }
}

#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub database: String,
    pub exists: bool,
    pub schema: schema::SchemaStatus,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
    pub indexed_at_ms: Option<i64>,
    pub content_revision: String,
    pub fts_synced_at_ms: Option<i64>,
    pub fts_source_revision: Option<String>,
    pub fts_dirty: bool,
    pub fts_fresh: bool,
    pub file_count_by_language: BTreeMap<String, u64>,
    pub parser_failures: u64,
    pub parser_failure_paths: Vec<ParserFailure>,
    /// HIGH-WATER MARK of watch-placement failures — the MOST any single watcher lifetime has
    /// recorded for this repo (a per-lifetime MAXIMUM, not a running sum across lifetimes: two
    /// watcher runs that dropped 3 and 2 watches report 3, not 5). A degradation SIGNAL, not a
    /// live "currently degraded" gauge. Nonzero means at least one directory has, at some
    /// point, fallen back to the periodic sweep because its watch could not be placed (on
    /// Linux, usually `fs.inotify.max_user_watches` exhaustion); it is worth investigating,
    /// not proof that watches are dropped right now. Deliberately NEVER lowered — a healthy or
    /// freshly-restarted watcher sharing the DB must not be able to erase a concurrently
    /// degraded watcher's record — so it does NOT clear when the condition is fixed and
    /// watches are re-placed cleanly; treat it as "has this index ever dropped a watch", not
    /// "is it dropping them now". 0 only when no watcher has ever recorded a failed placement.
    pub watch_placement_failures: u64,
    pub git_history: GitHistoryIndexStatus,
    pub papertrail: PapertrailStatus,
    pub llm: LlmStatus,
    pub anchor_health: AnchorHealth,
}

#[derive(Debug, Serialize)]
pub struct HealIndexReport {
    pub checked_files: u64,
    pub healed_files: u64,
    pub removed_files: u64,
    pub skipped_files: u64,
    pub fts_fresh: bool,
    /// FTS mirrors whose RANKED probe hit SQLITE_CORRUPT and were rebuilt from source (#582).
    /// Ranked because only rank/bm25 decodes docsize — the corruption class both
    /// `PRAGMA integrity_check` and FTS5's own `'integrity-check'` miss.
    pub fts_healed: Vec<String>,
    /// Mirrors whose probe hit corruption but whose repair was DEFERRED: a generation-staged
    /// rebuild is in flight, and 'delete-all' + re-derive would drop its not-yet-durable rows.
    /// Non-empty means ranked queries on these mirrors still fail — rerun `heal_index` once the
    /// rebuild completes (or after gc sweeps an abandoned staging).
    pub fts_deferred: Vec<String>,
    pub message: Option<String>,
}

const MAX_AUTO_HEAL_FILES_PER_CALL: usize = 4;
// Bumped whenever the resolved graph's SHAPE changes so an upgraded index re-resolves its edges on
// next open (`ensure_graph_index_current`) instead of carrying forward stale resolutions. 7: the
// per-package + module-aware import scope (#61) — re-resolve repopulates the `packages` rows and
// re-derives the dedicated `import_scope_*` edge columns, which the V022 migration only ADDED
// (never backfilled), so without the bump a migrated index keeps empty package scopes and the
// global-fallback behavior forever. The file→package mapping itself is computed at LOAD time from
// `packages` (no persisted `files.package_id`), so the re-resolve only needs the `packages` rows.
// 8: #200 — the graph re-extract+resolve now emits the `dispatch_construct`/`dispatch_handle` FACT
// rows and synthesizes `dispatches` edges; without the bump a migrated v7 index would carry NO
// dispatch edges until a manual rebuild, since the re-resolve (which re-extracts edges) never runs.
// 9: #207/#208 — the dispatch HANDLER detection changed (conservative closed recognizer), so the
// `dispatch_handle` facts an existing v8 index extracted are stale (old tail-only handlers); the
// bump re-extracts them so the corrected `dispatches` edges reach deployed indexes.
// 10: #208 review rounds 9/10 — effect-only handler fallback (`h()?; Ok(unit)`), wrapper/delegate
// reclassification (PascalCase tails, UFCS, Err), if-let payloads, `let mut`, field-store bails;
// re-extract so the corrected `dispatch_handle` set reaches deployed indexes.
// 11: #208 review round 11 — effect-only fallback records the direct call (no shadowing
// misresolve); method calls on scoped receivers (`worker.run()`) are recorded again; re-extract.
const GRAPH_INDEX_VERSION: &str = "11";

// Bumped when the DEFINITION of `files.generated` changes, so an existing index re-derives the flag
// on next open. Incremental discovery only rewrites a file row when its sha/language/kind changes —
// a flag whose *meaning* changed (not its inputs) never refreshes otherwise, so a clean
// `src/generated/...` file under a source target would keep `generated = 0` and stay in default
// symbol/search results until a manual full reindex. 1: #202 — `generated` now also covers
// `is_generated_path` codegen living under a source target.
const GENERATED_FLAGS_VERSION: &str = "1";
const GENERATED_FLAGS_VERSION_KEY: &str = "generated_flags_version";

// Bumped when [`graph_index::LogicalSymbolKey`]'s DERIVATION changes semantics — a
// signature-capture fix, a kind-classification change, a qualified-name derivation change, or a
// grammar bump that shifts any of them. The stable logical id hashes those fields, so a semantic
// change churns EVERY logical id in a repo at its next rebuild at once, stranding every
// logical-symbol memory binding (and moniker / call-path reference) simultaneously (#493). On a
// mismatch the rebuild snapshots the referenced old rows and realigns them onto the re-derived
// ids (`heal_logical_key_drift`); matching is evidence-gated, so bump this LIBERALLY on any doubt
// — a no-drift heal is a cheap no-op, an unbumped drift is a whole-repo stranding healed one
// validate at a time. Per-repo (`repo_meta`, like `graph_index_version`): a shared DB holds repos
// rebuilt by different binaries.
// 1: initial version (#493).
const LOGICAL_KEY_VERSION: &str = "1";
const LOGICAL_KEY_VERSION_KEY: &str = "logical_key_version";

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("Gone: indexed chunk {chunk_id} no longer exists")]
    Gone { chunk_id: i64 },
    #[error("StaleChunk: chunk {chunk_id} in {path} could not be relocated after reindex")]
    StaleChunk { chunk_id: i64, path: String },
    #[error("needs_reindex: {stale_files} stale files exceeds automatic heal cap {cap}")]
    NeedsReindex { stale_files: usize, cap: usize },
}

#[derive(Debug)]
struct FileRow {
    language: Language,
    kind: TargetKind,
}

#[derive(Debug)]
struct GraphReindexFile {
    id: i64,
    path: String,
    language: Language,
    kind: TargetKind,
}

#[derive(Debug)]
struct GraphPathRow {
    language: String,
    sha256: String,
    indexed_revision: String,
}

#[derive(Debug)]
struct IndexedFile {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexFile {
    full_path: PathBuf,
    relative_path: PathBuf,
    language: Language,
    kind: TargetKind,
    commit_sha: String,
    worktree_id: String,
}

#[derive(Debug, Clone)]
struct FileScope {
    commit_sha: String,
    worktree_id: String,
}

impl FileScope {
    fn commit(commit_sha: String) -> Self {
        Self { commit_sha, worktree_id: String::new() }
    }

    fn worktree(worktree_id: String) -> Self {
        Self { commit_sha: String::new(), worktree_id }
    }
}

#[cfg(test)]
mod schema_bootstrap_tests;

// The poison-sibling test harness. `pub(crate)` (not private) so the `rebuild` seam and the
// mutating tests across submodules can reach `seed_if_enabled` / `disable_poison_sibling` /
// `assert_sibling_intact`. See the module docs for what it enforces and how to opt out.
#[cfg(test)]
pub(crate) mod poison_sibling;

#[cfg(test)]
mod generated_path_tests {
    use rag_rat_base::path_class::is_generated_path;

    #[test]
    fn generated_dirs_match_at_any_depth_including_root() {
        // Nested and root-level codegen dirs both count (#202 review P2): the old
        // `contains("/generated/")` needed a leading separator and missed the root case.
        assert!(is_generated_path("packages/held-core/src/generated/foo.ts"));
        assert!(is_generated_path("generated/bindings.rs"));
        assert!(is_generated_path("generated-web/foo.ts"));
        assert!(is_generated_path("apps/web/src/generated-web/bar.ts"));
        // Declaration / wasm-bindgen output by suffix.
        assert!(is_generated_path("types/index.d.ts"));
        assert!(is_generated_path("pkg/app_bg.wasm.d.ts"));
        // Hand-written source is not generated — and a substring near-miss must not false-positive.
        assert!(!is_generated_path("src/lib.rs"));
        assert!(!is_generated_path("src/pre-generated-data/seed.rs"));
        assert!(!is_generated_path("src/generator.rs"));
    }
}
