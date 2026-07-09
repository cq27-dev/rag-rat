//! Dream v2 pass 1 — the MODEL verdict pass (rag-rat's first generative-model dependency, #122).
//!
//! The deterministic pass-0 substrate (`verify`) builds the churn-skip [`verification_queue`] and a
//! citation-checkable [`evidence_pack`]; this module renders that pack into a single-turn prompt,
//! asks a [`VerdictModel`] for a `current | diverged` verdict, guards against fabricated citations,
//! and records the accepted verdict into `memory_reality` — NEVER touching a `repo_memories` row.
//!
//! Two surfaces the rest of dream consumes:
//!   - [`run_verdict_pass`] — the budgeted runner: queue → pack → prompt → verdict → on accept,
//!     UPSERT `memory_reality`. Stamps `content_hash` / `checked_inputs_hash` with the SAME
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
use super::failure::{
    self, DreamFailureReason, DreamModelFailure, DreamModelPass, FailureStamp, RecordFailure,
};
use super::model::VerdictModel;
use super::verify::{
    self, EvidencePack, IdentifierResolution, ResolutionKind, VerificationQueueEntry,
    evidence_pack, verification_queue,
};
use crate::index::schema;

/// The verdict prompt version, stamped into `memory_reality.prompt_version`. Bump on any change to
/// [`VERDICT_PROMPT_HEAD`] or the pack rendering so a stale-prompt verdict is distinguishable — a
/// bump re-queues every prior verdict (`VerificationReason::PromptChanged`) and the finding surface
/// stops reporting stale-prompt verdicts until they are re-checked. v2: the identifier resolver
/// gained a verbatim-text tier + a shape-split terminal, and the prompt teaches those labels, so v1
/// verdicts (which over-reported divergence off bare NOT-FOUND rows) are not comparable.
pub(crate) const PROMPT_VERSION: &str = "verify-pack-v2";

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
/// a memory not visible in scope writes no verdict; deterministic model rejections are recorded in
/// `memory_model_failures` so unchanged inputs do not re-call the model every run. Repo-scoped;
/// never writes a `repo_memories` column.
pub(super) fn run_verdict_pass(
    conn: &Connection,
    pass: VerdictPass<'_>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let queue = verification_queue(conn, now_ms, usize::MAX)?;
    if queue.is_empty() {
        return Ok(());
    }
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_id = scope.as_deref().unwrap_or("__unassigned__");
    // Informational only: the commit the index is currently at, recorded so a note describing
    // unmerged in-flight work is reviewable rather than looking arbitrarily stale.
    let checked_against_commit = indexed_commit(conn, &scope)?;

    let mut processed = 0usize;
    for entry in queue {
        if processed >= pass.budget {
            break;
        }
        let pack = evidence_pack(conn, &entry.memory_id)?;
        let inputs_hash = verify::checked_inputs_hash(conn, &entry.memory_id, &scope)?;
        let content_hash = verify::note_content_hash(&entry.title, &entry.body);
        let failure_stamp = FailureStamp {
            memory_id: &entry.memory_id,
            repo_id,
            pass: DreamModelPass::Verify,
            content_hash: &content_hash,
            checked_inputs_hash: Some(&inputs_hash),
            prompt_version: PROMPT_VERSION,
            model_id: pass.model.model_id(),
        };
        if failure::blocking_failure_is_current(conn, &failure_stamp)? {
            continue;
        }
        processed += 1;
        // An uncitable pack (no identifiers, no excerpts) can only produce a discarded verdict, so
        // skip the model and record a TERMINAL verdict-less row: it stamps the churn-skip
        // comparators so the memory does not re-queue every run (starving later memories), while a
        // NULL verdict stays inert for verdict markers and divergence findings. A
        // body/inputs/prompt change re-queues it exactly like any other row.
        if !pack.is_citable() {
            record_uncitable(conn, Uncitable {
                memory_id: &entry.memory_id,
                repo_id,
                title: &entry.title,
                body: &entry.body,
                checked_inputs_hash: &inputs_hash,
                checked_against_commit: checked_against_commit.as_deref(),
                now_ms,
            })?;
            failure::clear_failure(conn, &failure_stamp)?;
            continue;
        }
        let pack_text = render_pack(&pack);
        let binding = binding_label(conn, &entry.memory_id, &scope)?;
        let prompt = render_verdict_prompt(&entry, &binding, &pack_text);
        let accepted = match obtain_verdict(pass.model, &prompt, &pack_text) {
            Ok(accepted) => accepted,
            Err(failure) => {
                failure::record_failure(conn, RecordFailure {
                    stamp: failure_stamp,
                    failure: &failure,
                    now_ms,
                })?;
                continue;
            },
        };
        record_verdict(conn, RecordVerdict {
            memory_id: &entry.memory_id,
            repo_id,
            title: &entry.title,
            body: &entry.body,
            accepted: &accepted,
            checked_inputs_hash: &inputs_hash,
            checked_against_commit: checked_against_commit.as_deref(),
            model_id: pass.model.model_id(),
            now_ms,
        })?;
        let failure_stamp = FailureStamp {
            memory_id: &entry.memory_id,
            repo_id,
            pass: DreamModelPass::Verify,
            content_hash: &content_hash,
            checked_inputs_hash: Some(&inputs_hash),
            prompt_version: PROMPT_VERSION,
            model_id: pass.model.model_id(),
        };
        failure::clear_failure(conn, &failure_stamp)?;
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
) -> Result<AcceptedVerdict, DreamModelFailure> {
    for attempt in 1..=2 {
        let raw = match model.complete(prompt) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!(target: "rag_rat_core::dream::verdict", attempt, %err, "verdict model call failed; discarding this memory's verdict");
                return Err(DreamModelFailure::with_detail(
                    DreamFailureReason::ModelCallFailed,
                    err.to_string(),
                ));
            },
        };
        let Some(parsed) = parse_verdict(&raw) else {
            // Malformed or a stray `unverifiable` — discard, no retry (not a citation fault).
            tracing::debug!(target: "rag_rat_core::dream::verdict", "discarding unparseable/unverifiable verdict completion");
            return Err(DreamModelFailure::new(DreamFailureReason::MalformedVerdict));
        };
        if verdict_is_cited(pack_text, &parsed) {
            return Ok(AcceptedVerdict {
                verdict: parsed.verdict,
                direction: parsed.direction,
                evidence: parsed.evidence,
            });
        }
        // Fabricated citation. Retry once, then discard.
        tracing::warn!(target: "rag_rat_core::dream::verdict", attempt, "verdict cited a line absent from the evidence pack (possible fabrication)");
    }
    Err(DreamModelFailure::new(DreamFailureReason::FabricatedEvidence))
}

// ── Prompt + pack rendering ──────────────────────────────────────────────────────────────────

/// The verdict prompt head, authored as markdown in `prompts/verdict_head.md` and embedded at
/// compile time via [`include_str!`] (no runtime IO or install-path lookup — the file ships inside
/// the binary). Ported from the eval's `VERIFY_PACK_PROMPT` with the measured round-4
/// boundary fix stated plainly: a bound file that EXISTS while the note's named mechanisms are NOT
/// FOUND is `diverged / note_ahead`, NOT `unverifiable`. `unverifiable` is dropped from the model's
/// vocabulary entirely — pass 0 decides it deterministically and never asks the model. Edits to the
/// prompt live in that `.md`; bump [`PROMPT_VERSION`] when they change. Trimmed at the render site
/// so a trailing newline in the file cannot perturb the rendered prompt (or its `PROMPT_VERSION`).
const VERDICT_PROMPT_HEAD: &str = include_str!("prompts/verdict_head.md");

/// Render the full single-turn prompt for one queued memory. Built by concatenation (not `format!`)
/// because the note body and pack text can contain literal `{`/`}`.
fn render_verdict_prompt(entry: &VerificationQueueEntry, binding: &str, pack_text: &str) -> String {
    let mut p =
        String::with_capacity(VERDICT_PROMPT_HEAD.len() + entry.body.len() + pack_text.len());
    p.push_str(VERDICT_PROMPT_HEAD.trim_end());
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
    // A `ResolutionKind::Unresolvable` span (a paraphrase / snippet / flag that is not code-shaped
    // and matches no text) carries no presence-or-absence signal, so it is NOT rendered — the model
    // never sees it and so cannot cite it to (wrongly) rule `diverged`. Symbol / file /
    // verbatim-text (presence) and NOT-FOUND (genuine absence) rows are all shown and citable.
    let shown: Vec<&IdentifierResolution> =
        pack.identifiers.iter().filter(|id| id.kind != ResolutionKind::Unresolvable).collect();
    if shown.is_empty() {
        s.push_str("- (no identifiers extracted)\n");
    } else {
        for id in shown {
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
            // A new VERDICT section RESETS the accumulated fields: a model that emits a scratchpad
            // / `<think>` block or self-corrects can produce more than one
            // VERDICT/EVIDENCE section, and only the LAST one is the answer. Without
            // the reset, evidence cited in an earlier (discarded) block could satisfy
            // the fabrication guard for a final verdict that changed its mind or
            // omitted evidence.
            verdict = Verdict::parse(rest);
            direction = Direction::Unknown;
            evidence.clear();
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

/// Minimum normalized (non-whitespace) length for a citation to count — a floor that keeps a bare
/// boilerplate token (`- source`, `verdict`, `note`) from satisfying the guard by matching a
/// content-line substring. Set just below the shortest LEGITIMATE citation the corpus produces (a
/// backticked identifier like `` `thing_two` `` = 11 chars); a higher floor would reject a real
/// short-identifier citation, so the content-line filter — not this length — is the primary
/// defense.
const MIN_CITATION_CHARS: usize = 10;

/// The fabrication guard: EVERY EVIDENCE line must appear in a pack CONTENT line
/// (whitespace-normalized substring) — an identifier-table entry or a bound-file excerpt line, NOT
/// a section header or boilerplate (see [`is_pack_content_line`]) — and be at least
/// [`MIN_CITATION_CHARS`] non-whitespace chars long. There must also be at least one line: an
/// empty-evidence verdict is rejected too, so the model can't skip citing. `pack_text` is the exact
/// string rendered into the prompt. Matching only content lines (not the flattened whole pack) is
/// what stops a citation of pack boilerplate — a header the guard itself emits — from passing.
fn verdict_is_cited(pack_text: &str, parsed: &ParsedVerdict) -> bool {
    if parsed.evidence.is_empty() {
        return false;
    }
    let content: Vec<String> =
        pack_text.lines().filter(|line| is_pack_content_line(line)).map(normalize_ws).collect();
    parsed.evidence.iter().all(|line| {
        let cite = normalize_ws(line);
        cite.chars().filter(|c| !c.is_whitespace()).count() >= MIN_CITATION_CHARS
            && !is_bare_locator(&cite)
            && content.iter().any(|c| c.contains(&cite))
    })
}

/// Whether a citation is ONLY a `path:line` (or `path:line:`) locator with no source text after it.
/// An excerpt content line renders as `path:line: <code>`, so a bare `src/lib.rs:12` is a substring
/// of it and would satisfy the substring guard without citing any actual code or identifier — a
/// content-free citation. Requiring text BEYOND the locator forces the model to cite the source it
/// claims supports the verdict. A backticked identifier citation (`` `thing_two` ``) has no
/// `:<line>` tail, so it is never a bare locator.
fn is_bare_locator(cite: &str) -> bool {
    let trimmed = cite.trim().trim_end_matches(':');
    // Any whitespace means there is text beyond the locator token — not bare.
    if trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    // A locator ends `<path>:<digits>`; treat that shape (nothing after the line number) as bare.
    match trimmed.rsplit_once(':') {
        Some((path, line)) =>
            !path.is_empty() && !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Whether a rendered pack line is CITABLE CONTENT — an identifier-table entry (`` - `ident` -> …
/// ``) or a bound-file excerpt line (`path:line: text`) — rather than a section header or
/// boilerplate (`IDENTIFIERS (…):`, `BOUND-FILE EXCERPTS (…):`, `- (no identifiers extracted)`,
/// `(no bound-file excerpts)`, or a `path:start-end` range header). The citation guard matches only
/// these, so pack scaffolding can't satisfy a fabricated citation. Mirrors [`render_pack`]'s
/// emitted shapes.
fn is_pack_content_line(line: &str) -> bool {
    let line = line.trim();
    // Identifier-table entry: `- ` then a backtick span. The `- (no identifiers extracted)`
    // boilerplate starts with `- (`, not a backtick, so it is excluded.
    if let Some(rest) = line.strip_prefix("- ") {
        return rest.starts_with('`');
    }
    // Bound-file excerpt line: a `path:<digits>: text` locator (the first `": "` is preceded by an
    // ASCII digit). The `path:start-end` range headers use a dash and the section
    // headers/boilerplate carry no such locator, so only real excerpt lines match.
    match line.find(": ") {
        Some(idx) => line[..idx].bytes().next_back().is_some_and(|b| b.is_ascii_digit()),
        None => false,
    }
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
    title: &'a str,
    body: &'a str,
    accepted: &'a AcceptedVerdict,
    checked_inputs_hash: &'a str,
    checked_against_commit: Option<&'a str>,
    model_id: &'a str,
    now_ms: i64,
}

/// UPSERT the accepted verdict into `memory_reality` (PK `(repo_id, memory_id)`), stamping the
/// churn-skip comparators (`content_hash`, `checked_inputs_hash`) exactly as the queue reads them
/// so the next run skips an unchanged memory, plus the verdict, advisory direction, cited evidence,
/// model id, prompt version, and check timestamp. NEVER writes a `repo_memories` column.
fn record_verdict(conn: &Connection, r: RecordVerdict<'_>) -> rusqlite::Result<()> {
    let content_hash = verify::note_content_hash(r.title, r.body);
    // Store the cited pack lines as a JSON array so `divergence_findings` can render a compact,
    // stable evidence string from them.
    let evidence_json =
        serde_json::to_string(&r.accepted.evidence).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, direction, \
         checked_against_commit, checked_inputs_hash, evidence_json, model_id, prompt_version, \
         checked_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(repo_id, \
         memory_id) DO UPDATE SET content_hash = excluded.content_hash, verdict = \
         excluded.verdict, direction = excluded.direction, checked_against_commit = \
         excluded.checked_against_commit, checked_inputs_hash = excluded.checked_inputs_hash, \
         evidence_json = excluded.evidence_json, model_id = excluded.model_id, prompt_version = \
         excluded.prompt_version, checked_at_ms = excluded.checked_at_ms",
        rusqlite::params![
            r.memory_id,
            r.repo_id,
            content_hash,
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

/// Params for a terminal, verdict-less `memory_reality` row — an uncitable memory the verdict pass
/// checked but could not put to the model (see [`EvidencePack::is_citable`]).
struct Uncitable<'a> {
    memory_id: &'a str,
    repo_id: &'a str,
    title: &'a str,
    body: &'a str,
    checked_inputs_hash: &'a str,
    checked_against_commit: Option<&'a str>,
    now_ms: i64,
}

/// Record a TERMINAL, verdict-less `memory_reality` row for an uncitable memory: NULL
/// `verdict`/`direction`/`model_id`, empty `evidence_json`, but the churn-skip comparators
/// (`content_hash`, `checked_inputs_hash`) and current `prompt_version` stamped so the memory
/// churn-skips instead of re-queuing every run. A NULL verdict is inert for verdict markers and
/// divergence findings (both filter on a concrete verdict). Re-evaluated when the note content,
/// evidence, or `PROMPT_VERSION` change — exactly like a real verdict row.
fn record_uncitable(conn: &Connection, r: Uncitable<'_>) -> rusqlite::Result<()> {
    let content_hash = verify::note_content_hash(r.title, r.body);
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, direction, \
         checked_against_commit, checked_inputs_hash, evidence_json, model_id, prompt_version, \
         checked_at_ms) VALUES (?1,?2,?3,NULL,NULL,?4,?5,'[]',NULL,?6,?7) ON CONFLICT(repo_id, \
         memory_id) DO UPDATE SET content_hash = excluded.content_hash, verdict = NULL, direction \
         = NULL, checked_against_commit = excluded.checked_against_commit, checked_inputs_hash = \
         excluded.checked_inputs_hash, evidence_json = '[]', model_id = NULL, prompt_version = \
         excluded.prompt_version, checked_at_ms = excluded.checked_at_ms",
        rusqlite::params![
            r.memory_id,
            r.repo_id,
            content_hash,
            r.checked_against_commit,
            r.checked_inputs_hash,
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

/// One `memory_reality` `diverged` row joined to its live memory title+body — the input to the
/// stale gates in [`divergence_findings`].
struct DivergenceRow {
    memory_id: String,
    direction: Option<String>,
    evidence_json: Option<String>,
    stored_content_hash: String,
    stored_inputs_hash: Option<String>,
    stored_prompt_version: Option<String>,
    title: String,
    body: String,
}

/// `memory_divergence` findings, derived EVERY run from the STORED `memory_reality` — every
/// `verdict='diverged'` row whose memory is still active AND whose stored `content_hash`,
/// `checked_inputs_hash`, and `prompt_version` still match the memory's CURRENT note (title+body),
/// evidence, and the current verdict prompt, repo-scoped. NOT from this run's fresh (budget-capped)
/// checks:
/// because `dream_findings` sync auto-resolves any finding not reported in a run, deriving from
/// fresh checks would resolve findings for merely-SKIPPED memories. Reading the stored table means
/// a divergence finding resolves exactly when a RE-CHECK flips the verdict to `current` (row no
/// longer `diverged`), the memory goes inactive, its body is edited, its evidence changes, OR the
/// verdict `PROMPT_VERSION` is bumped — in the last case the stored verdict came from an obsolete
/// prompt and is not comparable, so it must not keep refreshing a finding until a fresh verdict is
/// recorded (the queue already re-queues it as `PromptChanged`; this is the matching surfacing
/// gate). These are the SAME stale gates the queue's churn-skip uses (hashes computed in Rust
/// because SQLite has no sha256). A churn-SKIPPED but UNCHANGED memory keeps all three matching, so
/// its finding still surfaces — the resolve-trap protection holds. Mirrors how
/// `verify::unverifiable_findings` runs over the full population for the same reason.
pub(super) fn divergence_findings(conn: &Connection) -> rusqlite::Result<Vec<DreamFinding>> {
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "m");
    let reality_clause = schema::periphery_repo_scope_clause(&scope, "mr");
    let mut stmt = conn.prepare(&format!(
        "SELECT mr.memory_id, mr.direction, mr.evidence_json, mr.content_hash, \
         mr.checked_inputs_hash, mr.prompt_version, m.title, m.body FROM memory_reality mr JOIN \
         repo_memories m ON m.id = mr.memory_id{mem_clause} WHERE mr.verdict = 'diverged' AND \
         m.status = 'active'{reality_clause} ORDER BY mr.memory_id"
    ))?;
    let rows: Vec<DivergenceRow> = stmt
        .query_map([], |r| {
            Ok(DivergenceRow {
                memory_id: r.get(0)?,
                direction: r.get(1)?,
                evidence_json: r.get(2)?,
                stored_content_hash: r.get(3)?,
                stored_inputs_hash: r.get(4)?,
                stored_prompt_version: r.get(5)?,
                title: r.get(6)?,
                body: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // Stale gates: the verdict was checked against `stored_content_hash` + `stored_inputs_hash`
    // under `stored_prompt_version`; drop it once the note (title or body) is edited, the
    // evidence changes, OR the prompt version is bumped, so an out-of-date `diverged` verdict
    // is not surfaced against code the author has since changed or via an obsolete prompt.
    // (`checked_inputs_hash` is recomputed per diverged row — a small set — exactly as the
    // queue's comparator does.)
    let mut out = Vec::new();
    for row in rows {
        if row.stored_prompt_version.as_deref() != Some(PROMPT_VERSION) {
            continue;
        }
        if row.stored_content_hash != verify::note_content_hash(&row.title, &row.body) {
            continue;
        }
        let current_inputs = verify::checked_inputs_hash(conn, &row.memory_id, &scope)?;
        if row.stored_inputs_hash.as_deref() != Some(current_inputs.as_str()) {
            continue;
        }
        let direction = row.direction.unwrap_or_else(|| "unknown".to_string());
        let cited = compact_evidence(row.evidence_json.as_deref());
        out.push(DreamFinding {
            kind: "memory_divergence".into(),
            subject: row.memory_id,
            // Evidence is derived from the STORED row, so it is stable across skip-runs (refresh,
            // not supersede) and only changes when a re-check rewrites the row.
            evidence: format!(
                "model verdict: diverged (direction: {direction}); cited: {cited} [reality]"
            ),
            rank: DIVERGENCE_RANK,
        });
    }
    Ok(out)
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
    fn parse_resets_on_a_new_verdict_section_ignoring_a_scratchpad() {
        // Regression (PR #428): a model that emits a scratchpad VERDICT/EVIDENCE block and
        // then a FINAL one must be parsed from the LAST section only — the scratchpad's evidence
        // must not carry into (and back-justify) the final verdict.
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nEVIDENCE:\n- scratchpad cite\nREASON: \
             thinking...\nVERDICT: current\nEVIDENCE:\n- final cite\nREASON: done",
        )
        .unwrap();
        assert_eq!(parsed.verdict, Verdict::Current, "the FINAL verdict wins");
        assert_eq!(parsed.direction, Direction::Unknown, "the scratchpad direction is reset");
        assert_eq!(
            parsed.evidence,
            vec!["final cite".to_string()],
            "only the final section's evidence survives; the scratchpad cite is dropped"
        );
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
    fn citation_guard_rejects_header_and_boilerplate_fragments() {
        // The rendered pack's section headers + boilerplate are NOT citable content: a verdict that
        // cites one (even a long verbatim fragment lifted from the pack) is rejected, so the pack
        // scaffolding the guard itself emits can't satisfy a fabricated citation. Only
        // identifier-table entries and excerpt lines count.
        let pack = render_pack(&EvidencePack {
            memory_id: "m1".to_string(),
            identifiers: vec![verify::IdentifierResolution {
                identifier: "real_symbol".to_string(),
                resolution: "symbol src/lib.rs::real_symbol".to_string(),
                kind: verify::ResolutionKind::Symbol,
            }],
            excerpts: Vec::new(),
        });
        let cited = |frag: &str| ParsedVerdict {
            verdict: Verdict::Current,
            direction: Direction::Unknown,
            evidence: vec![frag.to_string()],
        };
        assert!(
            !verdict_is_cited(
                &pack,
                &cited("IDENTIFIERS (resolved against the whole source tree):")
            ),
            "the identifier section header is not citable content"
        );
        assert!(
            !verdict_is_cited(&pack, &cited("BOUND-FILE EXCERPTS (current source):")),
            "the excerpts section header is not citable content"
        );
        assert!(
            !verdict_is_cited(&pack, &cited("source")),
            "a bare boilerplate token is below the citation-length floor"
        );
        // The accept path stays green: a genuine identifier-table entry is still cited.
        assert!(
            verdict_is_cited(&pack, &cited("`real_symbol` -> symbol src/lib.rs::real_symbol")),
            "a real identifier-table entry is still accepted"
        );
    }

    #[test]
    fn render_pack_hides_unresolvable_rows_so_they_cannot_be_cited() {
        // An `Unresolvable` span (a paraphrase / snippet that is not code-shaped and matches no
        // text) carries no signal, so it is never rendered — the model can't see it, and the
        // fabrication guard (which matches rendered content lines) can't accept a citation of it.
        // Symbol / verbatim-text (presence) and NOT-FOUND (absence) rows ARE shown.
        let mk = |identifier: &str, resolution: &str, kind| verify::IdentifierResolution {
            identifier: identifier.to_string(),
            resolution: resolution.to_string(),
            kind,
        };
        let pack = render_pack(&EvidencePack {
            memory_id: "m1".to_string(),
            identifiers: vec![
                mk("real", "symbol src/a.rs::real", verify::ResolutionKind::Symbol),
                mk(
                    "gone_symbol",
                    "NOT FOUND anywhere in the source tree",
                    verify::ResolutionKind::Absent,
                ),
                mk(
                    "Ok(None)",
                    "not a resolvable identifier (no symbol, file, or verbatim-text match)",
                    verify::ResolutionKind::Unresolvable,
                ),
            ],
            excerpts: Vec::new(),
        });
        assert!(pack.contains("`real`"), "a symbol row is shown: {pack}");
        assert!(pack.contains("`gone_symbol`"), "a NOT-FOUND (absence) row is shown: {pack}");
        assert!(!pack.contains("Ok(None)"), "the unresolvable row is hidden: {pack}");
    }

    #[test]
    fn citation_guard_rejects_a_bare_locator_prefix() {
        // Regression (PR #428): an excerpt renders as `path:line: <code>`, so a bare
        // `path:line` locator is a substring of it and long enough — but cites no actual source.
        // The guard must require text beyond the locator.
        let pack = render_pack(&EvidencePack {
            memory_id: "m1".to_string(),
            identifiers: Vec::new(),
            excerpts: vec![verify::FileExcerpt {
                path: "crates/x/src/lib.rs".to_string(),
                start_line: 12,
                end_line: 12,
                text: "let handle = spawn();".to_string(),
            }],
        });
        let cited = |frag: &str| ParsedVerdict {
            verdict: Verdict::Diverged,
            direction: Direction::Unknown,
            evidence: vec![frag.to_string()],
        };
        assert!(
            !verdict_is_cited(&pack, &cited("crates/x/src/lib.rs:12")),
            "a bare path:line locator cites no source and is rejected"
        );
        assert!(
            !verdict_is_cited(&pack, &cited("crates/x/src/lib.rs:12:")),
            "a bare path:line: locator is rejected too"
        );
        // The full excerpt line (locator + code) is real evidence and still accepted.
        assert!(
            verdict_is_cited(&pack, &cited("crates/x/src/lib.rs:12: let handle = spawn();")),
            "the excerpt line with its source text is accepted"
        );
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
        let err = obtain_verdict(&model, "prompt", pack).expect_err("two fabrications fail");
        assert_eq!(err.reason, DreamFailureReason::FabricatedEvidence);
        assert_eq!(model.calls(), 2, "retried exactly once");
    }

    #[test]
    fn obtain_verdict_records_model_call_failure() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let model = MockVerdictModel::new(Vec::<String>::new());

        let err = obtain_verdict(&model, "prompt", pack).expect_err("model error fails");

        assert_eq!(err.reason, DreamFailureReason::ModelCallFailed);
        assert!(
            err.detail.as_deref().is_some_and(|detail| detail.contains("no responses")),
            "the model-call error detail is preserved"
        );
        assert_eq!(model.calls(), 1, "transport/model errors are not retried");
    }

    #[test]
    fn obtain_verdict_discards_malformed_without_retry() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let model =
            MockVerdictModel::new(["not a verdict".to_string(), current_citing("real_symbol")]);

        let err = obtain_verdict(&model, "prompt", pack).expect_err("malformed verdict fails");

        assert_eq!(err.reason, DreamFailureReason::MalformedVerdict);
        assert_eq!(model.calls(), 1, "malformed completions are discarded without retry");
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
                "SELECT verdict, direction, model_id, prompt_version, content_hash, checked_at_ms \
                 FROM memory_reality WHERE memory_id='m1' AND repo_id='r'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(row.0, "diverged");
        assert_eq!(row.1, "note_ahead");
        assert_eq!(row.2, "mock-verdict-model");
        assert_eq!(row.3, PROMPT_VERSION);
        assert_eq!(row.4, verify::note_content_hash("note", "describes `resolvable_thing`"));
        assert_eq!(row.5, 5000);
    }

    #[test]
    fn verdict_pass_budget_stops_before_second_memory() {
        let c = seeded_verifiable_repo();
        seed_symbol_file(&c, "src/other.rs", "second_thing", "r");
        seed_memory(&c, "m2", "note", "describes `second_thing`", "r");
        let model = MockVerdictModel::new([
            current_citing("resolvable_thing"),
            current_citing("second_thing"),
        ]);

        run_verdict_pass(&c, VerdictPass { model: &model, budget: 1 }, 5000).unwrap();

        assert_eq!(model.calls(), 1, "budget one verifies only the first queued memory");
        let rows: i64 =
            c.query_row("SELECT COUNT(*) FROM memory_reality", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "the second queued memory is left for a later run");
    }

    #[test]
    fn failed_verdict_completion_records_failure_row() {
        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new(["not a verdict"]);

        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 5000).unwrap();

        let (reason, attempts): (String, i64) = c
            .query_row(
                "SELECT reason, attempts FROM memory_model_failures WHERE repo_id='r' AND \
                 memory_id='m1' AND pass='verify'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, DreamFailureReason::MalformedVerdict.as_db_str());
        assert_eq!(attempts, 1);
        assert_eq!(model.calls(), 1);
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

        // A body edit changes content_hash → re-enqueued → the model runs again.
        c.execute(
            "UPDATE repo_memories SET body='describes `resolvable_thing` (edited)' WHERE id='m1'",
            [],
        )
        .unwrap();
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 3000).unwrap();
        assert_eq!(model.calls(), 2, "a body edit re-invokes the model");
    }

    #[test]
    fn current_failed_verdict_attempt_skips_model_until_input_changes() {
        let c = seeded_verifiable_repo();
        let scope = Some("r".to_string());
        let inputs = verify::checked_inputs_hash(&c, "m1", &scope).unwrap();
        let content_hash = verify::note_content_hash("note", "describes `resolvable_thing`");
        let stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "r",
            pass: DreamModelPass::Verify,
            content_hash: &content_hash,
            checked_inputs_hash: Some(&inputs),
            prompt_version: PROMPT_VERSION,
            model_id: "mock-verdict-model",
        };
        let failed = DreamModelFailure::new(DreamFailureReason::FabricatedEvidence);
        failure::record_failure(&c, RecordFailure { stamp, failure: &failed, now_ms: 1000 })
            .unwrap();

        let model = MockVerdictModel::new([current_citing("resolvable_thing")]);
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 2000).unwrap();
        assert_eq!(model.calls(), 0, "a current deterministic failure row suppresses the retry");

        c.execute(
            "UPDATE repo_memories SET body='describes `resolvable_thing` v2' WHERE id='m1'",
            [],
        )
        .unwrap();
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 3000).unwrap();
        assert_eq!(model.calls(), 1, "a content change invalidates the failure row");
        let failures: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM memory_model_failures WHERE repo_id='r' AND memory_id='m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failures, 0, "a successful verdict clears the stale failure row");
    }

    // ── divergence-finding lifecycle (the resolve trap) ──────────────────────────

    fn divergence_subjects(findings: &[crate::dream::WorklistFinding]) -> Vec<String> {
        findings
            .iter()
            .filter(|f| f.kind == "memory_divergence")
            .map(|f| f.subject.clone())
            .collect()
    }

    #[test]
    fn diverged_opens_finding_then_skip_keeps_it_then_recheck_current_resolves_it() {
        use super::super::{DreamOptions, dream_run_with_passes};

        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new([
            diverged_citing("resolvable_thing"), // run 1: opens the divergence finding
            current_citing("resolvable_thing"),  // run 3 (after body edit): flips to current
        ]);
        let opts = DreamOptions { now_ms: 1000, limit: 10, verify: true, include_reviewed: false };

        // Run 1: diverged verdict → memory_divergence finding opens.
        let r1 =
            dream_run_with_passes(&c, opts, Some(VerdictPass { model: &model, budget: 10 }), None)
                .unwrap();
        assert_eq!(divergence_subjects(&r1.findings), vec!["m1".to_string()], "divergence opens");
        assert_eq!(model.calls(), 1);

        // Run 2: the memory is UNCHANGED → the queue churn-skips (no model call), but the finding
        // is derived from the STORED diverged row, so it stays open (the resolve-trap
        // regression).
        let opts2 = DreamOptions { now_ms: 2000, ..opts };
        let r2 =
            dream_run_with_passes(&c, opts2, Some(VerdictPass { model: &model, budget: 10 }), None)
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
        let r3 =
            dream_run_with_passes(&c, opts3, Some(VerdictPass { model: &model, budget: 10 }), None)
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
    fn a_plain_dream_run_does_not_resolve_a_prior_verify_runs_divergence_finding() {
        // Regression (PR #428): the resolve sweep is kind-scoped, so a plain `dream`
        // (verify off) must NOT resolve the `memory_divergence` finding a prior `--verify` run
        // opened — it never re-evaluated that kind.
        use super::super::{DreamOptions, dream_run, dream_run_with_passes};

        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new([diverged_citing("resolvable_thing")]);
        let verify_opts =
            DreamOptions { now_ms: 1000, limit: 10, verify: true, include_reviewed: false };
        let r1 = dream_run_with_passes(
            &c,
            verify_opts,
            Some(VerdictPass { model: &model, budget: 10 }),
            None,
        )
        .unwrap();
        assert_eq!(divergence_subjects(&r1.findings), vec!["m1".to_string()], "divergence opens");

        // A plain deterministic run (verify OFF): the divergence kind is not computed, so its open
        // finding is left untouched — not resolved as "no longer seen". The emitted worklist reads
        // ALL open findings from the store, so the still-open divergence finding is still listed.
        let plain_opts =
            DreamOptions { now_ms: 2000, limit: 10, verify: false, include_reviewed: false };
        let r2 = dream_run(&c, plain_opts).unwrap();
        assert_eq!(
            divergence_subjects(&r2.findings),
            vec!["m1".to_string()],
            "the divergence finding survives a plain run (not resolved)"
        );
        let still_open: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM dream_findings WHERE kind='memory_divergence' AND \
                 status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_open, 1, "the divergence finding stays open across the plain run");
    }

    #[test]
    fn divergence_finding_drops_when_the_body_is_edited_without_a_recheck() {
        // Regression (PR #428): a stored `diverged` verdict is against the OLD body; once
        // the body is edited it must not be surfaced against the new note even if the model pass is
        // absent (disabled / budget-exhausted / skipped) so no re-check happened.
        use super::super::{DreamOptions, dream_run, dream_run_with_passes};

        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new([diverged_citing("resolvable_thing")]);
        let verify_opts =
            DreamOptions { now_ms: 1000, limit: 10, verify: true, include_reviewed: false };
        dream_run_with_passes(
            &c,
            verify_opts,
            Some(VerdictPass { model: &model, budget: 10 }),
            None,
        )
        .unwrap();

        // Edit the body — no model pass supplied on this verify run, so the stale verdict row is
        // not re-checked. The stale-body gate must drop it from the derived findings, and
        // because the kind IS computed (verify on) with the memory absent, the open finding
        // resolves.
        c.execute(
            "UPDATE repo_memories SET body='describes `resolvable_thing` v2' WHERE id='m1'",
            [],
        )
        .unwrap();
        let r = dream_run(&c, DreamOptions {
            now_ms: 2000,
            limit: 10,
            verify: true,
            include_reviewed: false,
        })
        .unwrap();
        assert!(
            divergence_subjects(&r.findings).is_empty(),
            "a diverged verdict against the pre-edit body is not surfaced against the new note"
        );
        let open: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM dream_findings WHERE kind='memory_divergence' AND \
                 status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0, "the stale divergence finding resolves once the body is edited");
    }

    #[test]
    fn divergence_finding_drops_under_an_obsolete_prompt_version() {
        // Regression (PR #428): a stored `diverged` verdict from an OLD verdict prompt is
        // not comparable; after a `PROMPT_VERSION` bump it must not keep refreshing a finding until
        // a fresh verdict is recorded (the queue re-queues it as `PromptChanged`; this is
        // the matching surfacing gate). Symmetric to the stale-body case.
        use super::super::{DreamOptions, dream_run, dream_run_with_passes};

        let c = seeded_verifiable_repo();
        let model = MockVerdictModel::new([diverged_citing("resolvable_thing")]);
        dream_run_with_passes(
            &c,
            DreamOptions { now_ms: 1000, limit: 10, verify: true, include_reviewed: false },
            Some(VerdictPass { model: &model, budget: 10 }),
            None,
        )
        .unwrap();

        // Simulate a PROMPT_VERSION bump: the stored verdict now predates the current prompt. No
        // model pass on the re-run, so the row is not re-checked; the stale-prompt gate must drop
        // it.
        c.execute(
            "UPDATE memory_reality SET prompt_version='verify-pack-OLD' WHERE memory_id='m1'",
            [],
        )
        .unwrap();
        let r = dream_run(&c, DreamOptions {
            now_ms: 2000,
            limit: 10,
            verify: true,
            include_reviewed: false,
        })
        .unwrap();
        assert!(
            divergence_subjects(&r.findings).is_empty(),
            "a diverged verdict from an obsolete prompt is not surfaced"
        );
        let open: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM dream_findings WHERE kind='memory_divergence' AND \
                 status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0, "the stale-prompt divergence finding resolves");
    }

    #[test]
    fn model_pass_never_mutates_a_repo_memories_column() {
        use super::super::{DreamOptions, dream_run_with_passes};

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
        let opts = DreamOptions { now_ms: 1000, limit: 10, verify: true, include_reviewed: false };
        dream_run_with_passes(&c, opts, Some(VerdictPass { model: &model, budget: 10 }), None)
            .unwrap();
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
        use super::super::{DreamOptions, dream_run_with_passes};

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
        let opts = DreamOptions { now_ms: 2000, limit: 10, verify: true, include_reviewed: false };
        let r2 = dream_run_with_passes(
            &c,
            opts,
            Some(VerdictPass { model: &model_r2, budget: 10 }),
            None,
        )
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
        // `divergence_findings` already returns only `memory_divergence` rows, so map subjects
        // directly (the `divergence_subjects` helper is for the WorklistFinding-typed dream_run
        // output).
        assert_eq!(r1_divergence.iter().map(|f| f.subject.clone()).collect::<Vec<_>>(), vec![
            "m1".to_string()
        ]);
    }
}
