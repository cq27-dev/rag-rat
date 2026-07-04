//! Dream v2 pass 1 — the MODEL verdict pass (rag-rat's first generative-model dependency, #122).
//!
//! The deterministic pass-0 substrate (`verify`) builds the churn-skip [`verification_queue`] and a
//! citation-checkable [`evidence_pack`]; this module renders that pack into a single-turn prompt,
//! asks a [`VerdictModel`] for a `current | diverged` verdict, guards against fabricated citations,
//! and records the accepted verdict into `memory_reality` — NEVER touching a `repo_memories` row.
//!
//! Two surfaces the rest of dream consumes:
//!   - [`run_verdict_pass`] — the budgeted runner: queue → pack → prompt → verdict → on accept,
//!     UPSERT `memory_reality`. Stamps `body_hash` / `checked_inputs_hash` with the SAME
//!     comparators the queue reads, so the next run churn-skips an unchanged memory (the model is
//!     not re-invoked).
//!   - [`divergence_findings`] — `memory_divergence` findings derived EVERY run from the STORED
//!     `memory_reality` table (all `verdict='diverged'` rows joined to still-active memories), NOT
//!     from this run's fresh (budget-capped) checks. That is the resolve-trap fix: `dream_findings`
//!     sync auto-resolves any current finding not reported in a run, and the checks are
//!     budget-capped — so a finding derived from a fresh check would wrongly resolve for a
//!     merely-SKIPPED memory. Deriving from stored state means a finding resolves exactly when a
//!     RE-CHECK flips the stored verdict to `current` (or the memory goes inactive), never because
//!     of a skip. `unverifiable` is decided in pass 0 and never asked of the model; the model only
//!     proposes — #262's review flow decides, and nothing here mutates a memory's status.

use rusqlite::Connection;

use super::DreamFinding;
use super::model::VerdictModel;
use super::verify::{
    self, EvidencePack, VerificationQueueEntry, evidence_pack, verification_queue,
};
use crate::index::schema;

/// The verdict prompt version, stamped into `memory_reality.prompt_version`. Bump on any change to
/// [`VERDICT_PROMPT_HEAD`] or the pack rendering so a stale-prompt verdict is distinguishable.
pub(super) const PROMPT_VERSION: &str = "verify-pack-v1";

/// Rank for a `memory_divergence` finding — high, but below a broken-anchor's pass-0 signal.
const DIVERGENCE_RANK: f64 = 0.8;

/// The verdict-pass configuration handed to [`run_verdict_pass`]: the model to ask and how many
/// queued memories it may check this run (the budget the queue is capped at). Separate from
/// [`DreamOptions`] because it carries a borrow (the model) — `DreamOptions` stays `Copy`.
pub struct VerdictPass<'a> {
    pub model: &'a dyn VerdictModel,
    pub budget: usize,
}

/// The model's verdict for a note vs. the current code. Persisted through [`Verdict::as_db_str`] —
/// `unverifiable` is deliberately ABSENT (pass 0 decides it deterministically; a stray model
/// `unverifiable` is discarded by [`parse_verdict`], never stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Current,
    Diverged,
}

impl Verdict {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Diverged => "diverged",
        }
    }

    /// Parse the VERDICT word. `None` for `unverifiable` (pass-0 territory) or anything
    /// unrecognized — the caller discards the completion.
    fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "current" => Some(Self::Current),
            "diverged" => Some(Self::Diverged),
            _ => None,
        }
    }
}

/// Advisory direction of a divergence (which side is newer). Never load-bearing — a hint for the
/// human review flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    CodeAhead,
    NoteAhead,
    Unknown,
}

impl Direction {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::CodeAhead => "code_ahead",
            Self::NoteAhead => "note_ahead",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the DIRECTION word, defaulting to `unknown` for a missing/unrecognized value.
    fn parse(word: &str) -> Self {
        match word.trim().to_ascii_lowercase().as_str() {
            "code_ahead" => Self::CodeAhead,
            "note_ahead" => Self::NoteAhead,
            _ => Self::Unknown,
        }
    }
}

/// A parsed (not yet citation-checked) model completion.
#[derive(Debug, Clone)]
struct ParsedVerdict {
    verdict: Verdict,
    direction: Direction,
    /// The EVIDENCE lines (leading `- ` stripped), each of which the fabrication guard checks
    /// against the rendered pack.
    evidence: Vec<String>,
}

/// A verdict that PASSED the citation guard and is ready to record.
#[derive(Debug, Clone)]
struct AcceptedVerdict {
    verdict: Verdict,
    direction: Direction,
    /// The cited pack lines, stored verbatim in `memory_reality.evidence_json`.
    evidence: Vec<String>,
}

// ── The pass-1 runner ────────────────────────────────────────────────────────────────────────

/// Run the model verdict pass over the churn-skip queue (budget-capped). For each queued memory:
/// build the deterministic evidence pack, render the prompt, ask the model, guard citations, and on
/// accept UPSERT `memory_reality`. A discarded verdict (unverifiable/malformed/fabricated-twice) or
/// a memory not visible in scope simply writes nothing — the next run re-queues it. Repo-scoped;
/// never writes a `repo_memories` column.
pub(super) fn run_verdict_pass(
    conn: &Connection,
    pass: VerdictPass<'_>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let queue = verification_queue(conn, now_ms, pass.budget)?;
    if queue.is_empty() {
        return Ok(());
    }
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_id = scope.as_deref().unwrap_or("__unassigned__");
    // Informational only: the commit the index is currently at, recorded so a note describing
    // unmerged in-flight work is reviewable rather than looking arbitrarily stale.
    let checked_against_commit = indexed_commit(conn, &scope)?;

    for entry in queue {
        let pack = evidence_pack(conn, &entry.memory_id)?;
        let pack_text = render_pack(&pack);
        let binding = binding_label(conn, &entry.memory_id, &scope)?;
        let prompt = render_verdict_prompt(&entry, &binding, &pack_text);
        let Some(accepted) = obtain_verdict(pass.model, &prompt, &pack_text) else {
            continue;
        };
        let inputs_hash = verify::checked_inputs_hash(conn, &entry.memory_id, &scope)?;
        record_verdict(conn, RecordVerdict {
            memory_id: &entry.memory_id,
            repo_id,
            body: &entry.body,
            accepted: &accepted,
            checked_inputs_hash: &inputs_hash,
            checked_against_commit: checked_against_commit.as_deref(),
            model_id: pass.model.model_id(),
            now_ms,
        })?;
    }
    Ok(())
}

/// Ask the model once, parse, and run the fabrication guard: EVERY EVIDENCE line must appear in the
/// rendered pack (whitespace-normalized substring). An unmatched citation rejects the completion
/// and RETRIES ONCE; a second fabrication (or a model error) discards the verdict. A malformed /
/// stray `unverifiable` completion is discarded WITHOUT a retry (it is not a citation fault — pass
/// 0 owns unverifiable). Small models measurably fabricate evidence, so this guard is load-bearing.
fn obtain_verdict(
    model: &dyn VerdictModel,
    prompt: &str,
    pack_text: &str,
) -> Option<AcceptedVerdict> {
    for attempt in 1..=2 {
        let raw = match model.complete(prompt) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!(target: "rag_rat_core::dream::verdict", attempt, %err, "verdict model call failed; discarding this memory's verdict");
                return None;
            },
        };
        let Some(parsed) = parse_verdict(&raw) else {
            // Malformed or a stray `unverifiable` — discard, no retry (not a citation fault).
            tracing::debug!(target: "rag_rat_core::dream::verdict", "discarding unparseable/unverifiable verdict completion");
            return None;
        };
        if verdict_is_cited(pack_text, &parsed) {
            return Some(AcceptedVerdict {
                verdict: parsed.verdict,
                direction: parsed.direction,
                evidence: parsed.evidence,
            });
        }
        // Fabricated citation. Retry once, then discard.
        tracing::warn!(target: "rag_rat_core::dream::verdict", attempt, "verdict cited a line absent from the evidence pack (possible fabrication)");
    }
    None
}

// ── Prompt + pack rendering ──────────────────────────────────────────────────────────────────

/// The verdict prompt head — ported from the eval's `VERIFY_PACK_PROMPT` with the measured round-4
/// boundary fix stated plainly: a bound file that EXISTS while the note's named mechanisms are NOT
/// FOUND is `diverged / note_ahead`, NOT `unverifiable`. `unverifiable` is dropped from the model's
/// vocabulary entirely — pass 0 decides it deterministically and never asks the model.
const VERDICT_PROMPT_HEAD: &str =
    "You are auditing a repo-intelligence memory NOTE against the repository as it exists RIGHT \
     NOW. You are given a mechanically-generated EVIDENCE PACK from the current checkout: a \
     whole-tree resolution of every identifier the note mentions (an identifier marked \"NOT \
     FOUND anywhere in the source tree\" truly does not exist in the source — this is exhaustive, \
     not a failed search), and the current text of the note's bound file. The note was written in \
     the past: the code may have moved past it, or the note may describe in-flight work not \
     present in this checkout, or they may agree.

Output EXACTLY this format and nothing else:
VERDICT: current | diverged
DIRECTION: code_ahead | note_ahead | unknown
EVIDENCE:
- <one line copied verbatim from the EVIDENCE PACK below that supports the verdict>
REASON: <one sentence>

Meanings: current = the note's load-bearing claims are visible in the pack as described. diverged \
     = the pack clearly contradicts a load-bearing claim. code_ahead = the code changed after the \
     note was written. note_ahead = the note describes work this checkout does not contain yet — \
     for example, the mechanisms or functions the note says were added are marked NOT FOUND WHILE \
     the note's bound file DOES exist; that is diverged / note_ahead, NOT a reason to give up. \
     DIRECTION is \"unknown\" unless VERDICT is diverged and you can tell which side is newer. \
     Every EVIDENCE line must be copied verbatim from the EVIDENCE PACK below — never invent one.";

/// Render the full single-turn prompt for one queued memory. Built by concatenation (not `format!`)
/// because the note body and pack text can contain literal `{`/`}`.
fn render_verdict_prompt(entry: &VerificationQueueEntry, binding: &str, pack_text: &str) -> String {
    let mut p =
        String::with_capacity(VERDICT_PROMPT_HEAD.len() + entry.body.len() + pack_text.len());
    p.push_str(VERDICT_PROMPT_HEAD);
    p.push_str("\n\nNOTE (anchored to ");
    p.push_str(binding);
    p.push_str("):\nTITLE: ");
    p.push_str(&entry.title);
    p.push('\n');
    p.push_str(&entry.body);
    p.push_str("\n\nEVIDENCE PACK:\n");
    p.push_str(pack_text);
    p
}

/// Render the deterministic [`EvidencePack`] into the prompt's EVIDENCE PACK section: an identifier
/// resolution table followed by bound-file excerpt blocks, in the pack's already-stable order
/// (identifiers sorted, excerpts by path+line). Every rendered line is a citable target for the
/// fabrication guard; excerpt lines carry a `path:line:` prefix so a precise citation resolves.
pub(super) fn render_pack(pack: &EvidencePack) -> String {
    let mut s = String::new();
    s.push_str("IDENTIFIERS (resolved against the whole source tree):\n");
    if pack.identifiers.is_empty() {
        s.push_str("- (no identifiers extracted)\n");
    } else {
        for id in &pack.identifiers {
            s.push_str("- `");
            s.push_str(&id.identifier);
            s.push_str("` -> ");
            s.push_str(&id.resolution);
            s.push('\n');
        }
    }
    s.push_str("\nBOUND-FILE EXCERPTS (current source):\n");
    if pack.excerpts.is_empty() {
        s.push_str("(no bound-file excerpts)\n");
    } else {
        for ex in &pack.excerpts {
            s.push_str(&ex.path);
            s.push(':');
            s.push_str(&ex.start_line.to_string());
            s.push('-');
            s.push_str(&ex.end_line.to_string());
            s.push('\n');
            for (offset, line) in ex.text.split('\n').enumerate() {
                let line_no = ex.start_line + offset as i64;
                s.push_str(&ex.path);
                s.push(':');
                s.push_str(&line_no.to_string());
                s.push_str(": ");
                s.push_str(line);
                s.push('\n');
            }
        }
    }
    s
}

/// The note's binding label for the prompt header — its first bound file path, or a conceptual-note
/// fallback (matching the eval harness's `bind_of`).
fn binding_label(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<String> {
    Ok(verify::bound_file_paths(conn, memory_id, scope)?
        .into_iter()
        .next()
        .unwrap_or_else(|| "(no source binding — a conceptual note)".to_string()))
}

// ── Parsing + the fabrication guard ──────────────────────────────────────────────────────────

/// Parse a model completion into a [`ParsedVerdict`]. Tolerant of surrounding prose: it scans for
/// the `VERDICT:` / `DIRECTION:` / `EVIDENCE:` / `REASON:` markers (case-insensitive) anywhere in
/// the output. `None` when there is no recognizable `current`/`diverged` VERDICT — malformed output
/// and a stray `unverifiable` both discard.
fn parse_verdict(output: &str) -> Option<ParsedVerdict> {
    let mut verdict = None;
    let mut direction = Direction::Unknown;
    let mut evidence = Vec::new();
    let mut in_evidence = false;
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if let Some(rest) = strip_ci(line, "VERDICT:") {
            verdict = Verdict::parse(rest);
            in_evidence = false;
        } else if let Some(rest) = strip_ci(line, "DIRECTION:") {
            direction = Direction::parse(rest);
            in_evidence = false;
        } else if strip_ci(line, "EVIDENCE:").is_some() {
            in_evidence = true;
        } else if strip_ci(line, "REASON:").is_some() {
            in_evidence = false;
        } else if in_evidence && let Some(item) = line.strip_prefix('-') {
            let item = item.trim();
            if !item.is_empty() {
                evidence.push(item.to_string());
            }
        }
    }
    Some(ParsedVerdict { verdict: verdict?, direction, evidence })
}

/// The fabrication guard: EVERY EVIDENCE line must appear in the rendered pack
/// (whitespace-normalized substring), and there must be at least one — an empty-evidence verdict is
/// rejected too, so the model can't skip citing. `pack_text` is the exact string rendered into the
/// prompt.
fn verdict_is_cited(pack_text: &str, parsed: &ParsedVerdict) -> bool {
    if parsed.evidence.is_empty() {
        return false;
    }
    let pack_norm = normalize_ws(pack_text);
    parsed.evidence.iter().all(|line| {
        let cite = normalize_ws(line);
        !cite.is_empty() && pack_norm.contains(&cite)
    })
}

/// Collapse every whitespace run (incl. newlines) to a single space and trim — so a citation that
/// differs only in spacing/wrapping still matches a pack line.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Case-insensitive prefix strip: `Some(rest)` when `line` begins with `prefix` (ASCII), else
/// `None`. `prefix` is an ASCII marker, so the split index is a char boundary.
fn strip_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = line.as_bytes();
    if bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

// ── memory_reality write + memory_divergence derivation ───────────────────────────────────────

/// Params for the single `memory_reality` UPSERT — one struct so the writer isn't a long positional
/// train of same-typed strings.
struct RecordVerdict<'a> {
    memory_id: &'a str,
    repo_id: &'a str,
    body: &'a str,
    accepted: &'a AcceptedVerdict,
    checked_inputs_hash: &'a str,
    checked_against_commit: Option<&'a str>,
    model_id: &'a str,
    now_ms: i64,
}

/// UPSERT the accepted verdict into `memory_reality` (PK `(repo_id, memory_id)`), stamping the
/// churn-skip comparators (`body_hash`, `checked_inputs_hash`) exactly as the queue reads them so
/// the next run skips an unchanged memory, plus the verdict, advisory direction, cited evidence,
/// model id, prompt version, and check timestamp. NEVER writes a `repo_memories` column.
fn record_verdict(conn: &Connection, r: RecordVerdict<'_>) -> rusqlite::Result<()> {
    let body_hash = crate::index::hex_sha256(r.body.as_bytes());
    // Store the cited pack lines as a JSON array so `divergence_findings` can render a compact,
    // stable evidence string from them.
    let evidence_json =
        serde_json::to_string(&r.accepted.evidence).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, body_hash, verdict, direction, \
         checked_against_commit, checked_inputs_hash, evidence_json, model_id, prompt_version, \
         checked_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(repo_id, \
         memory_id) DO UPDATE SET body_hash = excluded.body_hash, verdict = excluded.verdict, \
         direction = excluded.direction, checked_against_commit = \
         excluded.checked_against_commit, checked_inputs_hash = excluded.checked_inputs_hash, \
         evidence_json = excluded.evidence_json, model_id = excluded.model_id, prompt_version = \
         excluded.prompt_version, checked_at_ms = excluded.checked_at_ms",
        rusqlite::params![
            r.memory_id,
            r.repo_id,
            body_hash,
            r.accepted.verdict.as_db_str(),
            r.accepted.direction.as_db_str(),
            r.checked_against_commit,
            r.checked_inputs_hash,
            evidence_json,
            r.model_id,
            PROMPT_VERSION,
            r.now_ms,
        ],
    )?;
    Ok(())
}

/// The commit the index is currently at (`repo_meta` `git_commit`), for informational
/// `checked_against_commit` stamping. `None` outside a repo scope or when unrecorded.
fn indexed_commit(conn: &Connection, scope: &Option<String>) -> rusqlite::Result<Option<String>> {
    match scope {
        Some(repo_id) => crate::index::repo_meta(conn, repo_id, "git_commit"),
        None => Ok(None),
    }
}

/// `memory_divergence` findings, derived EVERY run from the STORED `memory_reality` — every
/// `verdict='diverged'` row whose memory is still active, repo-scoped. NOT from this run's fresh
/// (budget-capped) checks: because `dream_findings` sync auto-resolves any finding not reported in
/// a run, deriving from fresh checks would resolve findings for merely-SKIPPED memories. Reading
/// the stored table means a divergence finding resolves exactly when a RE-CHECK flips the verdict
/// to `current` (row no longer `diverged`) or the memory goes inactive — never on a skip. Mirrors
/// how `verify::unverifiable_findings` runs over the full population for the same reason.
pub(super) fn divergence_findings(conn: &Connection) -> rusqlite::Result<Vec<DreamFinding>> {
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "m");
    let reality_clause = schema::periphery_repo_scope_clause(&scope, "mr");
    let mut stmt = conn.prepare(&format!(
        "SELECT mr.memory_id, mr.direction, mr.evidence_json FROM memory_reality mr JOIN \
         repo_memories m ON m.id = mr.memory_id{mem_clause} WHERE mr.verdict = 'diverged' AND \
         m.status = 'active'{reality_clause} ORDER BY mr.memory_id"
    ))?;
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(memory_id, direction, evidence_json)| {
            let direction = direction.unwrap_or_else(|| "unknown".to_string());
            let cited = compact_evidence(evidence_json.as_deref());
            DreamFinding {
                kind: "memory_divergence".into(),
                subject: memory_id,
                // Evidence is derived from the STORED row, so it is stable across skip-runs
                // (refresh, not supersede) and only changes when a re-check
                // rewrites the row.
                evidence: format!(
                    "model verdict: diverged (direction: {direction}); cited: {cited} [reality]"
                ),
                rank: DIVERGENCE_RANK,
            }
        })
        .collect())
}

/// Render the stored `evidence_json` (a JSON array of cited pack lines) into a compact, stable,
/// bounded one-line string for a divergence finding's evidence. Falls back to the raw value when it
/// is not a JSON array.
fn compact_evidence(evidence_json: Option<&str>) -> String {
    const MAX_LEN: usize = 200;
    let joined = match evidence_json {
        Some(raw) => serde_json::from_str::<Vec<String>>(raw)
            .map(|lines| lines.join(" | "))
            .unwrap_or_else(|_| raw.to_string()),
        None => String::new(),
    };
    let joined = normalize_ws(&joined);
    if joined.chars().count() > MAX_LEN {
        let mut truncated = joined.chars().take(MAX_LEN).collect::<String>();
        truncated.push('…');
        truncated
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::mock::MockVerdictModel;
    use super::super::tests::{mem_db, set_repo};
    use super::*;

    fn seed_memory(c: &Connection, id: &str, title: &str, body: &str, repo_id: &str) {
        c.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
             (?1,'Invariant',?2,?3,'high','active','agent',1,1,'agent','v1',?4)",
            rusqlite::params![id, title, body, repo_id],
        )
        .unwrap();
    }

    fn seed_symbol_file(c: &Connection, path: &str, symbol: &str, repo_id: &str) {
        c.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES \
             (?1,'rust','source',?2,0,0,'','',?3,0)",
            rusqlite::params![path, format!("sha-{path}"), repo_id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) SELECT id, \
             'rust', ?2, 'function', 0, 0 FROM main.files WHERE path = ?1",
            rusqlite::params![path, symbol],
        )
        .unwrap();
    }

    /// A well-formed `current` completion citing an identifier known to be in the pack.
    fn current_citing(ident: &str) -> String {
        format!("VERDICT: current\nDIRECTION: unknown\nEVIDENCE:\n- `{ident}`\nREASON: matches.")
    }

    fn diverged_citing(ident: &str) -> String {
        format!(
            "VERDICT: diverged\nDIRECTION: note_ahead\nEVIDENCE:\n- `{ident}`\nREASON: not \
             present."
        )
    }

    // ── parsing ────────────────────────────────────────────────────────────────

    #[test]
    fn parse_reads_current_diverged_and_defaults_direction() {
        let current = parse_verdict("VERDICT: current\nEVIDENCE:\n- foo\nREASON: ok").unwrap();
        assert_eq!(current.verdict, Verdict::Current);
        assert_eq!(current.direction, Direction::Unknown, "missing DIRECTION defaults to unknown");
        assert_eq!(current.evidence, vec!["foo".to_string()]);

        let diverged = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nEVIDENCE:\n- a\n- b\nREASON: x",
        )
        .unwrap();
        assert_eq!(diverged.verdict, Verdict::Diverged);
        assert_eq!(diverged.direction, Direction::CodeAhead);
        assert_eq!(diverged.evidence, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_discards_malformed_and_stray_unverifiable() {
        assert!(parse_verdict("I could not determine anything").is_none(), "no VERDICT → discard");
        assert!(
            parse_verdict("VERDICT: unverifiable\nEVIDENCE:\n- x\nREASON: y").is_none(),
            "a stray `unverifiable` from the model is discarded, never stored"
        );
        assert!(parse_verdict("VERDICT: banana\nREASON: y").is_none(), "unknown verdict → discard");
    }

    // ── citation guard ─────────────────────────────────────────────────────────

    #[test]
    fn citation_guard_accepts_a_real_pack_line_and_rejects_fabrication() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let good = parse_verdict(&current_citing("real_symbol")).unwrap();
        assert!(verdict_is_cited(pack, &good), "a citation present in the pack is accepted");

        let fabricated = parse_verdict(&current_citing("ghost_symbol")).unwrap();
        assert!(
            !verdict_is_cited(pack, &fabricated),
            "a citation absent from the pack is rejected"
        );

        let empty =
            parse_verdict("VERDICT: current\nDIRECTION: unknown\nEVIDENCE:\nREASON: none").unwrap();
        assert!(!verdict_is_cited(pack, &empty), "an empty-evidence verdict is rejected too");
    }

    #[test]
    fn obtain_verdict_retries_once_then_accepts_bad_then_good() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        // First completion fabricates; the retry cites a real line → accepted.
        let model =
            MockVerdictModel::new([current_citing("ghost_symbol"), current_citing("real_symbol")]);
        let accepted = obtain_verdict(&model, "prompt", pack).expect("bad-then-good is accepted");
        assert_eq!(accepted.verdict, Verdict::Current);
        assert_eq!(model.calls(), 2, "one fabrication triggers exactly one retry");
    }

    #[test]
    fn obtain_verdict_discards_after_two_fabrications() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let model = MockVerdictModel::new([current_citing("ghost_a"), current_citing("ghost_b")]);
        assert!(
            obtain_verdict(&model, "prompt", pack).is_none(),
            "two fabrications discard the verdict"
        );
        assert_eq!(model.calls(), 2, "retried exactly once");
    }

    // ── prompt + pack rendering ──────────────────────────────────────────────────

    #[test]
    fn prompt_renders_pack_deterministically_with_table_and_excerpts() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_symbol_file(&c, "src/lib.rs", "real_symbol", "r");
        c.execute(
            "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
             text_hash) SELECT id,'code',0,0,1,1,'th' FROM main.files WHERE path='src/lib.rs'",
            [],
        )
        .unwrap();
        let chunk_id = c.last_insert_rowid();
        crate::index::chunk_text_store::seed_chunk_text(&c, chunk_id, "fn real_symbol() {}\n")
            .unwrap();
        seed_memory(&c, "m1", "note", "describes `real_symbol` and `ghost_symbol`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/lib.rs','src/lib.rs','current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let rendered_a = render_pack(&pack);
        let rendered_b = render_pack(&pack);
        assert_eq!(rendered_a, rendered_b, "pack rendering is deterministic");
        assert!(rendered_a.contains("`real_symbol` -> symbol"), "identifier table present");
        assert!(rendered_a.contains("NOT FOUND"), "the ghost identifier's NOT FOUND is rendered");
        assert!(rendered_a.contains("src/lib.rs:1: fn real_symbol()"), "excerpt line present");

        let entry = verification_queue(&c, 1, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.memory_id == "m1")
            .unwrap();
        let prompt = render_verdict_prompt(&entry, "src/lib.rs", &rendered_a);
        assert!(prompt.contains("VERDICT: current | diverged"), "the verdict format is stated");
        assert!(prompt.contains("EVIDENCE PACK:"), "the pack section header is present");
        assert!(prompt.contains("TITLE: note"), "the note title is included");
        assert!(prompt.contains("`real_symbol` -> symbol"), "the pack is embedded in the prompt");
    }

    // ── write path + churn-skip ──────────────────────────────────────────────────

    /// Seed m1 with a resolvable identifier (so it is verifiable) and no bindings, run the verdict
    /// pass, and return the connection. m1 is NeverChecked → the model is invoked once.
    fn seeded_verifiable_repo() -> Connection {
        let c = mem_db();
        set_repo(&c, "r");
        seed_symbol_file(&c, "src/lib.rs", "resolvable_thing", "r");
        seed_memory(&c, "m1", "note", "describes `resolvable_thing`", "r");
        c
    }

    #[test]
    fn verdict_pass_upserts_memory_reality_with_all_stamps() {
        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new([diverged_citing("resolvable_thing")]);
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 5000).unwrap();

        let row: (String, String, String, String, String, i64) = c
            .query_row(
                "SELECT verdict, direction, model_id, prompt_version, body_hash, checked_at_ms \
                 FROM memory_reality WHERE memory_id='m1' AND repo_id='r'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(row.0, "diverged");
        assert_eq!(row.1, "note_ahead");
        assert_eq!(row.2, "mock-verdict-model");
        assert_eq!(row.3, PROMPT_VERSION);
        assert_eq!(row.4, crate::index::hex_sha256(b"describes `resolvable_thing`"));
        assert_eq!(row.5, 5000);
    }

    #[test]
    fn second_run_churn_skips_and_body_edit_re_invokes() {
        let c = seeded_verifiable_repo();
        // Two responses queued; a churn-skipped second run must consume only the first.
        let model = MockVerdictModel::new([
            current_citing("resolvable_thing"),
            current_citing("resolvable_thing"),
        ]);
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 1000).unwrap();
        assert_eq!(model.calls(), 1, "the never-checked memory is verified once");

        // Unchanged memory → the queue churn-skips → the model is NOT re-invoked.
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 2000).unwrap();
        assert_eq!(
            model.calls(),
            1,
            "an unchanged verified memory is churn-skipped (no model call)"
        );

        // A body edit changes body_hash → re-enqueued → the model runs again.
        c.execute(
            "UPDATE repo_memories SET body='describes `resolvable_thing` (edited)' WHERE id='m1'",
            [],
        )
        .unwrap();
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 3000).unwrap();
        assert_eq!(model.calls(), 2, "a body edit re-invokes the model");
    }

    // ── divergence-finding lifecycle (the resolve trap) ──────────────────────────

    fn divergence_subjects(findings: &[DreamFinding]) -> Vec<String> {
        findings
            .iter()
            .filter(|f| f.kind == "memory_divergence")
            .map(|f| f.subject.clone())
            .collect()
    }

    #[test]
    fn diverged_opens_finding_then_skip_keeps_it_then_recheck_current_resolves_it() {
        use super::super::{DreamOptions, dream_run_with_verdict};

        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new([
            diverged_citing("resolvable_thing"), // run 1: opens the divergence finding
            current_citing("resolvable_thing"),  // run 3 (after body edit): flips to current
        ]);
        let opts = DreamOptions { now_ms: 1000, limit: 10, verify: true };

        // Run 1: diverged verdict → memory_divergence finding opens.
        let r1 = dream_run_with_verdict(&c, opts, Some(VerdictPass { model: &model, budget: 10 }))
            .unwrap();
        assert_eq!(divergence_subjects(&r1.findings), vec!["m1".to_string()], "divergence opens");
        assert_eq!(model.calls(), 1);

        // Run 2: the memory is UNCHANGED → the queue churn-skips (no model call), but the finding
        // is derived from the STORED diverged row, so it stays open (the resolve-trap
        // regression).
        let opts2 = DreamOptions { now_ms: 2000, ..opts };
        let r2 = dream_run_with_verdict(&c, opts2, Some(VerdictPass { model: &model, budget: 10 }))
            .unwrap();
        assert_eq!(model.calls(), 1, "run 2 churn-skips the model");
        assert_eq!(
            divergence_subjects(&r2.findings),
            vec!["m1".to_string()],
            "a skipped memory keeps its divergence finding open (not wrongly resolved)"
        );

        // Run 3: edit the body to force a re-check; the model now returns `current` → the stored
        // verdict flips → the divergence finding is no longer reported → sync resolves it.
        c.execute(
            "UPDATE repo_memories SET body='describes `resolvable_thing` v2' WHERE id='m1'",
            [],
        )
        .unwrap();
        let opts3 = DreamOptions { now_ms: 3000, ..opts };
        let r3 = dream_run_with_verdict(&c, opts3, Some(VerdictPass { model: &model, budget: 10 }))
            .unwrap();
        assert_eq!(model.calls(), 2, "the body edit re-invokes the model");
        assert!(
            divergence_subjects(&r3.findings).is_empty(),
            "a re-check flipping to current resolves the divergence finding"
        );
        let open: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM dream_findings WHERE kind='memory_divergence' AND \
                 status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0, "no open memory_divergence finding remains");
    }

    #[test]
    fn model_pass_never_mutates_a_repo_memories_column() {
        use super::super::{DreamOptions, dream_run_with_verdict};

        let c = seeded_verifiable_repo();
        let snap = |c: &Connection| -> (String, String, String) {
            c.query_row(
                "SELECT body, status, confidence FROM repo_memories WHERE id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        let before = snap(&c);
        let model = MockVerdictModel::new([diverged_citing("resolvable_thing")]);
        let opts = DreamOptions { now_ms: 1000, limit: 10, verify: true };
        dream_run_with_verdict(&c, opts, Some(VerdictPass { model: &model, budget: 10 })).unwrap();
        assert_eq!(before, snap(&c), "the model verdict pass leaves repo_memories byte-identical");
        // ...but it DID write a diverged verdict into the sibling table.
        let verdict: String = c
            .query_row("SELECT verdict FROM memory_reality WHERE memory_id='m1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(verdict, "diverged");
    }

    // ── poison-sibling: repo scoping ─────────────────────────────────────────────

    #[test]
    fn verdict_writes_and_divergence_are_repo_scoped() {
        use super::super::{DreamOptions, dream_run_with_verdict};

        // `repo_memories.id` is a global PK, so the two repos hold DISTINCT ids (m1 in r1, m2 in
        // r2); isolation is proved by the `repo_id` scope predicates, not an id collision.
        let c = mem_db();
        // repo r1 gets a diverged verdict for its m1.
        set_repo(&c, "r1");
        seed_symbol_file(&c, "src/a.rs", "thing_one", "r1");
        seed_memory(&c, "m1", "note", "describes `thing_one`", "r1");
        let model_r1 = MockVerdictModel::new([diverged_citing("thing_one")]);
        run_verdict_pass(&c, VerdictPass { model: &model_r1, budget: 10 }, 1000).unwrap();

        // The reality row is stamped with r1.
        let reality_repo: String = c
            .query_row("SELECT repo_id FROM memory_reality WHERE memory_id='m1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reality_repo, "r1", "the verdict is written under the active repo only");

        // Switch to repo r2 and run dream: r2 must NOT see r1's diverged row (its scoped divergence
        // query filters `mr.repo_id = 'r2'`), and r2 only verifies its OWN queued memory.
        set_repo(&c, "r2");
        seed_symbol_file(&c, "src/b.rs", "thing_two", "r2");
        seed_memory(&c, "m2", "note", "describes `thing_two`", "r2");
        let model_r2 = MockVerdictModel::new([current_citing("thing_two")]);
        let opts = DreamOptions { now_ms: 2000, limit: 10, verify: true };
        let r2 =
            dream_run_with_verdict(&c, opts, Some(VerdictPass { model: &model_r2, budget: 10 }))
                .unwrap();
        assert!(
            divergence_subjects(&r2.findings).is_empty(),
            "repo r2 does not see repo r1's diverged verdict as a divergence finding"
        );
        let r2_verdict: String = c
            .query_row(
                "SELECT verdict FROM memory_reality WHERE repo_id='r2' AND memory_id='m2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            r2_verdict, "current",
            "r2's own verdict is current, isolated from r1's diverged"
        );
        // r2 must NOT have written a reality row under r1's memory id.
        let r2_touched_r1: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM memory_reality WHERE repo_id='r2' AND memory_id='m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r2_touched_r1, 0, "r2's pass never wrote under repo r1's memory");

        // And back in r1, its divergence finding is intact.
        set_repo(&c, "r1");
        let r1_divergence = divergence_findings(&c).unwrap();
        assert_eq!(divergence_subjects(&r1_divergence), vec!["m1".to_string()]);
    }
}
