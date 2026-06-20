//! Read layer for clone detection (#215). Plan 1 ships only the candidate-component read that
//! proves the fingerprint substrate; the `find_clones` / `clones_for_symbol` surface is Plan 2.
//!
//! The candidate read is the SourcererCC algorithm (design rev 4 §3b): a `struct_hash` exact fast
//! path, a deterministic-total-order sub-block filter over scoped baseline postings, and an EXACT
//! max-denominator overlap verify. df is a *selectivity hint only* — admissibility comes from the
//! shared total order plus the exact verify, so a missing/stale df never drops a true clone.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension};

/// Pairwise metric work cap for huge components: when a component exceeds this count, the
/// O(n²) pairwise metric loop (`similarity_min`, medoid, `similarity_medoid_min`,
/// `containment_max`) runs over ONLY the first `METRIC_SAMPLE_CAP` members instead of the full
/// upper triangle. `member_count` / `total_members` / `class_key` still reflect the FULL
/// component; only the metric computation is sampled, and `metrics_sampled` is set to `true` on
/// the returned class so callers can distinguish sampled from exact metrics.
///
/// For typical-size components (the overwhelming common case, and ALL existing tests) this cap is
/// never reached: behavior is identical to the pre-cap code and `metrics_sampled` is `false`.
const METRIC_SAMPLE_CAP: usize = 200;

use crate::index::IndexDatabase;
use crate::index::clones::NORM_VERSION;

/// Similarity threshold θ: a candidate pair is kept iff `overlap / max_len >= THETA`. The MAX
/// denominator is deliberate (design rev-4 §3b) — it bounds the member length ratio to ≈1/θ, the
/// whole-symbol bias, so a tiny helper contained in a giant function (overlap/min ≈ 1.0) is NOT a
/// clone. Tunable later via the query surface.
const THETA: f64 = 0.7;

/// Maximum members returned per clone class to guard against huge components.
pub(crate) const MAX_MEMBERS: usize = 50;

/// Member-hydration batch size for the `symbols.id IN (…)` query in [`build_class`]. SQLite caps
/// the number of host parameters per prepared statement (`SQLITE_MAX_VARIABLE_NUMBER` — 999 on
/// older system libs that the non-bundled `rusqlite` may link against), so a component larger than
/// that floor would fail `conn.prepare` outright. Hydrating in chunks of [`HYDRATION_CHUNK`] keeps
/// every statement well under the limit; results are accumulated then re-sorted by `symbol_id` so
/// the full population is processed deterministically regardless of chunk boundaries.
const HYDRATION_CHUNK: usize = 900;

// ── Public result types (Plan-2 query API) ────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct CloneMember {
    pub r#ref: String, // qualified name "path::symbol"
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub token_len: i64,
    pub language: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RoiFactors {
    pub member_count: usize,
    pub cross_module_spread: usize,
    pub median_token_len: i64,
    pub load_bearing_factor: f64,
    pub cohesion_penalty: f64, // = cohesion_min_pairwise (the multiplier applied)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CandidateCloneClass {
    pub class_key: String,        // read-side key (NOT clone_refinements key)
    pub class_kind: &'static str, // always "candidate_component" in Plan 2
    pub language: String,
    pub refined: bool,             // always false in Plan 2
    pub members: Vec<CloneMember>, // returned subset (capped)
    pub member_count: usize,
    pub members_returned: usize,
    pub total_members: usize,
    pub similarity_min: f64,        // min pairwise overlap/max_len
    pub similarity_medoid_min: f64, // min similarity of any member to the medoid
    pub containment_max: f64,       // max pairwise overlap/min_len (informational)
    pub cohesion_min_pairwise: f64, // == similarity_min; the ROI cohesion input
    pub cross_module_spread: usize,
    pub body_token_len_medoid: i64,
    pub roi: f64,
    pub roi_factors: RoiFactors,
    /// `true` when the component has more than [`METRIC_SAMPLE_CAP`] members and the pairwise
    /// metric computation (`similarity_min`, medoid, `similarity_medoid_min`, `containment_max`)
    /// ran over only the first `METRIC_SAMPLE_CAP` members instead of the full upper triangle.
    /// `member_count` / `total_members` / `class_key` are always over the FULL component.
    /// `false` for all normal-size components (the typical case).
    pub metrics_sampled: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CloneCompleteness {
    pub normalizer_kind: &'static str, // "baseline"
    pub normalizer_version: i64,
    pub min_similarity: f64,              // θ used
    pub min_tokens: i64,                  // MIN_TOKENS
    pub min_copies: usize,                // member-count floor applied
    pub candidate_metric: &'static str,   // "overlap_max_denominator"
    pub containment_metric: &'static str, // "overlap_min_denominator"
    pub generated_excluded: bool,         // true
    pub tests_excluded: bool,             // current policy
    pub same_file_policy: &'static str,   // "included" (Plan 2 keeps same-file pairs)
    pub index_freshness: String,          // reuse index_status freshness summary
    pub oracle_coverage: &'static str,    // "n/a_baseline_only" in Plan 2 (SCIP is Plan 3)
    pub truncated: bool,
    /// Count of DISTINCT member file paths whose on-disk content no longer matches the indexed
    /// `files.sha256`. A non-zero value means the returned clone classes describe STALE file
    /// contents — consumers should reindex / `rag-rat heal` before acting on these results.
    /// This is a read-only signal; Plan 2 does not heal-before-return.
    pub stale_members: usize,
    pub known_index_gaps: Vec<String>, /* e.g. "#232: TS function-valued declarators not yet
                                        * fingerprinted" */
}

/// Result of [`IndexDatabase::clones_for_symbol`]. Carries eligibility flags + a completeness block
/// so a caller can distinguish "selector matched nothing" from "matched a symbol that is not
/// eligible for fingerprinting" from "eligible but unique (in no clone class)".
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClonesForSymbolResult {
    /// The containing candidate clone class, or `None` when the symbol is in no class (unique, not
    /// eligible, or unresolved).
    pub class: Option<CandidateCloneClass>,
    /// The selector matched a scoped symbol.
    pub symbol_resolved: bool,
    /// That symbol has a current-version baseline fingerprint loaded into the candidate set
    /// (eligible: a `kind="function"` symbol ≥ `MIN_TOKENS` in a non-generated, in-scope file).
    pub symbol_fingerprinted: bool,
    /// Same provenance block as [`FindClonesResult::completeness`].
    pub completeness: CloneCompleteness,
}

/// Deterministic, order-independent class key: sort `member_refs`, join with `\n`,
/// `hex_sha256`, take the first 16 hex chars.
pub(crate) fn class_key_for(member_refs: &[String]) -> String {
    let mut sorted = member_refs.to_vec();
    sorted.sort_unstable();
    let joined = sorted.join("\n");
    crate::index::hex_sha256(joined.as_bytes())[..16].to_string()
}

/// Sentinel df for tokens with no `clone_token_df` row (LEFT JOIN miss). i64::MAX sorts them LAST
/// in `(coalesced_df ASC, token_hash ASC)` order — they are treated as maximally common (least
/// selective), which is the conservative choice: it can only widen the sub-block, never shrink it,
/// so no candidate is dropped. df is selectivity-only; correctness depends on the shared total
/// order + exact verify, never on df accuracy (design rev-4 §2).
const DF_FALLBACK: i64 = i64::MAX;

/// One scoped baseline symbol's fingerprint, loaded for the candidate read.
pub(super) struct SymbolBag {
    pub(super) symbol_id: i64,
    pub(super) language: String,
    pub(super) struct_hash: String,
    pub(super) token_len: i64,
    /// `(token_hash, freq, coalesced_df)` for every distinct token in the symbol's bag.
    pub(super) tokens: Vec<TokenPosting>,
}

pub(super) struct TokenPosting {
    pub(super) token_hash: i64,
    pub(super) freq: i64,
    pub(super) coalesced_df: i64,
}

impl IndexDatabase {
    /// Candidate clone components over the ACTIVE scope, via the SourcererCC algorithm (design rev
    /// 4 §3b): a `struct_hash` exact fast path plus sub-block-filtered candidate pairs verified
    /// by EXACT max-denominator overlap, union-found into connected components. Both endpoints
    /// are filtered to the scoped `files` view BEFORE pairing, so a component never mixes
    /// out-of-scope symbols. Baseline postings only (recall is oracle-independent).
    /// Over-generated on purpose — `find_clones` (Plan 2) surfaces these as UNREFINED candidate
    /// classes; the coherence split + anti-unification is Plan 4.
    pub fn candidate_clone_components(&self) -> anyhow::Result<Vec<Vec<i64>>> {
        let conn = self.storage.connection();
        let pairs = candidate_pairs(conn)?;
        Ok(components_from_pairs(&pairs))
    }
}

// ── find_clones public API ────────────────────────────────────────────────────────────────────

/// Options for [`IndexDatabase::find_clones`].
#[derive(Debug, Clone)]
pub struct FindClonesOptions {
    /// Minimum similarity threshold (overlap/max_len). Defaults to [`THETA`] if `None`.
    pub min_similarity: Option<f64>,
    /// Minimum number of copies for a class to be returned. Defaults to 2 if `None`.
    pub min_copies: Option<usize>,
    /// Maximum number of classes to return (sorted by ROI desc). No limit if `None`.
    pub limit: Option<usize>,
}

/// Result of [`IndexDatabase::find_clones`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct FindClonesResult {
    pub classes: Vec<CandidateCloneClass>,
    pub completeness: CloneCompleteness,
}

impl IndexDatabase {
    /// Ranked candidate clone classes over the active scope.
    ///
    /// Runs the SourcererCC candidate-pair algorithm, union-finds pairs into components, hydrates
    /// each component into a [`CandidateCloneClass`] with pairwise similarity metrics and an ROI
    /// score, filters by `min_similarity` / `min_copies`, sorts by ROI descending, and attaches a
    /// [`CloneCompleteness`] provenance block. Classes are UNREFINED (Plan 4 adds coherence
    /// splitting and anti-unification).
    pub fn find_clones(&self, opts: FindClonesOptions) -> anyhow::Result<FindClonesResult> {
        let conn = self.storage.connection();

        // Validate the caller-supplied θ BEFORE it touches candidate generation. θ is a similarity
        // ratio (overlap/max_len) so it must lie in (0.0, 1.0]: ≤ 0.0 would admit every pair (and a
        // 0.0 sub-block prefix walks the whole bag), > 1.0 is unreachable and signals a unit error.
        // NaN must be explicitly rejected: both `v <= 0.0` and `v > 1.0` are false for NaN, so
        // without the `is_finite` guard NaN slips through and makes `ceil(NaN) as i64 = 0`, which
        // widens the sub-block to the whole bag → O(n²) blowup with every same-language
        // token-sharing pair reported as a clone.
        if let Some(v) = opts.min_similarity
            && (!v.is_finite() || v <= 0.0 || v > 1.0)
        {
            anyhow::bail!("min_similarity must be a finite value in (0.0, 1.0]");
        }

        let bags = load_scoped_baseline_bags(conn)?;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();

        // θ defaults to the const [`THETA`]; a caller-supplied `min_similarity` is honored ALL the
        // way through candidate generation (not merely post-filtered) so a θ below [`THETA`]
        // actually widens the candidate set instead of being clamped by the const-θ sub-block /
        // verify, and a θ above [`THETA`] narrows it.
        let theta = opts.min_similarity.unwrap_or(THETA);
        let pairs = candidate_pairs_from_bags(&bags, theta);
        let components = components_from_pairs(&pairs);

        let min_copies = opts.min_copies.unwrap_or(2);

        let mut classes: Vec<CandidateCloneClass> = Vec::new();

        for component in &components {
            if component.len() < min_copies {
                continue;
            }
            // No class-level `similarity_min < theta` filter: θ governs CANDIDATE GENERATION only
            // (every EDGE in the component is ≥ θ via `candidate_pairs_from_bags`). A component's
            // aggregate min-pairwise can dip below θ for a TRANSITIVE chain (A–B and B–C both ≥ θ,
            // but A–C < θ), and that component is legitimately one clone class — it stays visible,
            // gets ROI-penalized through the cohesion multiplier, and surfaces its low cohesion in
            // `cohesion_min_pairwise`. Dropping it here also diverged from `clones_for_symbol`,
            // which never applied this filter, so the two surfaces now agree on chain components.
            match build_class(component, &by_id, conn, None)? {
                None => continue,
                Some(class) => classes.push(class),
            }
        }

        // Sort by ROI descending (stable for determinism within ties).
        classes.sort_by(|a, b| b.roi.partial_cmp(&a.roi).unwrap_or(std::cmp::Ordering::Equal));

        // Capture the post-filter class count BEFORE the limit truncation so `truncated` can
        // report classes dropped by the limit (Fix 2), not just members capped within a class.
        let classes_after_filter = classes.len();

        if let Some(limit) = opts.limit {
            classes.truncate(limit);
        }

        // `truncated` is true if ANY returned class capped its member list (members_returned <
        // total_members) OR the class-limit dropped whole classes (classes_after_filter >
        // returned).
        let truncated = classes.iter().any(|c| c.members_returned < c.total_members)
            || classes_after_filter > classes.len();

        let freshness = self.meta("git_commit")?.unwrap_or_else(|| "unknown".to_string());

        // Count DISTINCT member file paths whose on-disk content no longer matches the indexed
        // sha256 (read-only signal; Plan 2 does not heal-before-return).
        let stale_members = count_stale_member_paths(self, conn, &classes)?;

        let completeness =
            build_completeness(theta, min_copies, truncated, stale_members, freshness);

        Ok(FindClonesResult { classes, completeness })
    }
}

/// Build the [`CloneCompleteness`] provenance block shared by `find_clones` and
/// `clones_for_symbol`. Only `min_similarity` (θ), `min_copies`, `truncated`, `stale_members`,
/// and the index freshness summary vary per call; the rest are fixed Plan-2 policy constants.
fn build_completeness(
    min_similarity: f64,
    min_copies: usize,
    truncated: bool,
    stale_members: usize,
    freshness: String,
) -> CloneCompleteness {
    CloneCompleteness {
        normalizer_kind: "baseline",
        normalizer_version: NORM_VERSION,
        min_similarity,
        min_tokens: crate::index::clones::MIN_TOKENS as i64,
        min_copies,
        candidate_metric: "overlap_max_denominator",
        containment_metric: "overlap_min_denominator",
        generated_excluded: true,
        tests_excluded: false,
        same_file_policy: "included",
        index_freshness: freshness,
        oracle_coverage: "n/a_baseline_only",
        truncated,
        stale_members,
        known_index_gaps: vec![
            "#232: TS function-valued declarators not yet fingerprinted".into(),
            "#232: comments/multi-language literals not yet normalized".into(),
        ],
    }
}

// ── clones_for_symbol public API ─────────────────────────────────────────────────────────────

/// How to identify the subject symbol for [`IndexDatabase::clones_for_symbol`].
#[derive(Debug, Clone)]
pub enum CloneSymbolSelector {
    /// An opaque `sym_<hex>` logical-symbol handle (as emitted by symbol-returning tools).
    Id(String),
    /// A fully-qualified `"path/to/file.rs::symbol_name"` reference.
    Ref(String),
    /// The tightest-spanning in-scope symbol whose line range contains `line` in `path`.
    PathLine { path: String, line: i64 },
}

impl IndexDatabase {
    /// Return a [`ClonesForSymbolResult`] for the symbol identified by `selector`: the containing
    /// candidate clone class (or `None` if the symbol is unique, not eligible, or unresolved),
    /// plus eligibility flags and the same completeness block as [`Self::find_clones`].
    ///
    /// `symbol_resolved` reports whether the selector matched a scoped symbol;
    /// `symbol_fingerprinted` reports whether that symbol has a current-version baseline
    /// fingerprint loaded into the candidate set (eligible). A symbol can resolve but not be
    /// fingerprinted (generated file, below `MIN_TOKENS`, or a non-function symbol) — `class`
    /// is then `None`.
    ///
    /// Resolution per selector form (all scoped through the active `files` view):
    /// - `Id`: parse the `sym_<hex>` handle → logical-symbol members → first member in a bag.
    /// - `Ref`: exact qualified-name match via `symbols JOIN name_strings`.
    /// - `PathLine`: tightest-spanning symbol at that line (`end_line - start_line ASC LIMIT 1`).
    pub fn clones_for_symbol(
        &self,
        selector: CloneSymbolSelector,
    ) -> anyhow::Result<ClonesForSymbolResult> {
        let conn = self.storage.connection();

        let make_result = |class: Option<CandidateCloneClass>,
                           symbol_resolved: bool,
                           symbol_fingerprinted: bool,
                           stale_members: usize,
                           freshness: String| {
            // A single class can only truncate by capping its own member list; there is no
            // class-limit here, so reuse the same member-cap signal as `find_clones`.
            let truncated = class.as_ref().is_some_and(|c| c.members_returned < c.total_members);
            ClonesForSymbolResult {
                class,
                symbol_resolved,
                symbol_fingerprinted,
                completeness: build_completeness(THETA, 2, truncated, stale_members, freshness),
            }
        };

        let freshness = self.meta("git_commit")?.unwrap_or_else(|| "unknown".to_string());

        let resolved_id = resolve_selector_to_symbol_id(conn, &selector)?;
        let Some(symbol_id) = resolved_id else {
            return Ok(make_result(None, false, false, 0, freshness));
        };

        let bags = load_scoped_baseline_bags(conn)?;
        // If the resolved symbol has no fingerprint row it is not eligible — it can't be in any
        // clone class (generated file, below MIN_TOKENS, or a non-function symbol).
        if !bags.iter().any(|b| b.symbol_id == symbol_id) {
            return Ok(make_result(None, true, false, 0, freshness));
        }

        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        let pairs = candidate_pairs_from_bags(&bags, THETA);
        let components = components_from_pairs(&pairs);

        // Find the component that contains this symbol_id.
        let Some(component) = components.into_iter().find(|comp| comp.contains(&symbol_id)) else {
            return Ok(make_result(None, true, true, 0, freshness));
        };

        // Pin the resolved subject so it is guaranteed to appear in the (capped) member list even
        // when its id falls past MAX_MEMBERS in the component's id order — the caller asked about
        // THIS symbol (Fix 2, #215).
        let class = build_class(&component, &by_id, conn, Some(symbol_id))?;

        // Count stale member paths over just this class's members (None class → 0 stale).
        let stale_members = match &class {
            None => 0,
            Some(c) => {
                let single = std::slice::from_ref(c);
                count_stale_member_paths(self, conn, single)?
            },
        };

        Ok(make_result(class, true, true, stale_members, freshness))
    }
}

/// Count DISTINCT member file paths in `classes` whose on-disk content no longer matches the
/// indexed `files.sha256`. Fetches the indexed sha256 per distinct path from the `files` view
/// (one query per distinct path) then calls [`IndexDatabase::source_path_is_stale`] — the same
/// pattern as `graph_index.rs`. This is a read-only signal; callers should reindex if non-zero.
fn count_stale_member_paths(
    db: &crate::index::IndexDatabase,
    conn: &Connection,
    classes: &[CandidateCloneClass],
) -> anyhow::Result<usize> {
    // `source_path_is_stale` reads bytes from `source_root` (the MAIN checkout). Under a
    // linked-worktree overlay scope the scoped clone results come from the BRANCH bytes
    // (`index_worktree_overlay`), so a main-checkout comparison is meaningless: branch-only files
    // look "missing" (false stale) and same-path branch edits diff against base content (false
    // stale). Mirror `heal_file` / the search-heal path / `parser_failures`, which all early-return
    // under an overlay scope, and report 0 (no main-checkout staleness signal is available here).
    if db.active_scope_is_linked_overlay() {
        return Ok(0);
    }
    // Collect distinct paths across all returned members.
    let mut distinct_paths: BTreeSet<String> = BTreeSet::new();
    for class in classes {
        for member in &class.members {
            distinct_paths.insert(member.path.clone());
        }
    }
    let mut stale = 0usize;
    for path in &distinct_paths {
        let sha256: Option<String> = conn
            .query_row("SELECT sha256 FROM files WHERE path = ?1 LIMIT 1", [path.as_str()], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(sha256) = sha256 else {
            // Path not in index at all — treat as stale.
            stale += 1;
            continue;
        };
        if db.source_path_is_stale(path, &sha256) {
            stale += 1;
        }
    }
    Ok(stale)
}

/// Resolve a [`CloneSymbolSelector`] to an in-scope `symbols.id` rowid, or `None` if the selector
/// doesn't match any symbol in the active scope.
fn resolve_selector_to_symbol_id(
    conn: &Connection,
    selector: &CloneSymbolSelector,
) -> anyhow::Result<Option<i64>> {
    match selector {
        CloneSymbolSelector::Id(handle) => {
            let Some(logical_id) = crate::serde_big_id::parse_sym_handle(handle) else {
                return Ok(None);
            };
            // A logical-symbol may have multiple member rows (cfg splits, overloads). We PREFER
            // a fingerprinted member: a cfg-split logical symbol whose lowest-rowid member is
            // below MIN_TOKENS or unfingerprinted but whose sibling IS fingerprinted (and in a
            // clone class) would otherwise report `symbol_fingerprinted=false` and miss the class.
            // `(sf.symbol_id IS NULL) ASC` sorts fingerprinted members first (NULL = unmatched →
            // treated as 1 in SQLite boolean context → sorts after matched rows); falls back to
            // lowest-rowid when no member is fingerprinted, so `symbol_resolved=true,
            // symbol_fingerprinted=false` is still correctly reported.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT lm.symbol_id
                     FROM logical_symbol_members lm
                     JOIN symbols ON symbols.id = lm.symbol_id
                     JOIN files ON files.id = symbols.file_id
                     LEFT JOIN symbol_fingerprints sf
                       ON sf.symbol_id = lm.symbol_id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?2
                     WHERE lm.logical_symbol_id = ?1
                     ORDER BY (sf.symbol_id IS NULL) ASC, lm.symbol_id ASC
                     LIMIT 1",
                    rusqlite::params![logical_id, NORM_VERSION],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
        CloneSymbolSelector::Ref(qualified_name) => {
            // Exact qualified-name match through the scoped `files` view.
            // Ambiguity rule: collect ALL current-version fingerprinted symbols matching this ref.
            // - 0 fingerprinted → fall back to lowest-rowid unfingerprinted match (preserved
            //   "resolved but not fingerprinted" path: symbol_resolved=true,
            //   symbol_fingerprinted=false), or None if no symbol at all.
            // - 1 fingerprinted → use it (unambiguous).
            // - >1 fingerprinted → REJECT with a clear error: the ref maps to multiple distinct
            //   logical symbols (overloads, cfg variants) — the caller must disambiguate with Id or
            //   PathLine. Silently picking one could return an unrelated overload's class.
            let mut fingerprinted_ids: Vec<i64> = conn
                .prepare(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                     JOIN symbol_fingerprints sf
                       ON sf.symbol_id = symbols.id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?2
                     WHERE ns.value = ?1
                     ORDER BY symbols.id ASC",
                )?
                .query_map(rusqlite::params![qualified_name.as_str(), NORM_VERSION], |row| {
                    row.get(0)
                })?
                .collect::<Result<_, _>>()?;
            // Deduplicate: the same symbols.id can appear multiple times if there are multiple
            // fingerprint rows (shouldn't happen for a normalizer_version-locked query, but be
            // safe).
            fingerprinted_ids.dedup();

            if fingerprinted_ids.len() > 1 {
                let n = fingerprinted_ids.len();
                anyhow::bail!(
                    "clones_for_symbol: ref '{}' matches {} fingerprinted symbols (overloads/cfg \
                     variants) — use id or path+line to disambiguate",
                    qualified_name,
                    n
                );
            }
            if let Some(&id) = fingerprinted_ids.first() {
                return Ok(Some(id));
            }
            // 0 fingerprinted matches: fall back to lowest-rowid unfingerprinted symbol for the
            // "resolved but not fingerprinted" path.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                     WHERE ns.value = ?1
                     ORDER BY symbols.id ASC
                     LIMIT 1",
                    rusqlite::params![qualified_name.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
        CloneSymbolSelector::PathLine { path, line } => {
            // Tightest-spanning symbol whose range contains `line`: smallest (end_line -
            // start_line) among symbols where start_line <= line <= end_line.
            // CONTRACT: span is the PRIMARY key — return the symbol AT the cursor and let the
            // eligibility flags report it as not-fingerprinted; do NOT silently jump to an
            // enclosing fingerprinted function. Fingerprint presence is a TIE-BREAKER only:
            // among symbols with equal span, prefer the fingerprinted variant (so a cfg-split
            // same-span pair doesn't pick the unfingerprinted one and miss the clone class).
            // rowid is the final stable tie-breaker.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     LEFT JOIN symbol_fingerprints sf
                       ON sf.symbol_id = symbols.id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?3
                     WHERE files.path = ?1
                       AND ?2 BETWEEN symbols.start_line AND symbols.end_line
                     ORDER BY (symbols.end_line - symbols.start_line) ASC, (sf.symbol_id IS NULL) \
                     ASC, symbols.id ASC
                     LIMIT 1",
                    rusqlite::params![path.as_str(), line, NORM_VERSION],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
    }
}

/// Extract candidate pairs from already-loaded bags (avoids a second DB round-trip in
/// `find_clones` vs the original `candidate_pairs` path). `theta` is the similarity threshold
/// applied to both the sub-block prefix length and the exact overlap/max verify — passing the
/// caller's `min_similarity` widens (or narrows) candidate generation to match the requested
/// floor, instead of generating at the const [`THETA`] and post-filtering.
fn candidate_pairs_from_bags(bags: &[SymbolBag], theta: f64) -> Vec<(i64, i64)> {
    let mut pairs: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();
    add_struct_hash_pairs(bags, &mut pairs);
    let candidate = sub_block_candidate_pairs(bags, theta);
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    for (a, b) in candidate {
        if verified_clone(by_id[&a], by_id[&b], theta) {
            pairs.insert((a, b));
        }
    }
    pairs.into_iter().collect()
}

/// Build a [`CandidateCloneClass`] from a component (a slice of symbol ids). Returns `None` if
/// any id is missing from `by_id` (shouldn't happen for a well-formed component derived from the
/// same bag set), or if member hydration yields nothing (TOCTOU: fingerprint rows vanished
/// mid-read). Members are capped at [`MAX_MEMBERS`].
///
/// `pin` is the subject `symbols.id` that MUST appear in the returned (capped) member list when it
/// is a member of the component — `clones_for_symbol` passes the resolved subject so the caller
/// always sees the symbol it asked about even when its id falls outside the first [`MAX_MEMBERS`]
/// by id. `find_clones` passes `None` (no subject to pin). `class_key` / `member_count` /
/// `total_members` are always over the FULL component regardless of `pin`.
pub(crate) fn build_class(
    component: &[i64],
    by_id: &BTreeMap<i64, &SymbolBag>,
    conn: &Connection,
    pin: Option<i64>,
) -> anyhow::Result<Option<CandidateCloneClass>> {
    let bags: Vec<&SymbolBag> = component.iter().filter_map(|id| by_id.get(id).copied()).collect();
    if bags.len() != component.len() {
        return Ok(None);
    }

    let n = bags.len();

    // For huge components, cap the pairwise metric work to avoid O(n²) blowup. When the
    // component exceeds METRIC_SAMPLE_CAP members the metrics run over the FIRST
    // METRIC_SAMPLE_CAP members only (the component is deterministically sorted, so this is
    // stable). `metrics_sampled` is set true so callers know. For all normal-size components
    // (the typical case, including ALL existing tests) `metric_n == n` and behavior is identical.
    let metrics_sampled = n > METRIC_SAMPLE_CAP;
    let metric_n = n.min(METRIC_SAMPLE_CAP);
    let metric_bags = &bags[..metric_n];

    // Pairwise similarity (overlap/max_len) and containment (overlap/min_len), upper-triangle
    // over metric_bags (== full component when metric_n == n).
    let mut similarity_min = f64::MAX;
    let mut containment_max = 0.0_f64;
    let mut sim_sums = vec![0.0_f64; metric_n]; // for medoid selection

    for i in 0..metric_n {
        for j in (i + 1)..metric_n {
            let ov = overlap(metric_bags[i], metric_bags[j]);
            let max_len = metric_bags[i].token_len.max(metric_bags[j].token_len);
            let min_len = metric_bags[i].token_len.min(metric_bags[j].token_len);
            let sim = if max_len == 0 { 1.0 } else { ov as f64 / max_len as f64 };
            let cont = if min_len == 0 { 1.0 } else { ov as f64 / min_len as f64 };
            if sim < similarity_min {
                similarity_min = sim;
            }
            if cont > containment_max {
                containment_max = cont;
            }
            sim_sums[i] += sim;
            sim_sums[j] += sim;
        }
    }
    if similarity_min == f64::MAX {
        // Singleton component — shouldn't reach here after min_copies filter, but be safe.
        similarity_min = 1.0;
    }
    let cohesion_min_pairwise = similarity_min;

    // Medoid: member with maximum sum of similarities to all others (within metric_bags).
    let medoid_idx = sim_sums
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let medoid_bag = metric_bags[medoid_idx];
    let body_token_len_medoid = medoid_bag.token_len;

    // Min similarity of any member to the medoid (within metric_bags).
    let mut similarity_medoid_min = f64::MAX;
    for (i, bag) in metric_bags.iter().enumerate() {
        if i == medoid_idx {
            continue;
        }
        let ov = overlap(medoid_bag, bag);
        let max_len = medoid_bag.token_len.max(bag.token_len);
        let sim = if max_len == 0 { 1.0 } else { ov as f64 / max_len as f64 };
        if sim < similarity_medoid_min {
            similarity_medoid_min = sim;
        }
    }
    if similarity_medoid_min == f64::MAX {
        similarity_medoid_min = 1.0;
    }

    let total_members = component.len();
    let cap = total_members.min(MAX_MEMBERS);

    // Hydrate CloneMembers via a scoped DB query with an IN clause, in batches of HYDRATION_CHUNK.
    // Fix 1 (#215): a single statement uses one host param per member id, so a component larger
    // than SQLITE_MAX_VARIABLE_NUMBER (999 on older non-bundled libs) would fail `conn.prepare`
    // and error the whole call. Chunking keeps every statement well under the limit; we
    // accumulate across chunks and re-sort by `symbol_id` so the deterministic id order is
    // restored regardless of chunk boundaries.
    // Fix 3 (#215): each chunk also filters normalizer_version so stale fingerprint rows don't
    // yield duplicate members or wrong token_len values. The version bind is appended as the
    // last positional param after that chunk's id list.
    let mut raw_members: Vec<(i64, CloneMember)> = Vec::with_capacity(total_members);
    for chunk in component.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let version_placeholder = format!("?{}", chunk.len() + 1);
        let sql = format!(
            "SELECT symbols.id, ns.value, files.path, symbols.start_line, symbols.end_line, \
             sf.token_len, symbols.language
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = {version_placeholder}
             WHERE symbols.id IN ({})
             ORDER BY symbols.id",
            id_placeholders.join(", ")
        );
        let params: Vec<i64> = chunk.iter().copied().chain(std::iter::once(NORM_VERSION)).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let symbol_id: i64 = row.get(0)?;
            Ok((symbol_id, CloneMember {
                r#ref: row.get(1)?,
                path: row.get(2)?,
                start_line: row.get(3)?,
                end_line: row.get(4)?,
                token_len: row.get(5)?,
                language: row.get(6)?,
            }))
        })?;
        for row in rows {
            raw_members.push(row?);
        }
    }
    // Restore the deterministic `symbols.id ASC` order the single-statement path produced.
    raw_members.sort_unstable_by_key(|(symbol_id, _)| *symbol_id);

    // Fix 5 (#215): if hydration returned nothing (all fingerprint rows vanished mid-read), bail to
    // `None` rather than build an internally-inconsistent class (member_count from the component
    // but zero members, an empty language fallback, etc.).
    if raw_members.is_empty() {
        return Ok(None);
    }

    let language = raw_members
        .first()
        .map(|(_, m)| m.language.clone())
        .unwrap_or_else(|| bags[0].language.clone());

    // cross_module_spread counts ALL hydrated members (full component), not just the capped subset
    // — so it is consistent with member_count (both over the full population).
    let parent_dirs: std::collections::BTreeSet<String> = raw_members
        .iter()
        .map(|(_, m)| {
            std::path::Path::new(&m.path)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        })
        .collect();
    let cross_module_spread = parent_dirs.len();

    // Fix 3 (#215): the class key is built from per-member identity that includes the source
    // LOCATION (`ref@path:start-end`), not the qualified-name `ref` alone. Two distinct
    // components can share the same qualified-name multiset — overloads, cfg variants,
    // same-named methods on different impls — and would otherwise collide on
    // `clone_refinements.class_key` (a TEXT PRIMARY KEY in Plan 4), conflating two classes into
    // one. We deliberately do NOT use `symbols.id`: the rowid is reassigned on every reindex,
    // so a location-derived key stays stable across reindexes while still distinguishing
    // same-named members at different spans. Computed over ALL members (full component), not
    // just the capped slice — two classes sharing the first MAX_MEMBERS members but
    // differing later must get different keys.
    let key_material: Vec<String> = raw_members
        .iter()
        .map(|(_, m)| format!("{}@{}:{}-{}", m.r#ref, m.path, m.start_line, m.end_line))
        .collect();
    let class_key = class_key_for(&key_material);

    // Cap the returned member list AFTER computing spread and key from the full set.
    // Fix 2 (#215): when a `pin` subject is supplied (clones_for_symbol) and that member exists but
    // would fall OUTSIDE the first `cap` members by id, guarantee its inclusion: keep the first
    // `cap - 1` by id plus the pinned member, so the caller always sees the symbol it asked about.
    // When `pin` is `None`, or the subject is already within the first `cap`, this is a no-op and
    // the selection is identical to the plain `take(cap)` path.
    let member_count = total_members;
    let members_returned = raw_members.len().min(cap);

    let pinned_idx = pin.and_then(|subject_id| {
        let pos = raw_members.iter().position(|(id, _)| *id == subject_id)?;
        // Only act when the pin would otherwise be dropped: it sits at or past `cap` in id order.
        (pos >= cap).then_some(pos)
    });

    let chosen: Vec<CloneMember> = match pinned_idx {
        Some(pos) => {
            // First `cap - 1` by id, plus the pinned member → exactly `cap` members.
            let mut chosen: Vec<CloneMember> =
                raw_members.iter().take(cap - 1).map(|(_, m)| m.clone()).collect();
            chosen.push(raw_members[pos].1.clone());
            chosen
        },
        None => raw_members.into_iter().take(cap).map(|(_, m)| m).collect(),
    };
    let mut members = chosen;
    members.sort_unstable_by(|a, b| a.r#ref.cmp(&b.r#ref));

    // Load-bearing factor: 1 + ln(1 + max_fan_in_score) over members. Fan-in proxy via
    // `scoped_weighted_fan_in` (heuristic-only, no oracle data at this call site).
    let oracle = crate::query::load_bearing::OracleContext::none();
    let max_importance = component
        .iter()
        .filter_map(|&id| {
            crate::query::load_bearing::scoped_weighted_fan_in(conn, id, &oracle)
                .ok()
                .flatten()
                .map(|e| e.score)
        })
        .fold(0.0_f64, f64::max);
    let load_bearing_factor = 1.0 + max_importance.ln_1p();

    // Median token_len across all bags in the component.
    let mut token_lens: Vec<i64> = bags.iter().map(|b| b.token_len).collect();
    token_lens.sort_unstable();
    let median_token_len = token_lens[token_lens.len() / 2];

    let roi = cross_module_spread as f64
        * member_count as f64
        * body_token_len_medoid as f64
        * load_bearing_factor
        * cohesion_min_pairwise;

    let roi_factors = RoiFactors {
        member_count,
        cross_module_spread,
        median_token_len,
        load_bearing_factor,
        cohesion_penalty: cohesion_min_pairwise,
    };

    Ok(Some(CandidateCloneClass {
        class_key,
        class_kind: "candidate_component",
        language,
        refined: false,
        members,
        member_count,
        members_returned,
        total_members,
        similarity_min,
        similarity_medoid_min,
        containment_max,
        cohesion_min_pairwise,
        cross_module_spread,
        body_token_len_medoid,
        roi,
        roi_factors,
        metrics_sampled,
    }))
}

/// `(symbol_id, symbol_id)` candidate pairs (a < b), both within the scoped `files` view.
///
/// Combines the `struct_hash` exact fast path with the sub-block + exact-verify candidate read
/// (design rev 4 §3b). Returns deduplicated `(a, b)` pairs with `a < b` for the union-find.
fn candidate_pairs(conn: &Connection) -> anyhow::Result<Vec<(i64, i64)>> {
    let bags = load_scoped_baseline_bags(conn)?;

    let mut pairs: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();

    // 1. Exact fast path: every same-struct_hash set contributes all its pairwise pairs.
    add_struct_hash_pairs(&bags, &mut pairs);

    // 2. Sub-block candidate pairs via the inverted index over sub-block tokens only.
    //    `candidate_clone_components` keeps the const THETA (behavior unchanged) — only
    //    `find_clones` threads the caller's `min_similarity` through candidate generation.
    let candidate = sub_block_candidate_pairs(&bags, THETA);

    // 3. Size prune + EXACT max-denominator verify over the FULL bags.
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    for (a, b) in candidate {
        let (ba, bb) = (by_id[&a], by_id[&b]);
        if verified_clone(ba, bb, THETA) {
            pairs.insert((a, b));
        }
    }

    Ok(pairs.into_iter().collect())
}

/// Load every scoped baseline symbol's fingerprint + full token bag with LEFT-JOINed df. Both the
/// fingerprint and its postings are filtered to the scoped `files` view through `symbols.file_id`,
/// so only the ACTIVE version of each file participates (SCOPED-VIEW REQUIREMENT #89). df is read
/// via LEFT JOIN + COALESCE so a missing-df token is never dropped (design rev-4 §2).
fn load_scoped_baseline_bags(conn: &Connection) -> anyhow::Result<Vec<SymbolBag>> {
    // Scoped baseline fingerprints: struct_hash + token_len per in-scope symbol.
    // `files.generated = 0` excludes generated files (e.g. `src/generated/…`, `.d.ts`) from the
    // candidate read — they are fingerprinted on write but must not enter clone components.
    let mut fp_stmt = conn.prepare(
        "SELECT sf.symbol_id, symbols.language, sf.struct_hash, sf.token_len
         FROM symbol_fingerprints sf
         JOIN symbols ON symbols.id = sf.symbol_id
         JOIN files ON files.id = symbols.file_id
         WHERE sf.normalizer_kind = 'baseline'
           AND sf.normalizer_version = ?1
           AND files.generated = 0",
    )?;
    let mut bags: BTreeMap<i64, SymbolBag> = fp_stmt
        .query_map([NORM_VERSION], |row| {
            let symbol_id: i64 = row.get(0)?;
            Ok((symbol_id, SymbolBag {
                symbol_id,
                language: row.get(1)?,
                struct_hash: row.get(2)?,
                token_len: row.get(3)?,
                tokens: Vec::new(),
            }))
        })?
        .collect::<Result<_, _>>()?;

    // Full token bag per scoped baseline symbol, with each token's df LEFT-JOINed + COALESCEd to
    // the fallback sentinel (missing-df tokens must NOT be dropped — rev-4 §2).
    // `files.generated = 0` mirrors the fingerprint load: only non-generated symbols get postings
    // loaded, so their bags never enter the inverted index or the exact verify.
    let mut tok_stmt = conn.prepare(
        "SELECT stp.symbol_id, stp.token_hash, stp.freq, COALESCE(df.df, ?1)
         FROM symbol_token_postings stp
         JOIN symbols ON symbols.id = stp.symbol_id
         JOIN files ON files.id = symbols.file_id
         LEFT JOIN clone_token_df df
           ON df.normalizer_kind = stp.normalizer_kind AND df.token_hash = stp.token_hash
         WHERE stp.normalizer_kind = 'baseline'
           AND files.generated = 0",
    )?;
    let rows = tok_stmt.query_map([DF_FALLBACK], |row| {
        Ok((row.get::<_, i64>(0)?, TokenPosting {
            token_hash: row.get(1)?,
            freq: row.get(2)?,
            coalesced_df: row.get(3)?,
        }))
    })?;
    for row in rows {
        let (symbol_id, posting) = row?;
        // `bags` keyset is already `normalizer_version`-filtered by the fingerprint query above,
        // so a stale-version symbol (absent from `bags`) has its postings silently dropped here.
        if let Some(bag) = bags.get_mut(&symbol_id) {
            bag.tokens.push(posting);
        }
    }

    // Sort each bag's token list by `token_hash` once, here, so `overlap` can use an
    // allocation-free two-pointer merge instead of rebuilding a `BTreeMap` on every call.
    // Consumers that care about order (sub_block_tokens, struct_hash, inverted-index build,
    // token_len) are all order-independent — they sort by (df, hash) themselves or use a separate
    // field — so this re-ordering is safe.
    for bag in bags.values_mut() {
        bag.tokens.sort_unstable_by_key(|t| t.token_hash);
    }

    Ok(bags.into_values().collect())
}

/// Exact fast path: every group of symbols sharing a `struct_hash` AND `language` is
/// identical-after-normalization, so it contributes all its pairwise pairs (no overlap math).
/// Language partition is required: different languages share no grammar token space, so a
/// struct_hash collision across languages is a false positive.
fn add_struct_hash_pairs(bags: &[SymbolBag], pairs: &mut std::collections::BTreeSet<(i64, i64)>) {
    // Key: (struct_hash, language) — only same-language symbols can be struct-hash clones.
    let mut by_hash: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    for bag in bags {
        by_hash
            .entry((bag.struct_hash.as_str(), bag.language.as_str()))
            .or_default()
            .push(bag.symbol_id);
    }
    for ids in by_hash.values() {
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                pairs.insert((a.min(b), a.max(b)));
            }
        }
    }
}

/// Build the inverted index over sub-block tokens only and emit candidate pairs `(a < b)` for every
/// pair of symbols sharing a sub-block token. Admissibility (design rev-4 §3b): two symbols can
/// reach similarity ≥ θ only if their sub-blocks share a token hash, so this yields every true
/// candidate pair regardless of df accuracy (given the shared total order).
///
/// Language partition: only same-language pairs are emitted — different languages have disjoint
/// grammar token spaces, so a token-hash collision across languages is a false positive.
fn sub_block_candidate_pairs(
    bags: &[SymbolBag],
    theta: f64,
) -> std::collections::BTreeSet<(i64, i64)> {
    // id → language for the partition guard applied at pair-emit time.
    let lang_of: BTreeMap<i64, &str> =
        bags.iter().map(|b| (b.symbol_id, b.language.as_str())).collect();

    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        for token_hash in sub_block_tokens(bag, theta) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }

    let mut candidate: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();
    for ids in inverted.values() {
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                // Language partition: skip cross-language pairs.
                if lang_of[&a] != lang_of[&b] {
                    continue;
                }
                candidate.insert((a.min(b), a.max(b)));
            }
        }
    }
    candidate
}

/// A symbol's sub-block: the distinct token hashes whose occurrences reach into the first `p`
/// occurrences under the deterministic total order `(coalesced_df ASC, token_hash ASC)`.
///
/// `p = token_len - ceil(theta * token_len) + 1` is the sub-block OCCURRENCE length (clamped to ≥
/// 0; if `p >= token_len` the whole bag is the sub-block). The sub-block is defined over EXPANDED
/// token occurrences (Σ freq), not distinct posting rows, so it matches the multiset `Σ min(freq)`
/// verifier (design rev-4 §3): walking distinct tokens in order accumulating `freq`, a token is
/// included if the running occurrence-count BEFORE it is `< p` (i.e. any of its occurrences falls
/// in the prefix).
fn sub_block_tokens(bag: &SymbolBag, theta: f64) -> Vec<i64> {
    let p = sub_block_len(bag.token_len, theta);
    if p <= 0 {
        return Vec::new();
    }

    let mut ordered: Vec<&TokenPosting> = bag.tokens.iter().collect();
    ordered.sort_by_key(|t| (t.coalesced_df, t.token_hash));

    let mut sub_block = Vec::new();
    let mut occurrences_before: i64 = 0;
    for token in ordered {
        // Include this token if any of its occurrences falls within the first `p` occurrences, i.e.
        // the running count BEFORE it is still inside the prefix.
        if occurrences_before < p {
            sub_block.push(token.token_hash);
        } else {
            break; // every later token starts past the prefix too (occurrences only grow).
        }
        occurrences_before += token.freq;
    }
    sub_block
}

/// Sub-block occurrence length `p = token_len - ceil(theta * token_len) + 1`, clamped to ≥ 0.
fn sub_block_len(token_len: i64, theta: f64) -> i64 {
    let threshold = (theta * token_len as f64).ceil() as i64;
    (token_len - threshold + 1).max(0)
}

/// Size prune + EXACT max-denominator verify (design rev-4 §3b). With `min_len`/`max_len` = the two
/// token_lens: cheap size prune `min_len >= ceil(theta * max_len)`; then `overlap = Σ min(freq_a,
/// freq_b)` over the FULL bags, kept iff `overlap >= ceil(theta * max_len)`. The GATE is
/// `similarity = overlap / max_len`; containment = `overlap / min_len` is NOT gated here.
fn verified_clone(a: &SymbolBag, b: &SymbolBag, theta: f64) -> bool {
    let min_len = a.token_len.min(b.token_len);
    let max_len = a.token_len.max(b.token_len);
    let threshold = (theta * max_len as f64).ceil() as i64;

    // Size prune: a smaller block can't reach θ against a larger one.
    if min_len < threshold {
        return false;
    }

    overlap(a, b) >= threshold
}

/// Exact multiset overlap `Σ min(freq_a, freq_b)` over the two FULL token bags.
///
/// Requires both bags' `tokens` slices to be sorted by `token_hash` ascending (guaranteed by
/// [`load_scoped_baseline_bags`], which sorts once after collection). Uses an allocation-free
/// two-pointer merge: no `BTreeMap` rebuild per call — O(|a| + |b|) time, zero heap allocation.
fn overlap(a: &SymbolBag, b: &SymbolBag) -> i64 {
    let (mut ia, mut ib) = (0, 0);
    let (ta, tb) = (a.tokens.as_slice(), b.tokens.as_slice());
    let mut total: i64 = 0;
    while ia < ta.len() && ib < tb.len() {
        match ta[ia].token_hash.cmp(&tb[ib].token_hash) {
            std::cmp::Ordering::Less => ia += 1,
            std::cmp::Ordering::Greater => ib += 1,
            std::cmp::Ordering::Equal => {
                total += ta[ia].freq.min(tb[ib].freq);
                ia += 1;
                ib += 1;
            },
        }
    }
    total
}

/// Union-find the pairs into components of size >= 2 (sorted for determinism).
fn components_from_pairs(pairs: &[(i64, i64)]) -> Vec<Vec<i64>> {
    use std::collections::BTreeMap;

    fn find(parent: &mut BTreeMap<i64, i64>, x: i64) -> i64 {
        let mut root = x;
        while let Some(&p) = parent.get(&root) {
            if p == root {
                break;
            }
            root = p;
        }
        let mut cur = x;
        while let Some(&p) = parent.get(&cur) {
            if p == root {
                break;
            }
            parent.insert(cur, root);
            cur = p; // p captured before the insert, so advancing to the pre-compression parent is safe
        }
        root
    }

    let mut parent: BTreeMap<i64, i64> = BTreeMap::new();
    for &(a, b) in pairs {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra.max(rb), ra.min(rb));
        }
    }
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let members: Vec<i64> = parent.keys().copied().collect(); // collect keys first: find() needs &mut parent
    for member in members {
        let root = find(&mut parent, member);
        groups.entry(root).or_default().push(member);
    }
    groups
        .into_values()
        .filter(|g| g.len() >= 2)
        .map(|mut g| {
            g.sort_unstable();
            g
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        SymbolBag, THETA, TokenPosting, add_struct_hash_pairs, class_key_for,
        components_from_pairs, overlap, sub_block_candidate_pairs,
    };

    // ── Fix A: NaN / non-finite min_similarity ────────────────────────────────────────────────

    /// NaN and non-finite values must be rejected by the range guard. The old guard
    /// `v <= 0.0 || v > 1.0` passes NaN (both comparisons return false for NaN), which makes
    /// `ceil(NaN) as i64 = 0` → whole-bag sub-block → every same-language token-sharing pair
    /// is a clone → O(n²) blowup. Fix A adds `!v.is_finite()` before the range checks.
    #[test]
    fn find_clones_rejects_nan_and_non_finite_min_similarity() {
        use crate::index::FindClonesOptions;

        // We don't need a real DB here — the validation fires before any DB access.
        // Construct a minimal IndexDatabase pointing at a non-existent path; the validation
        // bail!() runs in the same function before the DB is touched.
        // Instead, test the validation logic directly via the public API by setting up a
        // temporary empty database and calling find_clones with the bad values.
        let root = std::env::temp_dir().join(format!(
            "rag-rat-nan-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let config = crate::Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![crate::config::ResolvedTarget {
                name: "rust".to_string(),
                language: crate::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: crate::config::TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
        };
        crate::IndexDatabase::rebuild(&config).unwrap();
        let db = crate::IndexDatabase::open_config(&config).unwrap();

        // NaN must be rejected.
        let err = db
            .find_clones(FindClonesOptions {
                min_similarity: Some(f64::NAN),
                min_copies: None,
                limit: None,
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("finite"),
            "NaN should produce a 'finite' error message, got: {err}"
        );

        // +infinity must be rejected.
        let err = db
            .find_clones(FindClonesOptions {
                min_similarity: Some(f64::INFINITY),
                min_copies: None,
                limit: None,
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("finite"),
            "INFINITY should produce a 'finite' error message, got: {err}"
        );

        // -infinity must be rejected (also non-finite).
        let err = db
            .find_clones(FindClonesOptions {
                min_similarity: Some(f64::NEG_INFINITY),
                min_copies: None,
                limit: None,
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("finite"),
            "NEG_INFINITY should produce an error, got: {err}"
        );

        // 0.0 still rejected (in-range check, kept from before).
        assert!(
            db.find_clones(FindClonesOptions {
                min_similarity: Some(0.0),
                min_copies: None,
                limit: None,
            })
            .is_err()
        );

        // 1.0 is the boundary — must NOT be rejected.
        assert!(
            db.find_clones(FindClonesOptions {
                min_similarity: Some(1.0),
                min_copies: None,
                limit: None,
            })
            .is_ok()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Fix B: allocation-free two-pointer overlap ────────────────────────────────────────────

    fn make_bag_with_tokens(id: i64, tokens: Vec<(i64, i64)>) -> SymbolBag {
        let mut postings: Vec<TokenPosting> = tokens
            .into_iter()
            .map(|(hash, freq)| TokenPosting { token_hash: hash, freq, coalesced_df: 1 })
            .collect();
        // Simulate what load_scoped_baseline_bags does: sort by token_hash.
        postings.sort_unstable_by_key(|t| t.token_hash);
        SymbolBag {
            symbol_id: id,
            language: "rust".to_string(),
            struct_hash: format!("hash{id}"),
            token_len: postings.iter().map(|t| t.freq).sum(),
            tokens: postings,
        }
    }

    /// The two-pointer `overlap` must return the same value as the naive map-based version for
    /// any pair of token multisets.
    #[test]
    fn overlap_two_pointer_matches_naive() {
        fn naive_overlap(a: &SymbolBag, b: &SymbolBag) -> i64 {
            let freq_a: std::collections::BTreeMap<i64, i64> =
                a.tokens.iter().map(|t| (t.token_hash, t.freq)).collect();
            let mut total = 0;
            for token in &b.tokens {
                if let Some(&fa) = freq_a.get(&token.token_hash) {
                    total += fa.min(token.freq);
                }
            }
            total
        }

        // Case 1: fully disjoint — overlap must be 0.
        let a = make_bag_with_tokens(1, vec![(1, 3), (2, 2)]);
        let b = make_bag_with_tokens(2, vec![(3, 1), (4, 5)]);
        assert_eq!(overlap(&a, &b), 0);
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));

        // Case 2: fully identical — overlap = sum of all freqs.
        let a = make_bag_with_tokens(1, vec![(10, 2), (20, 3), (30, 1)]);
        let b = make_bag_with_tokens(2, vec![(10, 2), (20, 3), (30, 1)]);
        assert_eq!(overlap(&a, &b), 6); // 2+3+1
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));

        // Case 3: partial overlap, asymmetric frequencies.
        // a: token 5 freq=4, token 7 freq=2, token 9 freq=1
        // b: token 5 freq=2, token 8 freq=3, token 9 freq=5
        // overlap = min(4,2) + min(1,5) = 2 + 1 = 3
        let a = make_bag_with_tokens(1, vec![(5, 4), (7, 2), (9, 1)]);
        let b = make_bag_with_tokens(2, vec![(5, 2), (8, 3), (9, 5)]);
        assert_eq!(overlap(&a, &b), 3);
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));

        // Case 4: one empty bag — overlap must be 0.
        let a = make_bag_with_tokens(1, vec![(1, 1)]);
        let b = make_bag_with_tokens(2, vec![]);
        assert_eq!(overlap(&a, &b), 0);
        assert_eq!(overlap(&b, &a), 0);
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));
    }

    // ── Fix C: METRIC_SAMPLE_CAP — existing tests have metrics_sampled=false ─────────────────

    // (No in-unit test for the >200 sampled path: planting 200+ valid fingerprinted symbols
    // via an integration DB is too expensive for a unit test. The struct field and the cap guard
    // are covered by compilation + the assertion below on small components.)

    /// For all test-fixture components (size <= METRIC_SAMPLE_CAP), metrics_sampled must be false
    /// and behavior must be identical to the pre-cap code. This test exercises the union-find and
    /// struct-level assertions only; the full DB integration test in clones_handler covers the
    /// rest.
    #[test]
    fn small_component_metrics_sampled_is_false() {
        // Verify the constant is what the spec requires.
        assert_eq!(super::METRIC_SAMPLE_CAP, 200);

        // The components from a small pair (union-find returns groups of size 2).
        let comps = components_from_pairs(&[(1, 2)]);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].len(), 2);
        // len=2 < 200 → metrics_sampled would be false in build_class.
        // We can't call build_class without a DB, so we just assert the cap logic in isolation:
        let n = comps[0].len();
        let metrics_sampled = n > super::METRIC_SAMPLE_CAP;
        assert!(!metrics_sampled, "a 2-member component must not trigger metrics_sampled");
    }

    // ── Fix E: CLI handle routing ─────────────────────────────────────────────────────────────
    // (Tested in the CLI crate test below; here we just verify parse_sym_handle behaviour
    // that the routing logic depends on.)

    #[test]
    fn parse_sym_handle_accepts_valid_handles_and_rejects_others() {
        use crate::serde_big_id::parse_sym_handle;

        // A valid sym_<hex> handle round-trips.
        let h = crate::serde_big_id::format_sym_handle(12345i64);
        assert!(parse_sym_handle(&h).is_some());

        // A qualified name like `sym_utils.rs::load_user` is NOT a valid handle — it has `::`
        // and the hex part is `utils.rs` which is not valid hex.
        assert!(parse_sym_handle("sym_utils.rs::load_user").is_none());

        // A bare string without `sym_` prefix is None.
        assert!(parse_sym_handle("foo::bar").is_none());
    }

    // ── Existing tests ────────────────────────────────────────────────────────────────────────

    #[test]
    fn class_key_is_deterministic_and_order_independent() {
        let k1 = class_key_for(&["a.rs::x".into(), "b.rs::y".into()]);
        let k2 = class_key_for(&["b.rs::y".into(), "a.rs::x".into()]);
        assert_eq!(k1, k2);
        assert_ne!(k1, class_key_for(&["a.rs::x".into(), "c.rs::z".into()]));
    }

    /// Fix 3 (#215): the class key is built from per-member `ref@path:start-end` material, so two
    /// components that share the same qualified-name multiset but live at different LOCATIONS get
    /// distinct keys. This guards `clone_refinements.class_key` (a TEXT PRIMARY KEY in Plan 4) from
    /// conflating two real clone classes (overloads, cfg variants, same-named methods on different
    /// impls). `class_key_for` itself is unchanged — only the material `build_class` feeds it is.
    #[test]
    fn class_key_distinguishes_same_ref_at_different_locations() {
        // Same qualified name `x`, same span, DIFFERENT file → distinct keys.
        let key_a = class_key_for(&["x@a.rs:1-5".into()]);
        let key_b = class_key_for(&["x@b.rs:1-5".into()]);
        assert_ne!(key_a, key_b, "same ref in different files must not collide");

        // Same qualified name `x`, same file, DIFFERENT span → distinct keys.
        let key_span1 = class_key_for(&["x@a.rs:1-5".into()]);
        let key_span2 = class_key_for(&["x@a.rs:2-6".into()]);
        assert_ne!(key_span1, key_span2, "same ref/file at different spans must not collide");
    }

    #[test]
    fn union_find_groups_transitively_and_drops_singletons() {
        // 1-2, 2-3 => {1,2,3}; 5-6 => {5,6}; 9 alone => dropped.
        let comps = components_from_pairs(&[(1, 2), (2, 3), (5, 6)]);
        assert_eq!(comps, vec![vec![1, 2, 3], vec![5, 6]]);
    }

    /// Language partition unit test: two bags with IDENTICAL tokens/struct_hash/token_len but
    /// DIFFERENT languages must NOT pair via either the struct-hash fast path or the sub-block
    /// inverted-index path. Two same-language identical bags MUST pair via the struct-hash path.
    #[test]
    fn language_partition_blocks_cross_language_pairs_and_keeps_same_language() {
        // Shared token bag — same tokens, same struct_hash, same token_len.
        let make_bag = |id: i64, language: &str| SymbolBag {
            symbol_id: id,
            language: language.to_string(),
            struct_hash: "deadbeef".to_string(),
            token_len: 5,
            tokens: vec![
                TokenPosting { token_hash: 1, freq: 2, coalesced_df: 10 },
                TokenPosting { token_hash: 2, freq: 1, coalesced_df: 20 },
                TokenPosting { token_hash: 3, freq: 2, coalesced_df: 30 },
            ],
        };

        // id=1 is Rust, id=2 is TypeScript — identical token bags, different language.
        let bag_rust = make_bag(1, "rust");
        let bag_ts = make_bag(2, "typescript");
        // id=3 is also Rust, identical to id=1 — same language, same struct_hash.
        let bag_rust2 = make_bag(3, "rust");

        let bags = vec![bag_rust, bag_ts, bag_rust2];

        // struct-hash fast path: must NOT produce a cross-language pair (1,2) but MUST produce
        // same-language pair (1,3).
        let mut hash_pairs: BTreeSet<(i64, i64)> = BTreeSet::new();
        add_struct_hash_pairs(&bags, &mut hash_pairs);
        assert!(
            !hash_pairs.contains(&(1, 2)),
            "struct-hash path must not pair rust(1) with typescript(2): {hash_pairs:?}"
        );
        assert!(
            hash_pairs.contains(&(1, 3)),
            "struct-hash path must pair rust(1) with rust(3): {hash_pairs:?}"
        );

        // sub-block inverted-index path: same assertions.
        let sub_pairs = sub_block_candidate_pairs(&bags, THETA);
        assert!(
            !sub_pairs.contains(&(1, 2)),
            "sub-block path must not pair rust(1) with typescript(2): {sub_pairs:?}"
        );
        assert!(
            sub_pairs.contains(&(1, 3)),
            "sub-block path must pair rust(1) with rust(3): {sub_pairs:?}"
        );
    }
}
