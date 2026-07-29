//! The live oracle pass: wire the resident LSP client to the watcher's maintenance pass and WRITE
//! its per-callee verdicts into the `edge_oracle` seam under the live backend's distinct tool id
//! (`ra-lsp`, `ts-lsp`, …). The pass is backend-agnostic: which server it drives, which files it
//! accepts, and how it knows the server is ready all come from the session's `LiveBackend`.
//!
//! Invariants this module upholds (settled on #74):
//!
//! - **Tool identity.** Live rows carry the LIVE tool id + the session's probed `--version`
//!   (re-probed at every spawn). Reusing the batch tool's id is rejected: a live `oracle_runs` row
//!   under the batch id would make `auto_run_decision` see the index as always-fresh and silently
//!   stop the whole-checkout batch pass.
//! - **Moniker source.** A live verdict's `scip_symbol` is the resolved target's BATCH moniker from
//!   `logical_symbol_monikers` — byte-identical to what the batch pass writes, so clone-collapse
//!   (#275) and moniker-anchored memory relocation treat live and batch rows as one evidence set
//!   with zero consumer changes. When the target has no batch moniker (batch never ran, or the def
//!   is outside its definitions) the fallback is a `local <tool>-<hash>` SCIP-local sentinel:
//!   `is_local_symbol` skips it before the conflict logic, so it can never poison a batch moniker,
//!   while the row's `resolved_symbol_id` still upgrades the edge tier. The sentinel's hash is
//!   content-derived (callsite path + callee span), so it is STABLE across reindexes — a no-op
//!   re-pass is a same-value upsert and does not churn the refine cache. The LSP
//!   `textDocument/moniker` string is NEVER persisted: it never equals the SCIP moniker, and the
//!   cross-tool conflict-drop would destroy batch collapse coverage on any co-covered span. Live
//!   only READS the moniker table.
//! - **Currency.** The verdict rows + the backing `oracle_runs` row commit in ONE transaction — the
//!   per-tool currency gate (`callee_moniker_current_clause`) must never see live rows without a
//!   backing run. A run row is recorded for a pass that wrote ≥1 row or migrated tool-version
//!   currency. There is NO authoritative clear: live's scope is only this pass's changed files (the
//!   batch pass owns whole-checkout authority).
//! - **Refine cache.** When a live write CHANGES an existing row's `scip_symbol` for an UNCHANGED
//!   `file_sha`, the scip-mode clone refinements that consulted the old evidence are invalidated
//!   (nothing in the refinement key folds oracle content — the same reasoning as the batch hook in
//!   `run_in_tx`). Same-value upserts skip the invalidation.
//! - **Drift.** A verdict is written only when the callsite's disk bytes still hash to the indexed
//!   `file_sha`, and the definition document's disk bytes still hash to its indexed `files.sha256`
//!   — the live analog of the batch join's index-vs-disk content gate. Anything else is skipped,
//!   never mis-joined.
//!
//! - **Readiness.** A warming server is never asked for a definition, and a batch that straddled a
//!   reload is discarded rather than interpreted. This is a correctness rule, not a latency one: a
//!   not-yet-loaded server does not reliably answer `null`, it can answer WRONG (see
//!   `lsp::readiness`), and a wrong answer would be persisted as a real verdict.
//!
//! Out of scope: `resolved-external` verdicts (a live definition outside the checkout has no
//! batch-interchangeable SCIP symbol string, so it is skipped, not written).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::time::Instant;

use path_slash::PathExt as _;
use rag_rat_base::hash::hex_sha256;
use rusqlite::Connection;
use serde::Serialize;
use url::Url;

use super::backend::{self, LiveBackend, ProjectLayout};
use super::lsp::client::LspClient;
use super::lsp::position::LineIndex;
use super::store::{self, EdgeOracleRow};
use super::{OracleResolutionKind, OracleTool, ToolAvailability, ToolManifest, join};

/// A resident live-oracle language-server session: the spawned client, the backend it drives, the
/// `tool_version` its rows are stamped with, and the checkout root URI documents are opened
/// under. Owned by the watcher's pass worker across passes; spawned lazily on the first eligible
/// pass and shut down after an idle window (both driven by the watcher).
pub struct LiveOracleSession {
    backend: LiveBackend,
    client: LspClient,
    tool_version: String,
    root_uri: String,
    /// The checkout's project layout, and when it was resolved. Every layout question the pass
    /// asks reads this rather than re-walking the checkout — the pass holds the repository write
    /// lock while it runs — and it is re-resolved once it ages out (`LAYOUT_MAX_AGE`).
    layout: ProjectLayout,
    layout_resolved_at: Instant,
}

/// Why a live session could not be established. The distinction is operational, not cosmetic: a
/// `Prerequisite` block is a PERMANENT property of the checkout that a human has to act on, while
/// `Unavailable` is the ordinary missing-tool degradation the watcher just keeps retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSpawnBlocked {
    /// A checkout prerequisite this backend needs is unmet, with the operator-facing hint. Retrying
    /// cannot fix it — only a change to the checkout can — so the caller must SURFACE the hint
    /// rather than fold it into a spawn-failure retry loop.
    Prerequisite(String),
    /// The language server is absent, unrunnable, or the session could not be established.
    Unavailable,
}

/// What a spawn learns about the checkout before any process exists: the probed version its
/// verdicts are stamped with, and the project layout its argv is built from.
struct SpawnPreflight {
    version: String,
    layout: ProjectLayout,
}

/// The gates a spawn passes before it starts a process, in the order that costs the least and
/// diagnoses the best.
///
/// Availability is decided FIRST — the same order [`ToolManifest::probe_runnable_in`] applies, and
/// for the same two reasons. Resolving the layout walks the whole checkout, and spawn attempts are
/// made from the maintenance pass while it holds the repository write lock, so a checkout with no
/// language server must not pay for that walk on every attempt. It is also the better diagnosis: in
/// a checkout with neither the server nor a project, the absent server is what the operator has to
/// act on, and being told to add a compilation database for a program that cannot run sends them
/// after the wrong problem.
///
/// The layout is then resolved ONCE and shared by the prerequisite gate, the spawn argv, and every
/// later pass. Resolving it twice would both double the walk and let the gate and the argv observe
/// different layouts if a database moved between the two scans.
///
/// The prerequisite gate is not cosmetic for these backends: a checkout with no project emits no
/// readiness signal at all, so the session could only ever sit in `Warming`.
fn resolve_preflight(
    backend: &LiveBackend,
    manifest: &ToolManifest,
    checkout_root: &Path,
    availability: ToolAvailability,
) -> Result<SpawnPreflight, LiveSpawnBlocked> {
    let ToolAvailability::Available { version, .. } = availability else {
        return Err(LiveSpawnBlocked::Unavailable);
    };
    let layout = backend.resolve_layout(checkout_root);
    match manifest.prerequisite_blocked_with(checkout_root, Some(&layout)) {
        Some(hint) => Err(LiveSpawnBlocked::Prerequisite(hint)),
        None => Ok(SpawnPreflight { version, layout }),
    }
}

/// Test seam: the state [`LiveOracleSession::spawn`] derives from the checkout, supplied directly
/// so a test can drive the pass's layout-dependent branches — the per-file "can this session
/// configure that file" gate, and the age-out re-resolve that ends a session whose pinned database
/// has moved. Neither is reachable through a session whose layout is the empty default.
#[cfg(test)]
pub(crate) struct InjectedSession<'a> {
    pub(crate) tool: OracleTool,
    pub(crate) client: LspClient,
    pub(crate) tool_version: &'a str,
    pub(crate) root_uri: &'a str,
    /// Build this with [`LiveBackend::resolve_layout`] against a real fixture directory: a
    /// hand-built layout would assert against a resolver the pass never runs.
    pub(crate) layout: ProjectLayout,
    /// When `layout` was resolved. `Instant::now()` keeps it valid for the whole pass; anything
    /// older than [`backend::LAYOUT_MAX_AGE`] makes the first pass re-resolve and compare.
    pub(crate) layout_resolved_at: Instant,
}

impl LiveOracleSession {
    /// Probe `tool`'s language server and spawn the resident client for `checkout_root`.
    ///
    /// Never an error and never a failed pass — the same degrade-quietly UX as a missing embedding
    /// model or batch tool — but the two ways it can decline are reported separately so a
    /// permanent configuration problem does not masquerade as a transient one. The version is
    /// RE-probed at every spawn so `tool_version` always names the binary this session's verdicts
    /// came from.
    pub fn spawn(tool: OracleTool, checkout_root: &Path) -> Result<Self, LiveSpawnBlocked> {
        let Some(backend) = LiveBackend::for_tool(tool) else {
            return Err(LiveSpawnBlocked::Unavailable);
        };
        let manifest = ToolManifest::for_tool(tool);
        // The probe is this call's ARGUMENT, so the checkout walk inside can never precede it.
        let SpawnPreflight { version, layout } = resolve_preflight(
            &backend,
            &manifest,
            checkout_root,
            manifest.probe_in(checkout_root),
        )?;
        let Some(root_uri) = root_uri_for(checkout_root) else {
            return Err(LiveSpawnBlocked::Unavailable);
        };
        let mut client = LspClient::spawn(
            manifest.program,
            &backend.spawn_args(manifest.live_args, &layout),
            checkout_root,
            backend.readiness,
        )
        .map_err(|_| LiveSpawnBlocked::Unavailable)?;
        client.initialize(&root_uri).map_err(|_| LiveSpawnBlocked::Unavailable)?;
        Ok(Self {
            backend,
            client,
            tool_version: version,
            root_uri,
            layout,
            layout_resolved_at: Instant::now(),
        })
    }

    /// Test seam: a session over an injected (fake-server) client transport. Runs the
    /// `initialize` handshake exactly like [`Self::spawn`] so the negotiated encoding is real.
    #[cfg(test)]
    pub(crate) fn from_client(client: LspClient, tool_version: &str, root_uri: &str) -> Self {
        let mut session = Self::from_warming_client(client, tool_version, root_uri);
        session.client.assume_ready();
        session
    }

    #[cfg(test)]
    pub(crate) fn from_warming_client(
        client: LspClient,
        tool_version: &str,
        root_uri: &str,
    ) -> Self {
        Self::from_warming_client_for(OracleTool::RaLsp, client, tool_version, root_uri)
    }

    /// The tool-parameterized warm session seam: drives the pass through a specific backend's
    /// language ids, sentinel namespace, and moniker source.
    ///
    /// The layout is the empty default, which a backend with a checkout-scoped project marker
    /// reads as "no compilation database anywhere" and therefore skips every file for. Those
    /// backends need [`Self::from_injected`].
    #[cfg(test)]
    pub(crate) fn from_warming_client_for(
        tool: OracleTool,
        client: LspClient,
        tool_version: &str,
        root_uri: &str,
    ) -> Self {
        Self::from_injected(InjectedSession {
            tool,
            client,
            tool_version,
            root_uri,
            layout: ProjectLayout::default(),
            layout_resolved_at: Instant::now(),
        })
    }

    /// Test seam: a warming session over an injected client AND an injected project layout. Runs
    /// the `initialize` handshake exactly like [`Self::spawn`] so the negotiated encoding is real.
    #[cfg(test)]
    pub(crate) fn from_injected(injected: InjectedSession<'_>) -> Self {
        let InjectedSession {
            tool,
            mut client,
            tool_version,
            root_uri,
            layout,
            layout_resolved_at,
        } = injected;
        client.initialize(root_uri).expect("test server completes the handshake");
        Self {
            backend: LiveBackend::for_tool(tool).expect("a live backend"),
            client,
            tool_version: tool_version.to_string(),
            root_uri: root_uri.to_string(),
            layout,
            layout_resolved_at,
        }
    }

    pub fn tool(&self) -> OracleTool {
        self.backend.tool
    }

    pub fn tool_version(&self) -> &str {
        &self.tool_version
    }

    /// Graceful LSP teardown (`shutdown` + `exit`); the client's Drop hard-kills as the
    /// fallback. Consumes the session.
    pub fn shutdown(self) {
        let mut this = self;
        let _ = this.client.shutdown();
    }

    fn readiness_checkpoint(&mut self) -> std::io::Result<Option<u64>> {
        self.client.readiness_checkpoint()
    }

    /// Nudge a backend whose readiness signal only appears in response to an open document
    /// (`typescript-language-server` starts its project load on the first `didOpen`, not at
    /// `initialize`): send one `didOpen`/`didClose` pair for a worklist file that can actually
    /// produce the signal, then return.
    ///
    /// The file CHOICE matters, it is not just "the first one on disk": a document belonging to no
    /// tsconfig project opens as an inferred project silently, emitting no progress cycle, so
    /// warming on it leaves the session exactly as un-warmed as before. A worklist of only such
    /// files would re-open an ineffective document every pass and never warm — so when none of
    /// them qualifies the backend supplies one by SEARCHING the checkout. That is worth doing
    /// because the expensive warm-up is per server, not per project: once any real project has
    /// loaded, even the project-less files resolve correctly.
    ///
    /// Deliberately fire-and-forget. The pass runs under the repository write lock, so blocking
    /// here for a project load — seconds on a real project — would stall every writer. The
    /// notifications are enough: the server loads in the background, its progress cycle latches
    /// the session ready, and the NEXT pass resolves the deferred worklist normally. Closing the
    /// document immediately does not cancel the load.
    fn trigger_warmup_open(&mut self, checkout_root: &Path, worklist: &[String]) {
        let open_document = |absolute: &Path| {
            let text = std::fs::read_to_string(absolute).ok()?;
            let uri: String = Url::from_file_path(absolute).ok()?.into();
            let language_id = self.backend.language_id_for(&absolute.to_string_lossy());
            Some((language_id, uri, text))
        };
        let opened = worklist
            .iter()
            .filter(|path| self.backend.open_signals_readiness(checkout_root, path, &self.layout))
            .find_map(|path| open_document(&checkout_root.join(path)))
            .or_else(|| {
                self.backend
                    .warmup_document(checkout_root, &self.layout)
                    .as_deref()
                    .and_then(open_document)
            });
        let Some((language_id, uri, text)) = opened else {
            return;
        };
        if self.client.did_open(&uri, language_id, &text).is_ok() {
            let _ = self.client.did_close(&uri);
        }
    }

    fn needs_warmup_open(&self) -> bool {
        self.client.needs_warmup_open()
    }

    /// Whether this session can configure `path` (repo-relative) well enough to trust its answers.
    fn can_resolve_path(&self, checkout_root: &Path, path: &str) -> bool {
        self.backend.session_can_resolve(checkout_root, path, &self.layout)
    }

    /// Re-resolve the checkout's project layout if the cached one has aged out, and report
    /// whether the session is still valid for it.
    ///
    /// `false` means the checkout now pins a DIFFERENT database (one appeared, moved, or was
    /// removed). That cannot be corrected in place: the server was spawned with an argv derived
    /// from the old layout, so the session has to be replaced.
    fn layout_still_holds(&mut self, checkout_root: &Path) -> bool {
        if self.layout_resolved_at.elapsed() < backend::LAYOUT_MAX_AGE {
            return true;
        }
        let fresh = self.backend.resolve_layout(checkout_root);
        self.layout_resolved_at = Instant::now();
        if !fresh.pins_same_database_as(&self.layout) {
            return false;
        }
        self.layout = fresh;
        true
    }

    /// Test barrier: one synchronous round trip. The transport is FIFO, so a returned response
    /// proves the server has processed every fire-and-forget notification sent before it —
    /// without this, a test asserting on the warm-up `didOpen` races the server thread.
    #[cfg(test)]
    pub(crate) fn barrier(&mut self) {
        let _ = self.client.request("$/barrier", serde_json::Value::Null);
    }
}

/// Inputs for one live oracle pass.
pub struct LivePassInput<'a> {
    /// The active checkout the just-reindexed files (and their edge candidates) are scoped to.
    pub commit_sha: &'a str,
    pub worktree_id: &'a str,
    /// The checkout root: `url::Url` converts each document path, and target bytes are read from
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

/// Why a live pass ended early, when it did.
///
/// The distinction is operational, not cosmetic: a layout change means the checkout's projects
/// moved under the session, which is the one event that can make a file the pass skipped as
/// unconfigurable resolvable — every other early exit leaves that answer exactly as it was. Callers
/// branch on this rather than on [`LivePassReport::status`], whose wording is operator-facing text
/// and not a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePassAbort {
    /// The checkout now pins a DIFFERENT compilation database than the one this session's argv was
    /// derived from. The session cannot be corrected in place and has to be replaced.
    LayoutChanged,
    /// The language server died, wedged, or errored — mid-pass or at the pass-entry readiness
    /// check. The checkout is unchanged.
    Server,
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
    /// Writes skipped because a sibling checkout still owns the same content key + tool version
    /// for different file bytes (the schema cannot represent both because SHA is outside the PK).
    pub skipped_content_collisions: u64,
    /// Candidates skipped because this session could not configure their file — the backend's
    /// server would have answered with heuristic flags, which resolves cross-unit calls wrongly
    /// rather than not at all. Never deferred: only a change to the checkout's project layout can
    /// make them resolvable.
    pub skipped_unconfigured: u64,
    /// Definitions skipped as out-of-corpus for live purposes: target outside the checkout root,
    /// in an unindexed file, or mapping to no indexed symbol. Live writes no `resolved-external`
    /// rows (see the module docs).
    pub skipped_external: u64,
    /// Callees the quiescent server left unresolved (a `null` definition).
    pub unresolved: u64,
    /// Whether the scip-mode refine cache was invalidated (a row's `scip_symbol` changed under
    /// unchanged bytes, or newly inserted non-local evidence became visible).
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
    /// Worklist paths skipped WHOLE because this session could not configure them — the files
    /// `skipped_unconfigured` counted candidates in, at most one entry per worklist path.
    ///
    /// Deliberately not `unfinished_paths`: retrying cannot help until the checkout's layout
    /// changes, and a caller that reads a backlog as "schedule another pass" would then spin on
    /// work every pass can only skip again. The caller holds these aside instead and requeues them
    /// when a pass reports [`LivePassAbort::LayoutChanged`].
    #[serde(skip)]
    pub skipped_unconfigured_paths: Vec<String>,
    /// Why the pass ended early, if it did. `status` carries the same fact as operator-facing
    /// text; this is the half a caller branches on.
    #[serde(skip)]
    pub abort: Option<LivePassAbort>,
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
    // BEFORE the readiness gate, deliberately: a session that never becomes ready would otherwise
    // never reach this check, so a database removed during warm-up would leave it warming forever
    // with no way back. End the pass instead and let the watcher replace the session — its argv
    // was derived from the old layout, so it cannot be corrected in place.
    if !session.layout_still_holds(input.checkout_root) {
        report.unfinished_paths = input.worklist.to_vec();
        report.abort = Some(LivePassAbort::LayoutChanged);
        report.status =
            "Aborted: the checkout now points at a different compilation database".to_string();
        return Ok(report);
    }
    match session.readiness_checkpoint() {
        Ok(Some(_)) => {},
        Ok(None) => {
            // A backend whose readiness signal only fires for an open document can never report
            // ready while the pass declines to open one. Break that deadlock without blocking.
            if session.needs_warmup_open() {
                session.trigger_warmup_open(input.checkout_root, input.worklist);
            }
            report.unfinished_paths = input.worklist.to_vec();
            report.status = "Warming".to_string();
            return Ok(report);
        },
        Err(err) => {
            report.unfinished_paths = input.worklist.to_vec();
            report.abort = Some(LivePassAbort::Server);
            report.status = format!("Aborted: {err}");
            return Ok(report);
        },
    }
    let tool = session.tool();
    // The batch tool whose monikers live copies (rust-analyzer for ra-lsp, scip-typescript for
    // ts-lsp) — always `Some` for a live tool; the join is vacuous without it.
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
                return Ok(report);
            },
        }
    }
    let mut aborted: Option<String> = None;
    let mut warming = false;
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
        // The session may be unable to CONFIGURE this file even though its language qualifies:
        // with several compilation databases in a checkout, clangd is pointed at none, and a file
        // whose database it cannot find on its own gets heuristic flags. Measured, that resolves a
        // cross-translation-unit call to the callee's HEADER DECLARATION — a wrong verdict, not a
        // missing one, and the covered-skip budget would never revisit it. Skip rather than
        // resolve, and do NOT defer: retrying cannot help until the checkout's layout changes.
        // RETAINED for the caller instead, so the layout change that makes the file resolvable can
        // bring it back — without a backlog entry that would schedule passes able only to re-skip.
        if !session.can_resolve_path(input.checkout_root, path) {
            report.skipped_unconfigured += callees.len() as u64;
            report.skipped_unconfigured_paths.push(path.clone());
            continue;
        }
        // Budget gate: nothing left → every candidate-bearing path from here rides the backlog.
        let remaining = input.max_requests.saturating_sub(report.requests_used) as usize;
        if remaining == 0 {
            defer_candidate_paths_from(&mut report, input.worklist, position, &by_path);
            break 'files;
        }

        // Readiness is dynamic: Cargo metadata or workspace changes can put rust-analyzer back
        // into loading after the pass-entry gate. Never begin another definition batch while it is
        // non-quiescent, or temporary nulls would become permanently completed unresolved work.
        let readiness_checkpoint = match session.readiness_checkpoint() {
            Ok(Some(checkpoint)) => checkpoint,
            Ok(None) => {
                warming = true;
                defer_candidate_paths_from(&mut report, input.worklist, position, &by_path);
                break 'files;
            },
            Err(err) => {
                aborted = Some(err.to_string());
                defer_candidate_paths_from(&mut report, input.worklist, position, &by_path);
                break 'files;
            },
        };

        // Callsite drift gate: the server resolves the DIRTY disk bytes, so those bytes must
        // still hash to the indexed `file_sha` the candidates were built from — a mid-pass edit
        // makes the indexed callee ranges point at the wrong content. Skip, never mis-resolve.
        let Ok(bytes) = std::fs::read(input.checkout_root.join(path)) else {
            report.skipped_drifted += callees.len() as u64;
            report.unfinished_paths.push(path.clone());
            continue;
        };
        if hex_sha256(&bytes) != callees[0].file_sha {
            report.skipped_drifted += callees.len() as u64;
            report.unfinished_paths.push(path.clone());
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            report.skipped_drifted += callees.len() as u64;
            report.unfinished_paths.push(path.clone());
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

        // Platform-aware file URL conversion handles drive/UNC/verbatim prefixes and percent
        // encoding; hand-joining the root and DB path repeatedly produced malformed URIs.
        let uri: String = Url::from_file_path(input.checkout_root.join(path))
            .map_err(|()| anyhow::anyhow!("cannot convert live-oracle document path to file URL"))?
            .into();
        let starts: Vec<usize> =
            to_resolve.iter().filter_map(|c| usize::try_from(c.callee_start_byte).ok()).collect();
        report.requests_used += starts.len() as u64;
        let language_id = session.backend.language_id_for(path);
        let resolved = match session.client.resolve_definitions(&uri, language_id, &text, &starts) {
            Ok(resolved) => resolved,
            Err(err) => {
                // A dead/wedged server: keep what earlier files produced, REQUEUE this file and
                // every candidate-bearing path after it (the watcher rides them into the next
                // pass and replaces the session), and never fail the maintenance pass over it.
                aborted = Some(err.to_string());
                defer_candidate_paths_from(&mut report, input.worklist, position, &by_path);
                break 'files;
            },
        };
        // A reload may begin while the synchronous batch is in flight. Discard the whole batch
        // before interpreting any null definitions, and retry this file once the server is ready.
        match session.readiness_checkpoint() {
            Ok(Some(checkpoint)) if checkpoint == readiness_checkpoint => {},
            Ok(_) => {
                warming = true;
                defer_candidate_paths_from(&mut report, input.worklist, position, &by_path);
                break 'files;
            },
            Err(err) => {
                aborted = Some(err.to_string());
                defer_candidate_paths_from(&mut report, input.worklist, position, &by_path);
                break 'files;
            },
        }
        // The caller may change while synchronous requests are in flight. Re-read AFTER the batch:
        // the server resolved the didOpen snapshot, and writing it against an already-changed disk
        // file would look current until the queued reindex catches up (or forever if its event was
        // missed).
        if std::fs::read(input.checkout_root.join(path))
            .ok()
            .is_none_or(|current| hex_sha256(&current) != callees[0].file_sha)
        {
            report.skipped_drifted += to_resolve.len() as u64;
            report.unfinished_paths.push(path.clone());
            continue;
        }
        let mut retry_file = deferred_count > 0;
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
                retry_file = true;
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
                    retry_file = true;
                    continue;
                },
            }
            let index = LineIndex::new(&def_disk, session.client.encoding());
            let (Some(def_start), Some(def_end)) = (
                index.byte_at_position(target_range.start),
                index.byte_at_position(target_range.end),
            ) else {
                report.skipped_drifted += 1;
                retry_file = true;
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
                .unwrap_or_else(|| live_local_sentinel(tool, &candidate.source_path, candidate));

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
            let existing =
                store::existing_verdict_scip_symbol(conn, tool, session.tool_version(), &row)?;
            // The content-key PK excludes file_sha. Preserve a different-SHA row while any sibling
            // checkout still joins to it; overwriting would make that sibling lose live evidence.
            if let Some((old_sha, _)) = &existing
                && old_sha != &candidate.file_sha
                && store::verdict_content_is_current_anywhere(conn, &row, old_sha)?
            {
                report.skipped_content_collisions += 1;
                continue;
            }
            // Refine-cache interplay: CHANGED evidence under the same bytes, OR newly inserted
            // NON-LOCAL moniker evidence, moves data absent from the refinement key. Local
            // sentinels are filtered by the consumer and are refine-neutral.
            if existing.as_ref().is_some_and(|(old_sha, old_symbol)| {
                old_sha == &candidate.file_sha && old_symbol != &scip_symbol
            }) || (existing.is_none() && !super::scip::is_local_symbol(&scip_symbol))
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
        // Fully resolved only when nothing was budget- or drift-deferred. A definition that changed
        // during this caller's batch requeues the CALLER because the definition's own watcher event
        // does not enumerate every caller that resolved into it.
        if retry_file {
            if !report.unfinished_paths.contains(path) {
                report.unfinished_paths.push(path.clone());
            }
        } else {
            report.files_resolved += 1;
        }
    }

    // Every early exit reachable from here is the server's — a dead transport or a readiness
    // error. A layout change returns above, before any file is touched.
    if aborted.is_some() {
        report.abort = Some(LivePassAbort::Server);
    }
    // A run row is recorded for a pass that WROTE verdicts OR migrated the tool version: the
    // migration is what makes the new `tool_version` current for the currency gate, and without
    // a backing run row the migrated rows stay invisible (the gate keys on the LATEST run).
    report.version_migrated = version_migrated;
    if report.rows_written > 0 || version_migrated {
        report.run_recorded = true;
        report.status = match &aborted {
            Some(err) => format!("Aborted: {err}"),
            None if warming => "Warming".to_string(),
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
            None if warming => "Warming".to_string(),
            None if report.unfinished_paths.is_empty() => "NoVerdicts".to_string(),
            None => "BudgetExhausted".to_string(),
        };
    }
    tx.commit()?;
    Ok(report)
}

fn defer_candidate_paths_from(
    report: &mut LivePassReport,
    worklist: &[String],
    position: usize,
    by_path: &HashMap<&str, Vec<&store::EdgeJoinCandidate>>,
) {
    report.unfinished_paths.extend(
        worklist[position..].iter().filter(|path| by_path.contains_key(path.as_str())).cloned(),
    );
}

/// The content-stable SCIP-local sentinel a live verdict carries when the resolved symbol has no
/// batch moniker: `local <tool>-<hash>` keyed by the callsite (path + callee span), so a no-op
/// re-pass reproduces the SAME string (a same-value upsert — no refine-cache churn). Parses as a
/// SCIP local symbol, so `is_local_symbol` drops it before `current_callee_monikers`' conflict
/// logic and it can never poison a batch moniker.
///
/// The tool id namespaces the sentinel so two live backends can never mint the same string for
/// different evidence; the HASH input deliberately stays callsite-only, so `ra-lsp` rows written
/// before a second backend existed keep reproducing byte-identically.
fn live_local_sentinel(
    tool: OracleTool,
    source_path: &str,
    candidate: &store::EdgeJoinCandidate,
) -> String {
    let hash = hex_sha256(
        format!("{}:{}:{}", source_path, candidate.callee_start_byte, candidate.callee_end_byte)
            .as_bytes(),
    );
    format!("local {}-{}", tool.as_db_str(), &hash[..16])
}

/// The canonical `file://` URL of a checkout directory. `url` handles native disk,
/// verbatim-disk, UNC, and verbatim-UNC prefixes on Windows and percent-encodes path bytes.
fn root_uri_for(checkout_root: &Path) -> Option<String> {
    Url::from_directory_path(checkout_root).ok().map(Into::into)
}

/// The repo-relative slash path of a `file://` document URI under `root_uri`; `None` when either
/// URL is not a native file path or the target is outside the checkout.
fn path_from_uri(root_uri: &str, uri: &str) -> Option<String> {
    let root = Url::parse(root_uri).ok()?.to_file_path().ok()?;
    let target = Url::parse(uri).ok()?.to_file_path().ok()?;
    let relative = target.strip_prefix(root).ok()?;
    Some(relative.to_slash_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use rag_rat_base::test_scratch::ScratchDir;

    use super::*;

    #[test]
    fn a_checkout_with_neither_a_server_nor_a_project_reports_the_missing_server() {
        // A backend's prerequisite and its binary can be unmet at the same time, and the two are
        // not equally actionable: telling an operator to generate a compilation database for a
        // program that is not installed sends them after the wrong problem, and answering that
        // question at all costs a walk of the whole checkout while the maintenance pass holds the
        // repository write lock — paid on every spawn attempt, none of which can succeed.
        //
        // Availability is supplied rather than probed, so this pins the ORDER on any machine,
        // whether or not the servers happen to be installed on it. `ra-lsp` is absent from the
        // table deliberately: it has no checkout prerequisite beyond its binary, so there is no
        // second gate for the probe to outrank.
        for (tool, marker, version) in [
            (OracleTool::ClangdLsp, "compile_commands.json", "clangd-test-1"),
            (OracleTool::TsLsp, "tsconfig.json", "ts-test-1"),
        ] {
            let scratch = ScratchDir::new("live-preflight");
            let root = scratch.path();
            let backend = LiveBackend::for_tool(tool).expect("a live backend");
            let manifest = ToolManifest::for_tool(tool);
            let blocked = ToolAvailability::Blocked {
                tool: tool.as_db_str().to_string(),
                program: manifest.program.to_string(),
                hint: manifest.install_hint.to_string(),
            };

            assert_eq!(
                resolve_preflight(&backend, &manifest, root, blocked).err(),
                Some(LiveSpawnBlocked::Unavailable),
                "an absent {tool:?} outranks the checkout prerequisite it makes moot",
            );

            // Control: the SAME checkout blocks on its prerequisite once the program can run —
            // without this the assertion above would also pass for a gate that never reports a
            // prerequisite at all. The two ways a spawn declines stay operationally distinct: a
            // prerequisite block is permanent until the checkout changes and a human has to act on
            // it, while an absent server is the degradation the watcher just keeps retrying.
            let available = ToolAvailability::Available {
                tool: tool.as_db_str().to_string(),
                program: manifest.program.to_string(),
                version: version.to_string(),
            };
            match resolve_preflight(&backend, &manifest, root, available) {
                Err(LiveSpawnBlocked::Prerequisite(hint)) => {
                    assert!(hint.contains(marker), "the {tool:?} hint names the fix: {hint}");
                },
                Err(LiveSpawnBlocked::Unavailable) =>
                    panic!("a runnable {tool:?} plus no project is a prerequisite block"),
                Ok(_) => panic!("a checkout with no {marker} must not spawn a {tool:?} session"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_uri_encodes_spaces_and_preserves_slashes() {
        assert_eq!(
            root_uri_for(Path::new("/tmp/my repo")).as_deref(),
            Some("file:///tmp/my%20repo/")
        );
        assert_eq!(root_uri_for(Path::new("/plain/path")).as_deref(), Some("file:///plain/path/"));
    }

    #[cfg(windows)]
    #[test]
    fn root_uri_keeps_a_windows_drive_colon_verbatim() {
        // rust-analyzer returns `file:///C:/repo/…` canonically; encoding the drive colon
        // (`file:///C%3A/repo`) would break `path_from_uri`'s prefix match and class every
        // in-repo definition as external (#534 review).
        let root = root_uri_for(Path::new(r"C:\repo")).unwrap();
        assert_eq!(root, "file:///C:/repo/");
        assert_eq!(path_from_uri(&root, "file:///C:/repo/src/lib.rs"), Some("src/lib.rs".into()));
        // A drive root with a space still encodes the non-drive part.
        assert_eq!(
            root_uri_for(Path::new(r"D:\my repo")).as_deref(),
            Some("file:///D:/my%20repo/")
        );
    }

    #[cfg(windows)]
    #[test]
    fn root_uri_strips_windows_verbatim_prefixes() {
        // `url` maps canonical Windows verbatim prefixes to ordinary canonical file URLs.
        assert_eq!(root_uri_for(Path::new(r"\\?\C:\repo")).as_deref(), Some("file:///C:/repo/"));
        assert_eq!(
            root_uri_for(Path::new(r"\\?\UNC\server\share\repo")).as_deref(),
            Some("file://server/share/repo/")
        );
    }

    #[cfg(windows)]
    #[test]
    fn root_uri_maps_a_unc_path_to_an_authority_url() {
        // `\\server\share\repo` → `file://server/share/repo` (authority form), matching the
        // canonical URI rust-analyzer returns; a naive join would emit `file:////server/...`
        // and misclass every in-repo definition as external (#534 review).
        let root = root_uri_for(Path::new(r"\\server\share\repo")).unwrap();
        assert_eq!(root, "file://server/share/repo/");
        assert_eq!(
            path_from_uri(&root, "file://server/share/repo/src/a.rs"),
            Some("src/a.rs".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_from_uri_decodes_and_rejects_external() {
        let root = "file:///tmp/my%20repo/";
        assert_eq!(
            path_from_uri(root, "file:///tmp/my%20repo/src/a file.rs"),
            Some("src/a file.rs".to_string())
        );
        assert_eq!(path_from_uri(root, "file:///elsewhere/src/lib.rs"), None);
        assert_eq!(path_from_uri(root, "file:///tmp/my%20repository/src/lib.rs"), None);
    }

    #[cfg(unix)]
    #[test]
    fn document_uri_encodes_the_relative_path() {
        let root = root_uri_for(Path::new("/repo")).unwrap();
        let uri: String = Url::from_file_path("/repo/src/доки/a b#c%.rs").unwrap().into();
        assert!(uri.contains("a%20b%23c%25.rs"));
        assert_eq!(
            path_from_uri(&root, &uri),
            Some("src/доки/a b#c%.rs".to_string()),
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
        let a = live_local_sentinel(OracleTool::RaLsp, "src/lib.rs", &candidate);
        let b = live_local_sentinel(OracleTool::RaLsp, "src/lib.rs", &candidate);
        assert_eq!(a, b, "same callsite reproduces the same sentinel (no refine churn)");
        assert!(super::super::scip::is_local_symbol(&a), "parses as a SCIP local: {a}");
        // Pinned: rows written before a second live backend existed must keep reproducing this
        // exact string, or every ra-lsp verdict churns the refine cache on the next pass.
        assert!(a.starts_with("local ra-lsp-"));
        let other = store::EdgeJoinCandidate { callee_start_byte: 5, ..candidate };
        assert_ne!(a, live_local_sentinel(OracleTool::RaLsp, "src/lib.rs", &other), "span-keyed");
    }

    #[test]
    fn every_live_backend_mints_a_distinct_parseable_sentinel() {
        // Two backends must never mint the same sentinel for the same callsite (that would make
        // one backend's evidence indistinguishable from the other's), and every sentinel must
        // still parse as a SCIP local so it is dropped before the moniker-conflict logic.
        let candidate = store::EdgeJoinCandidate {
            edge_id: 1,
            source_path: "src/lib".to_string(),
            file_sha: "sha".to_string(),
            source_start_byte: 0,
            source_end_byte: 10,
            callee_start_byte: 4,
            callee_end_byte: 9,
            confidence: "NameOnly".to_string(),
            edge_kind: "calls_name".to_string(),
            to_symbol_id: None,
        };
        let mut seen = std::collections::HashSet::new();
        for &tool in OracleTool::ALL.iter().filter(|tool| !tool.batch_capable()) {
            let sentinel = live_local_sentinel(tool, "src/lib", &candidate);
            assert!(
                super::super::scip::is_local_symbol(&sentinel),
                "{sentinel} must parse as a SCIP local"
            );
            assert!(seen.insert(sentinel.clone()), "{sentinel} collides across backends");
        }
    }
}
