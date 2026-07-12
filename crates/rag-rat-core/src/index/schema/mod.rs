mod baseline;
mod migrations;
mod registry;
pub(crate) use baseline::*;
pub(crate) use migrations::*;
// `multiple_real_repos` lost its V042-era seam-guard callers (real `repo_id` predicates
// superseded them, A5), but A7's bare-open fail-fast made it production again: a config-less
// `IndexDatabase::open` refuses a multi-repo DB rather than silently scoping to the
// lexicographically-first repo.
pub(crate) use registry::multiple_real_repos;
pub(crate) use registry::{
    CONNECTION_CONTEXT_GENERATION_KEY, CONNECTION_CONTEXT_REPO_KEY, LIVE_FILES_GENERATION_META_KEY,
    active_generation, active_repo_id, connection_context_value, earliest_recorded_root,
    live_files_generation, periphery_repo_scope, periphery_repo_scope_clause, registered_repos,
    repo_has_recorded_root, repo_id_is_registered, resolve_config_repo_id, scope_context_repo_id,
    sole_repo_id,
};
pub use registry::{LEGACY_REPO_ID, RegisteredRepo, register_repo, register_repo_read_only};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const LATEST_SCHEMA_VERSION: u32 = 59;

/// Every oracle-DERIVED persisted table — the outputs an `oracle run` writes that must OUTLIVE a
/// reindex.
///
/// INVARIANT (load-bearing, #248): oracle-derived outputs MUST survive reindex. They achieve this
/// by being **content-keyed with NO reindex-cascading FK** to a reindex-volatile parent
/// ([`REINDEX_VOLATILE_PARENTS`]); their reads JOIN the live parents by that content key, so a
/// dangling row (whose parent was rewritten) simply never resolves rather than being CASCADE-wiped.
/// This is the model `logical_symbol_monikers` shipped (#70) and `edge_oracle` was retrofitted to
/// (#248) after its `FOREIGN KEY(edge_id) REFERENCES edges_data(id) ON DELETE CASCADE` silently
/// wiped every verdict on the first reindex.
///
/// The ENFORCING structural trip-wire `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`
/// scans EVERY table in `sqlite_master` (not this list) and asserts none carries an `ON DELETE
/// CASCADE`/`RESTRICT` FK to a volatile parent except the explicit [`CASCADE_FK_ALLOWLIST`] — the
/// exact check that would have caught the original `edge_oracle` FK, and which catches a future
/// oracle/durable table automatically even if it forgets to opt into any list. This const remains
/// the canonical DECLARATION of which outputs must survive reindex — the same trip-wire asserts
/// each listed table exists, and the lifecycle guard
/// `oracle_outputs_survive_full_and_incremental_reindex` exercises their survival behaviorally
/// across both reindex shapes.
// Consumed only by the reindex trip-wires (#[cfg(test)]), so the non-test lib build sees no reader;
// it is intentionally a durable schema-invariant declaration.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ORACLE_PERSISTED_TABLES: &[&str] =
    &["edge_oracle", "logical_symbol_monikers", "oracle_runs", "external_symbols"];

/// The parent tables a reindex REWRITES (full rebuild and/or per-file `remove_file_in_scope`), so
/// an `ON DELETE CASCADE`/`RESTRICT` FK from an oracle-derived table to one of these wipes the
/// oracle output on every reindex — the #248 bug class. The trip-wire forbids exactly such an FK.
/// `files` is rowid-keyed and rewritten per file; `edges_data` / `symbols` / `logical_symbols` are
/// all rebuilt (DELETE-all + reinsert) on a full reindex.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const REINDEX_VOLATILE_PARENTS: &[&str] =
    &["edges_data", "symbols", "logical_symbols", "files"];

/// The `(child_table, volatile_parent)` pairs that LEGITIMATELY carry an `ON DELETE
/// CASCADE`/`RESTRICT` FK to a reindex-volatile parent ([`REINDEX_VOLATILE_PARENTS`]) — every one a
/// table that is *rebuilt with its parent* on every reindex and holds NO oracle/durable state, so
/// the cascade is the desired freshness behavior (the child must die with the parent row that
/// produced it), not the #248 data-loss bug.
///
/// INVARIANT (#248): this is the EXPLICIT opt-out the enforcing trip-wire
/// [`no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`] consults. The trip-wire scans EVERY
/// table in `sqlite_master` (not a hand-maintained list), so a NEW table that adds a cascading FK
/// to a volatile parent FAILS the test automatically unless it is added here WITH a reason. Never
/// allowlist a table that holds oracle/durable state — that re-creates the #248 bug; re-anchor such
/// a table on a content key + drop the FK instead.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const CASCADE_FK_ALLOWLIST: &[(&str, &str)] = &[
    // `chunks` are per-file text/embedding inputs, re-chunked from scratch when a file is
    // reindexed.
    ("chunks", "files"),
    // `edges_data` is the heuristic graph itself, rebuilt per-file (it IS a volatile parent).
    ("edges_data", "files"),
    // `symbols` are rebuilt per-file (AUTOINCREMENT rowids re-mint on reindex).
    ("symbols", "files"),
    // Logical-symbol grouping is rebuilt wholesale from the live symbols on every pass.
    ("logical_symbol_members", "symbols"),
    ("logical_symbol_members", "logical_symbols"),
    // Per-symbol derived facts, rebuilt with the symbols they describe.
    ("symbol_facts", "symbols"),
    // Clone-detection fingerprints, rebuilt with the symbols. The token bag rides this row as the
    // `token_bag` BLOB column (#231) — `symbol_token_postings` was dropped in V032, so its former
    // allowlist entry is gone.
    ("symbol_fingerprints", "symbols"),
];

const DIRTY_MIGRATION_ID: &str = "__dirty__";
const MIGRATION_001_ID: &str = "001_sqlite_storage_baseline";
const MIGRATION_001_CHECKSUM: &str = "sha256:rag-rat-sqlite-baseline-v1";
const MIGRATION_001_DESCRIPTION: &str =
    "SQLite storage baseline with FTS, tree-sitter graph edges, git/GitHub, and local AI metadata";
const MIGRATION_002_ID: &str = "002_embedding_vector_metadata";
const MIGRATION_002_CHECKSUM: &str = "sha256:rag-rat-embedding-vector-metadata-v2";
const MIGRATION_002_DESCRIPTION: &str =
    "Add embedding model dimension metadata and per-vector dimensions for hybrid vector search";
const MIGRATION_003_ID: &str = "003_derived_artifact_reconcile_metadata";
const MIGRATION_003_CHECKSUM: &str = "sha256:rag-rat-derived-artifact-reconcile-metadata-v3";
const MIGRATION_003_DESCRIPTION: &str = "Add model version, retry metadata, summaries, and \
                                         reconcile meta for diff-based derived artifact \
                                         reconciliation";
const MIGRATION_004_ID: &str = "004_edge_source_target_spans";
const MIGRATION_004_CHECKSUM: &str = "sha256:rag-rat-edge-source-target-spans-v4";
const MIGRATION_004_DESCRIPTION: &str =
    "Add exact source call-site spans and resolved target line spans to graph edges";
const MIGRATION_005_ID: &str = "005_edge_evidence_and_resolution";
const MIGRATION_005_CHECKSUM: &str = "sha256:rag-rat-edge-evidence-resolution-v5";
const MIGRATION_005_DESCRIPTION: &str =
    "Add raw graph edge evidence, receiver hints, qualified targets, and resolution reasons";
const MIGRATION_006_ID: &str = "006_embedding_policy_and_input_hash";
const MIGRATION_006_CHECKSUM: &str = "sha256:rag-rat-embedding-policy-input-hash-v6";
const MIGRATION_006_DESCRIPTION: &str = "Add embedding eligibility policy, priority, bounded \
                                         input hash, and reconcile throughput metadata";
const MIGRATION_007_ID: &str = "007_logical_symbol_groups";
const MIGRATION_007_CHECKSUM: &str = "sha256:rag-rat-logical-symbol-groups-v7";
const MIGRATION_007_DESCRIPTION: &str =
    "Add logical symbol groups for cfg variants and duplicate definitions";
const MIGRATION_008_ID: &str = "008_commit_addressable_worktrees";
const MIGRATION_008_CHECKSUM: &str = "sha256:rag-rat-commit-addressable-worktrees-v8";
const MIGRATION_008_DESCRIPTION: &str =
    "Add commit_sha and worktree_id to files table for multi-worktree / multi-branch support";
const MIGRATION_009_ID: &str = "009_github_ref_sync_state";
const MIGRATION_009_CHECKSUM: &str = "sha256:rag-rat-github-ref-sync-state-v9";
const MIGRATION_009_DESCRIPTION: &str =
    "Add per-GitHub-ref sync state for resumable papertrail cache updates";
const MIGRATION_010_ID: &str = "010_symbol_facts";
const MIGRATION_010_CHECKSUM: &str = "sha256:rag-rat-symbol-facts-v10";
const MIGRATION_010_DESCRIPTION: &str =
    "Add normalized symbol facts for parsed language metadata such as Rust attributes";
const MIGRATION_011_ID: &str = "011_repo_memories";
const MIGRATION_011_CHECKSUM: &str = "sha256:rag-rat-repo-memories-v11";
const MIGRATION_011_DESCRIPTION: &str =
    "Add source-anchored repo memories bound to symbols, chunks, paths, and papertrail refs";
const MIGRATION_012_ID: &str = "012_repo_memory_call_paths";
const MIGRATION_012_CHECKSUM: &str = "sha256:rag-rat-repo-memory-call-paths-v12";
const MIGRATION_012_DESCRIPTION: &str =
    "Add edge and call-path memory bindings for graph traversal surfacing";
const MIGRATION_013_ID: &str = "013_graph_file_lookup_indexes";
const MIGRATION_013_CHECKSUM: &str = "sha256:rag-rat-graph-file-lookup-indexes-v13";
const MIGRATION_013_DESCRIPTION: &str =
    "Add graph file lookup indexes for ownership clustering and file-level graph summaries";
const MIGRATION_014_ID: &str = "014_repo_memory_binding_signals";
const MIGRATION_014_CHECKSUM: &str = "sha256:rag-rat-repo-memory-binding-signals-v14";
const MIGRATION_014_DESCRIPTION: &str =
    "Add symbol_kind + signature_hash to repo_memory_bindings for durable cross-file relocation";
const MIGRATION_015_ID: &str = "015_repo_memory_call_path_edges";
const MIGRATION_015_CHECKSUM: &str = "sha256:rag-rat-repo-memory-call-path-edges-v15";
const MIGRATION_015_DESCRIPTION: &str =
    "Add ordered edge fingerprints behind server-derived call-path hashes for validation";
const MIGRATION_016_ID: &str = "016_symbol_line_spans";
const MIGRATION_016_CHECKSUM: &str = "sha256:rag-rat-symbol-line-spans-v16";
const MIGRATION_016_DESCRIPTION: &str = "Store start_line/end_line on symbols so readers skip the \
                                         per-symbol chunk-containment subqueries";
const MIGRATION_017_ID: &str = "017_edge_callee_byte_range";
const MIGRATION_017_CHECKSUM: &str = "sha256:rag-rat-edge-callee-byte-range-v17";
const MIGRATION_017_DESCRIPTION: &str =
    "Add callee identifier byte range to edges for the SCIP occurrence join (#61 prerequisite)";
const MIGRATION_018_ID: &str = "018_scip_oracle_tables";
const MIGRATION_018_CHECKSUM: &str = "sha256:rag-rat-scip-oracle-tables-v18";
const MIGRATION_018_DESCRIPTION: &str =
    "Add oracle_runs + edge_oracle side tables for SCIP compiler-grade edge resolution (#68)";
const MIGRATION_019_ID: &str = "019_scip_moniker_anchors";
const MIGRATION_019_CHECKSUM: &str = "sha256:rag-rat-scip-moniker-anchors-v19";
const MIGRATION_019_DESCRIPTION: &str = "Add logical_symbol_monikers + moniker provenance and \
                                         relocation reason on repo memory bindings (#70)";
const MIGRATION_020_ID: &str = "020_edge_string_interning";
const MIGRATION_020_CHECKSUM: &str = "sha256:rag-rat-edge-string-interning-v20";
const MIGRATION_020_DESCRIPTION: &str = "Normalize repeated edge strings into the name_strings \
                                         dictionary behind the edges compatibility view (#79)";
const MIGRATION_021_ID: &str = "021_symbol_scope_path";
const MIGRATION_021_CHECKSUM: &str = "sha256:rag-rat-symbol-scope-path-v21";
const MIGRATION_021_DESCRIPTION: &str =
    "Add symbols.scope_path (semantic enclosing-scope path) for scope-aware edge resolution (#61)";
const MIGRATION_022_ID: &str = "022_per_package_import_scope";
const MIGRATION_022_CHECKSUM: &str = "sha256:rag-rat-per-package-import-scope-v22";
const MIGRATION_022_DESCRIPTION: &str = "Add packages table + dedicated edge import-scope columns \
                                         for per-package, module-aware import resolution (#61)";
const MIGRATION_023_ID: &str = "023_dispatch_edge_facts_view_exclusion";
const MIGRATION_023_CHECKSUM: &str = "sha256:rag-rat-dispatch-edge-facts-view-exclusion-v23";
const MIGRATION_023_DESCRIPTION: &str = "Recreate the edges compatibility view to exclude \
                                         internal dispatch FACT rows from query-layer reads (#200)";
const MIGRATION_024_ID: &str = "024_files_has_test_code";
const MIGRATION_024_CHECKSUM: &str = "sha256:rag-rat-files-has-test-code-v24";
const MIGRATION_024_DESCRIPTION: &str = "Add files.has_test_code flag (precomputed test-marker \
                                         detection) so impact_surface avoids a chunks.text scan \
                                         (#77)";
const MIGRATION_025_ID: &str = "025_chunk_text_compression_tables";
const MIGRATION_025_CHECKSUM: &str = "sha256:rag-rat-chunk-text-compression-tables-v25";
const MIGRATION_025_DESCRIPTION: &str = "Add chunk_text (zstd blob) + chunk_text_dict (shared \
                                         dictionary) tables for compressed chunk text (#77)";
const MIGRATION_026_ID: &str = "026_contentless_chunk_fts";
const MIGRATION_026_CHECKSUM: &str = "sha256:rag-rat-contentless-chunk-fts-v26";
const MIGRATION_026_DESCRIPTION: &str = "Recreate chunk_fts as a contentless FTS5 index and \
                                         repopulate it, so chunks.text can be dropped (#77 Phase \
                                         2)";
const MIGRATION_027_ID: &str = "027_drop_chunks_text";
const MIGRATION_027_CHECKSUM: &str = "sha256:rag-rat-drop-chunks-text-v27";
const MIGRATION_027_DESCRIPTION: &str = "Build the compressed chunk_text store from chunks.text, \
                                         then drop the chunks.text column (#77 Phase 2)";
const MIGRATION_028_ID: &str = "028_intern_symbol_qualified_names";
const MIGRATION_028_CHECKSUM: &str = "sha256:rag-rat-intern-symbol-qualified-names-v28";
const MIGRATION_028_DESCRIPTION: &str = "Intern symbols/logical_symbols qualified_name into the \
                                         shared name_strings pool, then drop the columns (#224)";
const MIGRATION_029_ID: &str = "029_clone_fingerprint_tables";
const MIGRATION_029_CHECKSUM: &str = "sha256:rag-rat-clone-fingerprint-tables-v29";
const MIGRATION_029_DESCRIPTION: &str = "Add symbol_fingerprints + symbol_token_postings + \
                                         clone_token_df + clone_refinements for clone detection \
                                         (#215)";
const MIGRATION_030_ID: &str = "030_clone_refinements_lcs_sampled";
const MIGRATION_030_CHECKSUM: &str = "sha256:rag-rat-clone-refinements-lcs-sampled-v30";
const MIGRATION_030_DESCRIPTION: &str =
    "Add clone_refinements.lcs_sampled (additive; heals indexes already at V029)";
const MIGRATION_031_ID: &str = "031_edge_oracle_content_anchor";
const MIGRATION_031_CHECKSUM: &str = "sha256:rag-rat-edge-oracle-content-anchor-v31";
const MIGRATION_031_DESCRIPTION: &str = "Rebuild edge_oracle content-anchored (drop edges_data FK \
                                         + edge_id PK) so verdicts survive reindex (#248)";
const MIGRATION_032_ID: &str = "032_clone_token_bag_blob";
const MIGRATION_032_CHECKSUM: &str = "sha256:rag-rat-clone-token-bag-blob-v32";
const MIGRATION_032_DESCRIPTION: &str = "Add symbol_fingerprints.token_bag BLOB + drop \
                                         symbol_token_postings (BLOB-pack the clone token bag) \
                                         (#231)";
const MIGRATION_033_ID: &str = "033_dream_findings";
const MIGRATION_033_CHECKSUM: &str = "sha256:rag-rat-dream-findings-v33";
const MIGRATION_033_DESCRIPTION: &str = "Add dream_findings (dream-mode worklist: findings ABOUT \
                                         memories, identity-keyed supersede/decay, never mutate \
                                         memories) (#122)";
const MIGRATION_034_ID: &str = "034_clone_graph_precompute";
const MIGRATION_034_CHECKSUM: &str = "sha256:rag-rat-clone-graph-precompute-v34";
const MIGRATION_034_DESCRIPTION: &str =
    "Add clone_graph_generations + clone_edges (content-anchored precomputed clone-edge graph so \
     find_clones reads a persisted graph instead of recomputing candidate pairs every query)";
const MIGRATION_035_ID: &str = "035_symbols_is_test";
const MIGRATION_035_CHECKSUM: &str = "sha256:rag-rat-symbols-is-test-v35";
const MIGRATION_035_DESCRIPTION: &str = "Add symbols.is_test (cross-language test-code marker: \
                                         test-file path, Rust #[test]/#[cfg(test)], Kotlin @Test, \
                                         Python test_*/TestCase) so clone detection can exclude \
                                         tests from the corpus";
const MIGRATION_036_ID: &str = "036_embedding_content_cache";
const MIGRATION_036_CHECKSUM: &str = "sha256:rag-rat-embedding-content-cache-v36";
const MIGRATION_036_DESCRIPTION: &str =
    "Add embedding_cache (content-addressed vectors keyed by input_hash) so embeddings survive \
     reindex / branch-switch and reconcile reuses unchanged content across contexts instead of \
     re-embedding; seeded from current chunk_embeddings (#357)";
const MIGRATION_037_ID: &str = "037_clone_subblock_postings";
const MIGRATION_037_CHECKSUM: &str = "sha256:rag-rat-clone-subblock-postings-v37";
const MIGRATION_037_DESCRIPTION: &str =
    "Add clone_subblock_postings (persisted content-anchored sub-block postings, \
     generation-staged like clone_edges) + clone_graph_generations.postings_written so the \
     write-time clone check does a bounded indexed lookup instead of rebuilding the RAM index and \
     scales past the 40k guard (#296)";
const MIGRATION_038_ID: &str = "038_repos_registry";
const MIGRATION_038_CHECKSUM: &str = "sha256:rag-rat-repos-registry-v38";
const MIGRATION_038_DESCRIPTION: &str =
    "Add repos registry + repo_roots + repo_meta (per-machine repo identity registry and per-repo \
     key/value store) with the __unassigned__ adoption placeholder — the substrate for the global \
     consolidated database and repo_id scoping (memory-sync phase A)";
const MIGRATION_039_ID: &str = "039_per_repo_meta";
const MIGRATION_039_CHECKSUM: &str = "sha256:rag-rat-per-repo-meta-v39";
const MIGRATION_039_DESCRIPTION: &str = "Relocate the per-repo singleton meta keys from the \
                                         global index_meta / reconcile_meta into repo_meta under \
                                         the __unassigned__ placeholder (memory-sync phase A2)";
const MIGRATION_040_ID: &str = "040_repo_id_core_scoping";
const MIGRATION_040_CHECKSUM: &str = "sha256:rag-rat-repo-id-core-scoping-v40";
const MIGRATION_040_DESCRIPTION: &str =
    "Add repo_id scoping to the core tables (files, packages, logical_symbols, docs, \
     parser_failures, git_commits + git_file_changes) with rebuilt UNIQUE / PK keys and the \
     re-pointed commit_fts external content, plus the two active-embedding-model provenance meta \
     keys moved to repo_meta (memory-sync phase A3)";
const MIGRATION_041_ID: &str = "041_github_repo_id_scoping";
const MIGRATION_041_CHECKSUM: &str = "sha256:rag-rat-github-repo-id-scoping-v41";
const MIGRATION_041_DESCRIPTION: &str =
    "Repo-scope the GitHub papertrail cache: add repo_id to the seven github_* tables (refs, \
     issues, comments, pull_requests, reviews, review_comments, ref_sync) and rebuild github_fts \
     with a repo_id UNINDEXED column, so lexical and papertrail queries in a consolidated \
     database never surface a sibling repo's refs or issues (memory-sync phase A4)";
const MIGRATION_042_ID: &str = "042_repo_id_periphery_scoping";
const MIGRATION_042_CHECKSUM: &str = "sha256:rag-rat-repo-id-periphery-scoping-v42";
const MIGRATION_042_DESCRIPTION: &str =
    "Repo-scope the clone / oracle / reconcile / memory periphery: add repo_id to \
     clone_graph_generations, oracle_runs, reconcile_attempts, repo_memories, \
     repo_memory_bindings (additive) and rebuild clone_token_df, clone_refinements, edge_oracle, \
     logical_symbol_monikers, dream_findings with repo_id in their PK / UNIQUE plus \
     repo_memory_fts with a repo_id UNINDEXED column, so clone stats, oracle runs, and memory \
     search in a consolidated database never pool or surface a sibling repo's rows (memory-sync \
     phase A5)";
const MIGRATION_043_ID: &str = "043_files_generation";
const MIGRATION_043_CHECKSUM: &str = "sha256:rag-rat-files-generation-v43";
const MIGRATION_043_DESCRIPTION: &str =
    "Add files.generation and widen the UNIQUE key to (repo_id, path, commit_sha, worktree_id, \
     generation), so a full rebuild can stage a fresh generation of every file row alongside the \
     live one and flip readers over atomically instead of clearing-then-reinserting inside one \
     long write-locked transaction (memory-sync phase A6)";
const MIGRATION_044_ID: &str = "044_github_natural_key_widening";
const MIGRATION_044_CHECKSUM: &str = "sha256:rag-rat-github-natural-key-widening-v44";
const MIGRATION_044_DESCRIPTION: &str =
    "Fold repo_id into the (owner, repo, number)-style GitHub natural keys — widen github_issues \
     / github_pull_requests UNIQUE and github_ref_sync PRIMARY KEY to (repo_id, owner, repo, \
     number) and re-create idx_github_refs_unique with a leading repo_id — so two repos in a \
     consolidated database can each cache the same external issue/PR/ref without one repo's sync \
     overwriting the other's row (memory-sync phase A7)";
const MIGRATION_045_ID: &str = "045_github_child_key_widening";
const MIGRATION_045_CHECKSUM: &str = "sha256:rag-rat-github-child-key-widening-v45";
const MIGRATION_045_DESCRIPTION: &str =
    "Fold repo_id into the id-keyed GitHub child caches — rebuild github_comments / \
     github_reviews / github_review_comments with (repo_id, id) uniqueness, backfilling one copy \
     per owning-parent repo — so two repos sharing an external issue/PR each keep that item's \
     comments and reviews in their scoped papertrail instead of last-syncer-owns restamping \
     (memory-sync phase A7)";
const MIGRATION_046_ID: &str = "046_memory_verification_reality_summaries";
const MIGRATION_046_CHECKSUM: &str = "sha256:rag-rat-memory-verification-reality-summaries-v46";
const MIGRATION_046_DESCRIPTION: &str =
    "Add the dream verification sibling tables memory_reality (one derived verdict/check row per \
     memory, keyed (repo_id, memory_id)) and memory_summaries (one per (repo_id, memory_id, \
     content_hash) so a body edit self-invalidates), both STRICT and repo_id-scoped. They hold \
     derived, regenerable data so dream verifies memories without ever mutating a repo_memories \
     row (dream v2 pass 0)";
const MIGRATION_047_ID: &str = "047_memory_model_failures";
const MIGRATION_047_CHECKSUM: &str = "sha256:rag-rat-memory-model-failures-v47";
const MIGRATION_047_DESCRIPTION: &str =
    "Add memory_model_failures, a repo_id-scoped dream sibling table that records deterministic \
     verdict/compaction model failures with stable enum tokens and input/model freshness stamps, \
     so rejected current attempts do not rerun every dream pass";
const MIGRATION_048_ID: &str = "048_memory_payload_json";
const MIGRATION_048_CHECKSUM: &str = "sha256:rag-rat-memory-payload-json-v48";
const MIGRATION_048_DESCRIPTION: &str =
    "Add repo_memories.payload_json, a nullable opaque canonical-JSON payload for polymorphic \
     memory nodes (the Task / Concept kinds), folded into the content_hash so a payload edit \
     self-invalidates the derived dream summary/verdict rows exactly as a title/body edit does";
const MIGRATION_049_ID: &str = "049_repo_node_edges";
const MIGRATION_049_CHECKSUM: &str = "sha256:rag-rat-repo-node-edges-v49";
const MIGRATION_049_DESCRIPTION: &str =
    "Add repo_node_edges, the typed content-addressed cross-repo edge set (#464): relation-typed \
     edges from a memory node to another node or a code/github target, with explicit owner + \
     target repo ids and a stable edge_key, no FK to volatile graph rows (only the durable source \
     memory)";
const MIGRATION_050_ID: &str = "050_clone_delta_maintenance";
const MIGRATION_050_CHECKSUM: &str = "sha256:rag-rat-clone-delta-maintenance-v50";
const MIGRATION_050_DESCRIPTION: &str =
    "Add the clone_subblock_postings (build_generation, path) index and the \
     clone_graph_generations.delta_files_applied counter, so the incremental clone-graph delta \
     pass can delete a changed file's postings without a table scan and track df drift toward the \
     next full rebuild";
const MIGRATION_051_ID: &str = "051_clone_df_epoch";
const MIGRATION_051_CHECKSUM: &str = "sha256:rag-rat-clone-df-epoch-v51";
const MIGRATION_051_DESCRIPTION: &str =
    "Add clone_df_epoch, the per-generation snapshot of clone_token_df taken at each fresh \
     clone-graph build (#479), so the persisted postings and the delta pass read their own \
     build's frozen token order while the live candidate paths read a clone_token_df that moves \
     again on incremental passes; backfilled from the current (freeze-pinned) df for existing \
     generations";
const MIGRATION_052_ID: &str = "052_oplog_storage";
const MIGRATION_052_CHECKSUM: &str = "sha256:rag-rat-oplog-storage-v52";
const MIGRATION_052_DESCRIPTION: &str =
    "Add the memory op-log storage tables (#503, phase B C4): oplog_entries — the layer-1 opaque \
     signed entry log, content-addressed on entry_hash, no FK; and the layer-2 shadow projection \
     (oplog_projected_nodes / oplog_projected_edges) plus oplog_meta, wholly rebuilt by the \
     full-replay fold. Fresh tables (no backfill); nothing is wired to the live write path yet";
const MIGRATION_053_ID: &str = "053_oplog_stream_scoping";
const MIGRATION_053_CHECKSUM: &str = "sha256:rag-rat-oplog-stream-scoping-v53";
const MIGRATION_053_DESCRIPTION: &str =
    "Scope the op-log by immutable stream identity (#509): rebuild the still-unwired (and \
     therefore empty) V052 op-log tables with a stream_id dimension — one signed chain per \
     (stream_id, device), UNIQUE(stream_id, device_fingerprint, lamport), projection keyed per \
     stream — and add oplog_fork_evidence, the quarantine that durably preserves BOTH heads of a \
     detected equivocation. Nothing is wired to the live write path yet";
const MIGRATION_054_ID: &str = "054_oplog_device_identity";
const MIGRATION_054_CHECKSUM: &str = "sha256:rag-rat-oplog-device-identity-v54";
const MIGRATION_054_DESCRIPTION: &str =
    "Add oplog_device_identity (#513, phase B): the ONE persisted ed25519 keypair per store that \
     the op-log write path signs every entry with — a single-row (id = 0) STRICT table holding \
     the 32-byte seed, its derived public_key, and the sha256(public_key) fingerprint. \
     Store-global, not repo-scoped (a device is a machine identity). Purely additive; nothing is \
     wired to the live write path yet";
const MIGRATION_055_ID: &str = "055_binding_downgrade_marker";
const MIGRATION_055_CHECKSUM: &str = "sha256:rag-rat-binding-downgrade-marker-v55";
const MIGRATION_055_DESCRIPTION: &str =
    "Add repo_memory_bindings.downgrade_pending_at_ms (#492), the anchor-status downgrade \
     hysteresis marker: a validate pass that observes a non-gone binding as gone arms the marker \
     instead of stamping, and only a SECOND consecutive gone observation persists the downgrade — \
     so a single torn observation (a validate racing a rebuild window, or a sweep from a narrower \
     checkout) cannot flip a healthy anchor to gone and hand doctor destructive advice";
const MIGRATION_056_ID: &str = "056_git_change_couplings";
const MIGRATION_056_CHECKSUM: &str = "sha256:rag-rat-git-change-couplings-v56";
const MIGRATION_056_DESCRIPTION: &str =
    "Add git_change_couplings (#566), the windowed file-pair change-coupling table derived from \
     git_file_changes: one STRICT symmetric row per unordered pair (path_a < path_b) holding raw \
     co-change + endpoint counts over a bounded recency window of eligible commits, keyed \
     (repo_id, path_a, path_b) with a secondary (repo_id, path_b) index. A DerivedIndex table \
     (repo_id-scoped, no FK to the volatile history rows): wholesale-recomputed lazily on the \
     impact_surface read path against a repo_meta 'git_coupling_stamp', never patched \
     incrementally. Fresh + empty on create; the first git-inclusive impact read fills it";
const MIGRATION_057_ID: &str = "057_external_symbols";
const MIGRATION_057_CHECKSUM: &str = "sha256:rag-rat-external-symbols-v57";
const MIGRATION_057_DESCRIPTION: &str =
    "Add external_symbols (#114), the per-moniker dependency contract oracle run parses out of \
     the .scip index.external_symbols (kind, display_name, signature_documentation text, \
     documentation, a derived deprecated flag) — data from_index previously discarded. \
     Oracle-persisted, content/moniker-keyed with NO reindex-cascading FK, checkout-scoped \
     (repo_id, tool, commit_sha, worktree_id) from birth; moniker is the RAW SCIP symbol string \
     so it exact-joins edge_oracle.scip_symbol. Backs the check_library_usage tool that surfaces \
     the current signature/docs at external call sites and flags deprecated usage";
const MIGRATION_058_ID: &str = "058_oplog_device_x25519";
const MIGRATION_058_CHECKSUM: &str = "sha256:rag-rat-oplog-device-x25519-v58";
const MIGRATION_058_DESCRIPTION: &str =
    "Add x25519_secret + x25519_public (nullable BLOB) to oplog_device_identity (sync phase C, \
     §5): the device's X25519 ENCRYPTION keypair beside its ed25519 signing key. Additive on the \
     STRICT table; an existing ed25519-only row is backfilled at the next local_device open via a \
     CAS UPDATE that mirrors the ed25519 mint-if-absent race, so concurrent opens converge on one \
     encryption identity. C1 only mints/persists/validates the key; ECDH + HKDF is C4";
const MIGRATION_059_ID: &str = "059_account_candidate_dag";
const MIGRATION_059_CHECKSUM: &str = "sha256:rag-rat-account-candidate-dag-v59-signed-envelope-key";
const MIGRATION_059_DESCRIPTION: &str =
    "The account-log CANDIDATE DAG (sync phase C, §16.1): account_entries (all branches, \
     grow-only, no seq-uniqueness — equivocation heads are first-class; the derived `accepted` \
     flag + the account_accepted_slot partial unique index pin accepted-set uniqueness per slot, \
     I10a), account_entry_status (the projected §16.3 taxonomy), and account_pre_verify (entries \
     whose signing device isn't yet resolvable, retried on a later DeviceAdd/AccountGenesis \
     arrival). All CREATE ... IF NOT EXISTS + STRICT tables";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchemaState {
    Missing,
    Compatible,
    Older,
    Newer,
    Dirty,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppliedMigration {
    pub id: String,
    pub applied_at_ms: i64,
    pub checksum: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaStatus {
    pub state: SchemaState,
    pub current_version: u32,
    pub latest_version: u32,
    pub migrations: Vec<AppliedMigration>,
    pub message: String,
}

/// Provision the baseline (001) idempotently: the schema_version ledger, then `apply_baseline`
/// (all `CREATE … IF NOT EXISTS`), then record 001. Shared by [`apply`] (fresh DB) and
/// [`migrate_forward`] (existing DB behind by N). LOAD-BEARING for forward-only: a pre-interning
/// (≤v19) DB lacks shared tables like `name_strings`/`edges_data` that later migrations INSERT
/// into, so the baseline must run BEFORE any forward step replays — exactly what the old full
/// `apply` did before the ladder.
///
/// The `__dirty__` marker wraps ONLY a provision that OWES the baseline record — 001 not yet
/// recorded (first-ever provision), or recorded with a stale checksum (a mismatch reads as Dirty,
/// and `index --full` recovery must refresh the row like it refreshes every step's) — where it
/// makes a crash mid-baseline detectable. A REPLAY over a current 001 (every open-time forward
/// migrate) must not touch the marker (#498): the marker choreography runs in autocommit, so the
/// stamp is durable and globally visible the moment it lands, and the GLOBAL schema lock
/// deliberately does not serialize against ordinary per-repo writers — a `SQLITE_BUSY` between
/// the stamp and the clear stranded a marker on a healthy DB and every subsequent open refused
/// until a manual `index --full` (observed live on V050→V051). The replay needs no marker
/// because it is CONVERGENT and DATA-PRESERVING, not because every statement is a no-op: creates
/// are `IF NOT EXISTS`, the destructive legacy conversions are CONDITIONAL — they fire only when
/// the legacy shape is present (`drop_legacy_ai_prototype_tables`, the `edge_strings` rename
/// below, `ensure_edges_data`'s self-wrapped copy) — and a torn replay leaves the owed step rows
/// unrecorded, so the DB still reads `Older` and the next open re-runs the baseline to
/// completion before anything serves.
fn provision_baseline(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_version(
            id TEXT PRIMARY KEY,
            applied_at_ms INTEGER NOT NULL,
            checksum TEXT NOT NULL,
            description TEXT NOT NULL
        );
        ",
    )?;
    let baseline_owed = conn
        .query_row("SELECT checksum FROM schema_version WHERE id = ?1", [MIGRATION_001_ID], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .as_deref()
        != Some(MIGRATION_001_CHECKSUM);
    if baseline_owed {
        conn.execute(
            "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
             VALUES (?1, ?2, ?3, ?4)",
            params![DIRTY_MIGRATION_ID, now_ms(), "", "partial migration in progress"],
        )?;
    }
    // The string pool was `edge_strings` before the name-pool merge (#224, the V028 bump); it now
    // also holds symbol qualified-names, so the current schema names it `name_strings`. On a
    // pre-merge DB the POPULATED table is still `edge_strings` — rename it into place HERE, before
    // `apply_baseline`'s `CREATE TABLE IF NOT EXISTS name_strings` (and the `ensure_edges_view` it
    // calls), so we adopt the real table instead of creating an empty one beside it (which would
    // orphan every edge's interned names). Runs before any migration replay, so V023's
    // `ensure_edges_view` then sees `name_strings`. Fresh / already-merged DBs have no
    // `edge_strings` → no-op. A pre-merge DB is `Older` (< V028), so it reaches this migrate path;
    // a `Compatible` open skips migration and is already `name_strings`.
    let pre_merge_pool: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'edge_strings'",
        [],
        |row| row.get(0),
    )?;
    let merged_pool: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'name_strings'",
        [],
        |row| row.get(0),
    )?;
    if pre_merge_pool > 0 && merged_pool == 0 {
        conn.execute_batch("ALTER TABLE edge_strings RENAME TO name_strings;")?;
    }
    let result = apply_baseline(conn);
    if let Err(err) = result {
        if baseline_owed {
            let _ =
                conn.execute("UPDATE schema_version SET description = ?2 WHERE id = ?1", params![
                    DIRTY_MIGRATION_ID,
                    format!("partial migration failed: {err}")
                ]);
        }
        return Err(err);
    }
    if baseline_owed {
        conn.execute("DELETE FROM schema_version WHERE id = ?1", [DIRTY_MIGRATION_ID])?;
        record_migration(
            conn,
            MIGRATION_001_ID,
            MIGRATION_001_CHECKSUM,
            MIGRATION_001_DESCRIPTION,
        )?;
    }
    Ok(())
}

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
    provision_baseline(conn)?;
    // Every additive migration in order. A fresh DB runs them all; data backfills on empty tables
    // are no-ops. (An EXISTING DB takes the forward-only path via `migrate_forward`, not this.)
    for step in ADDITIVE_MIGRATIONS {
        (step.apply)(conn)?;
        record_migration(conn, step.id, step.checksum, step.description)?;
    }
    // `index --full` is the sanctioned recovery the Dirty refusal names, and apply is its schema
    // step: every migration just re-ran to completion, so a `__dirty__` marker that still
    // survives (stranded by an older binary's failed mid-replay migrate) is provably stale —
    // clear it, or the remedy would leave the DB refusing every open forever (#498). A crash
    // before this point keeps the marker, so a genuinely torn apply still reads as Dirty.
    conn.execute("DELETE FROM schema_version WHERE id = ?1", [DIRTY_MIGRATION_ID])?;
    // #585: record which binary brought the schema current, so a stranded fleet is diagnosable.
    // Best-effort: the schema is already applied, and provenance is diagnostic — a stamp failure
    // must not fail the migration (the reader tolerates an absent record).
    let _ = record_migration_provenance(conn);
    Ok(())
}

/// One additive migration layered on the baseline (001): its identity + the function that applies
/// it. Single source of truth for both `apply` (fresh DB, runs all) and `migrate_forward` (existing
/// DB, runs only the unapplied ones).
struct Migration {
    id: &'static str,
    checksum: &'static str,
    description: &'static str,
    apply: fn(&Connection) -> rusqlite::Result<()>,
}

const ADDITIVE_MIGRATIONS: &[Migration] = &[
    Migration {
        id: MIGRATION_002_ID,
        checksum: MIGRATION_002_CHECKSUM,
        description: MIGRATION_002_DESCRIPTION,
        apply: apply_embedding_vector_metadata,
    },
    Migration {
        id: MIGRATION_003_ID,
        checksum: MIGRATION_003_CHECKSUM,
        description: MIGRATION_003_DESCRIPTION,
        apply: apply_derived_artifact_reconcile_metadata,
    },
    Migration {
        id: MIGRATION_004_ID,
        checksum: MIGRATION_004_CHECKSUM,
        description: MIGRATION_004_DESCRIPTION,
        apply: apply_edge_source_target_spans,
    },
    Migration {
        id: MIGRATION_005_ID,
        checksum: MIGRATION_005_CHECKSUM,
        description: MIGRATION_005_DESCRIPTION,
        apply: apply_edge_evidence_and_resolution,
    },
    Migration {
        id: MIGRATION_006_ID,
        checksum: MIGRATION_006_CHECKSUM,
        description: MIGRATION_006_DESCRIPTION,
        apply: apply_embedding_policy_and_input_hash,
    },
    Migration {
        id: MIGRATION_007_ID,
        checksum: MIGRATION_007_CHECKSUM,
        description: MIGRATION_007_DESCRIPTION,
        apply: apply_logical_symbol_groups,
    },
    Migration {
        id: MIGRATION_008_ID,
        checksum: MIGRATION_008_CHECKSUM,
        description: MIGRATION_008_DESCRIPTION,
        apply: apply_commit_addressable_worktrees,
    },
    Migration {
        id: MIGRATION_009_ID,
        checksum: MIGRATION_009_CHECKSUM,
        description: MIGRATION_009_DESCRIPTION,
        apply: apply_github_ref_sync,
    },
    Migration {
        id: MIGRATION_010_ID,
        checksum: MIGRATION_010_CHECKSUM,
        description: MIGRATION_010_DESCRIPTION,
        apply: apply_symbol_facts,
    },
    Migration {
        id: MIGRATION_011_ID,
        checksum: MIGRATION_011_CHECKSUM,
        description: MIGRATION_011_DESCRIPTION,
        apply: apply_repo_memories,
    },
    Migration {
        id: MIGRATION_012_ID,
        checksum: MIGRATION_012_CHECKSUM,
        description: MIGRATION_012_DESCRIPTION,
        apply: apply_repo_memory_call_paths,
    },
    Migration {
        id: MIGRATION_013_ID,
        checksum: MIGRATION_013_CHECKSUM,
        description: MIGRATION_013_DESCRIPTION,
        apply: apply_graph_file_lookup_indexes,
    },
    Migration {
        id: MIGRATION_014_ID,
        checksum: MIGRATION_014_CHECKSUM,
        description: MIGRATION_014_DESCRIPTION,
        apply: apply_memory_binding_signals,
    },
    Migration {
        id: MIGRATION_015_ID,
        checksum: MIGRATION_015_CHECKSUM,
        description: MIGRATION_015_DESCRIPTION,
        apply: apply_repo_memory_call_path_edges,
    },
    Migration {
        id: MIGRATION_016_ID,
        checksum: MIGRATION_016_CHECKSUM,
        description: MIGRATION_016_DESCRIPTION,
        apply: apply_symbol_line_spans,
    },
    Migration {
        id: MIGRATION_017_ID,
        checksum: MIGRATION_017_CHECKSUM,
        description: MIGRATION_017_DESCRIPTION,
        apply: apply_edge_callee_byte_range,
    },
    Migration {
        id: MIGRATION_018_ID,
        checksum: MIGRATION_018_CHECKSUM,
        description: MIGRATION_018_DESCRIPTION,
        apply: apply_oracle_tables,
    },
    Migration {
        id: MIGRATION_019_ID,
        checksum: MIGRATION_019_CHECKSUM,
        description: MIGRATION_019_DESCRIPTION,
        apply: apply_scip_moniker_anchors,
    },
    Migration {
        id: MIGRATION_020_ID,
        checksum: MIGRATION_020_CHECKSUM,
        description: MIGRATION_020_DESCRIPTION,
        apply: apply_edge_string_interning,
    },
    Migration {
        id: MIGRATION_021_ID,
        checksum: MIGRATION_021_CHECKSUM,
        description: MIGRATION_021_DESCRIPTION,
        apply: apply_symbol_scope_path,
    },
    Migration {
        id: MIGRATION_022_ID,
        checksum: MIGRATION_022_CHECKSUM,
        description: MIGRATION_022_DESCRIPTION,
        apply: apply_per_package_import_scope,
    },
    Migration {
        id: MIGRATION_023_ID,
        checksum: MIGRATION_023_CHECKSUM,
        description: MIGRATION_023_DESCRIPTION,
        apply: apply_dispatch_edge_facts_view_exclusion,
    },
    Migration {
        id: MIGRATION_024_ID,
        checksum: MIGRATION_024_CHECKSUM,
        description: MIGRATION_024_DESCRIPTION,
        apply: apply_files_has_test_code,
    },
    Migration {
        id: MIGRATION_025_ID,
        checksum: MIGRATION_025_CHECKSUM,
        description: MIGRATION_025_DESCRIPTION,
        apply: apply_chunk_text_compression_tables,
    },
    Migration {
        id: MIGRATION_026_ID,
        checksum: MIGRATION_026_CHECKSUM,
        description: MIGRATION_026_DESCRIPTION,
        apply: apply_contentless_chunk_fts,
    },
    Migration {
        id: MIGRATION_027_ID,
        checksum: MIGRATION_027_CHECKSUM,
        description: MIGRATION_027_DESCRIPTION,
        apply: apply_drop_chunks_text,
    },
    Migration {
        id: MIGRATION_028_ID,
        checksum: MIGRATION_028_CHECKSUM,
        description: MIGRATION_028_DESCRIPTION,
        apply: apply_intern_symbol_qualified_names,
    },
    Migration {
        id: MIGRATION_029_ID,
        checksum: MIGRATION_029_CHECKSUM,
        description: MIGRATION_029_DESCRIPTION,
        apply: apply_clone_fingerprint_tables,
    },
    Migration {
        id: MIGRATION_030_ID,
        checksum: MIGRATION_030_CHECKSUM,
        description: MIGRATION_030_DESCRIPTION,
        apply: apply_clone_refinements_lcs_sampled,
    },
    Migration {
        id: MIGRATION_031_ID,
        checksum: MIGRATION_031_CHECKSUM,
        description: MIGRATION_031_DESCRIPTION,
        apply: apply_edge_oracle_content_anchor,
    },
    Migration {
        id: MIGRATION_032_ID,
        checksum: MIGRATION_032_CHECKSUM,
        description: MIGRATION_032_DESCRIPTION,
        apply: apply_token_bag_blob,
    },
    Migration {
        id: MIGRATION_033_ID,
        checksum: MIGRATION_033_CHECKSUM,
        description: MIGRATION_033_DESCRIPTION,
        apply: apply_dream_findings,
    },
    Migration {
        id: MIGRATION_034_ID,
        checksum: MIGRATION_034_CHECKSUM,
        description: MIGRATION_034_DESCRIPTION,
        apply: apply_clone_graph_tables,
    },
    Migration {
        id: MIGRATION_035_ID,
        checksum: MIGRATION_035_CHECKSUM,
        description: MIGRATION_035_DESCRIPTION,
        apply: apply_symbols_is_test,
    },
    Migration {
        id: MIGRATION_036_ID,
        checksum: MIGRATION_036_CHECKSUM,
        description: MIGRATION_036_DESCRIPTION,
        apply: apply_embedding_content_cache,
    },
    Migration {
        id: MIGRATION_037_ID,
        checksum: MIGRATION_037_CHECKSUM,
        description: MIGRATION_037_DESCRIPTION,
        apply: apply_clone_subblock_postings_tables,
    },
    Migration {
        id: MIGRATION_038_ID,
        checksum: MIGRATION_038_CHECKSUM,
        description: MIGRATION_038_DESCRIPTION,
        apply: apply_repos_registry,
    },
    Migration {
        id: MIGRATION_039_ID,
        checksum: MIGRATION_039_CHECKSUM,
        description: MIGRATION_039_DESCRIPTION,
        apply: apply_move_per_repo_meta,
    },
    Migration {
        id: MIGRATION_040_ID,
        checksum: MIGRATION_040_CHECKSUM,
        description: MIGRATION_040_DESCRIPTION,
        apply: apply_repo_id_core_scoping,
    },
    Migration {
        id: MIGRATION_041_ID,
        checksum: MIGRATION_041_CHECKSUM,
        description: MIGRATION_041_DESCRIPTION,
        apply: apply_github_repo_id_scoping,
    },
    Migration {
        id: MIGRATION_042_ID,
        checksum: MIGRATION_042_CHECKSUM,
        description: MIGRATION_042_DESCRIPTION,
        apply: apply_repo_id_periphery_scoping,
    },
    Migration {
        id: MIGRATION_043_ID,
        checksum: MIGRATION_043_CHECKSUM,
        description: MIGRATION_043_DESCRIPTION,
        apply: apply_files_generation,
    },
    Migration {
        id: MIGRATION_044_ID,
        checksum: MIGRATION_044_CHECKSUM,
        description: MIGRATION_044_DESCRIPTION,
        apply: apply_github_natural_key_widening,
    },
    Migration {
        id: MIGRATION_045_ID,
        checksum: MIGRATION_045_CHECKSUM,
        description: MIGRATION_045_DESCRIPTION,
        apply: apply_github_child_key_widening,
    },
    Migration {
        id: MIGRATION_046_ID,
        checksum: MIGRATION_046_CHECKSUM,
        description: MIGRATION_046_DESCRIPTION,
        apply: apply_memory_verification_tables,
    },
    Migration {
        id: MIGRATION_047_ID,
        checksum: MIGRATION_047_CHECKSUM,
        description: MIGRATION_047_DESCRIPTION,
        apply: apply_memory_model_failures_table,
    },
    Migration {
        id: MIGRATION_048_ID,
        checksum: MIGRATION_048_CHECKSUM,
        description: MIGRATION_048_DESCRIPTION,
        apply: apply_memory_payload_json,
    },
    Migration {
        id: MIGRATION_049_ID,
        checksum: MIGRATION_049_CHECKSUM,
        description: MIGRATION_049_DESCRIPTION,
        apply: apply_repo_node_edges,
    },
    Migration {
        id: MIGRATION_050_ID,
        checksum: MIGRATION_050_CHECKSUM,
        description: MIGRATION_050_DESCRIPTION,
        apply: apply_clone_delta_maintenance,
    },
    Migration {
        id: MIGRATION_051_ID,
        checksum: MIGRATION_051_CHECKSUM,
        description: MIGRATION_051_DESCRIPTION,
        apply: apply_clone_df_epoch,
    },
    Migration {
        id: MIGRATION_052_ID,
        checksum: MIGRATION_052_CHECKSUM,
        description: MIGRATION_052_DESCRIPTION,
        apply: apply_oplog_storage,
    },
    Migration {
        id: MIGRATION_053_ID,
        checksum: MIGRATION_053_CHECKSUM,
        description: MIGRATION_053_DESCRIPTION,
        apply: apply_oplog_stream_scoping,
    },
    Migration {
        id: MIGRATION_054_ID,
        checksum: MIGRATION_054_CHECKSUM,
        description: MIGRATION_054_DESCRIPTION,
        apply: apply_oplog_device_identity,
    },
    Migration {
        id: MIGRATION_055_ID,
        checksum: MIGRATION_055_CHECKSUM,
        description: MIGRATION_055_DESCRIPTION,
        apply: apply_binding_downgrade_marker,
    },
    Migration {
        id: MIGRATION_056_ID,
        checksum: MIGRATION_056_CHECKSUM,
        description: MIGRATION_056_DESCRIPTION,
        apply: apply_git_change_couplings,
    },
    Migration {
        id: MIGRATION_057_ID,
        checksum: MIGRATION_057_CHECKSUM,
        description: MIGRATION_057_DESCRIPTION,
        apply: apply_external_symbols,
    },
    Migration {
        id: MIGRATION_058_ID,
        checksum: MIGRATION_058_CHECKSUM,
        description: MIGRATION_058_DESCRIPTION,
        apply: apply_oplog_device_x25519,
    },
    Migration {
        id: MIGRATION_059_ID,
        checksum: MIGRATION_059_CHECKSUM,
        description: MIGRATION_059_DESCRIPTION,
        apply: apply_account_candidate_dag,
    },
];

/// Apply ONLY the additive migrations not already recorded, in order — the forward-only path for an
/// existing index that lags this binary. It first provisions the baseline idempotently (so a
/// ≤v19 DB that predates a shared table like `name_strings`/`edges_data` has it before a later
/// migration INSERTs into it), then replays just the unapplied steps. Unlike [`apply`], it never
/// re-runs an already-applied migration, so a data backfill like 005's resolution rewrite (an
/// unconditional UPDATE) cannot clobber current values on a routine open. The caller guarantees a
/// versioned ledger exists; a ledger-less legacy DB is refused upstream, not force-migrated.
///
/// LOSER DISCIPLINE (#498): the ledger is read FIRST, and a migrate that finds nothing owed
/// returns without writing anything at all — no dirty stamp, no 001 re-record, no baseline
/// replay. The loser of a migration race normally never reaches here (the callers re-check state
/// under the GLOBAL schema lock), but a direct call must be equally write-free: every avoided
/// autocommit write is one fewer `SQLITE_BUSY` hazard against ordinary per-repo writers, which
/// the schema lock deliberately does not serialize against.
pub fn migrate_forward(conn: &Connection) -> anyhow::Result<()> {
    let applied: std::collections::HashSet<String> =
        applied_migrations(conn)?.into_iter().map(|migration| migration.id).collect();
    if applied.contains(MIGRATION_001_ID)
        && ADDITIVE_MIGRATIONS.iter().all(|step| applied.contains(step.id))
    {
        return Ok(());
    }
    provision_baseline(conn)?;
    for step in ADDITIVE_MIGRATIONS {
        if !applied.contains(step.id) {
            (step.apply)(conn)?;
            record_migration(conn, step.id, step.checksum, step.description)?;
        }
    }
    // #585: this path only runs when a forward migration actually happened (early-returned above
    // otherwise) — stamp who did it (the shared-store stranding path). Best-effort: the migration
    // has committed; a diagnostic stamp failure must not fail it (the reader tolerates absence).
    let _ = record_migration_provenance(conn);
    Ok(())
}

/// Appended to the `Newer` refusal: on Linux the fleet hot-upgrade re-execs armed MCP servers
/// when a new binary lands, so a stale server heals itself once rag-rat is reinstalled; on every
/// other platform running servers keep the old binary until their sessions restart, and the
/// message must say so (#484).
fn hot_upgrade_caveat() -> &'static str {
    if cfg!(target_os = "linux") {
        ""
    } else {
        "; running MCP servers do not hot-upgrade on this platform — restart their sessions after \
         upgrading"
    }
}

pub fn status(conn: &Connection) -> anyhow::Result<SchemaStatus> {
    if !table_exists(conn, "schema_version")? {
        let has_legacy_tables = table_exists(conn, "files")? || table_exists(conn, "chunks")?;
        return Ok(if has_legacy_tables {
            SchemaStatus {
                state: SchemaState::Older,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "legacy index schema has no version ledger; rebuild the derived index \
                          with `rag-rat index --full`"
                    .to_string(),
            }
        } else {
            SchemaStatus {
                state: SchemaState::Missing,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "index schema is not initialized; build the derived index with `rag-rat \
                          index` or `rag-rat index --full`"
                    .to_string(),
            }
        });
    }

    let migrations = applied_migrations(conn)?;
    if migrations.iter().any(|migration| migration.id == DIRTY_MIGRATION_ID) {
        return Ok(SchemaStatus {
            state: SchemaState::Dirty,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "dirty or partial schema migration detected; rebuild the derived index with \
                      `rag-rat index --full`"
                .to_string(),
        });
    }
    if migrations.iter().any(migration_checksum_mismatch) {
        return Ok(SchemaStatus {
            state: SchemaState::Dirty,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "schema migration checksum mismatch; refusing to open, rebuild the derived \
                      index with `rag-rat index --full`"
                .to_string(),
        });
    }
    if migrations.iter().any(|migration| !known_migration(&migration.id)) {
        return Ok(SchemaStatus {
            state: SchemaState::Newer,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            // #484/#585: on a shared global DB one upgraded agent migrates the schema and every
            // process still on an older binary lands here — the refusal must carry the remedy AND
            // name this binary's schema ceiling + WHO migrated the store (from provenance), because
            // it surfaces as the error text of every CLI/MCP open and is how a fleet outage is
            // diagnosed.
            message: format!(
                "index schema was created by a newer rag-rat; refusing to open — this rag-rat \
                 supports up to schema v{LATEST_SCHEMA_VERSION}, so upgrade rag-rat or restart \
                 sessions/servers still running an older binary{}{}",
                hot_upgrade_caveat(),
                migration_provenance_note(conn),
            ),
        });
    }
    let current_version = known_version(&migrations);
    if current_version < LATEST_SCHEMA_VERSION {
        return Ok(SchemaStatus {
            state: SchemaState::Older,
            current_version,
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "index schema is older than this rag-rat; it migrates forward automatically \
                      on open (or rebuild with `rag-rat index --full`)"
                .to_string(),
        });
    }
    Ok(SchemaStatus {
        state: SchemaState::Compatible,
        current_version,
        latest_version: LATEST_SCHEMA_VERSION,
        migrations,
        message: "schema is compatible".to_string(),
    })
}

/// Make the open-able index schema current, migrating FORWARD automatically when it lags this
/// binary. The index is rag-rat's own derived data, so upgrading the binary must never make a
/// developer hand-run `migrate` (or play devops) to get queries/memory writes working again — an
/// `Older` schema is applied in place here. Migrations are additive and idempotent (the same
/// `apply` the `migrate` command runs on `Older`), so bringing a partly-migrated DB to latest is
/// safe and a no-op for already-present tables/columns.
///
/// Only genuinely unrecoverable states still refuse:
/// - `Newer`: created by a future rag-rat — this binary can't apply migrations it doesn't have, and
///   downgrading derived data isn't safe.
/// - `Dirty`: a partial/crashed migration or checksum mismatch — needs a clean `index --full`.
/// - `Missing`: there is no index at this path yet — nothing to migrate; build one first.
pub fn ensure_compatible_or_migrate(conn: &Connection) -> anyhow::Result<()> {
    let current = status(conn)?;
    match current.state {
        SchemaState::Compatible => Ok(()),
        // Forward migration is automatic — but FORWARD-ONLY: apply just the unapplied migrations,
        // never the whole ladder. Re-running an applied data migration (e.g. 005's unconditional
        // `UPDATE edges SET resolution = …`) would clobber current resolver reasons on a routine
        // open. Forward-only needs a real version ledger to know what's applied; a legacy DB with
        // NO schema_version table (current_version 0) can't be safely advanced — we'd have to
        // re-run those data migrations — so it's refused, not force-migrated. Re-verify Compatible
        // so a half-migration can't slip through as "opened fine".
        SchemaState::Older => {
            if !table_exists(conn, "schema_version")? {
                anyhow::bail!(
                    "legacy index schema has no version ledger and can't be safely migrated \
                     forward; rebuild with `rag-rat index --full`"
                );
            }
            migrate_forward(conn)?;
            let after = status(conn)?;
            if after.state == SchemaState::Compatible {
                Ok(())
            } else {
                anyhow::bail!(
                    "auto-migration left the schema {:?}, not compatible; rebuild the derived \
                     index with `rag-rat index --full`",
                    after.state
                )
            }
        },
        SchemaState::Missing => anyhow::bail!(
            "no index at this path yet; build one with `rag-rat index` or `rag-rat index --full`"
        ),
        SchemaState::Newer | SchemaState::Dirty => anyhow::bail!("{}", current.message),
    }
}
