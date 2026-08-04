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
    self, DreamFailureReason, DreamModelFailure, DreamModelPass, FailureStamp, RecordFailure,
};
use super::verify::{
    self, EvidencePack, IdentifierResolution, ResolutionKind, VerificationQueueEntry,
    evidence_pack, verification_queue,
};

/// Rank for a `memory_divergence` finding — high, but below a broken-anchor's pass-0 signal.
const DIVERGENCE_RANK: f64 = 0.8;

/// The verdict-pass configuration handed to [`run_verdict_pass`]: the model to ask and how many
/// queued memories it may check this run (the budget the queue is capped at). Separate from
/// [`DreamOptions`] because it carries a borrow (the model) — `DreamOptions` stays `Copy`.
pub struct VerdictPass<'a> {
    pub model: &'a dyn ChatModel,
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
            // #767 review: the entry's writes commit under the removal-tombstone guard (the
            // model itself is never called for an uncitable entry, so the whole step is a write).
            super::removal_guarded_write_tx(conn, &scope, |tx| {
                record_uncitable(tx, Uncitable {
                    memory_id: &entry.memory_id,
                    repo_id,
                    title: &entry.title,
                    body: &entry.body,
                    checked_inputs_hash: &inputs_hash,
                    checked_against_commit: checked_against_commit.as_deref(),
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
            record_verdict(tx, RecordVerdict {
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
            failure::clear_failure(tx, &failure_stamp)?;
            Ok(())
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
    model: &dyn ChatModel,
    prompt: &str,
    note_title: &str,
    note_body: &str,
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
        if verdict_is_grounded(note_title, note_body, pack_text, &parsed) {
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

// ── Parsing + the fabrication guard ──────────────────────────────────────────────────────────

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

/// Minimum normalized length of a copied note claim. This rejects vacuous fragments such as a
/// single identifier while remaining below the shortest useful sentence in the reviewed replay
/// set.
const MIN_CLAIM_CHARS: usize = 20;
const MIN_CLAIM_WORDS: usize = 4;

/// Deterministic grounding guard for a parsed verdict.
///
/// Every verdict still needs real pack citations. A `diverged` verdict additionally needs a
/// substantial `CLAIM:` copied from the note, and it cannot rest solely on `TextPresent`
/// identifier rows. Text presence can support `current`, but by itself it is not proof that a
/// load-bearing note claim is contradicted.
fn verdict_is_grounded(
    note_title: &str,
    note_body: &str,
    pack_text: &str,
    parsed: &ParsedVerdict,
) -> bool {
    if !verdict_is_cited(pack_text, parsed) {
        return false;
    }
    if parsed.verdict == Verdict::Current {
        return true;
    }
    let Some(claim) = parsed.claim.as_deref() else {
        return false;
    };
    let claim = normalize_ws(claim.trim().trim_matches('"'));
    // The claim must ground in the title OR the body — never a span spliced across the seam the
    // prompt renders between them (`TITLE: {title}\n{body}`), which would let the model assert a
    // sentence the note never makes.
    if claim.chars().filter(|c| !c.is_whitespace()).count() < MIN_CLAIM_CHARS
        || claim.split_whitespace().count() < MIN_CLAIM_WORDS
        || !(claim_grounds_in_span(&claim, note_title) || claim_grounds_in_span(&claim, note_body))
    {
        return false;
    }

    let content: Vec<String> =
        pack_text.lines().filter(|line| is_pack_content_line(line)).map(normalize_ws).collect();
    parsed.evidence.iter().any(|evidence| {
        let cite = normalize_ws(evidence);
        content.iter().any(|line| {
            line.contains(&cite)
                && !line.contains("-> not a defined symbol; appears verbatim as source text")
                && !line.contains("-> not an indexed file; appears verbatim only as source text")
                && match identifier_from_pack_line(line) {
                    // An identifier row grounds divergence only as ABSENCE evidence the citation
                    // actually names: a `symbol`/`file` PRESENCE row never contradicts a claim on
                    // its own, and a bare resolution-label citation (`NOT FOUND …`) convicts
                    // whatever row it substring-matches without naming an identifier.
                    Some(ident) =>
                        line.contains("-> NOT FOUND")
                            && cite.contains(ident)
                            && claim_mentions_identifier(&claim, ident),
                    // Excerpts remain useful model context, but subject overlap cannot
                    // deterministically prove that source contradicts the copied claim.
                    None => false,
                }
        })
    })
}

/// Whether `identifier` occurs case-exactly in `text`, preserving every separator (`/`, `::`,
/// `.`, call punctuation) and delimited from surrounding word characters. Token-only comparison
/// aliases shape-distinct names such as `foo/bar` and `foo::bar`; raw exact shape and boundaries
/// are load-bearing for evidence links.
fn text_mentions_identifier(text: &str, identifier: &str) -> bool {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(identifier).any(|(start, _)| {
        let end = start + identifier.len();
        let left_ok = start == 0 || text[..start].chars().next_back().is_none_or(|c| !is_word(c));
        let right_ok = end == text.len() || text[end..].chars().next().is_none_or(|c| !is_word(c));
        left_ok && right_ok
    })
}

/// One item of a text's mixed stream: a word or a comparison/boolean operator, with the
/// adjacency facts the grounding checks need.
#[derive(Clone, Copy)]
enum StreamItem<'a> {
    /// `code` marks words inside a backtick span (identifiers): they match case-EXACTLY, per
    /// occurrence — the same spelling in prose still case-folds. `glued_to_prev` records
    /// zero-whitespace adjacency to the previous stream ITEM (word or operator).
    Word { text: &'a str, code: bool, glued_to_prev: bool },
    /// `glued_to_prev` as above — the matched claim window extends over glued operator chains
    /// (`Vec<T>`, `Option<Vec<T>>`, `!ready`) but never across whitespace into a sibling
    /// sentence.
    Op { symbol: &'a str, glued_to_prev: bool },
}

/// Words and operators in source order. Word boundaries follow the alphanumeric/`_` rule;
/// operators scan two-character forms first (so `<=` is not `<` + `=`, `!=` is not `!` + `=`),
/// then the single-character comparisons `<`/`>`. Syntax arrows (`->`, `=>`) are punctuation,
/// NOT operators. A unary `!` is an operator ONLY glued to a following word (`!ready`) — a
/// prose exclamation (`Warning!`) is punctuation a copying model may drop. Advances by CHAR,
/// not byte — notes are full of em-dashes, and a byte step off a multi-byte boundary panics.
fn mixed_stream(text: &str) -> Vec<StreamItem<'_>> {
    const OPS: &[&str] = &["==", "!=", "<=", ">=", "&&", "||"];
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    let mut items: Vec<StreamItem<'_>> = Vec::new();
    let mut rest = text;
    let mut in_code = false;
    // Byte offset of `rest` within `text` and the end of the last EMITTED item (glue detection).
    let mut base = 0usize;
    let mut prev_item_end: Option<usize> = None;
    let glued = |start: usize, prev: Option<usize>| prev == Some(start);
    while let Some(c) = rest.chars().next() {
        if c == '`' {
            in_code = !in_code;
            base += 1;
            rest = &rest[1..];
            continue;
        }
        // Syntax arrows are punctuation, not comparison operators.
        if rest.starts_with("->") || rest.starts_with("=>") {
            base += 2;
            rest = &rest[2..];
            continue;
        }
        if let Some(pos) = OPS.iter().position(|op| rest.starts_with(op)) {
            let end = base + 2;
            items.push(StreamItem::Op {
                symbol: OPS[pos],
                glued_to_prev: glued(base, prev_item_end),
            });
            prev_item_end = Some(end);
            base = end;
            rest = &rest[2..];
            continue;
        }
        if c == '<' || c == '>' || (c == '!' && rest[1..].chars().next().is_some_and(is_word_char))
        {
            let symbol = if c == '<' {
                "<"
            } else if c == '>' {
                ">"
            } else {
                "!"
            };
            let end = base + 1;
            items.push(StreamItem::Op { symbol, glued_to_prev: glued(base, prev_item_end) });
            prev_item_end = Some(end);
            base = end;
            rest = &rest[1..];
            continue;
        }
        if is_word_char(c) {
            let end_rel = rest.find(|ch: char| !is_word_char(ch)).unwrap_or(rest.len());
            items.push(StreamItem::Word {
                text: &rest[..end_rel],
                code: in_code,
                glued_to_prev: glued(base, prev_item_end),
            });
            prev_item_end = Some(base + end_rel);
            base += end_rel;
            rest = &rest[end_rel..];
            continue;
        }
        base += c.len_utf8();
        rest = &rest[c.len_utf8()..];
    }
    items
}

/// Whether the WHOLE claim grounds in the span: its words form one contiguous verbatim run
/// (prose case-folded, backticked identifiers case-exact per occurrence), and the claim's
/// operators occur in the same positions as the matched WINDOW's operators — the window extended
/// over operator chains GLUED to its boundary words (`Vec<T>`, `Option<Vec<T>>`, `!ready`) but
/// never across whitespace into a sibling sentence. Flipped, invented, or DROPPED operators
/// (`!ready` → `ready`) all reject. Operators stay out of the word comparison so a model that
/// drops backticks still matches.
fn claim_grounds_in_span(claim: &str, span: &str) -> bool {
    let stream = mixed_stream(span);
    let claim_stream = mixed_stream(claim);
    let claim_words: Vec<&str> = claim_stream
        .iter()
        .filter_map(|item| match item {
            StreamItem::Word { text, .. } => Some(*text),
            StreamItem::Op { .. } => None,
        })
        .collect();
    if claim_words.is_empty() {
        return false;
    }
    let word_positions: Vec<usize> = stream
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| matches!(item, StreamItem::Word { .. }).then_some(idx))
        .collect();
    if word_positions.len() < claim_words.len() {
        return false;
    }
    'windows: for start in 0..=(word_positions.len() - claim_words.len()) {
        for (offset, claim_word) in claim_words.iter().enumerate() {
            let StreamItem::Word { text: note_word, code, .. } =
                stream[word_positions[start + offset]]
            else {
                unreachable!("word_positions indexes only Word items")
            };
            let matches = if code {
                note_word == *claim_word
            } else {
                note_word.eq_ignore_ascii_case(claim_word)
            };
            if !matches {
                continue 'windows;
            }
        }
        let mut first = word_positions[start];
        let mut last = word_positions[start + claim_words.len() - 1];
        // Extend over operators glued to the boundary words (attached unary `!`, generic
        // brackets — including CHAINS like `>>`): they belong to the copied expression even
        // though they sit outside the first/last word.
        while first > 0
            && matches!(stream[first - 1], StreamItem::Op { .. })
            && matches!(
                stream[first],
                StreamItem::Word { glued_to_prev: true, .. }
                    | StreamItem::Op { glued_to_prev: true, .. }
            )
        {
            first -= 1;
        }
        while last + 1 < stream.len()
            && matches!(stream[last + 1], StreamItem::Op { glued_to_prev: true, .. })
        {
            last += 1;
        }
        let window_shape: Vec<Option<&str>> = stream[first..=last]
            .iter()
            .map(|item| match item {
                StreamItem::Op { symbol, .. } => Some(*symbol),
                StreamItem::Word { .. } => None,
            })
            .collect();
        let claim_shape: Vec<Option<&str>> = claim_stream
            .iter()
            .map(|item| match item {
                StreamItem::Op { symbol, .. } => Some(*symbol),
                StreamItem::Word { .. } => None,
            })
            .collect();
        if window_shape == claim_shape {
            return true;
        }
    }
    false
}

/// Whether a cited identifier occurs in the claim with its COMPLETE, case-exact shape — not as a
/// substring (`writer_state` vs `active_writer_stateful`), case variant (`foo` vs `Foo`), or
/// separator variant (`foo/bar` vs `foo::bar`).
fn claim_mentions_identifier(claim: &str, identifier: &str) -> bool {
    text_mentions_identifier(claim, identifier)
}

/// Extract the identifier from a rendered table row (``- `identifier` -> resolution``). Divergence
/// citations to identifier rows must name an identifier that also occurs in the copied claim; this
/// prevents an incidental NOT-FOUND row elsewhere in a long note from back-justifying an unrelated
/// load-bearing claim.
fn identifier_from_pack_line(line: &str) -> Option<&str> {
    line.trim().strip_prefix("- `")?.split_once("` ->").map(|(identifier, _)| identifier)
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
    crate::bump_memory_lens_lanes(conn, r.repo_id)?;
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
    use super::super::tests::{mem_db, set_repo};
    use super::*;
    use crate::mock_chat::MockChatModel;

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
    fn current_citing(ident: &str) -> String {
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
            claim: None,
            evidence: vec![frag.to_string()],
        };
        assert!(
            !verdict_is_cited(
                &pack,
                &cited("IDENTIFIERS (resolved against the active source index):")
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
            claim: Some("the note makes a substantial claim".to_string()),
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
    fn divergence_guard_requires_a_verbatim_note_claim() {
        let note = "The `gone_symbol` function remains available to callers.";
        let pack = "IDENTIFIERS:\n- `gone_symbol` -> NOT FOUND anywhere in the source tree\n";
        let grounded = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_symbol` function remains \
             available to callers.\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &grounded),
            "a substantial claim copied from the note plus absence evidence is grounded"
        );
        let quoted = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: \"The `gone_symbol` function \
             remains available to callers.\"\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND anywhere in \
             the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &quoted),
            "harmless outer quotes around a verbatim claim are accepted"
        );

        let missing = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND \
             anywhere in the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &missing),
            "divergence without a copied note claim is rejected"
        );

        let paraphrased = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: Callers can still use the gone \
             function.\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &paraphrased),
            "a plausible paraphrase is not deterministic claim grounding"
        );
    }

    #[test]
    fn divergence_guard_rejects_text_present_only_evidence() {
        let note = "The content_hash column is persisted with each verdict.";
        let pack = "IDENTIFIERS:\n- `content_hash` -> not a defined symbol; appears verbatim as \
                    source text\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: unknown\nCLAIM: The content_hash column is persisted \
             with each verdict.\nEVIDENCE:\n- `content_hash` -> not a defined symbol; appears \
             verbatim as source text\nREASON: not a symbol.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "text presence alone cannot establish a contradiction"
        );
    }

    #[test]
    fn divergence_guard_requires_cited_identifier_to_occur_in_the_claim() {
        let note = "The active writer remains serialized. An old fixture used `gone_helper`.";
        let pack = "IDENTIFIERS:\n- `gone_helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The active writer remains \
             serialized.\nEVIDENCE:\n- `gone_helper` -> NOT FOUND anywhere in the source \
             tree\nREASON: helper gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "an incidental absence elsewhere in the note cannot back-justify the copied claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_excerpt_only_contradictions() {
        let note = "The `active_writer` guard remains serialized.";
        let pack = "IDENTIFIERS:\n- `active_writer` -> symbol \
                    crates/x/src/lib.rs::active_writer\n\nBOUND-FILE \
                    EXCERPTS:\ncrates/x/src/lib.rs:12: let handle = \
                    spawn();\ncrates/x/src/lib.rs:40: let active_writer = spawn();\n";
        let unrelated = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer` guard remains \
             serialized.\nEVIDENCE:\n- crates/x/src/lib.rs:12: let handle = spawn();\nREASON: \
             contradicts.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &unrelated),
            "an excerpt about an unrelated mechanism is not linked evidence"
        );
        let same_subject = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer` guard remains \
             serialized.\nEVIDENCE:\n- crates/x/src/lib.rs:40: let active_writer = \
             spawn();\nREASON: contradicts.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &same_subject),
            "subject overlap cannot prove that an excerpt contradicts the claim"
        );
    }

    #[test]
    fn divergence_guard_matches_the_cited_identifier_as_a_complete_token() {
        let note = "The `active_writer_stateful` guard remains available. The legacy \
                    `writer_state` was removed.";
        let pack = "IDENTIFIERS:\n- `writer_state` -> NOT FOUND anywhere in the source tree\n- \
                    `active_writer_stateful` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer_stateful` guard \
             remains available.\nEVIDENCE:\n- `writer_state` -> NOT FOUND anywhere in the source \
             tree\nREASON: writer gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a substring of a longer identifier is not the cited identifier"
        );
        let grounded = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer_stateful` guard \
             remains available.\nEVIDENCE:\n- `active_writer_stateful` -> NOT FOUND anywhere in \
             the source tree\nREASON: contradicts the note.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &grounded),
            "the exact identifier in the claim still grounds the verdict"
        );
    }

    #[test]
    fn divergence_guard_preserves_identifier_separators() {
        let note = "The `foo/bar` file remains available; legacy `foo::bar` was removed.";
        let pack = "IDENTIFIERS:\n- `foo::bar` -> NOT FOUND anywhere in the source tree\n- \
                    `foo/bar` -> NOT FOUND anywhere in the source tree\n";
        let wrong_shape = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `foo/bar` file remains \
             available.\nEVIDENCE:\n- `foo::bar` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &wrong_shape),
            "a `foo::bar` absence row does not link to the `foo/bar` file claim"
        );
        let exact = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `foo/bar` file remains \
             available.\nEVIDENCE:\n- `foo/bar` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &exact),
            "the exact separator-preserving identifier still links"
        );
    }

    #[test]
    fn divergence_guard_delimits_punctuation_edged_identifiers() {
        assert!(!text_mentions_identifier("Status.idle", ".idle"));
        assert!(text_mentions_identifier("state is `.idle`", ".idle"));
        assert!(!text_mentions_identifier("foo()suffix", "foo()"));
        assert!(text_mentions_identifier("call `foo()` now", "foo()"));
    }

    #[test]
    fn divergence_guard_rejects_a_generic_word_excerpt_link() {
        // A generic domain word (`error`) shared with an excerpt about something else must not
        // establish the link — only a shared pack identifier does.
        let note = "The `active_writer` guard reports an error and halts.";
        let pack = "IDENTIFIERS:\n- `active_writer` -> symbol \
                    crates/x/src/lib.rs::active_writer\n\nBOUND-FILE EXCERPTS:\nsrc/parser.rs:9: \
                    // parse error recovery\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer` guard reports \
             an error and halts.\nEVIDENCE:\n- src/parser.rs:9: // parse error recovery\nREASON: \
             changed.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "`error` alone does not link an unrelated excerpt to the claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_an_excerpt_citation_without_its_locator() {
        // Source text can mimic pack rows: an excerpt whose CODE reads like a NOT FOUND row must
        // not ground a citation that omits the `path:line:` locator.
        let note = "The `gone_helper` function remains available.";
        let pack = "IDENTIFIERS:\n- `gone_helper` -> not a defined symbol; appears verbatim as \
                    source text\n\nBOUND-FILE EXCERPTS:\ncrates/x/src/lib.rs:12: // - \
                    `gone_helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_helper` function remains \
             available.\nEVIDENCE:\n- `gone_helper` -> NOT FOUND anywhere in the source \
             tree\nREASON: spoofed.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "label text mimicked inside excerpt code is not a NOT FOUND row"
        );
    }

    #[test]
    fn divergence_guard_rejects_an_operator_flipped_claim() {
        let note = "The parser rejects input where `limit <= 0` and returns an error.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             limit >= 0 and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a claim that flips the note's operator asserts the opposite and must not ground"
        );
        let verbatim = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             `limit <= 0` and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in \
             the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &verbatim),
            "a verbatim operator copy still grounds"
        );
    }

    #[test]
    fn divergence_guard_tolerates_non_ascii_note_text() {
        // Regression: the operator scan must not panic on multi-byte characters (em-dash) —
        // real notes are full of them.
        let note = "The writer — serialized globally — rejects `limit <= 0` here.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The writer — serialized globally — \
             rejects `limit <= 0` here.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "a grounded verdict over non-ASCII note text is accepted, not panicked on"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_single_char_operator_flip() {
        let note = "The parser rejects input where `limit > 0` and returns an error.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             limit < 0 and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a single-character `>`→`<` flip asserts the opposite and must not ground"
        );
        let verbatim = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             `limit > 0` and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &verbatim),
            "a verbatim copy carrying the same operator still grounds"
        );
    }

    #[test]
    fn divergence_guard_preserves_operator_to_operand_order() {
        let note = "The range requires `low < value && value > high` before continuing.";
        let pack = "IDENTIFIERS:\n- `value` -> NOT FOUND anywhere in the source tree\n";
        let flipped = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The range requires `low > value && \
             value < high` before continuing.\nEVIDENCE:\n- `value` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &flipped),
            "the same operator multiset attached to different operands must not ground"
        );
    }

    #[test]
    fn divergence_guard_does_not_alias_case_distinct_identifiers() {
        // The note says `Foo` remains while legacy `foo` was removed; a claim that LOWERCASES
        // `Foo` to `foo` and cites the `foo` absence row turns a confirmation into a false
        // divergence. Backticked words match case-exactly, so the lowercased claim never grounds.
        let note = "The `Foo` type remains available; legacy `foo` was removed.";
        let pack = "IDENTIFIERS:\n- `foo` -> NOT FOUND anywhere in the source tree\n- `Foo` -> \
                    symbol src/lib.rs::Foo\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The foo type remains available; \
             legacy foo was removed.\nEVIDENCE:\n- `foo` -> NOT FOUND anywhere in the source \
             tree\nREASON: foo gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a case-variant of a backticked identifier is not the note's identifier"
        );
        let exact = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `Foo` type remains available; \
             legacy `foo` was removed.\nEVIDENCE:\n- `foo` -> NOT FOUND anywhere in the source \
             tree\nREASON: foo gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &exact),
            "a case-exact verbatim copy still grounds and links"
        );
    }

    #[test]
    fn divergence_guard_accepts_a_verbatim_leading_negation() {
        // The matched window must extend over the operator glued to its first word, or a
        // verbatim `!ready` claim is wrongly discarded.
        let note = "The gate holds while `!ready` remains false.";
        let pack = "IDENTIFIERS:\n- `ready` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The gate holds while `!ready` \
             remains false.\nEVIDENCE:\n- `ready` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "a verbatim claim beginning with an attached operator grounds"
        );
    }

    #[test]
    fn divergence_guard_ignores_syntax_arrows() {
        // `->` and `=>` are syntax punctuation, not comparison operators. A copied claim may
        // omit them under punctuation-tolerant grounding without changing semantics.
        let note = "The `load` function returns `Result` as `fn load() -> Result`; the mapper \
                    uses `x => y`.";
        let pack = "IDENTIFIERS:\n- `load` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `load` function returns \
             `Result` as fn load Result; the mapper uses x y.\nEVIDENCE:\n- `load` -> NOT FOUND \
             anywhere in the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "dropping syntax arrows does not change the copied claim"
        );
    }

    #[test]
    fn divergence_guard_accepts_nested_generic_closers() {
        // The matched window must extend through the entire glued `>>` chain after its last word.
        let note = "The `value` field stores `Option<Vec<T>>` unchanged.";
        let pack = "IDENTIFIERS:\n- `value` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `value` field stores \
             `Option<Vec<T>>` unchanged.\nEVIDENCE:\n- `value` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "a verbatim nested generic includes every glued closing bracket"
        );
    }

    #[test]
    fn divergence_guard_tolerates_a_dropped_prose_exclamation() {
        // `Warning!` is punctuation, not a unary operator — a model that copies the words but
        // drops the bang still grounds.
        let note = "Warning! The helper remains available.";
        let pack = "IDENTIFIERS:\n- `helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: Warning! The helper remains \
             available.\nEVIDENCE:\n- `helper` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(verdict_is_grounded("note", note, pack, &parsed), "verbatim with the bang grounds");
        let dropped = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: Warning The helper remains \
             available.\nEVIDENCE:\n- `helper` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &dropped),
            "a dropped prose exclamation still grounds"
        );
    }

    #[test]
    fn divergence_guard_folds_case_for_prose_occurrences_of_a_backticked_word() {
        // The SAME spelling appears backticked (identifier) and bare (prose): only the backticked
        // occurrence is case-exact — a lowercased prose occurrence still grounds.
        let note = "The `Foo` type remains the Foo alias.";
        let pack = "IDENTIFIERS:\n- `Foo` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `Foo` type remains the foo \
             alias.\nEVIDENCE:\n- `Foo` -> NOT FOUND anywhere in the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "case is exact for the backticked occurrence, folded for the prose one"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_dropped_unary_negation() {
        let note = "The `publisher` stays enabled when `!ready` is true.";
        let pack = "IDENTIFIERS:\n- `publisher` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `publisher` stays enabled when \
             ready is true.\nEVIDENCE:\n- `publisher` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "dropping the note's unary `!` inverts the claim and must not ground"
        );
        let verbatim = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `publisher` stays enabled when \
             `!ready` is true.\nEVIDENCE:\n- `publisher` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &verbatim),
            "a verbatim copy keeping the negation still grounds"
        );
    }

    #[test]
    fn divergence_guard_pools_operators_only_within_the_grounding_span() {
        // An operator elsewhere in the note must not satisfy a flip in the copied sentence:
        // `old != new` in the TITLE does not excuse `<=`→`!=` in a claim copied from the BODY.
        let title = "Migration from `old != new` semantics";
        let body = "The parser rejects input where `limit <= 0` and returns an error.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             limit != 0 and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded(title, body, pack, &parsed),
            "an unrelated operator elsewhere in the note does not satisfy a flipped claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_title_body_splice_claim() {
        let pack = "IDENTIFIERS:\n- `sweeper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: lazy and the sweeper runs \
             hourly.\nEVIDENCE:\n- `sweeper` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded(
                "Cache eviction is lazy",
                "and the sweeper runs hourly.",
                pack,
                &parsed
            ),
            "a span spliced across the title/body seam is a sentence the note never makes"
        );
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
    fn divergence_guard_rejects_presence_row_and_bare_label_citations() {
        // A `symbol`/`file` PRESENCE row never contradicts a claim on its own, and a bare
        // resolution-label citation (`NOT FOUND …`) names no identifier — both must be rejected
        // even when the copied claim is perfectly grounded.
        let note = "The `gone_symbol` function remains available to callers.";
        let present_pack = "IDENTIFIERS:\n- `gone_symbol` -> symbol src/lib.rs::gone_symbol\n";
        let presence = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_symbol` function remains \
             available to callers.\nEVIDENCE:\n- `gone_symbol` -> symbol \
             src/lib.rs::gone_symbol\nREASON: hallucinated.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, present_pack, &presence),
            "a presence row is not contradiction evidence"
        );
        let absent_pack =
            "IDENTIFIERS:\n- `gone_symbol` -> NOT FOUND anywhere in the source tree\n";
        let bare_label = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_symbol` function remains \
             available to callers.\nEVIDENCE:\n- NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, absent_pack, &bare_label),
            "the citation must name the identifier it convicts"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_copied_prefix_with_invented_tail() {
        let note = "The active writer remains serialized.";
        let pack = "IDENTIFIERS:\n- `gone_helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The active writer remains \
             serialized and gone_helper remains available\nEVIDENCE:\n- `gone_helper` -> NOT \
             FOUND anywhere in the source tree\nREASON: helper gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a verbatim prefix must not ground invented prose appended to the claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_bare_long_identifier_as_the_claim() {
        let identifier = "a_tail_failure_leaves_the_old_generation_live";
        let note = format!("The `{identifier}` test documents tail-failure recovery.");
        let pack =
            format!("IDENTIFIERS:\n- `{identifier}` -> NOT FOUND anywhere in the source tree\n");
        let parsed = parse_verdict(&format!(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: \"{identifier}\"\nEVIDENCE:\n- \
             `{identifier}` -> NOT FOUND anywhere in the source tree\nREASON: gone."
        ))
        .unwrap();
        assert!(
            !verdict_is_grounded("note", &note, &pack, &parsed),
            "identifier length alone does not make it a load-bearing claim"
        );
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

        let entry = verification_queue(&c, 1, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.memory_id == "m1")
            .unwrap();
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
            .filter(|f| f.kind == "memory_divergence")
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
