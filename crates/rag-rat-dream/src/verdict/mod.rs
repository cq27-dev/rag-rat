//! Dream v2 pass 1 — the MODEL verdict pass (rag-rat's first generative-model dependency, #122).
//!
//! The deterministic pass-0 substrate (`verify`) builds the churn-skip [`verification_queue`] and a
//! citation-checkable [`evidence_pack`]; this module renders that pack into a single-turn prompt,
//! asks a [`ChatModel`] for a `current | diverged` verdict, guards against fabricated citations,
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

mod guard;

use rag_rat_db::schema;
use rag_rat_llm::chat::ChatModel;
/// The verdict prompt version, stamped into `memory_reality.prompt_version`. Bump on any
/// change to [`VERDICT_PROMPT_HEAD`] or the pack rendering so a stale-prompt verdict is
/// distinguishable — a bump re-queues every prior verdict
/// (`VerificationReason::PromptChanged`) and the finding surface stops reporting stale-prompt
/// verdicts until they are re-checked. v6 requires a whole verbatim note claim for divergence,
/// preserves identifier/operator shape, and accepts only a named authoritative NOT FOUND row
/// as contradiction evidence; excerpts and presence rows remain context only. Earlier verdicts
/// are not comparable.
pub(crate) use rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION as PROMPT_VERSION;
use rusqlite::Connection;

use super::DreamFinding;
use super::failure::{
    self, DreamFailureReason, DreamModelFailure, DreamModelPass, FailureStamp, Judgement,
    RecordFailure,
};
use super::findings::FindingKind;
use super::verify::{
    self, EvidencePack, IdentifierResolution, ResolutionKind, VerificationQueueEntry,
    evidence_pack, verification_queue,
};

/// The verdict-pass configuration handed to [`run_verdict_pass`]: the model to ask and how many
/// queued memories it may check this run (the runner stops once that many entries reach the model
/// or the uncitable short-circuit). Separate from
/// [`DreamOptions`] because it carries a borrow (the model) — `DreamOptions` stays `Copy`.
pub struct VerdictPass<'a> {
    pub model: &'a dyn ChatModel,
    pub budget: usize,
}

/// The model's verdict for a note vs. the current code. Persisted through [`Verdict::as_db_str`] —
/// `unverifiable` is deliberately ABSENT (pass 0 decides it deterministically; a stray model
/// `unverifiable` is discarded by [`parse_verdict`], never stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Verdict {
    Current,
    Diverged,
}

impl Verdict {
    #[cfg(test)]
    const ALL: [Self; 2] = [Self::Current, Self::Diverged];

    fn as_db_str(self) -> &'static str {
        self.into()
    }

    #[cfg(test)]
    fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
    }

    /// Parse the VERDICT word. `None` for `unverifiable` (pass-0 territory), anything
    /// unrecognized, or an ECHOED CHOICE (`VERDICT: current | diverged`, `current or diverged`)
    /// — the model selected nothing, and taking the first word would silently store `current`
    /// and churn-skip a real divergence. Trailing prose after a clear first word is tolerated.
    fn parse(word: &str) -> Option<Self> {
        let mut words = word.split_whitespace();
        let first = words.next().unwrap_or("").trim_end_matches(|c: char| !c.is_alphanumeric());
        let verdict = match first.to_ascii_lowercase().as_str() {
            "current" => Self::Current,
            "diverged" => Self::Diverged,
            _ => return None,
        };
        let mut choice_connector = false;
        let another_alternative = words.any(|word| {
            let token = word.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
            let is_alternative =
                choice_connector && matches!(token.as_str(), "current" | "diverged");
            choice_connector = matches!(token.as_str(), "or" | "not")
                || word.chars().any(|c| matches!(c, '|' | '/'));
            is_alternative
        });
        if another_alternative { None } else { Some(verdict) }
    }
}

/// Advisory direction of a divergence (which side is newer). Never load-bearing — a hint for the
/// human review flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Direction {
    CodeAhead,
    NoteAhead,
    Unknown,
}

impl Direction {
    #[cfg(test)]
    const ALL: [Self; 3] = [Self::CodeAhead, Self::NoteAhead, Self::Unknown];

    fn as_db_str(self) -> &'static str {
        self.into()
    }

    #[cfg(test)]
    fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
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
    /// A load-bearing claim copied from the note. Required for `diverged` verdicts so the
    /// deterministic guard can verify that the cited source evidence contradicts something the
    /// note actually says, rather than an identifier the model inferred importance for.
    claim: Option<String>,
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
    let queue = verification_queue(conn, now_ms)?;
    if queue.is_empty() {
        return Ok(());
    }
    let (scope, repo_id) = failure::pass_repo_scope(conn)?;
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
        let failure_stamp =
            failure_stamp(&entry, &repo_id, &content_hash, &inputs_hash, pass.model.model_id());
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
            // #767 review: the entry's writes commit under the removal-tombstone guard (the
            // model itself is never called for an uncitable entry, so the whole step is a write).
            super::removal_guarded_write_tx(conn, &scope, |tx| {
                record_reality(tx, RecordReality {
                    memory_id: &entry.memory_id,
                    repo_id: &repo_id,
                    title: &entry.title,
                    body: &entry.body,
                    verdict: None,
                    checked_inputs_hash: &inputs_hash,
                    checked_against_commit: checked_against_commit.as_deref(),
                    model_id: None,
                    now_ms,
                })?;
                failure::clear_failure(tx, &failure_stamp)?;
                Ok(())
            })?;
            continue;
        }
        let pack_text = render_pack(&pack);
        let binding = binding_label(conn, &entry.memory_id, &scope)?;
        let prompt = render_verdict_prompt(&entry, &binding, &pack_text);
        let accepted =
            match obtain_verdict(pass.model, &prompt, &entry.title, &entry.body, &pack_text) {
                Ok(accepted) => accepted,
                Err(failure) => {
                    super::removal_guarded_write_tx(conn, &scope, |tx| {
                        failure::record_failure(tx, RecordFailure {
                            stamp: failure_stamp,
                            failure: &failure,
                            now_ms,
                        })?;
                        Ok(())
                    })?;
                    continue;
                },
            };
        // #767 review: the verdict UPSERT + failure clear commit in ONE guarded transaction — the
        // tombstone re-check inside serializes with `rag-rat rm`'s purge, so a removal landing
        // mid-pass cannot leave this repo-scoped `memory_reality` row behind.
        super::removal_guarded_write_tx(conn, &scope, |tx| {
            record_reality(tx, RecordReality {
                memory_id: &entry.memory_id,
                repo_id: &repo_id,
                title: &entry.title,
                body: &entry.body,
                verdict: Some(&accepted),
                checked_inputs_hash: &inputs_hash,
                checked_against_commit: checked_against_commit.as_deref(),
                model_id: Some(pass.model.model_id()),
                now_ms,
            })?;
            failure::clear_failure(tx, &failure_stamp)?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Whether [`run_verdict_pass`] would call the model at all — the verify half of the ephemeral
/// zero-work guard ([`super::model_work_pending`]). It walks the runner's queue the runner's way:
/// failure-blocked entries are skipped before the budget counts them. It is CITABILITY-aware, not
/// just queue-emptiness: the runner records an UNCITABLE entry (prose-only / all-`NOT FOUND`, no
/// excerpts) as a terminal row WITHOUT calling the model, so a queue whose every entry is uncitable
/// is zero model work. The probe therefore builds each counted entry's evidence pack and answers
/// `true` on the FIRST citable one — the paid box it gates makes the extra pack builds worth it.
pub(super) fn verification_pending(
    conn: &Connection,
    now_ms: i64,
    budget: usize,
    model_id: &str,
) -> anyhow::Result<bool> {
    let (scope, repo_id) = failure::pass_repo_scope(conn)?;
    let mut considered = 0usize;
    for entry in verification_queue(conn, now_ms)? {
        if considered >= budget {
            break;
        }
        let inputs_hash = verify::checked_inputs_hash(conn, &entry.memory_id, &scope)?;
        let content_hash = verify::note_content_hash(&entry.title, &entry.body);
        let failure_stamp = failure_stamp(&entry, &repo_id, &content_hash, &inputs_hash, model_id);
        if failure::blocking_failure_is_current(conn, &failure_stamp)? {
            continue;
        }
        considered += 1;
        if evidence_pack(conn, &entry.memory_id)?.is_citable() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The verify pass's failure stamp for one queued entry — keyed on the note's content hash AND its
/// checked-inputs hash under the verdict [`PROMPT_VERSION`]. The runner and its zero-work probe
/// both build it here, so they gate on the same stamp shape.
fn failure_stamp<'a>(
    entry: &'a VerificationQueueEntry,
    repo_id: &'a str,
    content_hash: &'a str,
    inputs_hash: &'a str,
    model_id: &'a str,
) -> FailureStamp<'a> {
    FailureStamp {
        memory_id: &entry.memory_id,
        repo_id,
        pass: DreamModelPass::Verify,
        content_hash,
        checked_inputs_hash: Some(inputs_hash),
        prompt_version: PROMPT_VERSION,
        model_id,
    }
}

/// Ask the model once, parse, and run the fabrication guard: EVERY EVIDENCE line must appear in the
/// rendered pack (whitespace-normalized substring). An unmatched citation rejects the completion
/// and RETRIES ONCE; a second fabrication (or a model error) discards the verdict. A malformed /
/// stray `unverifiable` completion is discarded WITHOUT a retry (it is not a citation fault — pass
/// 0 owns unverifiable). Small models measurably fabricate evidence, so this guard is load-bearing.
fn obtain_verdict(
    model: &dyn ChatModel,
    prompt: &str,
    note_title: &str,
    note_body: &str,
    pack_text: &str,
) -> Result<AcceptedVerdict, DreamModelFailure> {
    let Ok(verdict) = failure::ask_with_one_retry::<_, std::convert::Infallible>(
        model,
        prompt,
        DreamModelPass::Verify,
        DreamFailureReason::FabricatedEvidence,
        |attempt, raw| {
            let Some(parsed) = parse_verdict(raw) else {
                // Malformed or a stray `unverifiable` — discard, no retry (not a citation fault).
                tracing::debug!(target: "rag_rat_core::dream::verdict", "discarding unparseable/unverifiable verdict completion");
                return Ok(Judgement::Reject(DreamModelFailure::new(
                    DreamFailureReason::MalformedVerdict,
                )));
            };
            if guard::verdict_is_grounded(note_title, note_body, pack_text, &parsed) {
                return Ok(Judgement::Accept(AcceptedVerdict {
                    verdict: parsed.verdict,
                    direction: parsed.direction,
                    evidence: parsed.evidence,
                }));
            }
            // Fabricated citation. Retry once, then discard.
            tracing::warn!(target: "rag_rat_core::dream::verdict", attempt, "verdict cited a line absent from the evidence pack (possible fabrication)");
            Ok(Judgement::Retry)
        },
    );
    verdict
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
const VERDICT_PROMPT_HEAD: &str = include_str!("../prompts/verdict_head.md");

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
    s.push_str("IDENTIFIERS (resolved against the active source index):\n");
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

// ── Parsing ──────────────────────────────────────────────────────────────────────────────────

/// Parse a model completion into a [`ParsedVerdict`]. Tolerant of surrounding prose: it scans for
/// the `VERDICT:` / `DIRECTION:` / `CLAIM:` / `EVIDENCE:` / `REASON:` markers
/// (case-insensitive) anywhere in the output. `None` when there is no recognizable
/// `current`/`diverged` VERDICT — malformed output and a stray `unverifiable` both discard.
fn parse_verdict(output: &str) -> Option<ParsedVerdict> {
    let mut verdict = None;
    let mut direction = Direction::Unknown;
    let mut claim = None;
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
            claim = None;
            evidence.clear();
            in_evidence = false;
        } else if let Some(rest) = strip_ci(line, "DIRECTION:") {
            direction = Direction::parse(rest);
            in_evidence = false;
        } else if let Some(rest) = strip_ci(line, "CLAIM:") {
            let rest = rest.trim();
            claim = (!rest.is_empty()).then(|| rest.to_string());
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
    Some(ParsedVerdict { verdict: verdict?, direction, claim, evidence })
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
/// train of same-typed strings. `verdict` and `model_id` are `None` together for an uncitable
/// memory the pass checked but could not put to the model (see [`EvidencePack::is_citable`]).
struct RecordReality<'a> {
    memory_id: &'a str,
    repo_id: &'a str,
    title: &'a str,
    body: &'a str,
    verdict: Option<&'a AcceptedVerdict>,
    checked_inputs_hash: &'a str,
    checked_against_commit: Option<&'a str>,
    model_id: Option<&'a str>,
    now_ms: i64,
}

/// UPSERT a checked memory into `memory_reality` (PK `(repo_id, memory_id)`), stamping the
/// churn-skip comparators (`content_hash`, `checked_inputs_hash`) exactly as the queue reads them
/// so the next run skips an unchanged memory, plus the verdict, advisory direction, cited evidence,
/// model id, prompt version, and check timestamp. NEVER writes a `repo_memories` column.
///
/// An uncitable memory records a TERMINAL, verdict-less row: NULL `verdict`/`direction`/`model_id`
/// and empty `evidence_json`, with the comparators and current `prompt_version` still stamped so it
/// churn-skips instead of re-queuing every run. A NULL verdict is inert for verdict markers and
/// divergence findings (both filter on a concrete verdict), and the row is re-evaluated when the
/// note content, evidence, or `PROMPT_VERSION` change — exactly like a real verdict row.
fn record_reality(conn: &Connection, r: RecordReality<'_>) -> rusqlite::Result<()> {
    let content_hash = verify::note_content_hash(r.title, r.body);
    // Store the cited pack lines as a JSON array so `divergence_findings` can render a compact,
    // stable evidence string from them.
    let evidence_json = r.verdict.map_or_else(
        || "[]".to_string(),
        |accepted| serde_json::to_string(&accepted.evidence).unwrap_or_else(|_| "[]".to_string()),
    );
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
            r.verdict.map(|accepted| accepted.verdict.as_db_str()),
            r.verdict.map(|accepted| accepted.direction.as_db_str()),
            r.checked_against_commit,
            r.checked_inputs_hash,
            evidence_json,
            r.model_id,
            PROMPT_VERSION,
            r.now_ms,
        ],
    )?;
    crate::bump_memory_lens_lanes(conn, r.repo_id)?;
    Ok(())
}

/// The commit the index is currently at (`repo_meta` `git_commit`), for informational
/// `checked_against_commit` stamping. `None` outside a repo scope or when unrecorded.
fn indexed_commit(conn: &Connection, scope: &Option<String>) -> rusqlite::Result<Option<String>> {
    match scope {
        Some(repo_id) => rag_rat_db::meta::repo_meta(conn, repo_id, "git_commit"),
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
         repo_memories m ON m.id = mr.memory_id{mem_clause} WHERE mr.verdict = ?1 AND m.status = \
         'active'{reality_clause} ORDER BY mr.memory_id"
    ))?;
    let rows: Vec<DivergenceRow> = stmt
        .query_map([Verdict::Diverged.as_db_str()], |r| {
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
        // A stored token passes through verbatim (it may come from a peer's newer build); only a
        // missing one takes this build's `unknown`.
        let direction = row.direction.unwrap_or_else(|| Direction::Unknown.as_db_str().to_string());
        let cited = compact_evidence(row.evidence_json.as_deref());
        out.push(DreamFinding {
            kind: FindingKind::MemoryDivergence,
            subject: row.memory_id,
            // Evidence is derived from the STORED row, so it is stable across skip-runs (refresh,
            // not supersede) and only changes when a re-check rewrites the row.
            evidence: format!(
                "model verdict: diverged (direction: {direction}); cited: {cited} [reality]"
            ),
            rank: FindingKind::MemoryDivergence.base_rank(),
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
    let joined = guard::normalize_ws(&joined);
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
    use super::super::tests::{mem_db, set_repo};
    use super::*;
    use crate::mock_chat::MockChatModel;

    #[test]
    fn persisted_verdict_and_direction_tokens_round_trip() {
        for (verdict, token) in Verdict::ALL.into_iter().zip(["current", "diverged"]) {
            assert_eq!(verdict.as_db_str(), token);
            assert_eq!(Verdict::from_db_str(token), Some(verdict));
        }
        for (direction, token) in
            Direction::ALL.into_iter().zip(["code_ahead", "note_ahead", "unknown"])
        {
            assert_eq!(direction.as_db_str(), token);
            assert_eq!(Direction::from_db_str(token), Some(direction));
        }
        assert_eq!(Verdict::from_db_str("unverifiable"), None);
    }

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

    fn seed_live_call_path(
        c: &Connection,
        memory_id: &str,
        path: &str,
        target: &str,
        repo_id: &str,
    ) {
        let hash = format!("hash-{memory_id}");
        c.execute(
            "INSERT INTO edges(from_name, to_name, edge_kind, confidence, source_file_id, \
             source_start_line, source_end_line) SELECT 'caller',?2,'calls_name','exact',id,1,1 \
             FROM main.files WHERE path=?1",
            rusqlite::params![path, target],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES (?1,'call_path',?2,NULL,'current',0,?3)",
            rusqlite::params![memory_id, hash, repo_id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
             edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
             callee_identity_known) VALUES \
             (?1,?2,0,'test-fingerprint','caller',?3,'calls_name',NULL,1)",
            rusqlite::params![memory_id, hash, target],
        )
        .unwrap();
    }

    /// A well-formed `current` completion citing an identifier known to be in the pack.
    pub(super) fn current_citing(ident: &str) -> String {
        format!(
            "VERDICT: current\nDIRECTION: unknown\nCLAIM: NONE\nEVIDENCE:\n- `{ident}`\nREASON: \
             matches."
        )
    }

    /// A well-formed `diverged` completion for the seeded m1: the claim is the note's full body
    /// verbatim and the citation names the note's ABSENT identifier (`gone_thing` resolves
    /// NOT FOUND — a PRESENCE row never grounds divergence).
    fn diverged_citing() -> String {
        "VERDICT: diverged\nDIRECTION: note_ahead\nCLAIM: The note describes `resolvable_thing` \
         and `gone_thing` as available.\nEVIDENCE:\n- `gone_thing`\nREASON: not present."
            .to_string()
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
            has_live_binding: false,
        });
        assert!(pack.contains("`real`"), "a symbol row is shown: {pack}");
        assert!(pack.contains("`gone_symbol`"), "a NOT-FOUND (absence) row is shown: {pack}");
        assert!(!pack.contains("Ok(None)"), "the unresolvable row is hidden: {pack}");
    }

    #[test]
    fn parse_rejects_an_echoed_verdict_choice() {
        for echoed in [
            "VERDICT: current | diverged\nEVIDENCE:\n- x\nREASON: y",
            "VERDICT: current or diverged\nEVIDENCE:\n- x\nREASON: y",
            "VERDICT: diverged, not current\nEVIDENCE:\n- x\nREASON: y",
        ] {
            assert!(parse_verdict(echoed).is_none(), "an echoed choice selects nothing: {echoed}");
        }
    }

    #[test]
    fn parse_accepts_a_chatty_verdict_line() {
        let parsed = parse_verdict(
            "VERDICT: diverged — the helper is gone\nDIRECTION: code_ahead\nEVIDENCE:\n- \
             x\nREASON: y",
        )
        .unwrap();
        assert_eq!(parsed.verdict, Verdict::Diverged, "trailing prose after the word is ignored");
        let mentions_other_word = parse_verdict(
            "VERDICT: diverged because current code removed the helper\nEVIDENCE:\n- x\nREASON: y",
        )
        .unwrap();
        assert_eq!(mentions_other_word.verdict, Verdict::Diverged);
    }

    #[test]
    fn obtain_verdict_retries_once_then_accepts_bad_then_good() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        // First completion fabricates; the retry cites a real line → accepted.
        let model =
            MockChatModel::new([current_citing("ghost_symbol"), current_citing("real_symbol")]);
        let accepted = obtain_verdict(&model, "prompt", "note", "body", pack)
            .expect("bad-then-good is accepted");
        assert_eq!(accepted.verdict, Verdict::Current);
        assert_eq!(model.calls(), 2, "one fabrication triggers exactly one retry");
    }

    #[test]
    fn obtain_verdict_discards_after_two_fabrications() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let model = MockChatModel::new([current_citing("ghost_a"), current_citing("ghost_b")]);
        let err = obtain_verdict(&model, "prompt", "note", "body", pack)
            .expect_err("two fabrications fail");
        assert_eq!(err.reason, DreamFailureReason::FabricatedEvidence);
        assert_eq!(model.calls(), 2, "retried exactly once");
    }

    #[test]
    fn obtain_verdict_records_model_call_failure() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let model = MockChatModel::new(Vec::<String>::new());

        let err =
            obtain_verdict(&model, "prompt", "note", "body", pack).expect_err("model error fails");

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
            MockChatModel::new(["not a verdict".to_string(), current_citing("real_symbol")]);

        let err = obtain_verdict(&model, "prompt", "note", "body", pack)
            .expect_err("malformed verdict fails");

        assert_eq!(err.reason, DreamFailureReason::MalformedVerdict);
        assert_eq!(model.calls(), 1, "malformed completions are discarded without retry");
    }

    #[test]
    fn record_reality_overwrites_a_verdict_row_with_a_terminal_uncitable_row() {
        // Both outcomes share one UPSERT: an uncitable re-check of a memory that previously got a
        // verdict must NULL the verdict columns and empty the evidence on the conflict path, not
        // leave the stale verdict behind.
        let c = mem_db();
        let row = |c: &Connection| {
            c.query_row(
                "SELECT content_hash, verdict, direction, checked_against_commit, \
                 checked_inputs_hash, evidence_json, model_id, prompt_version, checked_at_ms FROM \
                 memory_reality WHERE repo_id = 'r' AND memory_id = 'm1'",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, i64>(8)?,
                    ))
                },
            )
            .unwrap()
        };
        let content_hash = verify::note_content_hash("t", "b");
        let accepted = AcceptedVerdict {
            verdict: Verdict::Diverged,
            direction: Direction::NoteAhead,
            evidence: vec!["`gone_thing` -> gone".to_string()],
        };
        record_reality(&c, RecordReality {
            memory_id: "m1",
            repo_id: "r",
            title: "t",
            body: "b",
            verdict: Some(&accepted),
            checked_inputs_hash: "inputs-1",
            checked_against_commit: Some("abc"),
            model_id: Some("model"),
            now_ms: 1,
        })
        .unwrap();
        assert_eq!(
            row(&c),
            (
                content_hash.clone(),
                Some("diverged".to_string()),
                Some("note_ahead".to_string()),
                Some("abc".to_string()),
                Some("inputs-1".to_string()),
                Some(r#"["`gone_thing` -> gone"]"#.to_string()),
                Some("model".to_string()),
                Some(PROMPT_VERSION.to_string()),
                1,
            ),
            "a verdict row stamps every column"
        );

        record_reality(&c, RecordReality {
            memory_id: "m1",
            repo_id: "r",
            title: "t",
            body: "b",
            verdict: None,
            checked_inputs_hash: "inputs-2",
            checked_against_commit: None,
            model_id: None,
            now_ms: 2,
        })
        .unwrap();
        assert_eq!(
            row(&c),
            (
                content_hash,
                None,
                None,
                None,
                Some("inputs-2".to_string()),
                Some("[]".to_string()),
                None,
                Some(PROMPT_VERSION.to_string()),
                2,
            ),
            "an uncitable row replaces the verdict with NULLs and empty evidence"
        );
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
        rag_rat_db::chunk_text_store::seed_chunk_text(&c, chunk_id, "fn real_symbol() {}\n")
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
        assert!(rendered_a.contains("NOT FOUND"), "the exact-file-domain miss is rendered");
        assert!(rendered_a.contains("src/lib.rs:1: fn real_symbol()"), "excerpt line present");

        let entry =
            verification_queue(&c, 1).unwrap().into_iter().find(|e| e.memory_id == "m1").unwrap();
        let prompt = render_verdict_prompt(&entry, "src/lib.rs", &rendered_a);
        assert!(prompt.contains("VERDICT: current | diverged"), "the verdict format is stated");
        assert!(prompt.contains("CLAIM:"), "the grounded-claim format is stated");
        assert!(prompt.contains("EVIDENCE PACK:"), "the pack section header is present");
        assert!(prompt.contains("TITLE: note"), "the note title is included");
        assert!(prompt.contains("`real_symbol` -> symbol"), "the pack is embedded in the prompt");
    }

    // ── write path + churn-skip ──────────────────────────────────────────────────

    /// Seed m1 with a resolvable identifier (so it is verifiable) PLUS an absent one (so a
    /// divergence has NOT FOUND evidence), bound to a live server-derived call path, run the
    /// verdict pass, and return the connection. m1 is NeverChecked → the model is invoked once.
    fn seeded_verifiable_repo() -> Connection {
        let c = mem_db();
        set_repo(&c, "r");
        seed_symbol_file(&c, "src/lib.rs", "resolvable_thing", "r");
        seed_memory(
            &c,
            "m1",
            "note",
            "The note describes `resolvable_thing` and `gone_thing` as available.",
            "r",
        );
        seed_live_call_path(&c, "m1", "src/lib.rs", "resolvable_thing", "r");
        c
    }

    #[test]
    fn verdict_pass_upserts_memory_reality_with_all_stamps() {
        let c = seeded_verifiable_repo();
        let model = MockChatModel::new([diverged_citing()]);
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
        assert_eq!(row.2, "mock-chat-model");
        assert_eq!(row.3, PROMPT_VERSION);
        assert_eq!(
            row.4,
            verify::note_content_hash(
                "note",
                "The note describes `resolvable_thing` and `gone_thing` as available."
            )
        );
        assert_eq!(row.5, 5000);
    }

    #[test]
    fn verdict_pass_budget_stops_before_second_memory() {
        let c = seeded_verifiable_repo();
        seed_symbol_file(&c, "src/other.rs", "second_thing", "r");
        seed_memory(&c, "m2", "note", "The note describes `second_thing` as available.", "r");
        let model = MockChatModel::new([
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
        let model = MockChatModel::new(["not a verdict"]);

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
        let model = MockChatModel::new([
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
            "UPDATE repo_memories SET body='The note describes `resolvable_thing` and \
             `gone_thing` as available (edited).' WHERE id='m1'",
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
        let content_hash = verify::note_content_hash(
            "note",
            "The note describes `resolvable_thing` and `gone_thing` as available.",
        );
        let stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "r",
            pass: DreamModelPass::Verify,
            content_hash: &content_hash,
            checked_inputs_hash: Some(&inputs),
            prompt_version: PROMPT_VERSION,
            model_id: "mock-chat-model",
        };
        let failed = DreamModelFailure::new(DreamFailureReason::FabricatedEvidence);
        failure::record_failure(&c, RecordFailure { stamp, failure: &failed, now_ms: 1000 })
            .unwrap();

        let model = MockChatModel::new([current_citing("resolvable_thing")]);
        run_verdict_pass(&c, VerdictPass { model: &model, budget: 10 }, 2000).unwrap();
        assert_eq!(model.calls(), 0, "a current deterministic failure row suppresses the retry");

        c.execute(
            "UPDATE repo_memories SET body='The note describes `resolvable_thing` and \
             `gone_thing` as available v2.' WHERE id='m1'",
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

    fn divergence_subjects(findings: &[crate::WorklistFinding]) -> Vec<String> {
        findings
            .iter()
            .filter(|f| f.kind() == Some(FindingKind::MemoryDivergence))
            .map(|f| f.subject.clone())
            .collect()
    }

    #[test]
    fn diverged_opens_finding_then_skip_keeps_it_then_recheck_current_resolves_it() {
        use super::super::{DreamOptions, dream_run_with_passes};

        let c = seeded_verifiable_repo();
        let model = MockChatModel::new([
            diverged_citing(),                  // run 1: opens the divergence finding
            current_citing("resolvable_thing"), // run 3 (after body edit): flips to current
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
            "UPDATE repo_memories SET body='The note describes `resolvable_thing` and \
             `gone_thing` as available v2.' WHERE id='m1'",
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
        let model = MockChatModel::new([diverged_citing()]);
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
        let model = MockChatModel::new([diverged_citing()]);
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
            "UPDATE repo_memories SET body='The note describes `resolvable_thing` and \
             `gone_thing` as available v2.' WHERE id='m1'",
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
        let model = MockChatModel::new([diverged_citing()]);
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
        let model = MockChatModel::new([diverged_citing()]);
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
        seed_memory(
            &c,
            "m1",
            "note",
            "The note describes `thing_one` and `gone_one` as available.",
            "r1",
        );
        seed_live_call_path(&c, "m1", "src/a.rs", "thing_one", "r1");
        let model_r1 = MockChatModel::new(["VERDICT: diverged\nDIRECTION: note_ahead\nCLAIM: \
                                            The note describes `thing_one` and `gone_one` as \
                                            available.\nEVIDENCE:\n- `gone_one`\nREASON: not \
                                            present."
            .to_string()]);
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
        seed_memory(&c, "m2", "note", "The note describes `thing_two` as available.", "r2");
        let model_r2 = MockChatModel::new([current_citing("thing_two")]);
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
