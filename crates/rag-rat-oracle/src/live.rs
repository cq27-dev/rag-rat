//! The live oracle pass (#74 slice 2 / #534): wire the resident LSP client (slice 1's `lsp`
//! substrate) to the watcher's maintenance pass and WRITE its per-callee verdicts into the
//! `edge_oracle` seam, under the distinct [`OracleTool::RaLsp`] tool id.
//!
//! Invariants this module upholds (settled on #74):
//!
//! - **Tool identity.** Live rows carry `ra-lsp` + the session's probed `rust-analyzer --version`
//!   (re-probed at every spawn). Reusing the batch `rust-analyzer` id is rejected: a live
//!   `oracle_runs` row under the batch id would make `auto_run_decision` see the index as
//!   always-fresh and silently stop the whole-checkout batch pass.
//! - **Moniker source.** A live verdict's `scip_symbol` is the resolved target's BATCH moniker from
//!   `logical_symbol_monikers` — byte-identical to what the batch pass writes, so clone-collapse
//!   (#275) and moniker-anchored memory relocation treat live and batch rows as one evidence set
//!   with zero consumer changes. When the target has no batch moniker (batch never ran, or the def
//!   is outside its definitions) the fallback is a `local ra-lsp-<hash>` SCIP-local sentinel:
//!   `is_local_symbol` skips it before the conflict logic, so it can never poison a batch moniker,
//!   while the row's `resolved_symbol_id` still upgrades the edge tier. The sentinel's hash is
//!   content-derived (callsite path + callee span), so it is STABLE across reindexes — a no-op
//!   re-pass is a same-value upsert and does not churn the refine cache. The LSP
//!   `textDocument/moniker` string is NEVER persisted: it never equals the SCIP moniker, and the
//!   cross-tool conflict-drop would destroy batch collapse coverage on any co-covered span. Live
//!   only READS the moniker table.
//! - **Currency.** The verdict rows + the backing `oracle_runs` row commit in ONE transaction — the
//!   per-tool currency gate (`callee_moniker_current_clause`) must never see live rows without a
//!   backing run. A run row is recorded only for a pass that wrote ≥1 row. There is NO
//!   authoritative clear: live's scope is only this pass's changed files (the batch pass owns
//!   whole-checkout authority).
//! - **Refine cache.** When a live write CHANGES an existing row's `scip_symbol` for an UNCHANGED
//!   `file_sha`, the scip-mode clone refinements that consulted the old evidence are invalidated
//!   (nothing in the refinement key folds oracle content — the same reasoning as the batch hook in
//!   `run_in_tx`). Same-value upserts skip the invalidation.
//! - **Drift.** A verdict is written only when the callsite's disk bytes still hash to the indexed
//!   `file_sha`, and the definition document's disk bytes still hash to its indexed `files.sha256`
//!   — the live analog of the batch join's index-vs-disk content gate. Anything else is skipped,
//!   never mis-joined.
//!
//! Out of scope (later slices): crash/version/warm-up hardening and server→client request
//! handlers (#535), non-Rust backends (#536), `resolved-external` verdicts (a live def outside
//! the checkout has no batch-interchangeable SCIP symbol string, so it is skipped, not written).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use rag_rat_base::hash::hex_sha256;
use rusqlite::Connection;
use serde::Serialize;

use super::lsp::client::LspClient;
use super::lsp::position::LineIndex;
use super::store::{self, EdgeOracleRow};
use super::{OracleResolutionKind, OracleTool, ToolAvailability, ToolManifest, join};

/// A resident live-oracle language-server session: the spawned client, the `tool_version` its
/// rows are stamped with, and the checkout root URI documents are opened under. Owned by the
/// watcher's pass worker across passes; spawned lazily on the first eligible pass and shut down
/// after an idle window (both driven by the watcher).
pub struct LiveOracleSession {
    client: LspClient,
    tool_version: String,
    root_uri: String,
    last_used_ms: i64,
}

impl LiveOracleSession {
    /// Probe `rust-analyzer` and spawn the resident client for `checkout_root`, or `None` when
    /// the tool is unavailable / the session can't be established — the same degrade-quietly UX
    /// as a missing embedding model or batch tool (never an error, never a failed pass). The
    /// version is RE-probed at every spawn so `tool_version` always names the binary this
    /// session's verdicts came from.
    pub fn spawn(checkout_root: &Path, now_ms: i64) -> Option<Self> {
        let manifest = ToolManifest::for_tool(OracleTool::RaLsp);
        let ToolAvailability::Available { version, .. } = manifest.probe() else {
            return None;
        };
        let root_uri = root_uri_for(checkout_root);
        let mut client = LspClient::spawn(manifest.program, &[]).ok()?;
        client.initialize(&root_uri).ok()?;
        Some(Self { client, tool_version: version, root_uri, last_used_ms: now_ms })
    }

    /// Test seam: a session over an injected (fake-server) client transport. Runs the
    /// `initialize` handshake exactly like [`Self::spawn`] so the negotiated encoding is real.
    #[cfg(test)]
    pub(crate) fn from_client(mut client: LspClient, tool_version: &str, root_uri: &str) -> Self {
        client.initialize(root_uri).expect("test server completes the handshake");
        Self {
            client,
            tool_version: tool_version.to_string(),
            root_uri: root_uri.to_string(),
            last_used_ms: 0,
        }
    }

    pub fn tool_version(&self) -> &str {
        &self.tool_version
    }

    pub fn last_used_ms(&self) -> i64 {
        self.last_used_ms
    }

    /// Whether the session has gone `idle_ms` without a pass — the watcher's cue to shut it
    /// down (an idle server shouldn't hold rust-analyzer's resident memory).
    pub fn idle_for(&self, now_ms: i64, idle_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_used_ms) > idle_ms as i64
    }

    /// Graceful LSP teardown (`shutdown` + `exit`); the client's Drop hard-kills as the
    /// fallback. Consumes the session.
    pub fn shutdown(self) {
        let mut this = self;
        let _ = this.client.shutdown();
    }

    fn touch(&mut self, now_ms: i64) {
        self.last_used_ms = now_ms;
    }
}

/// Inputs for one live oracle pass.
pub struct LivePassInput<'a> {
    /// The active checkout the just-reindexed files (and their edge candidates) are scoped to.
    pub commit_sha: &'a str,
    pub worktree_id: &'a str,
    /// The checkout root: document URIs are `root_uri/path`, and target bytes are read from
    /// `checkout_root/path`.
    pub checkout_root: &'a Path,
    /// Repo-relative paths the pass may resolve (the maintenance pass's changed set plus any
    /// backlog), RUST files only — the ra-lsp backend's language. Processed in order; whatever
    /// the request budget doesn't cover rides `LivePassReport::unfinished_paths`.
    pub worklist: &'a [String],
    /// Cap on `textDocument/definition` requests this pass may issue.
    pub max_requests: u64,
    /// Unix-epoch ms the maintenance pass began — stamped as the run's `started_at`.
    pub started_at_ms: i64,
}

/// Outcome of one live pass, mirroring `OracleReport`'s shape. Persisted opaquely as the run
/// row's `stats_json` (when a run is recorded).
#[derive(Debug, Clone, Default, Serialize)]
pub struct LivePassReport {
    /// Worklist files carrying at least one edge candidate (the population the pass works over).
    pub files_with_candidates: u64,
    /// Files whose callees were all sent to the server (not deferred by the budget, not skipped
    /// for drift).
    pub files_resolved: u64,
    /// `textDocument/definition` requests issued.
    pub requests_used: u64,
    /// `edge_oracle` rows written this pass.
    pub rows_written: u64,
    pub upgraded: u64,
    pub confirmed: u64,
    pub contradicted: u64,
    /// Candidates/definitions skipped for content drift (callsite or definition document).
    pub skipped_drifted: u64,
    /// Definitions skipped as out-of-corpus for live purposes: target outside the checkout root,
    /// in an unindexed file, or mapping to no indexed symbol. Live writes no `resolved-external`
    /// rows (see the module docs).
    pub skipped_external: u64,
    /// Callees the server left unresolved (a `null` definition — e.g. still indexing).
    pub unresolved: u64,
    /// Whether the scip-mode refine cache was invalidated (a row's `scip_symbol` changed under
    /// an unchanged `file_sha`).
    pub refinements_invalidated: bool,
    /// Whether an `oracle_runs` row backs this pass (when `rows_written > 0` or a version
    /// migration made the new `tool_version` current).
    pub run_recorded: bool,
    /// Whether prior-version live verdicts were migrated to the session's `tool_version` (a
    /// `rust-analyzer` upgrade detected at spawn).
    pub version_migrated: bool,
    /// Worklist paths the request budget didn't reach — the caller's backlog into the next pass.
    #[serde(skip)]
    pub unfinished_paths: Vec<String>,
    pub status: String,
}

/// Run one live oracle pass: resolve the worklist's callees through the resident client and
/// write the verdicts + a backing run row in ONE transaction. Never fails the maintenance pass
/// for an LSP-side problem — a dead/wedged server aborts the remaining worklist (reported in
/// `status`) while the verdicts already gathered still commit; DB failures are the only `Err`.
pub fn live_oracle_pass(
    conn: &Connection,
    session: &mut LiveOracleSession,
    input: &LivePassInput<'_>,
) -> anyhow::Result<LivePassReport> {
    let mut report = LivePassReport::default();
    let tool = OracleTool::RaLsp;
    // The batch tool whose monikers live copies (rust-analyzer for ra-lsp) — always `Some` for a
    // live tool; the join is vacuous without it.
    let Some(moniker_source) = tool.batch_moniker_source() else {
        anyhow::bail!("live_oracle_pass requires a live (non-batch) tool");
    };

    let candidates = store::edge_join_candidates_for_paths(
        conn,
        input.commit_sha,
        input.worktree_id,
        input.worklist,
    )?;
    let mut by_path: HashMap<&str, Vec<&store::EdgeJoinCandidate>> = HashMap::new();
    for candidate in &candidates {
        by_path.entry(candidate.source_path.as_str()).or_default().push(candidate);
    }
    report.files_with_candidates = by_path.len() as u64;

    let tx = conn.unchecked_transaction()?;
    let mut refinements_stale = false;
    // Version transition: a respawn probing a NEW `rust-analyzer --version` must not strand the
    // prior version's still-current verdicts — the first partial pass's run row would become the
    // latest for the whole checkout and gate every prior-version verdict out of currency,
    // collapsing live coverage to the handful of files this pass revisits. Migrate the rows
    // (content-addressed, so `file_sha` still gates drift; SCOPED to this checkout so a sibling
    // worktree's rows and currency stay untouched) and invalidate the scip refinements whose
    // evidence just changed hands. The transition run is recorded whenever the version moved —
    // even with zero rows moved — because the session's binary IS the new version and the
    // currency gate must start selecting it (a sibling's migration may already have moved the
    // shared rows). Migration COPIES rather than relabels: identical-content siblings share the
    // old row and still need it under their old-version currency.
    let mut version_migrated = false;
    if let Some(old_version) =
        store::latest_run_tool_version(conn, tool, input.commit_sha, input.worktree_id)?
        && old_version != session.tool_version()
    {
        let migration = store::migrate_live_verdicts_to_version(
            conn,
            tool,
            &old_version,
            session.tool_version(),
            input.commit_sha,
            input.worktree_id,
        )?;
        match migration {
            store::LiveVersionMigration::Copied(moved) => {
                version_migrated = true;
                if moved > 0 {
                    refinements_stale = true;
                }
            },
            store::LiveVersionMigration::BlockedByContentCollision => {
                // The content-key PK cannot hold both checkouts' different bytes under the new
                // version. Do NOT process/write with that version or record its currency: preserve
                // the active checkout's old-version coverage and retry after the collision clears.
                report.unfinished_paths = input.worklist.to_vec();
                report.status = "VersionMigrationBlocked".to_string();
                tx.commit()?;
                session.touch(rag_rat_base::time::now_ms());
                return Ok(report);
            },
        }
    }
    let mut aborted: Option<String> = None;
    // Per-pass caches: definition-file disk bytes / indexed sha / symbol spans, keyed by
    // repo-relative path (the LSP returns a bounded set of def paths, so these stay small — the
    // whole-checkout sha map is deliberately NOT loaded).
    let mut def_bytes: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut def_indexed_sha: HashMap<String, Option<String>> = HashMap::new();
    let mut def_spans: HashMap<String, Vec<store::SymbolSpan>> = HashMap::new();
    let logical_cache: std::cell::RefCell<HashMap<i64, Option<i64>>> =
        std::cell::RefCell::new(HashMap::new());
    let mut moniker_cache: HashMap<i64, Option<String>> = HashMap::new();

    'files: for (position, path) in input.worklist.iter().enumerate() {
        let Some(callees) = by_path.get(path.as_str()) else {
            continue;
        };
        // Budget gate: nothing left → every candidate-bearing path from here rides the backlog.
        let remaining = input.max_requests.saturating_sub(report.requests_used) as usize;
        if remaining == 0 {
            report.unfinished_paths.extend(
                input.worklist[position..]
                    .iter()
                    .filter(|p| by_path.contains_key(p.as_str()))
                    .cloned(),
            );
            break 'files;
        }

        // Callsite drift gate: the server resolves the DIRTY disk bytes, so those bytes must
        // still hash to the indexed `file_sha` the candidates were built from — a mid-pass edit
        // makes the indexed callee ranges point at the wrong content. Skip, never mis-resolve.
        let Ok(bytes) = std::fs::read(input.checkout_root.join(path)) else {
            report.skipped_drifted += callees.len() as u64;
            continue;
        };
        if hex_sha256(&bytes) != callees[0].file_sha {
            report.skipped_drifted += callees.len() as u64;
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            report.skipped_drifted += callees.len() as u64;
            continue;
        };

        // Covered-skip continuation: an edge with a CURRENT live verdict (same tool_version +
        // file_sha, FULL content key — two edges may share a callee token, and a start-byte key
        // would let one written row starve the other) is NEVER re-resolved for the same
        // content — the budget only ever spends on unverdicted edges. A file a prior pass
        // truncated therefore resumes exactly where it stopped, and a fully-covered file costs
        // nothing (the first-file exemption this replaces let a huge generated file blow the
        // budget). Re-resolution happens naturally on content change (a new `file_sha`
        // un-covers the edge) — e.g. upgrading a sentinel to a batch moniker — or on a version
        // migration, so a stale sentinel costs only cosmetics: the batch row carries the real
        // evidence for any span both tools cover.
        let covered = store::live_covered_edges_for_path(
            conn,
            tool,
            session.tool_version(),
            path,
            &callees[0].file_sha,
            input.commit_sha,
            input.worktree_id,
        )?;
        let is_covered = |c: &store::EdgeJoinCandidate| {
            covered.contains(&(
                c.source_start_byte,
                c.source_end_byte,
                c.callee_start_byte,
                c.callee_end_byte,
                c.edge_kind.clone(),
            ))
        };
        let unverdicted: Vec<&store::EdgeJoinCandidate> =
            callees.iter().copied().filter(|c| !is_covered(c)).collect();
        if unverdicted.is_empty() {
            continue;
        }
        let (to_resolve, deferred) = unverdicted.split_at(remaining.min(unverdicted.len()));
        let deferred_count = deferred.len();

        // Join on exactly one separator: a root URI already ending in `/` (a filesystem-root `/`
        // or a drive root `C:\`) would otherwise produce `file:////…` / `file:///C://…`, which
        // rust-analyzer's canonical single-slash form then fails to prefix-match (#534 review).
        // `strip_suffix` drops at most ONE slash — `trim_end_matches('/')` would eat `file:///`
        // down to `file:`.
        let root = session.root_uri.strip_suffix('/').unwrap_or(&session.root_uri);
        let uri = format!("{root}/{}", encode_uri_path(path));
        let starts: Vec<usize> =
            to_resolve.iter().filter_map(|c| usize::try_from(c.callee_start_byte).ok()).collect();
        report.requests_used += starts.len() as u64;
        let resolved = match session.client.resolve_definitions(&uri, "rust", &text, &starts) {
            Ok(resolved) => resolved,
            Err(err) => {
                // A dead/wedged server: keep what earlier files produced, REQUEUE this file and
                // every candidate-bearing path after it (the watcher rides them into the next
                // pass and replaces the session), and never fail the maintenance pass over it
                // (#535 hardens this further).
                aborted = Some(err.to_string());
                report.unfinished_paths.extend(
                    input.worklist[position..]
                        .iter()
                        .filter(|p| by_path.contains_key(p.as_str()))
                        .cloned(),
                );
                break 'files;
            },
        };
        // Fully sent only when nothing was budget-deferred: `files_resolved` documents "every
        // callee of this file was sent", so a truncated file must not inflate it (the field
        // rides `oracle_runs.stats_json` into status consumers).
        if deferred_count == 0 {
            report.files_resolved += 1;
        } else {
            // The budget cut this file short; it rides the backlog and resumes at the deferred
            // callees (the covered-skip above makes that resume exact).
            report.unfinished_paths.push(path.clone());
        }
        // Definition bytes are cached ONLY within this file's batch: a def file edited between
        // two source files' LSP requests would otherwise be hashed + position-converted from a
        // stale snapshot, defeating the definition-side drift gate. (The indexed sha + symbol
        // spans stay cached: the write lock pins the index for the whole pass.)
        def_bytes.clear();

        for (candidate, definition) in to_resolve.iter().zip(resolved.iter()) {
            let Some((target_uri, target_range)) = definition else {
                report.unresolved += 1;
                continue;
            };
            // The definition must land inside this checkout: an external target (a dependency
            // source outside the root) has no indexed symbol and no batch-interchangeable
            // moniker, so live writes nothing for it.
            let Some(def_path) = path_from_uri(&session.root_uri, target_uri) else {
                report.skipped_external += 1;
                continue;
            };
            // Definition-side drift gate: the LSP range converts against the def file's CURRENT
            // disk bytes, so those must still be the indexed bytes the symbol spans came from.
            let def_disk = def_bytes
                .entry(def_path.clone())
                .or_insert_with(|| std::fs::read(input.checkout_root.join(&def_path)).ok())
                .clone();
            let Some(def_disk) = def_disk else {
                report.skipped_drifted += 1;
                continue;
            };
            let indexed_sha = match def_indexed_sha.entry(def_path.clone()) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => entry
                    .insert(store::indexed_file_sha_for_path(
                        conn,
                        &def_path,
                        input.commit_sha,
                        input.worktree_id,
                    )?)
                    .clone(),
            };
            match indexed_sha {
                // Not indexed in this checkout — nothing live can map to it.
                None => {
                    report.skipped_external += 1;
                    continue;
                },
                Some(sha) if sha == hex_sha256(&def_disk) => {},
                Some(_) => {
                    report.skipped_drifted += 1;
                    continue;
                },
            }
            let index = LineIndex::new(&def_disk, session.client.encoding());
            let (Some(def_start), Some(def_end)) = (
                index.byte_at_position(target_range.start),
                index.byte_at_position(target_range.end),
            ) else {
                report.skipped_drifted += 1;
                continue;
            };
            let spans = match def_spans.entry(def_path.clone()) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(store::symbol_spans_for_path(
                    conn,
                    &def_path,
                    input.commit_sha,
                    input.worktree_id,
                )?),
            };
            let Some(symbol_id) = join::map_definition_to_symbol(spans, def_start, def_end) else {
                // The def is in an indexed file but under no indexed symbol (macro-generated
                // code, a symbol kind without a row): nothing trustworthy to write.
                report.skipped_external += 1;
                continue;
            };

            // Resolve the logical ids of the heuristic + compiler targets up front, propagating
            // a DB failure instead of swallowing it to `None` — a swallowed error would degrade
            // a real Confirm (same logical symbol) into a Contradict and corrupt precision while
            // later writes still succeed (#534 review). The closure then reads the warmed cache.
            let warm_logical = |id: i64| -> anyhow::Result<()> {
                if !logical_cache.borrow().contains_key(&id) {
                    let logical = store::logical_symbol_id_for_member(conn, id)?;
                    logical_cache.borrow_mut().insert(id, logical);
                }
                Ok(())
            };
            warm_logical(symbol_id)?;
            if let Some(heuristic_id) = candidate.to_symbol_id {
                warm_logical(heuristic_id)?;
            }
            let logical_symbol_of =
                |id: i64| -> Option<i64> { logical_cache.borrow().get(&id).copied().flatten() };
            let kind = join::classify_in_corpus(
                &candidate.confidence,
                candidate.to_symbol_id,
                symbol_id,
                &logical_symbol_of,
            );

            // Moniker: the target's batch moniker verbatim, else the content-stable local
            // sentinel (module docs — NEVER the LSP moniker string). A DB failure propagates.
            if let std::collections::hash_map::Entry::Vacant(slot) = moniker_cache.entry(symbol_id)
            {
                slot.insert(store::batch_moniker_for_symbol(conn, symbol_id, moniker_source)?);
            }
            let scip_symbol = moniker_cache
                .get(&symbol_id)
                .and_then(Clone::clone)
                .unwrap_or_else(|| live_local_sentinel(&candidate.source_path, candidate));

            let row = EdgeOracleRow {
                source_path: &candidate.source_path,
                source_start_byte: candidate.source_start_byte,
                source_end_byte: candidate.source_end_byte,
                callee_start_byte: candidate.callee_start_byte,
                callee_end_byte: candidate.callee_end_byte,
                edge_kind: &candidate.edge_kind,
                file_sha: &candidate.file_sha,
                resolved_symbol_id: Some(symbol_id),
                scip_symbol: &scip_symbol,
                kind,
            };
            // Refine-cache interplay: a CHANGED scip_symbol under an UNCHANGED file_sha moves
            // oracle evidence nothing in the refinement key folds — invalidate (same-value
            // upserts deliberately skip).
            if let Some((old_sha, old_symbol)) =
                store::existing_verdict_scip_symbol(conn, tool, session.tool_version(), &row)?
                && old_sha == candidate.file_sha
                && old_symbol != scip_symbol
            {
                refinements_stale = true;
            }
            store::write_edge_oracle(conn, tool, session.tool_version(), &row)?;
            report.rows_written += 1;
            match kind {
                OracleResolutionKind::Upgrade => report.upgraded += 1,
                OracleResolutionKind::Confirm => report.confirmed += 1,
                OracleResolutionKind::Contradict => report.contradicted += 1,
                // Live never emits ResolvedExternal (out-of-corpus defs are skipped above).
                OracleResolutionKind::ResolvedExternal => {},
            }
        }
    }

    // A run row is recorded for a pass that WROTE verdicts OR migrated the tool version: the
    // migration is what makes the new `tool_version` current for the currency gate, and without
    // a backing run row the migrated rows stay invisible (the gate keys on the LATEST run).
    report.version_migrated = version_migrated;
    if report.rows_written > 0 || version_migrated {
        report.run_recorded = true;
        report.status = match &aborted {
            Some(err) => format!("Aborted: {err}"),
            None if report.rows_written == 0 => "VersionMigrated".to_string(),
            None if report.unfinished_paths.is_empty() => "Completed".to_string(),
            None => "BudgetExhausted".to_string(),
        };
        if refinements_stale {
            rag_rat_clones::refine::cache::invalidate_scip_refinements(conn)?;
            report.refinements_invalidated = true;
        }
        // Serialize AFTER every report field is final (the invalidation flag above rides the
        // same stats_json the status surface reads).
        store::record_oracle_run_at(
            conn,
            tool,
            session.tool_version(),
            input.commit_sha,
            input.worktree_id,
            input.started_at_ms,
            &report.status,
            &serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string()),
        )?;
    } else {
        report.status = match &aborted {
            Some(err) => format!("Aborted: {err}"),
            None if report.unfinished_paths.is_empty() => "NoVerdicts".to_string(),
            None => "BudgetExhausted".to_string(),
        };
    }
    tx.commit()?;
    // Stamp usage at COMPLETION, not the pass's start: a pass longer than the idle window must
    // not read as idle on the next pass (that would force a needless respawn + warm-up).
    session.touch(rag_rat_base::time::now_ms());
    Ok(report)
}

/// The content-stable SCIP-local sentinel a live verdict carries when the resolved symbol has no
/// batch moniker: `local ra-lsp-<hash>` keyed by the callsite (path + callee span), so a no-op
/// re-pass reproduces the SAME string (a same-value upsert — no refine-cache churn). Parses as a
/// SCIP local symbol, so `is_local_symbol` drops it before `current_callee_monikers`' conflict
/// logic and it can never poison a batch moniker.
fn live_local_sentinel(source_path: &str, candidate: &store::EdgeJoinCandidate) -> String {
    let hash = hex_sha256(
        format!("{}:{}:{}", source_path, candidate.callee_start_byte, candidate.callee_end_byte)
            .as_bytes(),
    );
    format!("local ra-lsp-{}", &hash[..16])
}

/// The `file://` URI of a checkout root — the base every document URI hangs off. Encodes any
/// non-URI-path byte as %XX (spaces, non-ASCII); `/` is preserved. Two Windows forms keep the
/// shape rust-analyzer returns canonically (so `path_from_uri`'s literal prefix match agrees,
/// #534 review): a drive prefix (`C:`) keeps its colon unencoded (`file:///C:/repo`), and a UNC
/// path (`\\server\share`) becomes an authority URI (`file://server/share/…`, no extra slash).
fn root_uri_for(checkout_root: &Path) -> String {
    let lossy = checkout_root.to_string_lossy().replace('\\', "/");
    // `Path::canonicalize` on Windows returns verbatim paths. Normalize those BEFORE the UNC
    // branch: `\\?\C:\repo` becomes `//?/C:/repo` after separator replacement and would otherwise
    // be mistaken for an authority named `?`; verbatim UNC uses the `\\?\UNC\server\share` form.
    let lossy = if let Some(unc) = lossy.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else if let Some(drive) = lossy.strip_prefix("//?/") {
        drive.to_string()
    } else {
        lossy
    };
    // UNC: `//server/share/…` → `file://server/share/…` (the server is the URI authority, not a
    // path segment). Encode each segment but keep the `/` separators.
    if let Some(unc) = lossy.strip_prefix("//") {
        return format!("file://{}", encode_uri_path(unc));
    }
    let with_lead = if lossy.starts_with('/') { lossy } else { format!("/{lossy}") };
    let bytes = with_lead.as_bytes();
    // Drive prefix: `/C:/…` after the leading-slash normalization.
    let drive_len =
        if bytes.len() >= 4 && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' { 3 } else { 0 };
    format!("file://{}{}", &with_lead[..drive_len], encode_uri_path(&with_lead[drive_len..]))
}

/// Percent-encode a URI path: keep the RFC 3986 unreserved set plus `/`; %XX-encode the rest.
fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' =>
                out.push(byte as char),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            },
        }
    }
    out
}

/// The repo-relative path of a `file://` document URI under `root_uri`, percent-decoded; `None`
/// when the URI points outside the checkout (an external dependency — nothing live can write).
fn path_from_uri(root_uri: &str, uri: &str) -> Option<String> {
    let rest = uri.strip_prefix(root_uri)?;
    // Require a path boundary after a non-root checkout URI. A lexical prefix such as
    // `file:///work/repo` must not accept `file:///work/repository/x.rs` as `sitory/x.rs`.
    // Slash-terminated roots (`file:///`, `file:///C:/`) already supply that boundary.
    let rest = if root_uri.ends_with('/') { rest } else { rest.strip_prefix('/')? };
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_uri_encodes_spaces_and_preserves_slashes() {
        assert_eq!(root_uri_for(Path::new("/tmp/my repo")), "file:///tmp/my%20repo");
        assert_eq!(root_uri_for(Path::new("/plain/path")), "file:///plain/path");
    }

    #[test]
    fn root_uri_keeps_a_windows_drive_colon_verbatim() {
        // rust-analyzer returns `file:///C:/repo/…` canonically; encoding the drive colon
        // (`file:///C%3A/repo`) would break `path_from_uri`'s prefix match and class every
        // in-repo definition as external (#534 review).
        let root = root_uri_for(Path::new(r"C:\repo"));
        assert_eq!(root, "file:///C:/repo");
        assert_eq!(path_from_uri(&root, "file:///C:/repo/src/lib.rs"), Some("src/lib.rs".into()));
        // A drive root with a space still encodes the non-drive part.
        assert_eq!(root_uri_for(Path::new(r"D:\my repo")), "file:///D:/my%20repo");
    }

    #[test]
    fn root_uri_strips_windows_verbatim_prefixes() {
        // Canonical Windows paths carry `\\?\`; rust-analyzer emits ordinary canonical file URLs.
        assert_eq!(root_uri_for(Path::new(r"\\?\C:\repo")), "file:///C:/repo");
        assert_eq!(
            root_uri_for(Path::new(r"\\?\UNC\server\share\repo")),
            "file://server/share/repo"
        );
    }

    #[test]
    fn root_uri_maps_a_unc_path_to_an_authority_url() {
        // `\\server\share\repo` → `file://server/share/repo` (authority form), matching the
        // canonical URI rust-analyzer returns; a naive join would emit `file:////server/...`
        // and misclass every in-repo definition as external (#534 review).
        let root = root_uri_for(Path::new(r"\\server\share\repo"));
        assert_eq!(root, "file://server/share/repo");
        assert_eq!(
            path_from_uri(&root, "file://server/share/repo/src/a.rs"),
            Some("src/a.rs".into())
        );
    }

    #[test]
    fn document_join_uses_one_separator_at_a_slash_terminated_root() {
        // A checkout AT the filesystem root (`/`) or a drive root (`C:\`) yields a root URI
        // ending in `/`; joining a document must not double the separator or the server's
        // canonical URI won't prefix-match (#534 review).
        let root = root_uri_for(Path::new("/"));
        assert_eq!(root, "file:///");
        let doc = format!(
            "{}/{}",
            root.strip_suffix('/').unwrap_or(&root),
            encode_uri_path("src/lib.rs")
        );
        assert_eq!(doc, "file:///src/lib.rs");
        assert_eq!(path_from_uri(&root, &doc), Some("src/lib.rs".into()));

        let drive = root_uri_for(Path::new(r"C:\"));
        assert_eq!(drive, "file:///C:/");
        let doc = format!(
            "{}/{}",
            drive.strip_suffix('/').unwrap_or(&drive),
            encode_uri_path("src/lib.rs")
        );
        assert_eq!(doc, "file:///C:/src/lib.rs");
        assert_eq!(path_from_uri(&drive, &doc), Some("src/lib.rs".into()));
    }

    #[test]
    fn path_from_uri_decodes_and_rejects_external() {
        let root = "file:///tmp/my%20repo";
        assert_eq!(
            path_from_uri(root, "file:///tmp/my%20repo/src/a file.rs"),
            Some("src/a file.rs".to_string())
        );
        assert_eq!(path_from_uri(root, "file:///elsewhere/src/lib.rs"), None);
        assert_eq!(path_from_uri(root, "file:///tmp/my%20repository/src/lib.rs"), None);
    }

    #[test]
    fn document_uri_encodes_the_relative_path() {
        // A repo path carrying URI-reserved bytes (`#`, `%`, spaces, non-ASCII) must be encoded
        // BEFORE joining the root URI, or the server parses `b.rs` as a fragment and the opened
        // document never associates with the indexed file (#534 review).
        assert_eq!(encode_uri_path("src/a b#c%.rs"), "src/a%20b%23c%25.rs");
        let root = root_uri_for(Path::new("/repo"));
        let uri = format!("{}/{}", root, encode_uri_path("src/доки/a b.rs"));
        assert_eq!(
            path_from_uri(&root, &uri),
            Some("src/доки/a b.rs".to_string()),
            "encode → open → decode round-trips the reserved-byte path"
        );
    }

    #[test]
    fn sentinel_is_content_stable_and_local() {
        let candidate = store::EdgeJoinCandidate {
            edge_id: 1,
            source_path: "src/lib.rs".to_string(),
            file_sha: "sha".to_string(),
            source_start_byte: 0,
            source_end_byte: 10,
            callee_start_byte: 4,
            callee_end_byte: 9,
            confidence: "NameOnly".to_string(),
            edge_kind: "calls_name".to_string(),
            to_symbol_id: None,
        };
        let a = live_local_sentinel("src/lib.rs", &candidate);
        let b = live_local_sentinel("src/lib.rs", &candidate);
        assert_eq!(a, b, "same callsite reproduces the same sentinel (no refine churn)");
        assert!(super::super::scip::is_local_symbol(&a), "parses as a SCIP local: {a}");
        assert!(a.starts_with("local ra-lsp-"));
        let other = store::EdgeJoinCandidate { callee_start_byte: 5, ..candidate };
        assert_ne!(a, live_local_sentinel("src/lib.rs", &other), "span-keyed");
    }
}
