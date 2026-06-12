//! The oracle pass: consume a pre-built `.scip`, join its occurrences against edge candidates,
//! write `edge_oracle` verdicts, and return an [`OracleReport`]. Phase 1 entry point — no indexer
//! invocation, no CLI/MCP surface (#69).
//!
//! Resumable-friendly shape: candidates are loaded once and processed in document order, each
//! verdict written independently, so a future incremental driver can scope `edge_join_candidates`
//! to a changed-file set without touching this loop.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::join::{self, JoinInput};
use super::scip::{self, ScipIndex};
use super::store::{self, CALL_EDGE_KIND, EdgeOracleRow};
use super::{OracleReport, OracleResolutionKind, OracleTool};

/// Inputs for one oracle pass.
pub(crate) struct OracleRunInput<'a> {
    pub(crate) tool: OracleTool,
    pub(crate) tool_version: &'a str,
    /// The commit/worktree the edges are scoped to (and which the `.scip` was built against).
    pub(crate) commit_sha: &'a str,
    pub(crate) worktree_id: &'a str,
    /// Serialized `.scip` bytes.
    pub(crate) scip_bytes: &'a [u8],
    /// Checkout root: document paths in the `.scip` are joined against this to read current bytes
    /// for position-encoding conversion.
    pub(crate) checkout_root: &'a Path,
    /// `relative_path -> hex sha256` of the disk bytes captured the instant the tool finished
    /// producing the `.scip`, for a tool-driven run; `None` for a pre-built `--scip` (no
    /// production moment we control). When present it adds the scip-vs-disk leg of the content
    /// gate (#82 TOCTOU): the `.scip`'s occurrence offsets describe the bytes the subprocess saw,
    /// so the join only trusts them for a document whose disk content is STILL that snapshot. Both
    /// documents a verdict depends on are pinned against it — the call-site document
    /// (pre-classify) AND the resolved symbol's definition document (post-classify) — since
    /// the watcher reindexing EITHER in the lock-free window corrupts the join while `disk_sha
    /// == file_sha` (both the new content) still passes the index-vs-disk gate. See
    /// [`super::ScipProduction::Produced`].
    pub(crate) production_sha: Option<&'a HashMap<String, String>>,
    /// The indexed `(path -> files.sha256)` snapshot taken **before the tool subprocess was
    /// spawned** (#83); `None` for a pre-built `--scip` (no spawn). The post-exit
    /// `production_sha` gate cannot see INSIDE the subprocess: the tool reads each source near
    /// the start of a run that can take tens of seconds, so a file edited mid-run and
    /// reindexed before the join leaves `.scip` describing the OLD content while
    /// `files.sha256`, the disk bytes, and `production_sha` are all the NEW content — every
    /// post-exit gate passes and a stale verdict persists. Requiring the join-time indexed sha
    /// to ALSO equal this pre-spawn snapshot asserts the watcher reindexed nothing across the
    /// ENTIRE window (spawn → join), for both the call-site and definition documents. Residual
    /// ABA (an edit reverted to byte-identical content) is acceptable — the verdict is then
    /// correct anyway.
    pub(crate) pre_spawn_sha: Option<&'a HashMap<String, String>>,
}

/// Run the oracle join over all current edge candidates and persist verdicts + a run row.
///
/// The run is **authoritative** for its `(tool, tool_version)`: it first clears that scope's prior
/// `edge_oracle` rows, then writes the current `.scip`'s verdicts — all in one transaction. Without
/// the clear, a rerun whose new `.scip` no longer yields a verdict for an edge would leave the
/// prior (now stale) verdict in place (the per-edge upsert only overwrites edges the rerun
/// revisits), so `status`/eval would keep counting a verdict the current oracle doesn't stand
/// behind.
pub(crate) fn run(conn: &Connection, input: &OracleRunInput<'_>) -> anyhow::Result<OracleReport> {
    let mut report = OracleReport::default();

    let candidates = store::edge_join_candidates(conn, input.commit_sha, input.worktree_id)?;

    // Authoritative-rerun clear + the per-edge writes + the run row are one atomic unit, so the
    // table is never observed mid-clear and a failed run rolls back to the prior verdicts. All
    // inner reads/writes run on `conn` (the same underlying connection `tx` guards), so they
    // are part of this transaction; `tx` exists only to BEGIN/COMMIT around them.
    let tx = conn.unchecked_transaction()?;
    store::clear_edge_oracle_for_tool(
        conn,
        input.tool,
        input.tool_version,
        input.commit_sha,
        input.worktree_id,
    )?;
    // Monikers are authoritative per tool the same way verdicts are per (tool, tool_version):
    // clear, then write the current `.scip`'s definitions, all in this transaction (#70).
    store::clear_logical_symbol_monikers_for_tool(conn, input.tool)?;

    // Parse the `.scip`, reading each document's current checkout bytes for encoding conversion.
    // The bytes we read here are the SAME bytes whose hash we compare against each candidate's
    // `file_sha` below (content-integrity, finding 2): occurrence byte ranges are derived from
    // these live disk bytes, so if they drifted from what the edge candidate / `.scip` were
    // built against, the join is comparing incompatible coordinate spaces.
    let checkout_root = input.checkout_root.to_path_buf();
    let mut source_cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let index = ScipIndex::parse(input.scip_bytes, |path| {
        source_cache
            .entry(path.to_string())
            .or_insert_with(|| std::fs::read(checkout_root.join(path)).ok())
            .clone()
    })?;

    // Per-document hash of the disk bytes actually read, for the content-integrity check. A
    // candidate whose `file_sha` (the `files.sha256` the edge was indexed against) differs from the
    // disk bytes we just converted occurrences from is drifted — we skip its verdict rather than
    // emit one from mismatched content (finding 2). `None`-valued cache entries (unreadable files,
    // already skipped during parse) carry no hash.
    let disk_sha: HashMap<String, String> = source_cache
        .iter()
        .filter_map(|(path, bytes)| bytes.as_ref().map(|b| (path.clone(), hex_sha256(b))))
        .collect();

    // The join-time indexed `(path -> files.sha256)` map for the active checkout. Used by the
    // pre-spawn gate's DEFINITION-side check (#83) and the moniker pass below (the call-site side
    // compares against `candidate.file_sha`, which already IS the join-time indexed sha).
    let indexed_shas =
        store::indexed_file_shas_in_scope(conn, input.commit_sha, input.worktree_id)?;

    // Cache symbol spans per source path (resolve_symbol is called per candidate). RefCell so the
    // resolver closure stays `Fn` (the join expects `&dyn Fn`) while lazily populating the cache.
    let symbol_span_cache: RefCell<HashMap<String, Vec<store::SymbolSpan>>> =
        RefCell::new(HashMap::new());

    // Track which (path, occurrence) the edges covered, to compute the recall gap. EVERY verdict's
    // occurrence goes here (so a covered occurrence — call or not — is never re-counted as a gap).
    let mut matched_occurrences: HashSet<(String, usize, usize)> = HashSet::new();
    // The covered side of recall: distinct (path, occurrence) covered specifically by a CALL
    // (`calls_name`) edge. This is the numerator companion to `oracle_only_calls`, occurrence-
    // deduped and call-only so both sides of recall range over the same population (finding 1).
    // A non-call edge kind (`references_type` / `uses_macro` / …) marks `matched_occurrences` but
    // NOT this set, so its confirmation can't inflate recall.
    let mut covered_call_occurrences: HashSet<(String, usize, usize)> = HashSet::new();

    // Resolve a SCIP definition `(path, byte-range)` back to one of OUR symbols, scoped to the
    // active `(commit_sha, worktree_id)` checkout (via `symbol_spans_for_path`). Used both by the
    // per-edge join below and by the recall-gap pass, so a `.scip` def in a file rag-rat didn't
    // index (or in another checkout) maps to `None` and is excluded from both — they must agree on
    // what "in the indexed corpus" means.
    let commit_sha = input.commit_sha;
    let worktree_id = input.worktree_id;
    let resolve_symbol = |def_path: &str, def_start: usize, def_end: usize| -> Option<i64> {
        if !symbol_span_cache.borrow().contains_key(def_path) {
            let loaded = store::symbol_spans_for_path(conn, def_path, commit_sha, worktree_id)
                .unwrap_or_default();
            symbol_span_cache.borrow_mut().insert(def_path.to_string(), loaded);
        }
        let cache = symbol_span_cache.borrow();
        let spans = cache.get(def_path)?;
        join::map_definition_to_symbol(spans, def_start, def_end)
    };

    for candidate in &candidates {
        report.edges_examined += 1;
        let Some(occurrences) = index.occurrences_by_path.get(&candidate.source_path) else {
            report.no_occurrence += 1;
            continue;
        };

        // Content-integrity gate (finding 2): the occurrence byte ranges in `occurrences` were
        // derived from the document's CURRENT disk bytes, but this candidate's callee byte range
        // was recorded against `file_sha` (the indexed content). If those disagree, the file
        // drifted between the index build and now — joining the two byte spaces yields a
        // silently-wrong verdict. Skip the candidate and tally it as drifted so `eval` can
        // warn. A file with no computed hash (unreadable) already produced no occurrences,
        // so it never reaches here.
        let disk = disk_sha.get(&candidate.source_path).map(String::as_str);
        if disk != Some(&candidate.file_sha) {
            report.skipped_drifted += 1;
            continue;
        }

        // scip-vs-disk gate (#82 TOCTOU): the index-vs-disk check above only proves the EDGE and
        // the disk agree — both could be the NEW content the watcher reindexed in the
        // lock-free window after the tool built the `.scip` against the OLD content. For a
        // tool-driven run we also hold the disk hash captured at production time; require
        // it to still match disk, so the `.scip`'s occurrence offsets describe the very
        // bytes the join reads. A drifted document is skipped (tallied as drifted), exactly
        // like index-vs-disk drift. A pre-built `--scip` (`production_sha == None`) has no
        // production moment to pin, so it keeps the index-vs-disk gate only.
        if let Some(production_sha) = input.production_sha
            && production_sha.get(&candidate.source_path).map(String::as_str) != disk
        {
            report.skipped_drifted += 1;
            continue;
        }

        // Pre-spawn gate, CALL-SITE side (#83): the two gates above only prove the edge, the
        // disk, and the post-exit production snapshot agree — all three can be the NEW content
        // when a file was edited DURING the subprocess and the watcher reindexed it before the
        // join, while the `.scip` still describes the old bytes. Requiring the join-time indexed
        // sha (`candidate.file_sha`) to equal the snapshot taken BEFORE the spawn asserts no
        // reindex happened across the entire spawn → join window.
        if let Some(pre_spawn) = input.pre_spawn_sha
            && pre_spawn.get(&candidate.source_path).map(String::as_str)
                != Some(candidate.file_sha.as_str())
        {
            report.skipped_drifted += 1;
            continue;
        }

        let verdict = join::classify_edge(&JoinInput {
            callee_start_byte: candidate.callee_start_byte,
            callee_end_byte: candidate.callee_end_byte,
            confidence: &candidate.confidence,
            heuristic_symbol_id: candidate.to_symbol_id,
            occurrences,
            index: &index,
            resolve_symbol: &resolve_symbol,
        });

        let Some(verdict) = verdict else {
            report.no_occurrence += 1;
            continue;
        };

        // scip-vs-disk gate, DEFINITION side (#82 TOCTOU, def-document variant). The call-site gate
        // above only pins the document the occurrence lives in. But an in-corpus verdict also
        // depends on the DEFINITION document: the resolved symbol comes from converting the `.scip`
        // def occurrence's offsets against the def file's join-time disk bytes, then mapping that
        // byte range to a current indexed symbol. If the watcher reindexed the DEF file in the
        // lock-free window, that conversion lands on the wrong bytes and resolves the wrong symbol
        // (a mis-targeted `Upgrade`/`Contradict`, or a false external when it maps to
        // nothing). Pin the def document the same way — its hash is already in the
        // production snapshot. An external symbol with no in-corpus definition entry has no
        // def document to pin, so it's unaffected.
        if let Some(production_sha) = input.production_sha
            && let Some(def) = index.definitions.get(&verdict.scip_symbol)
            && production_sha.get(&def.path).map(String::as_str)
                != disk_sha.get(&def.path).map(String::as_str)
        {
            report.skipped_drifted += 1;
            continue;
        }

        // Pre-spawn gate, DEFINITION side (#83): the same mid-subprocess hole applies to the
        // resolved symbol's defining document — a def file edited during the subprocess and
        // reindexed before the join resolves the byte-converted def range against the wrong
        // symbol while every post-exit gate passes. The join-time indexed sha of the def file
        // must equal the pre-spawn snapshot.
        if let Some(pre_spawn) = input.pre_spawn_sha
            && let Some(def) = index.definitions.get(&verdict.scip_symbol)
            && pre_spawn.get(&def.path).map(String::as_str)
                != indexed_shas.get(&def.path).map(String::as_str)
        {
            report.skipped_drifted += 1;
            continue;
        }

        // Mark EXACTLY the occurrence the verdict matched as covered (finding 4): use the range the
        // join selected (reference-preferred, full containment), not a re-derived start-only match
        // — on overlapping occurrences the two could pick different occurrences. Every
        // verdict marks `matched_occurrences` (so it's never a recall gap); only a CALL
        // (`calls_name`) edge also marks `covered_call_occurrences` (the recall numerator
        // population — finding 1).
        let (occ_start, occ_end) = verdict.matched_occurrence;
        let key = (candidate.source_path.clone(), occ_start, occ_end);
        matched_occurrences.insert(key.clone());
        if candidate.edge_kind == CALL_EDGE_KIND {
            covered_call_occurrences.insert(key);
        }

        join::tally(&mut report, verdict.kind);
        store::write_edge_oracle(conn, input.tool, input.tool_version, &EdgeOracleRow {
            edge_id: candidate.edge_id,
            file_sha: &candidate.file_sha,
            resolved_symbol_id: verdict.resolved_symbol_id,
            scip_symbol: &verdict.scip_symbol,
            kind: verdict.kind,
        })?;
        report.rows_written += 1;
    }
    report.covered_calls = covered_call_occurrences.len() as u64;

    // Moniker pass (#70 phase 3): for every SCIP definition that maps to an in-corpus symbol,
    // record the SCIP symbol string as that logical symbol's moniker. Same drift discipline as the
    // edge join, applied to the DEFINITION document: the def occurrence's byte range was converted
    // against live disk bytes, so the indexed content (the symbol spans we map against) and — for
    // a tool-driven run — the production snapshot must both still match those bytes, or the
    // mapping landed in the wrong coordinate space and would anchor the moniker to the wrong
    // symbol. `local N` symbols never reach here (dropped at parse).
    //
    // SEVERAL defs can containment-map to ONE logical symbol: a struct's fields / consts / enum
    // variants have no symbol row of their own, so their def occurrences map up to the enclosing
    // symbol alongside the symbol's own def. The stored moniker must be DETERMINISTIC — a binding
    // records it at create time and relocation later re-matches it against a fresh run's rows, so
    // a last-writer-wins over HashMap iteration order would silently break relocation whenever the
    // arbitrary winner changed between runs. Pick the best moniker per logical symbol instead:
    // shortest, then lexicographic. SCIP member monikers extend the parent's descriptor chain
    // (`…Struct#field.` vs `…Struct#`), so shortest selects the symbol's own moniker.
    let mut best_monikers: HashMap<i64, &str> = HashMap::new();
    for (scip_symbol, def) in &index.definitions {
        let disk = disk_sha.get(&def.path).map(String::as_str);
        if disk.is_none() || indexed_shas.get(&def.path).map(String::as_str) != disk {
            continue;
        }
        if let Some(production_sha) = input.production_sha
            && production_sha.get(&def.path).map(String::as_str) != disk
        {
            continue;
        }
        // Pre-spawn gate (#83): same mid-subprocess discipline as the verdict join — a def file
        // reindexed during the subprocess maps the moniker to the wrong symbol while every
        // post-exit gate passes. (`disk` equals the join-time indexed sha here, per the first
        // check above.)
        if let Some(pre_spawn) = input.pre_spawn_sha
            && pre_spawn.get(&def.path).map(String::as_str) != disk
        {
            continue;
        }
        let Some(symbol_id) = resolve_symbol(&def.path, def.start_byte, def.end_byte) else {
            continue;
        };
        let Some(logical_symbol_id) = store::logical_symbol_id_for_member(conn, symbol_id)? else {
            continue;
        };
        best_monikers
            .entry(logical_symbol_id)
            .and_modify(|best| {
                if (scip_symbol.len(), scip_symbol.as_str()) < (best.len(), *best) {
                    *best = scip_symbol;
                }
            })
            .or_insert(scip_symbol);
    }
    for (logical_symbol_id, moniker) in &best_monikers {
        store::write_logical_symbol_moniker(
            conn,
            input.tool,
            input.tool_version,
            *logical_symbol_id,
            moniker,
        )?;
        report.monikers_written += 1;
    }

    // Recall gap: in-corpus *reference* occurrences whose symbol resolves inside the corpus but
    // that no edge candidate covered — calls the heuristic never emitted. Two scope filters, both
    // required: (1) the resolver maps each SCIP *definition* back to a scoped DB symbol, so a
    // `.scip` def in a file rag-rat didn't index is excluded; (2) `indexed_paths` restricts the
    // *occurrence* (call site) to a file rag-rat indexed in THIS checkout — no edge candidate
    // can cover a call from an unindexed source file, so those occurrences are out of scope,
    // not misses.
    let indexed_paths = store::indexed_paths_in_scope(conn, commit_sha, worktree_id)?;
    report.oracle_only_calls =
        count_uncovered_calls(&index, &matched_occurrences, &indexed_paths, &resolve_symbol);

    report.status = "Completed".to_string();
    store::record_oracle_run(
        conn,
        input.tool,
        input.tool_version,
        input.commit_sha,
        input.worktree_id,
        &report.status,
        &serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string()),
    )?;

    tx.commit()?;
    Ok(report)
}

/// Hex SHA-256 of a byte slice — matches `files.sha256` (computed as `hex_sha256(fs::read(file))`),
/// so the content-integrity check compares the same hash space the indexer wrote. `pub(crate)` so
/// the production-time snapshot in `produce_scip_with_tool` hashes disk bytes in the SAME space the
/// join's `disk_sha` / the candidate's `file_sha` live in (the scip-vs-disk gate, #82 TOCTOU).
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Count *call-like* reference occurrences whose symbol is defined inside **rag-rat's indexed set**
/// but that no edge covered — the heuristic's missed calls (the recall gap). Definitions and
/// symbols defined outside the corpus don't count (the former aren't calls; the latter are
/// external).
///
/// "Call-like" is necessarily a heuristic: SCIP roles don't mark "call", and the raw set of
/// non-definition occurrences includes imports, type annotations, and field/member references the
/// call graph was never meant to emit — counting those falsely lowers recall. We therefore restrict
/// the denominator to occurrences that plausibly correspond to a CALL edge: (1) not an `Import`
/// occurrence (`use foo::bar` is a reference, not a call), and (2) whose symbol is a *callable*
/// (function / method / constructor) per `scip::symbol_is_callable` — a type/field/module reference
/// is excluded. This is keyed on symbol-kind + role, the most defensible proxy available.
///
/// CRITICAL: "defined inside the index" means defined inside the set of files rag-rat actually
/// indexed, NOT merely "the `.scip` has a definition for it". A `.scip` covers tests, examples,
/// generated code, and dependency sources rag-rat skips; counting calls to those as a recall gap is
/// a false miss (the heuristic was never asked to emit an edge there). So each candidate definition
/// is resolved back to a scoped DB symbol via `resolve_definition` (the same
/// `map_definition_to_symbol` against the active-checkout symbols the join uses); a def that maps
/// to no indexed symbol is dropped, exactly as the join would record no in-corpus upgrade for it.
///
/// CRITICAL (occurrence side, the round-3 fix): the same skip-set logic applies to the file the
/// *call site* lives in. An occurrence in a SOURCE document rag-rat didn't index (excluded
/// test/example/generated source the `.scip` still covers) can NEVER be covered by an edge
/// candidate — `edge_join_candidates` only emits candidates for indexed files — so counting it as
/// an uncovered call is a false miss. `indexed_paths` is the set of files indexed in THIS checkout;
/// an occurrence whose `path` is not in it is dropped before the def-resolution check.
fn count_uncovered_calls(
    index: &ScipIndex,
    matched: &HashSet<(String, usize, usize)>,
    indexed_paths: &HashSet<String>,
    resolve_definition: &dyn Fn(&str, usize, usize) -> Option<i64>,
) -> u64 {
    let mut count = 0u64;
    for (path, occurrences) in &index.occurrences_by_path {
        // The call site must live in a file rag-rat indexed in this checkout; a call from an
        // unindexed source is unreachable by any edge candidate, so it is not a recall gap.
        if !indexed_paths.contains(path) {
            continue;
        }
        for occ in occurrences {
            if occ.is_definition || occ.is_import {
                continue;
            }
            if !scip::symbol_is_callable(&occ.symbol) {
                continue;
            }
            // The symbol must have a SCIP definition that resolves to one of OUR indexed symbols.
            let Some(def) = index.definitions.get(&occ.symbol) else {
                continue;
            };
            if resolve_definition(&def.path, def.start_byte, def.end_byte).is_none() {
                continue;
            }
            if !matched.contains(&(path.clone(), occ.start_byte, occ.end_byte)) {
                count += 1;
            }
        }
    }
    count
}

/// Build the per-kind verdict counts for status/eval, without re-running the join. SCOPED to the
/// active `(commit_sha, worktree_id)` checkout: every count routes through
/// `store::count_edge_oracle_scoped` (the shared `edge_oracle -> edges -> files` scope join), so a
/// sibling worktree's verdicts for the same `(tool, tool_version)` never enter THIS checkout's
/// totals. The total and the per-kind counts cover the same scope by construction — they share one
/// helper — so precision/recall can't mix a cross-checkout numerator with a scoped denominator
/// (the round-3 Codex finding: this was the last unscoped `edge_oracle` read).
pub(crate) fn verdict_counts(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<VerdictCounts> {
    let count = |kind| {
        store::count_edge_oracle_scoped(conn, tool, tool_version, commit_sha, worktree_id, kind)
    };
    Ok(VerdictCounts {
        total: count(None)?,
        upgraded: count(Some(OracleResolutionKind::Upgrade))?,
        resolved_external: count(Some(OracleResolutionKind::ResolvedExternal))?,
        confirmed: count(Some(OracleResolutionKind::Confirm))?,
        contradicted: count(Some(OracleResolutionKind::Contradict))?,
    })
}

/// Per-kind `edge_oracle` counts for the status read.
#[derive(Debug, Clone, Default)]
pub(crate) struct VerdictCounts {
    pub(crate) total: u64,
    pub(crate) upgraded: u64,
    pub(crate) resolved_external: u64,
    pub(crate) confirmed: u64,
    pub(crate) contradicted: u64,
}

/// Heuristic-vs-oracle eval metrics, computed by diffing `edge_oracle` against `edges` for a
/// tool/version. These are the #68 eval acceptance bar. All rates are
/// in `[0, 1]`; denominators of 0 yield `1.0` (vacuously perfect — nothing to get wrong), matching
/// the rest of `eval`'s hit-rate convention.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct OracleEvalMetrics {
    /// Oracle-confirmed fraction of `Exact`/`Syntactic` edges the oracle had an opinion on:
    /// `confirm / (confirm + contradict)`.
    pub precision: f64,
    /// CALL recall: of all in-corpus *calls* the oracle saw, the fraction a `calls_name` edge
    /// covered — `covered_calls / (covered_calls + oracle_only_calls)`. BOTH sides are occurrence-
    /// counted over the same call population (the covered side counts only `calls_name`-edge
    /// occurrences, NOT confirmations on `references_type`/`uses_macro`/… ; the gap counts only
    /// callable occurrences whose def maps in-corpus). The recall *gap* is `1 - recall`. (#81
    /// finding 1: before this, the covered side counted ALL edge kinds while the gap admitted
    /// field/const Term reads, so the two sides measured different populations and recall was
    /// uninterpretable.)
    pub recall: f64,
    /// Fraction of `NameOnly`/`Ambiguous` edges the oracle upgraded to an in-corpus symbol.
    pub name_only_recovery_rate: f64,
    /// Fraction of low-confidence (`NameOnly`/`Ambiguous`) edges the oracle could resolve
    /// (in-corpus upgrade OR resolved-external) — the "oracle-upgradeable fraction of
    /// unresolved". Both numerator and denominator range over the low-confidence population,
    /// so this is bounded by `1.0`.
    pub oracle_upgradeable_fraction: f64,
    /// Raw verdict counts behind the rates, for transparency in `eval --json`.
    pub confirmed: u64,
    pub contradicted: u64,
    pub upgraded: u64,
    pub resolved_external: u64,
    /// The covered side of recall: distinct call occurrences a `calls_name` edge covered.
    pub covered_calls: u64,
    pub oracle_only_calls: u64,
}

/// The two occurrence-counted sides of CALL recall, carried in from the run's [`OracleReport`].
/// Neither is reconstructable from `edge_oracle` alone (the gap is computed from `.scip`
/// occurrences the heuristic never emitted; the covered side is occurrence-deduped and call-only,
/// which the raw per-kind verdict counts can't reproduce). Keeping them paired in one struct
/// prevents a caller from passing the gap without the matching covered count (the bug the old
/// all-kinds SQL covered side hid).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecallCalls {
    /// Distinct call occurrences a `calls_name` edge covered ([`OracleReport::covered_calls`]).
    pub covered: u64,
    /// In-corpus call occurrences no edge covered ([`OracleReport::oracle_only_calls`]).
    pub oracle_only: u64,
}

/// Compute [`OracleEvalMetrics`] from the persisted side tables for a tool/version. The recall
/// call counts are carried in from the [`OracleReport`] of the run via [`RecallCalls`] (they aren't
/// reconstructable from `edge_oracle` alone).
pub(crate) fn eval_metrics(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
    recall_calls: RecallCalls,
) -> anyhow::Result<OracleEvalMetrics> {
    let counts = verdict_counts(conn, tool, tool_version, commit_sha, worktree_id)?;

    // Precision: of Exact/Syntactic edges the oracle judged, how many it confirmed.
    let judged = counts.confirmed + counts.contradicted;
    let precision = ratio(counts.confirmed, judged);

    // NameOnly/Ambiguous recovery: upgrades among low-confidence edges. The NUMERATOR must be
    // counted over the SAME low-confidence population as the denominator — `upgrade` verdicts on
    // `NameOnly`/`Ambiguous` edges — NOT the raw `counts.upgraded`. `classify_resolved` only emits
    // `Upgrade` for non-Exact/Syntactic edges TODAY, but that is a property of the classifier, not
    // of the table: an `Exact`-confidence edge carrying a NULL `to_symbol_id` (a future writer bug)
    // would be `heuristic_resolved_in_corpus == false` and so get an `Upgrade`, and the raw count
    // would then admit an upgrade NOT in the low-confidence denominator → recovery > 1.0. Scoping
    // the numerator to the low-confidence join (exactly as `count_upgradeable_low_confidence` does)
    // keeps it ⊆ the denominator structurally, mirroring the over-1.0 hardening on the upgradeable
    // fraction (#81 finding 6b).
    let low_conf_seen =
        count_low_confidence_with_oracle(conn, tool, tool_version, commit_sha, worktree_id)?;
    let low_conf_upgraded =
        count_low_confidence_upgrades(conn, tool, tool_version, commit_sha, worktree_id)?;
    let name_only_recovery_rate = ratio(low_conf_upgraded, low_conf_seen);

    // Oracle-upgradeable fraction of unresolved: upgrade-OR-external verdicts on low-confidence
    // (`NameOnly`/`Ambiguous`) edges, over all such candidates. BOTH numerator and denominator must
    // range over the SAME population — low-confidence edges. The numerator must NOT use the raw
    // `counts.{upgraded,resolved_external}`: those tally every `edge_oracle` row of that kind,
    // including `resolved-external` verdicts on already-`Exact`/`Syntactic` edges, which are not in
    // the denominator — counting them let the fraction exceed 1.0. Scoping the numerator to the
    // low-confidence join keeps it ⊆ the denominator, so the fraction is bounded by 1.0 (at most
    // one verdict per edge).
    let unresolved_total = count_unresolved_candidates(conn, commit_sha, worktree_id)?;
    let upgradeable_low_conf =
        count_upgradeable_low_confidence(conn, tool, tool_version, commit_sha, worktree_id)?;
    let oracle_upgradeable_fraction = ratio(upgradeable_low_conf, unresolved_total);

    // CALL recall: covered call occurrences vs. all in-corpus call occurrences the oracle saw
    // (covered + oracle-only). BOTH sides are occurrence-counted over the call population, carried
    // in from the run (`covered` counts only `calls_name`-edge occurrences, deduped; `oracle_only`
    // counts only callable occurrences whose def maps in-corpus). They measure the same population,
    // so the ratio is a real recall — unlike the old `confirm+contradict+upgrade` covered side,
    // which counted verdicts on every edge kind (type refs, macros, …) against a call-only gap.
    let RecallCalls { covered: covered_calls, oracle_only: oracle_only_calls } = recall_calls;
    let recall = ratio(covered_calls, covered_calls + oracle_only_calls);

    Ok(OracleEvalMetrics {
        precision,
        recall,
        name_only_recovery_rate,
        oracle_upgradeable_fraction,
        confirmed: counts.confirmed,
        contradicted: counts.contradicted,
        upgraded: counts.upgraded,
        resolved_external: counts.resolved_external,
        covered_calls,
        oracle_only_calls,
    })
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 { 1.0 } else { numerator as f64 / denominator as f64 }
}

/// Count edges with a low-confidence heuristic resolution (`NameOnly`/`Ambiguous`) that the oracle
/// produced a verdict for (i.e. have an `edge_oracle` row), scoped to the active checkout. Built on
/// the shared `store::edge_oracle_scope_join()` (the `?1..?4 = tool/version/commit_sha/worktree_id`
/// scope) so the scope predicate is never re-spelled here — only the extra confidence filter is
/// appended.
fn count_low_confidence_with_oracle(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*){} AND edges.confidence IN ('NameOnly', 'Ambiguous')",
            store::edge_oracle_scope_join()
        ),
        rusqlite::params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Count `upgrade` verdicts on low-confidence (`NameOnly`/`Ambiguous`) edges — the numerator of
/// `name_only_recovery_rate`, scoped to the SAME low-confidence population as its denominator
/// (`count_low_confidence_with_oracle`), so the rate can never exceed 1.0. The `edges.confidence`
/// filter is what guards against an `Exact`-with-NULL-`to_symbol_id` writer bug recreating an
/// over-1.0 rate (the raw per-kind `counts.upgraded` would admit such an upgrade; this won't).
/// Built on the shared `store::edge_oracle_scope_join()`, like the upgradeable-fraction numerator.
fn count_low_confidence_upgrades(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*){} AND edge_oracle.kind = 'upgrade' AND edges.confidence IN \
             ('NameOnly', 'Ambiguous')",
            store::edge_oracle_scope_join()
        ),
        rusqlite::params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Count low-confidence (`NameOnly`/`Ambiguous`) edges the oracle could upgrade — an `upgrade` (an
/// in-corpus recovery) OR a `resolved-external` (placed to a dependency) verdict. This is the
/// numerator of the oracle-upgradeable fraction, scoped to the SAME low-confidence population the
/// denominator (`count_unresolved_candidates`) counts, so the fraction can never exceed 1.0. There
/// is at most one `edge_oracle` row per edge (PK `(edge_id, tool, tool_version)`), so this is a
/// subset count, not a sum that could double-count. Built on the shared
/// `store::edge_oracle_scope_join()` so it stays ⊆ the scoped denominator by construction.
fn count_upgradeable_low_confidence(
    conn: &Connection,
    tool: OracleTool,
    tool_version: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*){} AND edge_oracle.kind IN ('upgrade', 'resolved-external') AND \
             edges.confidence IN ('NameOnly', 'Ambiguous')",
            store::edge_oracle_scope_join()
        ),
        rusqlite::params![tool.as_db_str(), tool_version, commit_sha, worktree_id],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Count all unresolved/low-confidence edge candidates (carrying a callee range) — the denominator
/// for the oracle-upgradeable fraction. These are the rows the oracle *could* have helped.
///
/// SCOPE (load-bearing): restricted to the active `(commit_sha, worktree_id)` checkout via the
/// `edges -> files` join, mirroring `edge_join_candidates`. Without it a low-confidence edge in
/// ANOTHER worktree would inflate this denominator and dilute the current run's fraction — the
/// numerator (`count_upgradeable_low_confidence`) is already scoped, so the two must match.
fn count_unresolved_candidates(
    conn: &Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        &format!(
            "
        SELECT COUNT(*)
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.callee_start_byte IS NOT NULL
          AND edges.confidence IN ('NameOnly', 'Ambiguous')
          AND {scope}
        ",
            scope = store::active_checkout_file_predicate("?1", "?2"),
        ),
        rusqlite::params![commit_sha, worktree_id],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}
