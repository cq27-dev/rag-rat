//! Dream v2 pass 0 — the DETERMINISTIC verification substrate (no LLM).
//!
//! Three surfaces, all repo-scoped and reading the whole-tree index as dream's source of truth (not
//! the filesystem):
//!   - [`verification_queue`] — active memories that need (re)verification, ranked and capped by a
//!     budget. Churn-skip is the point: a memory is enqueued only when a binding anchor is
//!     stale/gone (reusing the doctor predicate), it has no `memory_reality` row yet, or its
//!     current body / bound-file inputs no longer match the last-checked hashes. This is the
//!     substrate the phase-B model verdict pass consumes — it never writes here.
//!   - [`evidence_pack`] — a deterministic, citation-checkable pack for one memory: an identifier
//!     table (backticked spans + long snake_case tokens resolved EXHAUSTIVELY against
//!     symbols/files, where "NOT FOUND anywhere" is authoritative because the index is whole-tree)
//!     plus current text excerpts of the memory's bound file(s), windowed around identifier hits.
//!   - [`unverifiable_findings`] — the deterministic `memory_unverifiable` decision: a memory whose
//!     bindings are all gone/absent AND none of whose identifiers resolve. Decided HERE, never by a
//!     model; folded into the identity-keyed `dream_findings` lifecycle by `dream_run`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use super::DreamFinding;
use crate::index::schema;

/// The authoritative "resolves nowhere" verdict — trustworthy precisely because the index is
/// whole-tree, so a miss is a real absence, not a scoping artifact.
const NOT_FOUND: &str = "NOT FOUND anywhere in the source tree";
/// Context lines above/below an identifier hit in a bound-file excerpt window.
const EXCERPT_RADIUS: i64 = 3;
/// Upper bound on the total excerpt lines an evidence pack carries (keeps a single-turn verdict
/// prompt bounded regardless of how many bound files / hits a memory has).
const MAX_EXCERPT_LINES: usize = 140;
/// Minimum length for a snake_case token to count as an identifier (short tokens like `is_ok` are
/// noise; the eval settled on 8).
const MIN_SNAKE_LEN: usize = 8;

/// Backticked spans: `` `foo::bar` ``, `` `src/lib.rs` ``. Capture group 1 is the span contents.
static BACKTICK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("static regex"));
/// snake_case tokens: a lowercase-led run with at least one internal underscore. Length is filtered
/// separately (`MIN_SNAKE_LEN`).
static SNAKE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b").expect("static regex"));
/// A bare symbol name we are willing to look up in `symbols.name` (skip spans with whitespace or
/// path separators — those resolve as files, not symbols).
static BARE_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static regex"));

/// Why a memory is in the verification queue. Not persisted (a transient queue reason), so it
/// carries no `as_db_str`; the [`Self::rank`] priority orders the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationReason {
    /// A binding anchor is stale/gone (the doctor population) — the note may point at dead code.
    AnchorBroken,
    /// No `memory_reality` row yet — never verified.
    NeverChecked,
    /// The note content (title or body) changed since it was last checked (`content_hash`
    /// mismatch).
    ContentChanged,
    /// The evidence pack changed since the last check (`checked_inputs_hash` mismatch).
    InputsChanged,
    /// The stored verdict was produced by an older verdict `PROMPT_VERSION` — the prompt or
    /// evidence-pack format changed, so the old verdict is not comparable and must be re-checked.
    PromptChanged,
}

impl VerificationReason {
    /// Priority rank: broken anchors first, then never-checked, then content churn, then
    /// input/prompt churn (a stale-prompt row's verdict is at least self-consistent, so it ranks
    /// last).
    fn rank(self) -> f64 {
        match self {
            Self::AnchorBroken => 1.0,
            Self::NeverChecked => 0.75,
            Self::ContentChanged => 0.5,
            Self::InputsChanged => 0.25,
            Self::PromptChanged => 0.2,
        }
    }
}

/// One memory needing (re)verification, with why. The phase-B verdict pass builds an
/// [`evidence_pack`] for each and records the model's verdict into `memory_reality`.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationQueueEntry {
    pub memory_id: String,
    pub title: String,
    pub body: String,
    pub reason: VerificationReason,
    pub rank: f64,
}

/// The deterministic evidence pack for one memory — the input to a single-turn model verdict, and
/// the set of lines a fabrication guard checks citations against.
#[derive(Debug, Clone, Serialize)]
pub struct EvidencePack {
    pub memory_id: String,
    pub identifiers: Vec<IdentifierResolution>,
    pub excerpts: Vec<FileExcerpt>,
}

impl EvidencePack {
    /// Whether the pack has any SUBSTANTIVE content a verdict could cite — at least one bound-file
    /// excerpt, or at least one identifier that RESOLVED (not [`NOT_FOUND`]). Two memories look
    /// uncitable and must stay out of the model pass:
    ///   - a prose-only / conceptual note with no identifiers and no excerpts (only boilerplate);
    ///   - a note whose EVERY identifier resolves to `NOT_FOUND` and has no live binding — pass 0
    ///     already decides this `memory_unverifiable`, and counting those NOT_FOUND rows as citable
    ///     would let the model be asked anyway and accept a `diverged` verdict citing "x -> NOT
    ///     FOUND", opening `memory_divergence` and burning budget against the module contract that
    ///     unverifiable is never asked of the model.
    ///
    /// The verdict pass records a terminal (verdict-less) row for an uncitable pack instead of
    /// calling the model, so it churn-skips rather than re-queuing every run.
    pub(super) fn is_citable(&self) -> bool {
        !self.excerpts.is_empty() || self.identifiers.iter().any(|id| id.resolution != NOT_FOUND)
    }
}

/// One extracted identifier and where (if anywhere) it resolves in the whole-tree index.
#[derive(Debug, Clone, Serialize)]
pub struct IdentifierResolution {
    pub identifier: String,
    /// `symbol <path>::<name>`, `file <path>`, or the authoritative [`NOT_FOUND`].
    pub resolution: String,
}

/// A current-text excerpt window from a bound file, addressed by absolute line range.
#[derive(Debug, Clone, Serialize)]
pub struct FileExcerpt {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub text: String,
}

/// Active memories that need (re)verification, ranked (broken anchors first) and capped at
/// `budget`. A memory is enqueued when it has no `memory_reality` row, its note content changed
/// (`content_hash`, covering title+body), or its evidence changed (`checked_inputs_hash`); a
/// stale/gone anchor (the doctor
/// predicate) raises the RANK of such a memory to the top but does NOT by itself enqueue one whose
/// stored verdict still matches — everything else is CHURN-SKIPPED, which is what makes running
/// this a few times a day cheap. Repo-scoped: only the active repo's memories are considered.
///
/// `now_ms` is reserved for the caller's verdict stamping (`memory_reality.checked_at_ms`);
/// pass-0 selection is time-independent, so the queue itself does not read the clock.
pub fn verification_queue(
    conn: &Connection,
    now_ms: i64,
    budget: usize,
) -> rusqlite::Result<Vec<VerificationQueueEntry>> {
    let _ = now_ms;
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let reality_clause = schema::periphery_repo_scope_clause(&scope, "memory_reality");
    // Reuse the doctor's anchor predicate rather than re-inlining it here.
    let broken: HashSet<String> =
        crate::query::memory::memory_ids_with_broken_anchors(conn)?.into_iter().collect();

    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, body FROM repo_memories WHERE status = 'active'{mem_clause} ORDER BY id"
    ))?;
    let mems: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut queue = Vec::new();
    for (memory_id, title, body) in mems {
        let reason =
            queue_reason(conn, &memory_id, &title, &body, &broken, &scope, &reality_clause)?;
        if let Some(reason) = reason {
            queue.push(VerificationQueueEntry {
                rank: reason.rank(),
                memory_id,
                title,
                body,
                reason,
            });
        }
    }
    // Deterministic order: rank desc, then memory_id asc; then cap by budget.
    queue.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
    queue.truncate(budget);
    Ok(queue)
}

/// Decide why (if at all) `memory_id` needs verification — the churn-skip gate. The stored
/// `memory_reality` comparators are consulted FIRST: a row whose `content_hash` AND
/// `checked_inputs_hash` still match the current note (title+body) + evidence skips (`None`)
/// REGARDLESS of anchor status — the stored verdict stands, and a broken anchor is surfaced by
/// `memory doctor` and the unverifiable/divergence findings, not by re-checking an unchanged note
/// (else a broken-anchor memory would re-enqueue every run at the top rank and starve NeverChecked;
/// a genuinely changed evidence set changes `checked_inputs_hash` and re-enqueues via InputsChanged
/// anyway). A memory that DOES need a first/re-check takes the top `AnchorBroken` rank when its
/// anchor is broken, otherwise the specific churn reason (NeverChecked / ContentChanged /
/// InputsChanged / PromptChanged).
fn queue_reason(
    conn: &Connection,
    memory_id: &str,
    title: &str,
    body: &str,
    broken: &HashSet<String>,
    scope: &Option<String>,
    reality_clause: &str,
) -> rusqlite::Result<Option<VerificationReason>> {
    let stored: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            &format!(
                "SELECT content_hash, checked_inputs_hash, prompt_version FROM memory_reality \
                 WHERE memory_id = ?1{reality_clause}"
            ),
            [memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((stored_content_hash, stored_inputs_hash, stored_prompt_version)) = stored else {
        // Never checked → needs a first check; a broken anchor takes the top rank.
        return Ok(Some(if broken.contains(memory_id) {
            VerificationReason::AnchorBroken
        } else {
            VerificationReason::NeverChecked
        }));
    };
    // The note the prompt audits is TITLE + body, so the content hash covers both — a title-only
    // edit re-queues just like a body edit.
    let content_changed = stored_content_hash != note_content_hash(title, body);
    let current_inputs = checked_inputs_hash(conn, memory_id, scope)?;
    let inputs_changed = stored_inputs_hash.as_deref() != Some(current_inputs.as_str());
    // A row from an older verdict prompt/pack version is not comparable to a fresh check — re-queue
    // it so a `PROMPT_VERSION` bump doesn't leave every unchanged memory driving markers/findings
    // off a stale-prompt verdict forever. (An uncitable memory's terminal row is stamped with the
    // current version too, so it also re-evaluates on a bump.)
    let prompt_changed = stored_prompt_version.as_deref() != Some(super::verdict::PROMPT_VERSION);
    if !content_changed && !inputs_changed && !prompt_changed {
        // Verified AND unchanged — churn-skip regardless of anchor status (the stored verdict
        // stands; anchor breakage is surfaced elsewhere).
        return Ok(None);
    }
    // A change since the last check → re-check. A broken anchor still takes the top rank.
    Ok(Some(if broken.contains(memory_id) {
        VerificationReason::AnchorBroken
    } else if content_changed {
        VerificationReason::ContentChanged
    } else if inputs_changed {
        VerificationReason::InputsChanged
    } else {
        VerificationReason::PromptChanged
    }))
}

/// The dream freshness key for a memory's NOTE content — the single `content_hash` stamped into
/// `memory_reality` / `memory_summaries`. Hashes the TRIMMED title AND body, newline-delimited (the
/// house `memory_input_hash` style — deliberately NOT a bare `title ‖ body` concat, which would let
/// `"ab"+"c"` and `"a"+"bc"` collide), because BOTH the verdict prompt and the compaction prompt
/// audit the whole note (TITLE + body). So a title-only edit re-verifies / re-summarizes and drops
/// the stale verdict / summary / marker exactly as a body edit does. (Kept dream-local rather than
/// reusing `memory_input_hash`, which also folds kind + tags — dimensions the prompts don't audit —
/// and reads the frozen-at-creation stored `input_hash`.)
///
/// `pub(crate)` so the surfacing hydrator recomputes it identically to the queue / verdict-pass
/// stamp.
pub(crate) fn note_content_hash(title: &str, body: &str) -> String {
    crate::index::hex_sha256(format!("{}\n{}", title.trim(), body.trim()).as_bytes())
}

/// sha256 fingerprint of a memory's ENTIRE deterministic evidence pack — the churn comparator that
/// beats a commit-ancestry walk. Named `checked_inputs_hash` after the column it is stamped into;
/// it covers BOTH halves of what the verdict model is shown:
///   - the memory's bound-file inputs, as the sorted `(path, sha)` MULTISET (not bare shas: a set
///     of shas is blind to a rebind that keeps identical content — a same-sha rebind, or a
///     duplicate-content child add/remove under a directory binding, would leave the hash unchanged
///     while the stored verdict still points at the old path);
///   - the memory's identifier RESOLUTIONS, as the sorted `(identifier, resolution)` pairs. A
///     memory with no bound file (or only a dead binding) still has a verdict grounded purely in
///     whole-tree identifier resolution, and that resolution flips when a named symbol/path is
///     ADDED or REMOVED from the index. Folding it in is what makes such a memory re-queue — and
///     its stored verdict / uncitable terminal row drop — when its ACTUAL evidence changes, not
///     only when a bound file does (else an all-NOT_FOUND memory recorded uncitable would keep
///     skipping after the code later adds the symbol, and an identifier-only `current` verdict
///     would survive that evidence disappearing). The excerpt TEXT is a pure function of these two
///     inputs (bound-file content by sha + identifier positions in the unchanged body), so hashing
///     them fingerprints the whole pack without rebuilding excerpts.
///
/// `pub(crate)` so the phase-B verdict pass (`verdict`) recomputes it EXACTLY as the queue's
/// comparator does when it stamps `memory_reality.checked_inputs_hash` — same function, so the next
/// run churn-skips instead of re-checking — and so the surfacing hydrator / divergence finder gate
/// a stale verdict on it the same way the queue does. Cost note: resolving identifiers loads
/// `indexed_file_paths` per call; acceptable for the deterministic pass (`unverifiable_findings`
/// already resolves every memory's identifiers each run) and the opt-in `surface = "summary"` read.
pub(crate) fn checked_inputs_hash(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<String> {
    let file_pairs: BTreeSet<String> = resolve_bound_files(conn, memory_id, scope)?
        .into_iter()
        .map(|(path, _, sha)| format!("{path}\u{1f}{sha}"))
        .collect();
    let ident_pairs: BTreeSet<String> = identifier_resolution_pairs(conn, memory_id, scope)?
        .into_iter()
        .map(|(ident, res)| format!("{ident}\u{1f}{res}"))
        .collect();
    let files = file_pairs.into_iter().collect::<Vec<_>>().join("\u{1e}");
    let idents = ident_pairs.into_iter().collect::<Vec<_>>().join("\u{1e}");
    // `\u{1d}` (group separator) splits the two sections so a path can never collide with an
    // identifier pair.
    Ok(crate::index::hex_sha256(format!("{files}\u{1d}{idents}").as_bytes()))
}

/// The `(identifier, resolution)` half of the evidence-pack fingerprint — the memory's identifiers
/// (from title+body) each resolved against the whole-tree index EXACTLY as [`evidence_pack`] does,
/// so the churn key matches what the model is actually shown. Empty when the memory is not visible
/// in scope or carries no identifiers.
fn identifier_resolution_pairs(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mem_clause = schema::periphery_repo_scope_clause(scope, "repo_memories");
    let row: Option<(String, String)> = conn
        .query_row(
            &format!("SELECT title, body FROM repo_memories WHERE id = ?1{mem_clause}"),
            [memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((title, body)) = row else {
        return Ok(Vec::new());
    };
    let identifiers = extract_identifiers(&title, &body);
    if identifiers.is_empty() {
        return Ok(Vec::new());
    }
    let file_paths = indexed_file_paths(conn)?;
    let mut out = Vec::with_capacity(identifiers.len());
    for ident in &identifiers {
        out.push((ident.clone(), resolve_identifier(conn, ident, &file_paths)?));
    }
    Ok(out)
}

/// Deterministic evidence pack for one memory. Returns an EMPTY pack when the memory is not visible
/// in the active repo scope (so a stray cross-repo call surfaces nothing rather than erroring).
pub fn evidence_pack(conn: &Connection, memory_id: &str) -> anyhow::Result<EvidencePack> {
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let row: Option<(String, String)> = conn
        .query_row(
            &format!("SELECT title, body FROM repo_memories WHERE id = ?1{mem_clause}"),
            [memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((title, body)) = row else {
        return Ok(EvidencePack {
            memory_id: memory_id.to_string(),
            identifiers: Vec::new(),
            excerpts: Vec::new(),
        });
    };
    let identifiers = extract_identifiers(&title, &body);
    let file_paths = indexed_file_paths(conn)?;
    let mut resolutions = Vec::with_capacity(identifiers.len());
    for ident in &identifiers {
        resolutions.push(IdentifierResolution {
            resolution: resolve_identifier(conn, ident, &file_paths)?,
            identifier: ident.clone(),
        });
    }
    let excerpts = bound_file_excerpts(conn, memory_id, &scope, &identifiers)?;
    Ok(EvidencePack { memory_id: memory_id.to_string(), identifiers: resolutions, excerpts })
}

/// The deterministic `memory_unverifiable` findings: active memories whose bindings are all
/// gone/absent (no live non-`scip_moniker` binding) AND none of whose identifiers resolve anywhere
/// in the whole-tree index. Repo-scoped; the evidence names exactly what was checked. Folded into
/// the identity-keyed `dream_findings` lifecycle by `dream_run` (so a memory that becomes
/// verifiable again is resolved), which is why this runs over the full active population, not the
/// budget.
pub(super) fn unverifiable_findings(conn: &Connection) -> rusqlite::Result<Vec<DreamFinding>> {
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let file_paths = indexed_file_paths(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, body FROM repo_memories WHERE status = 'active'{mem_clause} ORDER BY id"
    ))?;
    let mems: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::new();
    for (memory_id, title, body) in mems {
        if memory_has_live_binding(conn, &memory_id, &scope)? {
            continue;
        }
        let identifiers = extract_identifiers(&title, &body);
        if any_identifier_resolves(conn, &identifiers, &file_paths)? {
            continue;
        }
        let named = if identifiers.is_empty() {
            String::new()
        } else {
            format!(": {}", identifiers.join(", "))
        };
        out.push(DreamFinding {
            kind: "memory_unverifiable".into(),
            subject: memory_id,
            evidence: format!(
                "no live binding and none of {} identifier(s) resolve in the index{named} [E0]",
                identifiers.len(),
            ),
            rank: 0.9,
        });
    }
    Ok(out)
}

/// Whether `memory_id` has any live binding: a non-`scip_moniker` binding whose anchor is not
/// `gone` (`scip_moniker` self-heals on the next oracle run and is never rebind-actionable,
/// matching `doctor_report`). "Every binding gone/absent" is the negation.
fn memory_has_live_binding(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<bool> {
    let bind_clause = schema::periphery_repo_scope_clause(scope, "repo_memory_bindings");
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM repo_memory_bindings WHERE memory_id = ?1 AND binding_kind != \
             'scip_moniker' AND anchor_status != 'gone'{bind_clause}"
        ),
        [memory_id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Backticked spans + long snake_case tokens from title+body, trimmed, de-duplicated and SORTED (a
/// `BTreeSet`), so the identifier table is byte-stable across runs.
fn extract_identifiers(title: &str, body: &str) -> Vec<String> {
    let text = format!("{title}\n{body}");
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for cap in BACKTICK_RE.captures_iter(&text) {
        if let Some(span) = cap.get(1) {
            let span = span.as_str().trim();
            if !span.is_empty() {
                ids.insert(span.to_string());
            }
        }
    }
    for m in SNAKE_RE.find_iter(&text) {
        if m.as_str().len() >= MIN_SNAKE_LEN {
            ids.insert(m.as_str().to_string());
        }
    }
    ids.into_iter().collect()
}

/// Resolve one identifier EXHAUSTIVELY: a symbol by (bare / last-`::`-segment) name first, then a
/// file by path segment (suffix-aware), else the authoritative [`NOT_FOUND`].
fn resolve_identifier(
    conn: &Connection,
    ident: &str,
    file_paths: &[String],
) -> rusqlite::Result<String> {
    let symbol_name = ident.rsplit("::").next().unwrap_or(ident);
    if BARE_NAME_RE.is_match(symbol_name) {
        let locs = resolve_symbol(conn, symbol_name)?;
        match locs.as_slice() {
            [] => {},
            [one] => return Ok(format!("symbol {one}")),
            // AMBIGUOUS: more than one live definition shares this bare name. Present ALL of them
            // (sorted) rather than the first — the model must not audit against, and the churn key
            // must not be pinned to, an UNRELATED same-named symbol (so deleting/renaming the one
            // the note actually described re-verifies even when a namesake survives).
            many => return Ok(format!("symbols ({}): {}", many.len(), many.join(", "))),
        }
    }
    // A shorthand path (`lib.rs`, `src/lib.rs`) can suffix-match MORE than one indexed file;
    // present ALL of them for the SAME reason as ambiguous symbols above — else
    // deleting/changing the file the note meant is masked by an unrelated same-suffix file, and
    // the churn key stays pinned to the first match. An exact path match is definitive
    // (returned alone).
    let files = resolve_file_segment(ident, file_paths);
    match files.as_slice() {
        [] => {},
        [one] => return Ok(format!("file {one}")),
        many => return Ok(format!("files ({}): {}", many.len(), many.join(", "))),
    }
    Ok(NOT_FOUND.to_string())
}

/// Whether ANY extracted identifier resolves — the "zero identifiers resolve" gate for
/// `unverifiable_findings` (short-circuits on the first hit).
fn any_identifier_resolves(
    conn: &Connection,
    identifiers: &[String],
    file_paths: &[String],
) -> rusqlite::Result<bool> {
    for ident in identifiers {
        if resolve_identifier(conn, ident, file_paths)? != NOT_FOUND {
            return Ok(true);
        }
    }
    Ok(false)
}

/// EVERY live symbol whose `name` matches, as `path::name`, path-sorted, through the `files` view
/// (repo-scoped). Empty when the name is unknown anywhere in the tree. Returns all matches (not
/// `LIMIT 1`) so a common bare name is surfaced as ambiguous rather than silently pinned to its
/// first definition — see [`resolve_identifier`]. `DISTINCT` collapses a symbol indexed twice at
/// the same path (e.g. across chunks) but keeps genuinely distinct same-named definitions.
fn resolve_symbol(conn: &Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.path, s.name FROM symbols s JOIN files f ON f.id = s.file_id WHERE \
         s.name = ?1 ORDER BY f.path, s.name",
    )?;
    stmt.query_map([name], |r| {
        Ok(format!("{}::{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?
    .collect()
}

/// EVERY indexed path that equals `ident` or ends in `/ident` — suffix-aware like
/// `stale_reference`'s resolver, so prose shorthand (`src/lib.rs`, `lib.rs`) still resolves.
/// Returns ALL matches (path-sorted; `file_paths` is already sorted) so a shorthand that hits more
/// than one file is surfaced as ambiguous rather than silently pinned to the first — see
/// [`resolve_identifier`]. An EXACT path match is definitive (a full real path) and is returned
/// alone; only the suffix case can be ambiguous.
fn resolve_file_segment(ident: &str, file_paths: &[String]) -> Vec<String> {
    if let Some(p) = file_paths.iter().find(|p| p.as_str() == ident) {
        return vec![p.clone()];
    }
    let suffix = format!("/{ident}");
    file_paths.iter().filter(|p| p.ends_with(&suffix)).cloned().collect()
}

/// Escape a string for use as a SQLite `LIKE` pattern under `ESCAPE '\'` — the three special chars
/// `\`, `%`, `_` are backslash-escaped so a bound path with a literal `_`/`%` matches literally.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Every indexed file path for the active repo (through the `files` view), sorted for deterministic
/// segment resolution.
fn indexed_file_paths(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT path FROM files ORDER BY path")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect()
}

/// Distinct, sorted, non-null binding paths for a memory (repo-scoped) — the memory's bound files.
/// `pub(super)` so the verdict pass can label a note by its first bound path.
pub(super) fn bound_file_paths(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<Vec<String>> {
    let bind_clause = schema::periphery_repo_scope_clause(scope, "repo_memory_bindings");
    conn.prepare(&format!(
        "SELECT DISTINCT path FROM repo_memory_bindings WHERE memory_id = ?1 AND path IS NOT \
         NULL{bind_clause} ORDER BY path"
    ))?
    .query_map([memory_id], |r| r.get::<_, String>(0))?
    .collect()
}

/// Every indexed `(path, file_id, sha256)` the memory's bindings cover, de-duplicated by file_id.
/// Each binding `path` is resolved as one of:
///   - the REPO ROOT — an empty path (a `--dir .` binding normalizes to `""`), which matches EVERY
///     indexed file (a root-scoped note is invalidated by any repo change);
///   - a DIRECTORY — `path LIKE ?1 || '/%'` folds in every child file;
///   - a FILE — an exact `path = ?1` (a real file has no `<path>/…` children, so no spurious rows).
///
/// The single expansion both the churn hash and the excerpt builder share, so a directory (or root)
/// binding hashes AND shows excerpts over its child files identically — without it, a directory
/// binding's `files.path` never matches, leaving the empty inputs sentinel (stale churn-skip) and
/// dropping every bound-source excerpt from the verdict prompt.
///
/// Sorted by `(path, file_id)` — NOT the reindex-volatile `files.id` rowid order the expansion
/// queries return. The excerpt builder consumes the `MAX_EXCERPT_LINES` budget in THIS order, so an
/// unsorted (rowid) order would let a full/incremental reindex that leaves the same `(path, sha)`
/// set — and thus the same churn-skipped `checked_inputs_hash` — change WHICH files land in the
/// evidence pack. Path order makes the pack deterministic and consistent with the path-independent
/// hash.
fn resolve_bound_files(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<Vec<(String, i64, String)>> {
    // file_id -> (path, sha) so a file bound via both an exact path and its parent dir counts once.
    let mut by_id: BTreeMap<i64, (String, String)> = BTreeMap::new();
    for path in bound_file_paths(conn, memory_id, scope)? {
        let mut push = |id: i64, p: String, sha: String| {
            by_id.entry(id).or_insert((p, sha));
        };
        if path.is_empty() {
            // Repo-root binding (`--dir .`): every indexed file in scope.
            let mut stmt = conn.prepare_cached("SELECT id, path, sha256 FROM files")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?;
            for row in rows {
                let (id, p, sha) = row?;
                push(id, p, sha);
            }
        } else {
            // The directory pattern is `<dir>/%`, but `_` and `%` in the bound path are SQLite LIKE
            // wildcards — an un-escaped `src/foo_bar` would also match `src/fooXbar/…`, folding an
            // unrelated sibling's files into this memory's hash/excerpts. Escape the path and use
            // an explicit ESCAPE char; the exact-file arm (`path = ?1`) needs no
            // escaping.
            let dir_pattern = format!("{}/%", like_escape(&path));
            let mut stmt = conn.prepare_cached(
                "SELECT id, path, sha256 FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            )?;
            let rows = stmt.query_map(rusqlite::params![&path, &dir_pattern], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?;
            for row in rows {
                let (id, p, sha) = row?;
                push(id, p, sha);
            }
        }
    }
    let mut out: Vec<(String, i64, String)> =
        by_id.into_iter().map(|(id, (path, sha))| (path, id, sha)).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(out)
}

/// Current-text excerpt windows around identifier hits in the memory's bound files, from the
/// indexed chunk text (the index, not the filesystem, is dream's source of truth). Bounded at
/// `MAX_EXCERPT_LINES` total, ordered by (path, start_line).
fn bound_file_excerpts(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
    identifiers: &[String],
) -> anyhow::Result<Vec<FileExcerpt>> {
    let mut excerpts = Vec::new();
    let mut used_lines = 0usize;
    // Resolve directory/root bindings to their child files (see `resolve_bound_files`) so a
    // `--dir` note's verdict prompt carries the bound-source excerpts, not an empty section — an
    // exact-file binding resolves to just itself.
    for (path, file_id, _sha) in resolve_bound_files(conn, memory_id, scope)? {
        if used_lines >= MAX_EXCERPT_LINES {
            break;
        }
        let lines = file_lines(conn, file_id)?;
        for (start, end) in identifier_windows(&lines, identifiers) {
            if used_lines >= MAX_EXCERPT_LINES {
                break;
            }
            // Clamp THIS window to the remaining budget: `identifier_windows` merges adjacent hits
            // into one range, so a single window over a generated table / repeated config key can
            // be thousands of lines. Checking the cap only before appending would then
            // blow the pack past MAX_EXCERPT_LINES in one push and overflow the model
            // prompt — truncate the range.
            let remaining = MAX_EXCERPT_LINES - used_lines;
            let end = end.min(start + remaining as i64 - 1);
            let text = (start..=end)
                .filter_map(|ln| lines.get(&ln).map(String::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            used_lines += (end - start + 1) as usize;
            excerpts.push(FileExcerpt {
                path: path.clone(),
                start_line: start,
                end_line: end,
                text,
            });
        }
    }
    excerpts.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));
    Ok(excerpts)
}

/// Reconstruct a file's absolute line-number → text map from its indexed chunk text (decoded
/// through the shared dict decoder), so excerpts read current source without touching disk.
fn file_lines(conn: &Connection, file_id: i64) -> anyhow::Result<BTreeMap<i64, String>> {
    use crate::index::text_compression::{ChunkTextDecoder, ChunkTextRow};
    let dicts = crate::query::chunk_text_dicts(conn)?;
    let mut decoder = ChunkTextDecoder::new(&dicts);
    let mut stmt = conn.prepare(
        "SELECT chunks.start_line, chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version \
         FROM chunks JOIN chunk_text ON chunk_text.chunk_id = chunks.id WHERE chunks.file_id = ?1 \
         ORDER BY chunks.start_line",
    )?;
    let rows = stmt.query_map([file_id], |r| {
        Ok((r.get::<_, i64>(0)?, ChunkTextRow {
            blob: r.get(1)?,
            raw_len: r.get(2)?,
            dict_version: r.get(3)?,
        }))
    })?;
    let mut lines = BTreeMap::new();
    for row in rows {
        let (start_line, text_row) = row?;
        let text = text_row.resolve(&mut decoder)?;
        for (offset, line) in text.split('\n').enumerate() {
            lines.insert(start_line + offset as i64, line.to_string());
        }
    }
    Ok(lines)
}

/// Merged, radius-expanded windows around every line that contains any identifier. Deterministic:
/// hits are line-ordered and adjacent/overlapping windows are merged left-to-right.
fn identifier_windows(lines: &BTreeMap<i64, String>, identifiers: &[String]) -> Vec<(i64, i64)> {
    if identifiers.is_empty() || lines.is_empty() {
        return Vec::new();
    }
    let (Some(&min_line), Some(&max_line)) = (lines.keys().next(), lines.keys().next_back()) else {
        return Vec::new();
    };
    let mut windows: Vec<(i64, i64)> = Vec::new();
    for (&line_no, text) in lines {
        if !identifiers.iter().any(|id| text.contains(id.as_str())) {
            continue;
        }
        let start = (line_no - EXCERPT_RADIUS).max(min_line);
        let end = (line_no + EXCERPT_RADIUS).min(max_line);
        match windows.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => windows.push((start, end)),
        }
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::super::tests::{mem_db, set_repo};
    use super::*;

    /// Seed an active memory under the connection's active repo. Returns its id.
    fn seed_memory(c: &Connection, id: &str, title: &str, body: &str, repo_id: &str) {
        c.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
             (?1,'Invariant',?2,?3,'high','active','agent',1,1,'agent','v1',?4)",
            rusqlite::params![id, title, body, repo_id],
        )
        .unwrap();
    }

    /// Seed a file + one chunk carrying `text`, under `repo_id`. Returns the file id.
    fn seed_file(c: &Connection, path: &str, text: &str, repo_id: &str) -> i64 {
        c.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES \
             (?1,'rust','source',?2,0,0,'','',?3,0)",
            rusqlite::params![path, format!("sha-{path}"), repo_id],
        )
        .unwrap();
        let file_id = c.last_insert_rowid();
        let line_count = text.split('\n').count() as i64;
        c.execute(
            "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
             text_hash) VALUES (?1,'code',0,0,1,?2,'th')",
            rusqlite::params![file_id, line_count],
        )
        .unwrap();
        let chunk_id = c.last_insert_rowid();
        crate::index::chunk_text_store::seed_chunk_text(c, chunk_id, text).unwrap();
        file_id
    }

    fn content_hash(title: &str, body: &str) -> String {
        note_content_hash(title, body)
    }

    #[test]
    fn model_work_pending_is_citability_aware_for_verify() {
        // The ephemeral zero-work guard. VERIFY counts only CITABLE entries — an uncitable
        // prose-only / all-NOT_FOUND memory records a terminal row WITHOUT a model call, so
        // it is zero model work and must NOT cold-start a paid box (PR #438 review).
        // COMPACT counts the whole queue (every memory is summarized). Neither flag → never
        // pending.
        let c = mem_db();
        set_repo(&c, "r");
        let opts = crate::dream::DreamOptions {
            now_ms: 1,
            limit: 10,
            verify: true,
            include_reviewed: false,
        };
        assert!(
            !crate::dream::model_work_pending(&c, opts, 10, true, true).unwrap(),
            "empty repo → no work"
        );

        // An UNCITABLE prose-only memory (no identifiers, no bindings): verify is NOT model work,
        // but compaction WILL summarize it.
        seed_memory(&c, "m1", "t", "a prose note with no identifiers", "r");
        assert!(
            !crate::dream::model_work_pending(&c, opts, 10, true, false).unwrap(),
            "an all-uncitable verify queue is NOT model work — never cold-start a box for it"
        );
        assert!(
            crate::dream::model_work_pending(&c, opts, 10, false, true).unwrap(),
            "the same memory IS compact-pending (compaction has no uncitable short-circuit)"
        );

        // A CITABLE memory whose identifier resolves to a real symbol → verify IS model work.
        let fid = seed_file(&c, "src/x.rs", "fn f() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','resolve_marker_token','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        seed_memory(&c, "m2", "t", "a note about `resolve_marker_token`", "r");
        assert!(
            crate::dream::model_work_pending(&c, opts, 10, true, false).unwrap(),
            "a citable never-checked memory is verify-pending"
        );

        assert!(
            !crate::dream::model_work_pending(&c, opts, 10, false, false).unwrap(),
            "neither flag → never pending"
        );
    }

    #[test]
    fn queue_enqueues_anchor_gone_and_skips_a_verified_unchanged_memory() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a plain note", "r");
        // A gone binding puts m1 in the doctor population → enqueued.
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
             created_at_ms, repo_id) VALUES ('m1','symbol','foo','gone',0,'r')",
            [],
        )
        .unwrap();
        let q = verification_queue(&c, 1000, 10).unwrap();
        assert_eq!(q.len(), 1, "the anchor-gone memory is enqueued");
        assert_eq!(q[0].reason, VerificationReason::AnchorBroken);

        // Record a matching reality row (as the verdict pass would) AND heal the anchor: now
        // nothing is stale/gone and the body/inputs match → churn-skip on the second run.
        c.execute(
            "UPDATE repo_memory_bindings SET anchor_status = 'current' WHERE memory_id='m1'",
            [],
        )
        .unwrap();
        let inputs = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
             prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,?3,1000)",
            rusqlite::params![
                content_hash("t", "a plain note"),
                inputs,
                crate::dream::verdict::PROMPT_VERSION
            ],
        )
        .unwrap();
        let q = verification_queue(&c, 2000, 10).unwrap();
        assert!(q.is_empty(), "a verified + unchanged memory is churn-skipped: {q:?}");
    }

    #[test]
    fn checked_inputs_hash_folds_child_files_of_a_directory_binding() {
        // Regression (PR #428 Codex P2): a `--dir` binding stores a directory in
        // repo_memory_bindings.path that never equals a files.path, so the inputs hash must fold in
        // the directory's CHILD files — else it stays the empty sentinel and a dir-scoped memory
        // churn-skips with a stale verdict as its files change.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about the whole module", "r");
        seed_file(&c, "src/dir/a.rs", "fn a() {}\n", "r");
        seed_file(&c, "src/dir/b.rs", "fn b() {}\n", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','dir','src/dir','src/dir','current',0,'r')",
            [],
        )
        .unwrap();
        let scope = Some("r".to_string());
        let before = checked_inputs_hash(&c, "m1", &scope).unwrap();

        // A child file's sha changing must change the directory binding's inputs hash.
        c.execute("UPDATE main.files SET sha256 = 'sha-changed' WHERE path = 'src/dir/b.rs'", [])
            .unwrap();
        let after = checked_inputs_hash(&c, "m1", &scope).unwrap();
        assert_ne!(before, after, "a child file change moves the directory binding's inputs hash");

        // And it is NOT the empty sentinel (children were actually folded in).
        let empty = {
            let d = mem_db();
            set_repo(&d, "r");
            seed_memory(&d, "m2", "t", "note with no bindings", "r");
            checked_inputs_hash(&d, "m2", &scope).unwrap()
        };
        assert_ne!(
            before, empty,
            "the directory binding hashed real child files, not the sentinel"
        );
    }

    #[test]
    fn checked_inputs_hash_folds_all_files_for_a_repo_root_binding() {
        // Regression (PR #428 Codex P2): a `--dir .` binding normalizes to an empty path, which
        // matches no `files.path` under the file-or-child pattern — it must instead fold in EVERY
        // indexed file (a root-scoped note is invalidated by any repo change).
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about the whole repo", "r");
        seed_file(&c, "src/a.rs", "fn a() {}\n", "r");
        seed_file(&c, "docs/b.md", "# b\n", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES ('m1','dir','','','current',0,'r')",
            [],
        )
        .unwrap();
        let scope = Some("r".to_string());
        let before = checked_inputs_hash(&c, "m1", &scope).unwrap();
        // A change to ANY file moves a root binding's inputs hash.
        c.execute("UPDATE main.files SET sha256 = 'sha-x' WHERE path = 'docs/b.md'", []).unwrap();
        assert_ne!(
            before,
            checked_inputs_hash(&c, "m1", &scope).unwrap(),
            "any file change counts"
        );
        let empty = {
            let d = mem_db();
            set_repo(&d, "r");
            seed_memory(&d, "m2", "t", "no bindings", "r");
            checked_inputs_hash(&d, "m2", &scope).unwrap()
        };
        assert_ne!(before, empty, "the root binding folded real files, not the empty sentinel");
    }

    #[test]
    fn directory_binding_does_not_fold_a_like_wildcard_sibling() {
        // Regression (PR #428 Codex P2): a bound dir path with a SQLite LIKE wildcard (`_`) must be
        // escaped, or `src/foo_bar` also matches an unrelated `src/fooXbar/…`, folding a sibling's
        // files into this memory's hash.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note", "r");
        seed_file(&c, "src/foo_bar/a.rs", "fn a() {}\n", "r");
        seed_file(&c, "src/fooXbar/b.rs", "fn b() {}\n", "r"); // the wildcard-collision sibling
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','dir','src/foo_bar','src/foo_bar','current',0,'r')",
            [],
        )
        .unwrap();
        let scope = Some("r".to_string());
        let before = checked_inputs_hash(&c, "m1", &scope).unwrap();
        // Changing the SIBLING (fooXbar) must NOT move the hash — it isn't under the bound dir.
        c.execute("UPDATE main.files SET sha256 = 'sha-x' WHERE path = 'src/fooXbar/b.rs'", [])
            .unwrap();
        assert_eq!(before, checked_inputs_hash(&c, "m1", &scope).unwrap(), "sibling is excluded");
        // Changing the real child DOES move it.
        c.execute("UPDATE main.files SET sha256 = 'sha-y' WHERE path = 'src/foo_bar/a.rs'", [])
            .unwrap();
        assert_ne!(
            before,
            checked_inputs_hash(&c, "m1", &scope).unwrap(),
            "real child is included"
        );
    }

    #[test]
    fn checked_inputs_hash_reflects_the_bound_path_not_just_content() {
        // Regression (PR #428 Codex P2): hashing only unique shas is blind to a rebind that keeps
        // identical content — the (path, sha) multiset must change when the bound path changes.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note", "r");
        seed_file(&c, "src/a.rs", "same\n", "r");
        seed_file(&c, "src/b.rs", "same\n", "r"); // identical content → identical sha-{...}? no: sha-{path}
        // seed_file stamps sha = sha-{path}, so force identical shas to isolate the path axis.
        c.execute(
            "UPDATE main.files SET sha256 = 'same-sha' WHERE path IN ('src/a.rs','src/b.rs')",
            [],
        )
        .unwrap();
        let bind = |path: &str| {
            c.execute("DELETE FROM repo_memory_bindings WHERE memory_id='m1'", []).unwrap();
            c.execute(
                "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
                 anchor_status, created_at_ms, repo_id) VALUES ('m1','path',?1,?1,'current',0,'r')",
                [path],
            )
            .unwrap();
        };
        let scope = Some("r".to_string());
        bind("src/a.rs");
        let hash_a = checked_inputs_hash(&c, "m1", &scope).unwrap();
        bind("src/b.rs");
        let hash_b = checked_inputs_hash(&c, "m1", &scope).unwrap();
        assert_ne!(
            hash_a, hash_b,
            "rebinding to a same-content file at a different path changes the hash"
        );
    }

    #[test]
    fn checked_inputs_hash_tracks_identifier_resolution_with_no_bound_file() {
        // Regression (PR #428 Codex P2): the churn key must fingerprint the WHOLE evidence pack,
        // not just bound files. A memory with no binding whose identifier flips from
        // NOT_FOUND to a real symbol (the index gained it) must change hash, so an
        // all-NOT_FOUND uncitable memory re-queues once the code adds the symbol.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about `resolve_marker_token`", "r");
        let scope = Some("r".to_string());
        let before = checked_inputs_hash(&c, "m1", &scope).unwrap(); // identifier resolves NOT_FOUND
        let fid = seed_file(&c, "src/x.rs", "fn f() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','resolve_marker_token','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let after = checked_inputs_hash(&c, "m1", &scope).unwrap(); // now resolves to a symbol
        assert_ne!(
            before, after,
            "the identifier's resolution flip changes the evidence fingerprint"
        );
    }

    #[test]
    fn evidence_pack_excerpts_respect_the_line_cap_across_a_merged_window() {
        // Regression (PR #428 Codex P2): `identifier_windows` merges adjacent hits into ONE range,
        // so an identifier repeated on hundreds of lines yields a single huge window. The
        // cap must be enforced per-append (clamping the range), not only checked before it,
        // or one push blows past MAX_EXCERPT_LINES and overflows the model prompt.
        let c = mem_db();
        set_repo(&c, "r");
        // 400 lines each mentioning the identifier → one merged window far larger than the cap.
        let body_text =
            (0..400).map(|_| "let shared_marker_token = 1;").collect::<Vec<_>>().join("\n");
        seed_file(&c, "src/big.rs", &body_text, "r");
        seed_memory(&c, "m1", "t", "a note about `shared_marker_token`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/big.rs','src/big.rs','current',0,'r')",
            [],
        )
        .unwrap();
        let pack = evidence_pack(&c, "m1").unwrap();
        let total: i64 = pack.excerpts.iter().map(|e| e.end_line - e.start_line + 1).sum();
        assert!(total <= MAX_EXCERPT_LINES as i64, "excerpt lines {total} exceed the cap");
    }

    #[test]
    fn a_memory_whose_identifiers_all_resolve_nowhere_is_not_citable() {
        // Regression (PR #428 Codex P2): a note whose every identifier resolves to NOT_FOUND and
        // has no bound-file excerpts is the pass-0 unverifiable case and must NOT be
        // citable, or the model would be asked and could open a divergence citing the
        // NOT_FOUND rows.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about `never_defined_symbol` and nothing else", "r");
        let pack = evidence_pack(&c, "m1").unwrap();
        assert!(!pack.identifiers.is_empty(), "the identifier was extracted");
        assert!(
            pack.identifiers.iter().all(|id| id.resolution == NOT_FOUND),
            "and it resolves nowhere"
        );
        assert!(!pack.is_citable(), "an all-NOT_FOUND, no-excerpt pack is uncitable");
    }

    #[test]
    fn queue_records_a_terminal_row_for_an_uncitable_memory_so_it_stops_re_queuing() {
        // Regression (PR #428 Codex P2): a prose-only memory with no identifiers / excerpts yields
        // an uncitable pack; the verdict pass must record a terminal (verdict-less) row so
        // it churn-skips instead of consuming a budget slot forever.
        use super::super::model::mock::MockVerdictModel;
        use super::super::{VerdictPass, verdict};
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "just prose, nothing to resolve", "r");
        assert!(!evidence_pack(&c, "m1").unwrap().is_citable(), "the pack is uncitable");

        // A model with NO queued responses panics if `complete` is called — proving the uncitable
        // memory is handled deterministically and never reaches the model.
        let model = MockVerdictModel::new(Vec::<String>::new());
        verdict::run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 1000).unwrap();
        assert_eq!(model.calls(), 0, "the uncitable memory never calls the model");
        let (verdict_val, pv): (Option<String>, Option<String>) = c
            .query_row(
                "SELECT verdict, prompt_version FROM memory_reality WHERE memory_id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(verdict_val, None, "the terminal row has no verdict");
        assert_eq!(pv.as_deref(), Some(verdict::PROMPT_VERSION), "stamped with the current prompt");

        // Second run: unchanged → churn-skipped (empty queue), so it never re-consumes budget.
        assert!(
            verification_queue(&c, 2000, 10).unwrap().is_empty(),
            "the uncitable memory churn-skips after its terminal row"
        );
    }

    #[test]
    fn queue_re_enqueues_when_the_verdict_prompt_version_changes() {
        // Regression (PR #428 Codex P2): a stored verdict from an older PROMPT_VERSION is not
        // comparable, so an unchanged memory must re-queue on a prompt bump instead of skipping.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a plain note", "r");
        let inputs = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
             prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,'an-old-prompt-version',1000)",
            rusqlite::params![content_hash("t", "a plain note"), inputs],
        )
        .unwrap();
        let q = verification_queue(&c, 2000, 10).unwrap();
        assert_eq!(q.len(), 1, "a stale-prompt-version row re-queues");
        assert_eq!(q[0].reason, VerificationReason::PromptChanged);
    }

    #[test]
    fn queue_skips_a_broken_anchor_with_matching_hashes_so_it_never_starves_never_checked() {
        let c = mem_db();
        set_repo(&c, "r");
        // m1: a GONE binding (broken anchor) BUT a stored reality row whose content_hash + inputs
        // still match — the verdict stands, so it must churn-skip REGARDLESS of the broken anchor.
        // The pre-fix bug re-enqueued a broken anchor every run at the top AnchorBroken rank,
        // starving the never-checked memories below it. Anchor breakage is surfaced by `memory
        // doctor` and the unverifiable/divergence findings, not by re-checking an unchanged note.
        seed_memory(&c, "m1", "t", "a plain note", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
             created_at_ms, repo_id) VALUES ('m1','symbol','foo','gone',0,'r')",
            [],
        )
        .unwrap();
        let inputs = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
             prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,?3,1000)",
            rusqlite::params![
                content_hash("t", "a plain note"),
                inputs,
                crate::dream::verdict::PROMPT_VERSION
            ],
        )
        .unwrap();
        // Two never-checked memories that must still get slots.
        seed_memory(&c, "m2", "t", "never checked one", "r");
        seed_memory(&c, "m3", "t", "never checked two", "r");

        let q = verification_queue(&c, 1000, 10).unwrap();
        let ids: Vec<&str> = q.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(
            !ids.contains(&"m1"),
            "the unchanged broken-anchor memory is churn-skipped, not re-enqueued: {q:?}"
        );
        assert_eq!(ids, vec!["m2", "m3"], "the never-checked memories get slots (not starved)");
        assert!(
            q.iter().all(|e| e.reason == VerificationReason::NeverChecked),
            "only never-checked reasons remain in the queue: {q:?}"
        );
    }

    #[test]
    fn queue_re_enqueues_on_body_edit_and_on_bound_file_sha_change() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn a() {}\n", "r");
        seed_memory(&c, "m1", "t", "note about src/lib.rs", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/lib.rs','src/lib.rs','current',0,'r')",
            [],
        )
        .unwrap();
        let inputs = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
             prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,?3,1000)",
            rusqlite::params![
                content_hash("t", "note about src/lib.rs"),
                inputs,
                crate::dream::verdict::PROMPT_VERSION
            ],
        )
        .unwrap();
        assert!(
            verification_queue(&c, 1, 10).unwrap().is_empty(),
            "baseline: verified + unchanged"
        );

        // Body edit → content_hash mismatch re-enqueues.
        c.execute("UPDATE repo_memories SET body = 'a rewritten note' WHERE id='m1'", []).unwrap();
        let q = verification_queue(&c, 1, 10).unwrap();
        assert_eq!(q.iter().map(|e| e.reason).collect::<Vec<_>>(), vec![
            VerificationReason::ContentChanged
        ]);

        // Restore the body, change the bound file's sha → checked_inputs_hash mismatch re-enqueues.
        c.execute("UPDATE repo_memories SET body = 'note about src/lib.rs' WHERE id='m1'", [])
            .unwrap();
        c.execute("UPDATE main.files SET sha256 = 'sha-CHANGED' WHERE path='src/lib.rs'", [])
            .unwrap();
        let q = verification_queue(&c, 1, 10).unwrap();
        assert_eq!(q.iter().map(|e| e.reason).collect::<Vec<_>>(), vec![
            VerificationReason::InputsChanged
        ]);
    }

    #[test]
    fn queue_re_enqueues_on_title_only_edit() {
        // Regression (PR #428 Codex P2): the verdict prompt audits TITLE + body, so the
        // content_hash covers the title — a title-only edit must re-queue even when the
        // body is unchanged.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "original title", "a stable body", "r");
        let inputs = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_inputs_hash, \
             prompt_version, checked_at_ms) VALUES ('m1','r',?1,?2,?3,1000)",
            rusqlite::params![
                content_hash("original title", "a stable body"),
                inputs,
                crate::dream::verdict::PROMPT_VERSION
            ],
        )
        .unwrap();
        assert!(
            verification_queue(&c, 1, 10).unwrap().is_empty(),
            "baseline: verified + unchanged"
        );

        c.execute("UPDATE repo_memories SET title = 'a corrected title' WHERE id='m1'", [])
            .unwrap();
        let q = verification_queue(&c, 1, 10).unwrap();
        assert_eq!(q.iter().map(|e| e.reason).collect::<Vec<_>>(), vec![
            VerificationReason::ContentChanged
        ]);
    }

    #[test]
    fn resolve_bound_files_is_path_sorted_not_rowid_order() {
        // Regression (PR #428 Codex P2): the excerpt-budget cap consumes files in THIS order, so it
        // must be path-sorted (deterministic across reindex), not the volatile `files.id` rowid
        // order the expansion query returns.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a dir note", "r");
        // Insert under src/ in NON-alphabetical order so rowid order != path order.
        for p in ["src/zeta.rs", "src/alpha.rs", "src/mid.rs"] {
            seed_file(&c, p, "fn f() {}\n", "r");
        }
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','dir','src','src','current',0,'r')",
            [],
        )
        .unwrap();
        let paths: Vec<String> = resolve_bound_files(&c, "m1", &Some("r".to_string()))
            .unwrap()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        assert_eq!(paths, vec!["src/alpha.rs", "src/mid.rs", "src/zeta.rs"], "path-sorted");
    }

    #[test]
    fn resolve_symbol_returns_all_same_named_matches_as_ambiguous() {
        // Regression (PR #428 Codex P2): a common bare name must surface as AMBIGUOUS (all
        // matches), not silently pinned to the first path-ordered definition — else the
        // model audits against, and the churn key pins to, an unrelated namesake.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about `shared_name`", "r");
        let mk = |path: &str| {
            let fid = seed_file(&c, path, "fn f() {}\n", "r");
            c.execute(
                "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
                 (?1,'rust','shared_name','function',0,0)",
                rusqlite::params![fid],
            )
            .unwrap();
        };
        mk("src/b.rs");
        mk("src/a.rs");
        let locs = resolve_symbol(&c, "shared_name").unwrap();
        assert_eq!(
            locs,
            vec!["src/a.rs::shared_name", "src/b.rs::shared_name"],
            "all, path-sorted"
        );
        let files = indexed_file_paths(&c).unwrap();
        let res = resolve_identifier(&c, "shared_name", &files).unwrap();
        assert!(res.starts_with("symbols (2):"), "renders ambiguous: {res}");
    }

    #[test]
    fn resolve_file_segment_returns_all_same_suffix_matches_as_ambiguous() {
        // Regression (PR #428 Codex P2): a shorthand path (`lib.rs`) suffix-matching more than one
        // indexed file must surface as AMBIGUOUS, not pinned to the first — same class as ambiguous
        // symbols, so deleting the file the note meant re-verifies even when a same-suffix file
        // survives.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about `lib.rs`", "r");
        seed_file(&c, "crates/b/lib.rs", "fn f() {}\n", "r");
        seed_file(&c, "crates/a/lib.rs", "fn f() {}\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let res = resolve_identifier(&c, "lib.rs", &files).unwrap();
        assert!(res.starts_with("files (2):"), "renders ambiguous: {res}");
        assert!(
            res.contains("crates/a/lib.rs") && res.contains("crates/b/lib.rs"),
            "lists both matches: {res}"
        );
    }

    #[test]
    fn queue_caps_at_budget_in_deterministic_order() {
        let c = mem_db();
        set_repo(&c, "r");
        // Four never-checked memories (same reason/rank) → ordered by memory_id, capped at 2.
        for id in ["m4", "m1", "m3", "m2"] {
            seed_memory(&c, id, "t", "note", "r");
        }
        let q = verification_queue(&c, 1, 2).unwrap();
        assert_eq!(q.iter().map(|e| e.memory_id.as_str()).collect::<Vec<_>>(), vec!["m1", "m2"]);
    }

    #[test]
    fn evidence_pack_is_byte_identical_across_runs_and_reports_not_found() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "crates/x/src/thing.rs", "fn real_symbol() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) SELECT id, \
             'rust', 'real_symbol', 'function', 0, 0 FROM main.files WHERE path = \
             'crates/x/src/thing.rs'",
            [],
        )
        .unwrap();
        seed_memory(&c, "m1", "t", "refs `real_symbol` and `ghost_symbol`", "r");
        let a = evidence_pack(&c, "m1").unwrap();
        let b = evidence_pack(&c, "m1").unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "the pack is byte-identical across runs"
        );
        let resolved = a
            .identifiers
            .iter()
            .find(|i| i.identifier == "real_symbol")
            .expect("real_symbol identifier present");
        assert!(resolved.resolution.starts_with("symbol "), "known symbol resolves");
        let missing = a
            .identifiers
            .iter()
            .find(|i| i.identifier == "ghost_symbol")
            .expect("ghost_symbol identifier present");
        assert_eq!(missing.resolution, NOT_FOUND, "a whole-tree miss is authoritative NOT FOUND");
    }

    #[test]
    fn evidence_pack_excerpt_contains_the_identifier_line() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(
            &c,
            "src/lib.rs",
            "fn top() {}\nfn verification_queue() {}\nfn bottom() {}\n",
            "r",
        );
        seed_memory(&c, "m1", "t", "the note describes `verification_queue`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/lib.rs','src/lib.rs','current',0,'r')",
            [],
        )
        .unwrap();
        let pack = evidence_pack(&c, "m1").unwrap();
        assert!(
            pack.excerpts.iter().any(|e| e.text.contains("fn verification_queue()")),
            "the bound-file excerpt contains the identifier's line: {:?}",
            pack.excerpts
        );
    }

    #[test]
    fn unverifiable_when_no_live_binding_and_zero_identifiers_resolve() {
        let c = mem_db();
        set_repo(&c, "r");
        // m1: no binding, no resolvable identifier → unverifiable.
        seed_memory(&c, "m1", "t", "a purely prose note with no code refs", "r");
        // m2: no binding but a resolvable identifier → NOT unverifiable.
        seed_file(&c, "src/lib.rs", "fn resolvable_thing() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) SELECT id, \
             'rust', 'resolvable_thing', 'function', 0, 0 FROM main.files WHERE path = \
             'src/lib.rs'",
            [],
        )
        .unwrap();
        seed_memory(&c, "m2", "t", "refs `resolvable_thing`", "r");
        // m3: a live binding → NOT unverifiable even with no resolvable identifier.
        seed_memory(&c, "m3", "t", "prose", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
             created_at_ms, repo_id) VALUES ('m3','symbol','foo','current',0,'r')",
            [],
        )
        .unwrap();

        let subjects: Vec<String> =
            unverifiable_findings(&c).unwrap().into_iter().map(|f| f.subject).collect();
        assert_eq!(
            subjects,
            vec!["m1".to_string()],
            "only the truly unverifiable memory is flagged"
        );
    }
}
