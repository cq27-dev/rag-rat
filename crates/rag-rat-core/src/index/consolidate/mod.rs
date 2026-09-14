//! `rag-rat consolidate` (memory-sync phase A7): import a repo's legacy per-repo index into the
//! consolidated GLOBAL database, then rename the legacy file out of the way so the config default
//! resolves to the global store from then on.
//!
//! What is imported is exactly the AUTHORED and EXPENSIVE data — `repo_memories` (with their
//! bindings / tags / call-paths / call-path edges), the content-addressed `embedding_cache`, and
//! the durable model-identity `repo_meta` keys. Everything else is DERIVED (chunks, symbols, edges,
//! FTS, …) and is cheaper and safer to rebuild than to translate across rowid spaces — so a fresh
//! index of the consolidated repo regenerates it, and the carried `embedding_cache` makes the
//! re-embedding a no-op (its `input_hash` folds model + version + input text).
//!
//! POSTURE (spec §3.4):
//! - Portable bindings only: the memory bindings' LOCAL rowid columns (`logical_symbol_id` /
//!   `symbol_id` / `chunk_id` / `edge_id`) are NULLed on import — those rowids mean nothing in a
//!   fresh index — and the normal validate loop re-resolves them from the portable anchor (path /
//!   commit / tracker / moniker fields, which ARE copied verbatim) after the next index pass.
//! - `live_files_generation` is NOT carried (absent ⇒ 0 is load-bearing: a fresh index of the
//!   consolidated repo stages above 0 and flips normally — A6 handoff rule #1).
//! - Idempotent AND crash-honest: a no-edit retry writes nothing (content-gated upserts / `INSERT
//!   OR IGNORE`), and a retry after a crashed RENAME carries legacy-side edits made in the window
//!   forward — the legacy file is the live store until the rename lands, so its content REPLACES
//!   stale same-repo global copies (children included). Once the legacy file is renamed to
//!   `index.sqlite.imported` a re-run is a no-op with a notice.
//! - LOCKED: the whole registration → import → rename sequence runs under the repo's per-repo write
//!   locks — on the TARGET side (a writer's held lock must match the repo id it writes under, the
//!   A6 structural rule) AND on the SOURCE side (a watcher / MCP server still pointed at the legacy
//!   file keys its lock beside THAT path; holding it means no lock-disciplined writer can append to
//!   the legacy DB between the snapshot read and the rename — writes there would otherwise silently
//!   vanish into the renamed artifact).
//! - An explicit `[index] database` key is REFUSED outright (no import, no side effects): renaming
//!   the file out from under the still-pinned config would strand it on a fresh empty DB, and an
//!   early import WITHOUT the rename would open a DELIBERATE divergence window with the global
//!   store reachable, which the refusal exists to prevent (the crash-retry upsert covers the
//!   accidental window a failed rename creates). A pinned config never reads the global store, so
//!   an early import has no value; the refusal names the remedy (remove the key, re-run) and the
//!   single completing run imports and renames atomically with no window.

use std::path::PathBuf;

mod copy_children;
pub(crate) mod import;
mod meta_merge;
mod run;

pub use run::{run, run_with_config_path};

/// The `repo_meta` keys consolidate carries from the legacy DB into the global DB. Carried as a
/// per-key MIRROR of the source (the import's mirror invariant): a key present in the legacy DB
/// upserts (value-gated, so an unchanged retry writes nothing), a carried key ABSENT there
/// deletes any stale target row — the legacy DB is the live store until the rename lands, so a
/// model switch made in the crash-retry window (including one that removes a key, e.g. dropping
/// the remote config when moving to a local model) must win over the previous run's copy.
/// Nothing else writes these keys for a mid-consolidate repo (the import holds the repo's write
/// locks and the target migrate is schema-only), so there is no competing "config-seeded" value
/// to preserve.
///
/// CLASSIFICATION RULE (every `repo_meta` key must land in exactly one class — classify a NEW key
/// here at birth):
///
///  (a) REPO-PORTABLE CONFIGURATION — durable "how this repo embeds" identity/state that must
///      survive the move or the carried `embedding_cache` / remote transport is stranded → COPIED
///      (as ONE model-state unit — see [`meta_merge::copy_model_state`], which also carries the
/// active      model's `ai_models` readiness row):
///      * `active_embedding_model` — which embedder the cache rows belong to;
///      * `embedding_active_model_version` — the active model's freshness key;
///      * `active_embedding_remote_config` — the persisted remote-endpoint config
///        `active_embedder()` reconstructs its query/connect-mode transport from; dropping it would
///        silently reroute post-consolidation searches to the local backend (or lexical) until the
///        model is reinstalled;
///      * `active_embedding_model_provisional` — SEMANTIC state, not transient: absent reads as
///        NON-provisional (an explicit, config-immune choice), so dropping a set `"1"` would
///        CONVERT an auto-selected model into a confirmed one and `seed_active_embedding_model`
///        could no longer override/clear it from config. ABSENCE-HAS-MEANING keys like this need
///        the absent state classified too, not just the value.
///      * `memory_stream_seal_policy` — the one-way privacy intent for the repo's owner stream.
///        Unlike model-state keys it is MERGED monotonically: `sealed` on either side wins, absence
///        never clears it, and unknown values abort consolidation before reconciliation.
///      * `memory_stream_pin` — the owner this repo has trusted (see `sync subscribe`). MERGED, not
///        copied verbatim: it is the one subscription field built to outlive the subscription, so
///        dropping it on the move would let a changed `.rag-rat-stream` read as first use. The
///        source's EFFECTIVE pin crosses — its recorded pin, else the owner it is subscribed to,
///        which a store from before the pin existed records its trust decision as (that
///        subscription itself never crosses — see (b)). Until the rename lands the legacy index is
///        the live store, so a retry of the SAME legacy source replaces the pin its earlier,
///        unfinished run wrote (recorded, with that source, in `memory_stream_pin_imported`, and
///        retired once the rename lands); any other conflicting pin REFUSES the run, since carrying
///        either would silently override the other trust root.
///
///  (b) DB-LOCAL STATE — never copied; each entry states why:
///      * freshness/progress cursors that would make a fresh 0-row index falsely report itself
///        current: `content_revision`, `git_commit`, `git_dirty`, `git_history_indexed_head` /
///        `_root` / `_shallow` / `_complete`, `papertrail_last_sync_ms`, `graph_index_version`,
///        `indexed_at_ms`, `vector_int8_reencode_done` / `_cursor`,
///        `last_embedding_reconcile_started_at_ms` / `_finished_at_ms`;
///      * pointers/state owned by THIS database file's lifecycle: `live_files_generation` (absent ⇒
///        0 is load-bearing — A6), `clone_graph_live_generation`, `shallow_boundary` (adoption
///        proof for the legacy file's own registry), `source_root` (re-recorded by registration);
///      * derived/re-derivable caches: `local_crate_roots` (re-read from manifests),
///        `embedding_throughput_tune_v1` (a tuning cache, re-derived);
///      * `memory_contribution_owner` / `memory_subscription_owner` — the accounts whose stream
///        materializes the repo. Not copied and never needed:
///        `ensure_not_mirroring_another_account` REFUSES the whole run when the TARGET carries
///        either key, and a legacy source that carries one has nowhere to put it (the target's own
///        key must stay authoritative — it is what the target's drain already acted on). A repo
///        consolidates only while it owns its own stream.
///      * `memory_subscription_peers` / `memory_subscription_relay` — how to reach the subscribed
///        owner's host. They describe a subscription, and the subscription never crosses; carried
///        alone they would route toward an owner this repo no longer mirrors.
///      * `memory_stream_pin_imported` — the pin an UNFINISHED consolidation wrote and the legacy
///        source it came from, which is how a retry of that source tells its own stale copy from a
///        trust decision; it is retired once the rename lands, and any `sync subscribe` clears it.
const CARRIED_META_KEYS: &[&str] = &[
    "active_embedding_model",
    "embedding_active_model_version",
    "active_embedding_remote_config",
    "active_embedding_model_provisional",
    "memory_stream_seal_policy",
    "memory_stream_access_mode",
];

const MEMORY_STREAM_SEAL_POLICY_META_KEY: &str = "memory_stream_seal_policy";
const MEMORY_STREAM_ACCESS_MODE_META_KEY: &str = "memory_stream_access_mode";
const MEMORY_STREAM_PIN_META_KEY: &str = "memory_stream_pin";
const MEMORY_STREAM_PIN_IMPORTED_META_KEY: &str = "memory_stream_pin_imported";
const MEMORY_SUBSCRIPTION_OWNER_META_KEY: &str = "memory_subscription_owner";

/// The result of a `rag-rat consolidate` run — the CLI renders it; the core owns the logic.
#[derive(Debug)]
pub enum ConsolidateOutcome {
    /// The repo already resolves to the global database (no `database` key, no legacy file, or an
    /// explicit `database` already pointing at the global store) — nothing to do.
    AlreadyGlobal { database: PathBuf },
    /// A previous run already imported this repo (the `.imported` marker is present) — a no-op.
    AlreadyImported { imported: PathBuf },
    /// No legacy per-repo index exists to import (a fresh repo already on the global default).
    NoLegacyIndex { source: PathBuf },
    /// The legacy index was imported (and, unless the config pins an explicit `database` key,
    /// renamed away).
    Imported(ImportSummary),
}

/// Per-run import counts + the paths involved, for the CLI summary. Counts reflect rows actually
/// WRITTEN — an idempotent re-run over already-imported rows reports zeros, not phantom copies.
#[derive(Debug)]
pub struct ImportSummary {
    pub repo_id: String,
    pub source: PathBuf,
    /// The `index.sqlite.imported` marker the legacy file was renamed to (always — a pinned config
    /// is refused before any import, so a summary only exists for a completed import + rename).
    pub renamed_to: PathBuf,
    pub target: PathBuf,
    pub counts: ImportCounts,
}

/// Import counts, threaded back to the [`ImportSummary`].
#[derive(Debug)]
pub struct ImportCounts {
    pub memories: u64,
    pub bindings: u64,
    pub tags: u64,
    pub call_paths: u64,
    pub call_path_edges: u64,
    pub edges: u64,
    pub embedding_cache_rows: u64,
    pub meta_keys: u64,
}
