//! Dream v2 pass 2 — the MODEL compaction pass (the second generative-model dependency, #122).
//!
//! Where pass 1 (`verdict`) audits a note against the code, this pass REWRITES the note body into a
//! 3–4 sentence, self-contained summary a drive-by surface can show instead of the full body. It is
//! deliberately spartan: one model turn per memory, NO tools, NO code context, NO evidence pack —
//! the note body is the whole input (the offline eval measured that code context DEGRADES
//! compaction fidelity). Accepted summaries land in `memory_summaries` (PK `(repo_id, memory_id,
//! content_hash)`, so a title/body edit self-invalidates and re-queues); a rejected one records a
//! failure row while the absence of a summary row remains the title-only fallback at surfacing.
//!
//! Two surfaces the rest of dream consumes:
//!   - [`run_compact_pass`] — the budgeted runner: queue (active memories with no summary for their
//!     CURRENT note) → prompt → summary → deterministic acceptance guards (retry once) → on accept,
//!     UPSERT `memory_summaries` and prune the superseded (older content_hash) rows.
//!   - the [`guards`] module — the ONLY runtime checks (a deliberate design decision: no
//!     shape-regex ref linting). Sentence/word bounds, no paragraph breaks, non-empty, and
//!     tracker-ref resolvability by SET-MEMBERSHIP against the indexed papertrail.
//!
//! Like every dream pass it NEVER writes a `repo_memories` column — `memory_summaries` holds
//! derived, regenerable data.

use rag_rat_db::schema;
use rusqlite::{Connection, OptionalExtension};

use super::failure::{
    self, DreamFailureReason, DreamModelFailure, DreamModelPass, FailureStamp, RecordFailure,
};
use super::model::VerdictModel;

/// The compaction prompt version, stamped into `memory_summaries.prompt_version`. Bump on any
/// change to [`COMPACT_PROMPT_HEAD`] so a stale-prompt summary is distinguishable (and can be
/// regenerated).
pub(crate) const COMPACT_PROMPT_VERSION: &str = "compact-v1";

/// The compaction-pass configuration handed to [`run_compact_pass`]: the model to ask and how many
/// queued memories it may compact this run. Mirrors [`super::VerdictPass`] — a borrowed model + a
/// budget — so it carries a lifetime and stays out of the `Copy` [`super::DreamOptions`]. It reuses
/// the SAME [`VerdictModel`] trait (both passes are single-turn temperature-0 completions).
pub struct CompactPass<'a> {
    pub model: &'a dyn VerdictModel,
    pub budget: usize,
}

/// One memory that needs (re)compaction — an active memory with no `memory_summaries` row for its
/// CURRENT note (title+body). Ordered by `memory_id`, capped by the pass budget.
struct CompactionEntry {
    memory_id: String,
    title: String,
    body: String,
}

// ── The pass-2 runner ─────────────────────────────────────────────────────────────────────────

/// Run the compaction pass over the churn-skip queue (budget-capped). For each queued memory:
/// render the prompt (note body only), ask the model, run the deterministic acceptance guards
/// (retry once), and on accept UPSERT `memory_summaries` (pruning superseded rows). A
/// rejected-twice completion records `memory_model_failures`; a transient model-call error is
/// recorded for audit but remains retryable. The absence of a summary row is still the title-only
/// fallback at surfacing. Repo-scoped; never writes a `repo_memories` column.
pub(super) fn run_compact_pass(
    conn: &Connection,
    pass: CompactPass<'_>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let queue = compaction_queue(conn, usize::MAX)?;
    if queue.is_empty() {
        return Ok(());
    }
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_id = scope.as_deref().unwrap_or("__unassigned__");

    let mut processed = 0usize;
    for entry in queue {
        if processed >= pass.budget {
            break;
        }
        let content_hash = super::verify::note_content_hash(&entry.title, &entry.body);
        let failure_stamp = FailureStamp {
            memory_id: &entry.memory_id,
            repo_id,
            pass: DreamModelPass::Compact,
            content_hash: &content_hash,
            checked_inputs_hash: None,
            prompt_version: COMPACT_PROMPT_VERSION,
            model_id: pass.model.model_id(),
        };
        if failure::blocking_failure_is_current(conn, &failure_stamp)? {
            continue;
        }
        processed += 1;
        let prompt = render_compact_prompt(&entry.title, &entry.body);
        let summary = match obtain_summary(conn, pass.model, &prompt)? {
            Ok(summary) => summary,
            Err(failure) => {
                failure::record_failure(conn, RecordFailure {
                    stamp: failure_stamp,
                    failure: &failure,
                    now_ms,
                })?;
                continue;
            },
        };
        record_summary(conn, RecordSummary {
            memory_id: &entry.memory_id,
            repo_id,
            title: &entry.title,
            body: &entry.body,
            summary: &summary,
            model_id: pass.model.model_id(),
            now_ms,
        })?;
        let failure_stamp = FailureStamp {
            memory_id: &entry.memory_id,
            repo_id,
            pass: DreamModelPass::Compact,
            content_hash: &content_hash,
            checked_inputs_hash: None,
            prompt_version: COMPACT_PROMPT_VERSION,
            model_id: pass.model.model_id(),
        };
        failure::clear_failure(conn, &failure_stamp)?;
    }
    Ok(())
}

/// Active memories that still need a summary — no `memory_summaries` row keyed on their CURRENT
/// `content_hash`. A title or body edit changes the key and re-enqueues (the summary
/// self-invalidates); an unchanged, already-summarized memory churn-skips, so re-running is cheap.
/// Repo-scoped, ordered by `memory_id`, capped at `budget`.
fn compaction_queue(conn: &Connection, budget: usize) -> rusqlite::Result<Vec<CompactionEntry>> {
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let summary_clause = schema::periphery_repo_scope_clause(&scope, "memory_summaries");

    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, body FROM repo_memories WHERE status = 'active'{mem_clause} ORDER BY id"
    ))?;
    let mems: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut queue = Vec::new();
    for (memory_id, title, body) in mems {
        // The current content hash keys the summary; a stored summary under a DIFFERENT hash is
        // stale (a TITLE or body edit — the compaction prompt frames both) and does NOT count as
        // covered, so the memory re-queues. The `prompt_version` predicate does the same for a
        // `COMPACT_PROMPT_VERSION` bump: a summary produced by an older prompt/guards no longer
        // counts as covered, so a prompt change regenerates every summary instead of surfacing
        // stale ones indefinitely.
        let content_hash = super::verify::note_content_hash(&title, &body);
        let covered = conn
            .query_row(
                &format!(
                    "SELECT 1 FROM memory_summaries WHERE memory_id = ?1 AND content_hash = ?2 \
                     AND prompt_version = ?3{summary_clause}"
                ),
                rusqlite::params![memory_id, content_hash, COMPACT_PROMPT_VERSION],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !covered {
            queue.push(CompactionEntry { memory_id, title, body });
        }
    }
    queue.truncate(budget);
    Ok(queue)
}

/// Whether the compaction queue has ANY pending memory (budget-capped) — the compaction half of the
/// ephemeral zero-work guard ([`super::model_work_pending`]). Cheap: the same churn-skip query the
/// pass runs, short-circuited to emptiness so a fully-summarized repo never cold-starts a paid box.
pub(super) fn compaction_pending(
    conn: &Connection,
    budget: usize,
    model_id: &str,
) -> rusqlite::Result<bool> {
    if budget == 0 {
        return Ok(false);
    }
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_id = scope.as_deref().unwrap_or("__unassigned__");
    for entry in compaction_queue(conn, usize::MAX)? {
        let content_hash = super::verify::note_content_hash(&entry.title, &entry.body);
        let failure_stamp = FailureStamp {
            memory_id: &entry.memory_id,
            repo_id,
            pass: DreamModelPass::Compact,
            content_hash: &content_hash,
            checked_inputs_hash: None,
            prompt_version: COMPACT_PROMPT_VERSION,
            model_id,
        };
        if failure::blocking_failure_is_current(conn, &failure_stamp)? {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// Ask the model once, strip any think block, and run the deterministic acceptance guards. On a
/// guard failure RETRY ONCE (same prompt); a second failure returns a typed deterministic failure.
/// A model call error also returns a typed failure, but that reason does not suppress future work;
/// a DB error in the guards propagates. Small models drift, so the guards are load-bearing — an
/// unchecked summary would surface a wrong 3-line claim as if authoritative.
fn obtain_summary(
    conn: &Connection,
    model: &dyn VerdictModel,
    prompt: &str,
) -> rusqlite::Result<Result<String, DreamModelFailure>> {
    for attempt in 1..=2 {
        let raw = match model.complete(prompt) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!(target: "rag_rat_core::dream::compact", attempt, %err, "compaction model call failed; skipping this memory");
                return Ok(Err(DreamModelFailure::with_detail(
                    DreamFailureReason::ModelCallFailed,
                    err.to_string(),
                )));
            },
        };
        let summary = strip_think(&raw);
        if guards::accepts(conn, summary)? {
            return Ok(Ok(summary.to_string()));
        }
        tracing::warn!(target: "rag_rat_core::dream::compact", attempt, "compaction summary failed the acceptance guards");
    }
    Ok(Err(DreamModelFailure::new(DreamFailureReason::SummaryGuardRejected)))
}

/// Drop a leading `<think>…</think>` reasoning block a thinking model may prepend (the default
/// `qwen3:4b-instruct` does not think, but an operator may point `[llm.dream.remote]` at one that
/// does), then trim — mirrors the eval harness's `strip_think`. Everything after the LAST
/// `</think>` is the summary.
fn strip_think(raw: &str) -> &str {
    match raw.rsplit_once("</think>") {
        Some((_, after)) => after.trim(),
        None => raw.trim(),
    }
}

// ── Prompt rendering ──────────────────────────────────────────────────────────────────────────

/// The compaction prompt head — the v2 self-containment prompt from the memory-compaction eval. It
/// pins the shape (exactly 3–4 sentences, ≤90 words), the polarity guard (ONLY / NEVER / NOT /
/// UNLESS / EXCEPT keep their meaning), the no-added-facts rule, and the SELF-CONTAINMENT rule: the
/// reader sees the codebase but NOT issue trackers or review threads, so state the fact a tracker /
/// phase / review-round label stands for rather than citing it — while KEEPING in-code identifiers
/// (function / table / config names, migration names like `V042`). A bug-and-fix note states the
/// post-fix behavior as current. The runtime guards enforce only what is deterministically
/// decidable (shape + tracker-ref resolvability); the prose rules the guards cannot check are the
/// model's job, measured offline. Authored as markdown in `prompts/compact_head.md` and embedded
/// at compile time via [`include_str!`] (no runtime IO or install-path lookup — the file ships
/// inside the binary); edits live in that `.md`, and [`COMPACT_PROMPT_VERSION`] is bumped when they
/// change. Trimmed at the render
/// site so a trailing newline in the file cannot perturb the rendered prompt.
const COMPACT_PROMPT_HEAD: &str = include_str!("prompts/compact_head.md");

/// Render the full single-turn compaction prompt for one memory. Built by concatenation (not
/// `format!`) because the title/body can contain literal `{`/`}`. Mirrors the eval harness's
/// `TITLE:` / `NOTE:` framing.
fn render_compact_prompt(title: &str, body: &str) -> String {
    let mut p = String::with_capacity(COMPACT_PROMPT_HEAD.len() + title.len() + body.len() + 16);
    p.push_str(COMPACT_PROMPT_HEAD.trim_end());
    p.push_str("\n\nTITLE: ");
    p.push_str(title);
    p.push_str("\nNOTE:\n");
    p.push_str(body);
    p
}

// ── memory_summaries write ────────────────────────────────────────────────────────────────────

/// Params for the single `memory_summaries` UPSERT — one struct so the writer isn't a long
/// positional train of same-typed strings.
struct RecordSummary<'a> {
    memory_id: &'a str,
    repo_id: &'a str,
    title: &'a str,
    body: &'a str,
    summary: &'a str,
    model_id: &'a str,
    now_ms: i64,
}

/// UPSERT the accepted summary into `memory_summaries` (PK `(repo_id, memory_id, content_hash)`)
/// AND prune every superseded row (same memory, a DIFFERENT content_hash) in ONE transaction. That
/// prune is the invariant: in steady state `memory_summaries` holds exactly ONE row per memory —
/// the summary of its current note. Without it a churny memory would accrete a stale summary per
/// past content_hash, and the surfacing LEFT JOIN (keyed on the current content_hash) would still
/// be correct but the table would grow unboundedly. NEVER writes a `repo_memories` column.
fn record_summary(conn: &Connection, r: RecordSummary<'_>) -> rusqlite::Result<()> {
    let content_hash = super::verify::note_content_hash(r.title, r.body);
    let tx = conn.unchecked_transaction()?;
    // Prune superseded summaries (older content_hash) FIRST, so the memory is left with only its
    // current-note summary after the UPSERT.
    tx.execute(
        "DELETE FROM memory_summaries WHERE repo_id = ?1 AND memory_id = ?2 AND content_hash != ?3",
        rusqlite::params![r.repo_id, r.memory_id, content_hash],
    )?;
    tx.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, model_id, \
         prompt_version, generated_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(repo_id, \
         memory_id, content_hash) DO UPDATE SET summary = excluded.summary, model_id = \
         excluded.model_id, prompt_version = excluded.prompt_version, generated_at_ms = \
         excluded.generated_at_ms",
        rusqlite::params![
            r.memory_id,
            r.repo_id,
            content_hash,
            r.summary,
            r.model_id,
            COMPACT_PROMPT_VERSION,
            r.now_ms,
        ],
    )?;
    tx.commit()
}

// ── Deterministic acceptance guards ──────────────────────────────────────────────────────────

/// The ONLY runtime checks on a candidate summary (a deliberate design decision: no shape-regex ref
/// linting). Everything here is deterministically decidable; the prose rules the guards cannot
/// check (polarity, no-added-facts, self-containment prose) are the model's job, measured offline.
pub(super) mod guards {
    use std::collections::BTreeSet;
    use std::sync::LazyLock;

    use rag_rat_db::schema;
    use regex::Regex;
    use rusqlite::{Connection, OptionalExtension};

    /// Word-count ceiling — headroom over the prompt's "at most 90 words" so a slightly-long but
    /// otherwise faithful summary is not thrown away.
    const MAX_WORDS: usize = 110;
    /// A summary is exactly 3–4 sentences (the prompt's shape).
    const MIN_SENTENCES: usize = 3;
    const MAX_SENTENCES: usize = 4;

    /// `#123` — the tracker shorthand.
    static HASH_REF_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"#(\d+)").expect("static regex"));
    /// `PR 123` / `PR #123` / `issue 123` / `pull request 123` (case-insensitive). Captures the
    /// number.
    static WORD_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:pr|pull request|pull|issue)\s+#?(\d+)").expect("static regex")
    });

    /// Whether a candidate summary passes every deterministic guard. Order is cheapest-first so a
    /// malformed candidate short-circuits before the DB read: non-empty, no paragraph break, word
    /// cap, sentence count, then tracker-ref resolvability. The DB read is a `rusqlite::Result` so
    /// a storage error propagates (it is not a "reject" — the caller discards the memory).
    pub(super) fn accepts(conn: &Connection, summary: &str) -> rusqlite::Result<bool> {
        let trimmed = summary.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }
        // No paragraph breaks: a summary is one tight block, not a mini-document.
        if summary.contains("\n\n") {
            return Ok(false);
        }
        if word_count(trimmed) > MAX_WORDS {
            return Ok(false);
        }
        if !(MIN_SENTENCES..=MAX_SENTENCES).contains(&count_sentences(trimmed)) {
            return Ok(false);
        }
        tracker_refs_resolve(conn, trimmed)
    }

    /// Whitespace-delimited word count.
    fn word_count(summary: &str) -> usize {
        summary.split_whitespace().count()
    }

    /// Identifier-tolerant sentence count: a terminator (`.`/`!`/`?`) closes a sentence ONLY when
    /// it is at end-of-text (nothing but whitespace after) OR followed by whitespace and then a
    /// capital / backtick / open-paren. So an in-identifier dot — `files.generation`, `3.5x`,
    /// `e.g. foo` — does NOT split, which naive `split('.')` gets wrong.
    pub(super) fn count_sentences(summary: &str) -> usize {
        let chars: Vec<char> = summary.chars().collect();
        let mut count = 0usize;
        for (i, &c) in chars.iter().enumerate() {
            if !matches!(c, '.' | '!' | '?') {
                continue;
            }
            let rest = &chars[i + 1..];
            // End-of-text: the terminator closes the final sentence.
            if rest.iter().all(|c| c.is_whitespace()) {
                count += 1;
                continue;
            }
            // Otherwise require whitespace THEN a sentence-opening glyph. `rest` is non-empty here
            // (the all-whitespace case returned above), so `rest[0]` is in bounds.
            if !rest[0].is_whitespace() {
                continue;
            }
            let mut j = 0;
            while j < rest.len() && rest[j].is_whitespace() {
                j += 1;
            }
            if let Some(&next) = rest.get(j)
                && (next.is_uppercase() || next == '`' || next == '(')
            {
                count += 1;
            }
        }
        count
    }

    /// Whether every tracker reference in the summary RESOLVES in the indexed papertrail — the only
    /// ref check (no shape lint). A ref that resolves is KEPT (a consumer can expand it via the
    /// papertrail tools); an unresolvable one FAILS the guard. Refs are matched by SET-MEMBERSHIP
    /// against `papertrail_items` (repo-scoped; either item kind counts). When the papertrail is
    /// EMPTY (sync never ran), the guard is SKIPPED rather than failing everything — otherwise a
    /// repo with no synced items could never keep a legitimately-referenced number.
    fn tracker_refs_resolve(conn: &Connection, summary: &str) -> rusqlite::Result<bool> {
        let refs = extract_tracker_numbers(summary);
        if refs.is_empty() {
            return Ok(true);
        }
        let scope = schema::periphery_repo_scope(conn, "papertrail_items")?;
        if papertrail_is_empty(conn, &scope)? {
            return Ok(true);
        }
        for number in refs {
            if !ref_exists(conn, number, &scope)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Every candidate tracker number in the summary (`#123`, `PR 123`, `issue 123`, …), deduped
    /// and sorted. Numbers that overflow `i64` are dropped (no real tracker number is that
    /// large). Bare in-code identifiers with no `#`/`PR`/`issue` cue — migration names
    /// (`V042`), phase labels (`A5`, `R2`) — are NOT extracted, so they never trigger this
    /// guard (they are the prompt's job, measured offline).
    fn extract_tracker_numbers(summary: &str) -> BTreeSet<i64> {
        let mut numbers = BTreeSet::new();
        for caps in HASH_REF_RE.captures_iter(summary).chain(WORD_REF_RE.captures_iter(summary)) {
            if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok()) {
                numbers.insert(n);
            }
        }
        numbers
    }

    /// Whether the active repo's papertrail is empty (sync never ran) — the skip signal. The
    /// table is repo-scoped, so a sibling repo's items never make THIS repo's papertrail read
    /// non-empty.
    fn papertrail_is_empty(conn: &Connection, scope: &Option<String>) -> rusqlite::Result<bool> {
        let item_clause = schema::periphery_repo_scope_clause(scope, "papertrail_items");
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM papertrail_items WHERE 1=1{item_clause}"),
            [],
            |r| r.get(0),
        )?;
        Ok(count == 0)
    }

    /// Whether `number` exists as ANY item (issue or change request) in the active repo's
    /// papertrail. `item_key` is TEXT (provider keys are not uniformly numeric), so the extracted
    /// number matches by its canonical decimal rendering.
    fn ref_exists(
        conn: &Connection,
        number: i64,
        scope: &Option<String>,
    ) -> rusqlite::Result<bool> {
        let item_clause = schema::periphery_repo_scope_clause(scope, "papertrail_items");
        Ok(conn
            .query_row(
                &format!("SELECT 1 FROM papertrail_items WHERE item_key = ?1{item_clause} LIMIT 1"),
                [number.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    #[cfg(test)]
    mod tests {
        use super::super::super::tests::{mem_db, set_repo};
        use super::*;

        /// A well-formed 3-sentence summary with an in-identifier dot that must NOT split.
        const GOOD: &str = "The helper files.generation returns the live generation. It scopes \
                            every read to the active repo. Callers must never bypass it.";

        #[test]
        fn count_sentences_is_identifier_tolerant() {
            assert_eq!(count_sentences(GOOD), 3, "`files.generation` must not split the count");
            assert_eq!(count_sentences("One. Two. Three. Four."), 4);
            assert_eq!(count_sentences("Only one sentence here."), 1);
            // Backtick / open-paren open a sentence; a lowercase-led fragment does not.
            assert_eq!(count_sentences("First. `code` opens one. (and this) does too."), 3);
            // A decimal and an abbreviation do not split.
            assert_eq!(count_sentences("It grew 3.5x overall. See e.g. the note. Done."), 3);
        }

        #[test]
        fn guards_accept_a_well_formed_summary_and_reject_the_malformed() {
            let c = mem_db();
            set_repo(&c, "r");
            assert!(guards_ok(&c, GOOD), "a 3-sentence, sub-cap, ref-free summary is accepted");
            // Two sentences — below the floor.
            assert!(
                !guards_ok(&c, "One sentence. Two sentences."),
                "under 3 sentences is rejected"
            );
            // Five sentences — above the ceiling.
            assert!(
                !guards_ok(&c, "A. B thing. C thing. D thing. E thing."),
                "over 4 sentences is rejected"
            );
            // Empty.
            assert!(!guards_ok(&c, "   "), "an empty/whitespace summary is rejected");
        }

        #[test]
        fn guards_reject_a_paragraph_break_and_the_word_cap() {
            let c = mem_db();
            set_repo(&c, "r");
            assert!(
                !guards_ok(&c, "First sentence here. Second one.\n\nA second paragraph. Third."),
                "a paragraph break is rejected"
            );
            // 120 words across three sentences (capitalized openers so the identifier-tolerant
            // counter sees the boundaries) — over the 110 cap.
            let sentence = |lead: &str| format!("{lead} {}", vec!["word"; 39].join(" "));
            let long =
                format!("{}. {}. {}.", sentence("Alpha"), sentence("Beta"), sentence("Gamma"));
            assert_eq!(count_sentences(&long), 3, "the fixture is 3 sentences");
            assert_eq!(long.split_whitespace().count(), 120, "the fixture is 120 words");
            assert!(!guards_ok(&c, &long), "over the word cap is rejected");
        }

        fn guards_ok(c: &Connection, s: &str) -> bool {
            accepts(c, s).unwrap()
        }
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

    /// A well-formed 3-sentence summary that passes every guard (no tracker refs).
    const GOOD_SUMMARY: &str = "The queue enqueues an active memory with no summary. It skips a \
                                memory whose body is unchanged. A body edit re-queues it.";

    fn summary_rows(c: &Connection, memory_id: &str) -> Vec<(String, String)> {
        c.prepare(
            "SELECT content_hash, summary FROM memory_summaries WHERE memory_id = ?1 ORDER BY \
             content_hash",
        )
        .unwrap()
        .query_map([memory_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    // ── guards over the papertrail (DB-backed) ───────────────────────────────────

    fn seed_issue(c: &Connection, number: i64, repo_id: &str) {
        c.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, synced_at_ms, repo_id)
             VALUES ('github','o/r','issue',?1,'http://x','open','t','b',0,?2)",
            rusqlite::params![number.to_string(), repo_id],
        )
        .unwrap();
    }

    #[test]
    fn resolvable_tracker_ref_is_kept_and_unresolvable_is_rejected() {
        let c = mem_db();
        set_repo(&c, "r");
        // Papertrail has issue #42 (so it is non-empty → the guard is active).
        seed_issue(&c, 42, "r");
        // A summary citing the resolvable #42 is KEPT.
        assert!(
            guards::accepts(
                &c,
                "It fixes the leak described in #42. The scope is per-repo. Done now."
            )
            .unwrap(),
            "a ref that resolves in the papertrail is kept"
        );
        // A summary citing the UNRESOLVABLE #999 is rejected.
        assert!(
            !guards::accepts(&c, "It fixes the leak from #999. The scope is per-repo. Done now.")
                .unwrap(),
            "a ref absent from the papertrail is rejected"
        );
    }

    #[test]
    fn tracker_ref_guard_skips_when_the_papertrail_is_empty() {
        let c = mem_db();
        set_repo(&c, "r");
        // No papertrail items at all (sync never ran) → the ref guard is SKIPPED, so even an
        // otherwise-unresolvable #999 does not fail acceptance.
        assert!(
            guards::accepts(&c, "It fixes the leak from #999. The scope is per-repo. Done now.")
                .unwrap(),
            "with an empty papertrail the ref guard is skipped, not failed"
        );
    }

    // ── retry flow ────────────────────────────────────────────────────────────────

    #[test]
    fn bad_then_good_writes_a_row_on_two_calls() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        // First completion is malformed (one sentence → guard rejects); the retry is well-formed.
        let model = MockVerdictModel::new(["just one sentence", GOOD_SUMMARY]);
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 5000).unwrap();
        assert_eq!(model.calls(), 2, "one guard failure triggers exactly one retry");
        let rows = summary_rows(&c, "m1");
        assert_eq!(rows.len(), 1, "the accepted retry wrote exactly one summary row");
        assert_eq!(rows[0].1, GOOD_SUMMARY);
    }

    #[test]
    fn bad_then_bad_writes_nothing() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        // Both completions are malformed (one sentence each) → rejected twice → nothing stored.
        let model = MockVerdictModel::new(["one sentence only", "still one sentence"]);
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 5000).unwrap();
        assert_eq!(model.calls(), 2, "retried exactly once");
        assert!(
            summary_rows(&c, "m1").is_empty(),
            "two guard failures store nothing (title-only fallback)"
        );
        let reason: String = c
            .query_row(
                "SELECT reason FROM memory_model_failures WHERE repo_id='r' AND memory_id='m1' \
                 AND pass='compact'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, DreamFailureReason::SummaryGuardRejected.as_db_str());

        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 6000).unwrap();
        assert_eq!(model.calls(), 2, "the current deterministic failure suppresses another retry");

        c.execute("UPDATE repo_memories SET body='edited body' WHERE id='m1'", []).unwrap();
        let good = MockVerdictModel::new([GOOD_SUMMARY]);
        run_compact_pass(&c, CompactPass { model: &good, budget: 10 }, 7000).unwrap();
        assert_eq!(good.calls(), 1, "a content change invalidates the failure row");
        assert_eq!(summary_rows(&c, "m1").len(), 1, "the changed memory can be summarized");
        let failures: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM memory_model_failures WHERE repo_id='r' AND memory_id='m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failures, 0, "a successful summary clears the stale failure row");
    }

    #[test]
    fn zero_budget_does_not_call_model_or_report_pending_compaction() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        let model = MockVerdictModel::new([GOOD_SUMMARY]);

        run_compact_pass(&c, CompactPass { model: &model, budget: 0 }, 1000).unwrap();

        assert_eq!(model.calls(), 0, "budget zero stops before the first model turn");
        assert!(
            !compaction_pending(&c, 0, "mock-verdict-model").unwrap(),
            "budget zero is no pending model work"
        );
        assert!(summary_rows(&c, "m1").is_empty(), "no summary is written at budget zero");
    }

    #[test]
    fn current_compact_failure_suppresses_pending_compaction() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        let content_hash = crate::dream::note_content_hash("note", "a body worth compacting");
        let stamp = FailureStamp {
            memory_id: "m1",
            repo_id: "r",
            pass: DreamModelPass::Compact,
            content_hash: &content_hash,
            checked_inputs_hash: None,
            prompt_version: COMPACT_PROMPT_VERSION,
            model_id: "mock-verdict-model",
        };
        let failure = DreamModelFailure::new(DreamFailureReason::SummaryGuardRejected);
        failure::record_failure(&c, RecordFailure { stamp, failure: &failure, now_ms: 1 }).unwrap();

        assert!(
            !compaction_pending(&c, 10, "mock-verdict-model").unwrap(),
            "a current deterministic compact failure means the memory is already annotated"
        );
    }

    #[test]
    fn model_error_records_retryable_compact_failure() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        let model = MockVerdictModel::new(Vec::<String>::new());

        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 1000).unwrap();

        let (reason, detail): (String, String) = c
            .query_row(
                "SELECT reason, detail FROM memory_model_failures WHERE repo_id='r' AND \
                 memory_id='m1' AND pass='compact'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, DreamFailureReason::ModelCallFailed.as_db_str());
        assert!(detail.contains("no responses"), "the transient model error is stored for audit");
        assert!(
            compaction_pending(&c, 10, "mock-verdict-model").unwrap(),
            "model-call failures remain retryable work"
        );
    }

    // ── write-path stamps + churn / invalidation ─────────────────────────────────

    #[test]
    fn accepted_summary_upserts_all_stamps() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        let model = MockVerdictModel::new([GOOD_SUMMARY]);
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 7000).unwrap();
        let row: (String, String, String, String, i64) = c
            .query_row(
                "SELECT summary, model_id, prompt_version, content_hash, generated_at_ms FROM \
                 memory_summaries WHERE memory_id='m1' AND repo_id='r'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row.0, GOOD_SUMMARY);
        assert_eq!(row.1, "mock-verdict-model");
        assert_eq!(row.2, COMPACT_PROMPT_VERSION);
        assert_eq!(row.3, crate::dream::note_content_hash("note", "a body worth compacting"));
        assert_eq!(row.4, 7000);
    }

    #[test]
    fn title_only_edit_recompacts() {
        // Regression (PR #428 Codex P2): the compaction prompt frames TITLE + body, so the
        // content_hash covers the title — a title-only edit re-queues for a fresh summary.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "original title", "a stable body", "r");
        let model = MockVerdictModel::new([GOOD_SUMMARY, GOOD_SUMMARY]);
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 1000).unwrap();
        assert_eq!(model.calls(), 1, "the un-summarized memory is compacted once");
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 2000).unwrap();
        assert_eq!(model.calls(), 1, "unchanged → churn-skip");

        c.execute("UPDATE repo_memories SET title='a corrected title' WHERE id='m1'", []).unwrap();
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 3000).unwrap();
        assert_eq!(model.calls(), 2, "a title-only edit re-compacts");
        assert_eq!(summary_rows(&c, "m1").len(), 1, "old-note summary pruned, one row remains");
    }

    #[test]
    fn second_run_churn_skips_and_body_edit_recompacts_and_prunes() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "original body", "r");
        let model = MockVerdictModel::new([GOOD_SUMMARY, GOOD_SUMMARY]);

        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 1000).unwrap();
        assert_eq!(model.calls(), 1, "the un-summarized memory is compacted once");

        // Unchanged memory → the queue churn-skips → the model is NOT re-invoked.
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 2000).unwrap();
        assert_eq!(model.calls(), 1, "an unchanged, already-summarized memory is churn-skipped");

        // A body edit changes content_hash → re-enqueued → the model runs again and the OLD summary
        // row (under the previous content_hash) is pruned, leaving exactly one row.
        c.execute("UPDATE repo_memories SET body='edited body' WHERE id='m1'", []).unwrap();
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 3000).unwrap();
        assert_eq!(model.calls(), 2, "a body edit re-invokes the model");
        let rows = summary_rows(&c, "m1");
        assert_eq!(rows.len(), 1, "steady state is one summary row per memory (old body pruned)");
        assert_eq!(
            rows[0].0,
            crate::dream::note_content_hash("note", "edited body"),
            "the row is the new note's"
        );
    }

    #[test]
    fn compact_pass_never_mutates_a_repo_memories_column() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "note", "a body worth compacting", "r");
        let snap = |c: &Connection| -> (String, String, String) {
            c.query_row("SELECT body, status, title FROM repo_memories WHERE id='m1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
        };
        let before = snap(&c);
        let model = MockVerdictModel::new([GOOD_SUMMARY]);
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 1000).unwrap();
        assert_eq!(before, snap(&c), "the compaction pass leaves repo_memories byte-identical");
        // ...but it DID write a summary into the sibling table.
        let stored: String = c
            .query_row("SELECT summary FROM memory_summaries WHERE memory_id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, GOOD_SUMMARY);
    }

    #[test]
    fn queue_caps_at_budget_in_memory_id_order() {
        let c = mem_db();
        set_repo(&c, "r");
        for id in ["m4", "m1", "m3", "m2"] {
            seed_memory(&c, id, "note", "body", "r");
        }
        let queue = compaction_queue(&c, 2).unwrap();
        assert_eq!(
            queue.iter().map(|e| e.memory_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2"],
            "the queue is ordered by memory_id and capped at the budget"
        );
    }

    // ── poison-sibling: repo scoping ─────────────────────────────────────────────

    #[test]
    fn compaction_queue_and_writes_are_repo_scoped() {
        // `repo_memories.id` is a global PK, so the two repos hold DISTINCT ids (m1 in r1, m2 in
        // r2); isolation is proved by the `repo_id` scope predicates. A run under r1 must compact
        // ONLY r1's memory and write ONLY under r1.
        let c = mem_db();
        set_repo(&c, "r1");
        seed_memory(&c, "m1", "note", "repo one body", "r1");
        seed_memory(&c, "m2", "note", "repo two body", "r2");

        // The queue under r1 holds only r1's memory.
        let queue = compaction_queue(&c, 10).unwrap();
        assert_eq!(
            queue.iter().map(|e| e.memory_id.as_str()).collect::<Vec<_>>(),
            vec!["m1"],
            "the compaction queue is scoped to the active repo"
        );

        let model = MockVerdictModel::new([GOOD_SUMMARY]);
        run_compact_pass(&c, CompactPass { model: &model, budget: 10 }, 1000).unwrap();

        // The summary is written under r1 for m1, and NOTHING is written under r2 / for m2.
        let r1_row: (String, String) = c
            .query_row(
                "SELECT repo_id, memory_id FROM memory_summaries WHERE memory_id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(r1_row, ("r1".to_string(), "m1".to_string()));
        let r2_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM memory_summaries WHERE repo_id='r2' OR memory_id='m2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(r2_count, 0, "a run under r1 never wrote a summary under repo r2 / for m2");
    }
}
