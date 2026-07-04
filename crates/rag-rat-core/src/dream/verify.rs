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
    /// The memory body changed since it was last checked (`body_hash` mismatch).
    BodyChanged,
    /// A bound file changed since the last check (`checked_inputs_hash` mismatch).
    InputsChanged,
}

impl VerificationReason {
    /// Priority rank: broken anchors first, then never-checked, then body churn, then input churn.
    fn rank(self) -> f64 {
        match self {
            Self::AnchorBroken => 1.0,
            Self::NeverChecked => 0.75,
            Self::BodyChanged => 0.5,
            Self::InputsChanged => 0.25,
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
/// `budget`. A memory is enqueued when it has no `memory_reality` row, its body changed
/// (`body_hash`), or a bound file changed (`checked_inputs_hash`); a stale/gone anchor (the doctor
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
        let reason = queue_reason(conn, &memory_id, &body, &broken, &scope, &reality_clause)?;
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
/// `memory_reality` comparators are consulted FIRST: a row whose `body_hash` AND
/// `checked_inputs_hash` still match the current body + bound-file inputs skips (`None`) REGARDLESS
/// of anchor status — the stored verdict stands, and a broken anchor is surfaced by `memory doctor`
/// and the unverifiable/divergence findings, not by re-checking an unchanged note (else a
/// broken-anchor memory would re-enqueue every run at the top rank and starve NeverChecked; a
/// genuinely changed bound-file set changes `checked_inputs_hash` and re-enqueues via InputsChanged
/// anyway). A memory that DOES need a first/re-check takes the top `AnchorBroken` rank when its
/// anchor is broken, otherwise the specific churn reason (NeverChecked / BodyChanged /
/// InputsChanged).
fn queue_reason(
    conn: &Connection,
    memory_id: &str,
    body: &str,
    broken: &HashSet<String>,
    scope: &Option<String>,
    reality_clause: &str,
) -> rusqlite::Result<Option<VerificationReason>> {
    let stored: Option<(String, Option<String>)> = conn
        .query_row(
            &format!(
                "SELECT body_hash, checked_inputs_hash FROM memory_reality WHERE memory_id = \
                 ?1{reality_clause}"
            ),
            [memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((stored_body_hash, stored_inputs_hash)) = stored else {
        // Never checked → needs a first check; a broken anchor takes the top rank.
        return Ok(Some(if broken.contains(memory_id) {
            VerificationReason::AnchorBroken
        } else {
            VerificationReason::NeverChecked
        }));
    };
    let body_changed = stored_body_hash != crate::index::hex_sha256(body.as_bytes());
    let current_inputs = checked_inputs_hash(conn, memory_id, scope)?;
    let inputs_changed = stored_inputs_hash.as_deref() != Some(current_inputs.as_str());
    if !body_changed && !inputs_changed {
        // Verified AND unchanged — churn-skip regardless of anchor status (the stored verdict
        // stands; anchor breakage is surfaced elsewhere).
        return Ok(None);
    }
    // A change since the last check → re-check. A broken anchor still takes the top rank.
    Ok(Some(if broken.contains(memory_id) {
        VerificationReason::AnchorBroken
    } else if body_changed {
        VerificationReason::BodyChanged
    } else {
        VerificationReason::InputsChanged
    }))
}

/// sha256 over the sorted, de-duplicated current sha256s of the memory's bound files (through the
/// `files` view, so it is repo- and generation-scoped) — the cheap churn comparator that beats a
/// commit-ancestry walk. An empty bound-file set hashes to a stable sentinel.
///
/// `pub(super)` so the phase-B verdict pass (`verdict`) recomputes it EXACTLY as the queue's
/// comparator does when it stamps `memory_reality.checked_inputs_hash` — same function, so the next
/// run churn-skips instead of re-checking.
pub(super) fn checked_inputs_hash(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<String> {
    let mut shas = BTreeSet::new();
    for path in bound_file_paths(conn, memory_id, scope)? {
        let sha: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE path = ?1 ORDER BY sha256 LIMIT 1",
                [&path],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(sha) = sha {
            shas.insert(sha);
        }
    }
    let joined = shas.into_iter().collect::<Vec<_>>().join("\u{1f}");
    Ok(crate::index::hex_sha256(joined.as_bytes()))
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
    if BARE_NAME_RE.is_match(symbol_name)
        && let Some(loc) = resolve_symbol(conn, symbol_name)?
    {
        return Ok(format!("symbol {loc}"));
    }
    if let Some(path) = resolve_file_segment(ident, file_paths) {
        return Ok(format!("file {path}"));
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

/// The first (path-ordered) live symbol whose `name` matches, as `path::name`, through the `files`
/// view (repo-scoped). `None` when the name is unknown anywhere in the tree.
fn resolve_symbol(conn: &Connection, name: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT f.path, s.name FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.name = ?1 \
         ORDER BY f.path, s.name LIMIT 1",
        [name],
        |r| Ok(format!("{}::{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
}

/// The first (sorted) indexed path that equals `ident` or ends in `/ident` — suffix-aware exactly
/// like `stale_reference`'s resolver, so prose shorthand (`src/lib.rs`) still resolves.
fn resolve_file_segment(ident: &str, file_paths: &[String]) -> Option<String> {
    if let Some(p) = file_paths.iter().find(|p| p.as_str() == ident) {
        return Some(p.clone());
    }
    let suffix = format!("/{ident}");
    file_paths.iter().find(|p| p.ends_with(&suffix)).cloned()
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
    for path in bound_file_paths(conn, memory_id, scope)? {
        if used_lines >= MAX_EXCERPT_LINES {
            break;
        }
        let Some(file_id) = file_id_for_path(conn, &path)? else {
            continue;
        };
        let lines = file_lines(conn, file_id)?;
        for (start, end) in identifier_windows(&lines, identifiers) {
            if used_lines >= MAX_EXCERPT_LINES {
                break;
            }
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

/// The active-repo `files.id` for `path` (through the view), or `None` when the path is not
/// indexed.
fn file_id_for_path(conn: &Connection, path: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT id FROM files WHERE path = ?1 ORDER BY id LIMIT 1", [path], |r| r.get(0))
        .optional()
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

    fn body_hash(body: &str) -> String {
        crate::index::hex_sha256(body.as_bytes())
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
            "INSERT INTO memory_reality(memory_id, repo_id, body_hash, checked_inputs_hash, \
             checked_at_ms) VALUES ('m1','r',?1,?2,1000)",
            rusqlite::params![body_hash("a plain note"), inputs],
        )
        .unwrap();
        let q = verification_queue(&c, 2000, 10).unwrap();
        assert!(q.is_empty(), "a verified + unchanged memory is churn-skipped: {q:?}");
    }

    #[test]
    fn queue_skips_a_broken_anchor_with_matching_hashes_so_it_never_starves_never_checked() {
        let c = mem_db();
        set_repo(&c, "r");
        // m1: a GONE binding (broken anchor) BUT a stored reality row whose body_hash + inputs
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
            "INSERT INTO memory_reality(memory_id, repo_id, body_hash, checked_inputs_hash, \
             checked_at_ms) VALUES ('m1','r',?1,?2,1000)",
            rusqlite::params![body_hash("a plain note"), inputs],
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
            "INSERT INTO memory_reality(memory_id, repo_id, body_hash, checked_inputs_hash, \
             checked_at_ms) VALUES ('m1','r',?1,?2,1000)",
            rusqlite::params![body_hash("note about src/lib.rs"), inputs],
        )
        .unwrap();
        assert!(
            verification_queue(&c, 1, 10).unwrap().is_empty(),
            "baseline: verified + unchanged"
        );

        // Body edit → body_hash mismatch re-enqueues.
        c.execute("UPDATE repo_memories SET body = 'a rewritten note' WHERE id='m1'", []).unwrap();
        let q = verification_queue(&c, 1, 10).unwrap();
        assert_eq!(q.iter().map(|e| e.reason).collect::<Vec<_>>(), vec![
            VerificationReason::BodyChanged
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
