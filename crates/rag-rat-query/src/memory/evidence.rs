//! Dream v2 pass 0 — the DETERMINISTIC verification substrate (no LLM).
//!
//! Three surfaces, all repo-scoped and reading the active index as dream's source of truth (not
//! the filesystem):
//!   - [`verification_queue`] — active memories that need (re)verification, ranked and capped by a
//!     budget. Churn-skip is the point: a memory is enqueued only when a binding anchor is
//!     stale/gone (reusing the doctor predicate), it has no `memory_reality` row yet, or its
//!     current body / bound-file inputs no longer match the last-checked hashes. This is the
//!     substrate the phase-B model verdict pass consumes — it never writes here.
//!   - [`evidence_pack`] — a deterministic, citation-checkable pack for one memory: an identifier
//!     table (backticked spans + long snake_case tokens resolved against indexed symbols/files;
//!     "NOT FOUND anywhere" is emitted only when exact live files or a live call path prove the
//!     note's declared domain) plus current text excerpts of the memory's bound file(s), windowed
//!     around identifier hits.
//!   - [`unverifiable_findings`] — the deterministic `memory_unverifiable` decision: a memory whose
//!     bindings are all gone/absent AND none of whose identifiers resolve. Decided HERE, never by a
//!     model; folded into the identity-keyed `dream_findings` lifecycle by `dream_run`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

use rag_rat_db::schema;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use super::{BINDING_CURRENT_BINDING_ID, BINDING_CURRENT_PATH, EdgeLooseIdentity, resolve};

/// The authoritative "resolves nowhere" verdict, emitted only when the note's binding proves the
/// searched domain is live and covered. Public because the dream divergence guard recognizes an
/// absence row by this exact label.
pub const NOT_FOUND: &str = "NOT FOUND anywhere in the source tree";
/// The resolution for a code-shaped-but-unresolved span that is uninformative — a paraphrase,
/// snippet, or flag whose non-match is a shape artifact, NEVER evidence of divergence.
const UNRESOLVABLE: &str = "not a resolvable identifier (no symbol, file, or verbatim-text match)";
/// An absence cannot be authoritative when the note's own source binding falls outside the active
/// index coverage (for example a workflow, TOML, or cookbook file excluded by `target_bindings`).
const OUTSIDE_INDEX_COVERAGE: &str =
    "absence indeterminate because the note binding is outside indexed source coverage";
/// The resolution for a `mem_<hex>` id that is a cross-reference to ANOTHER repo memory, not a code
/// entity — uninformative (never NOT_FOUND, never source presence). Shared by the tier-2.5 arms.
const MEM_XREF: &str = "a cross-reference to another repo memory (not a code entity)";
/// The verbatim-text label for a present-but-not-a-defined-symbol span (a table/column name, a
/// local, an expression). Shared by the general text tier and the ambiguous mem-id prefix arm so
/// the label (and thus the churn-key string) can't drift between the two paths.
pub const TEXT_PRESENT_SYMBOL: &str = "not a defined symbol; appears verbatim as source text";
/// The verbatim-text label for a path-shaped span that is not an indexed file but appears verbatim
/// in source — the file twin of [`TEXT_PRESENT_SYMBOL`].
pub const TEXT_PRESENT_FILE: &str = "not an indexed file; appears verbatim only as source text";
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
/// A bare or `::`-qualified name (`foo`, `foo::bar::Baz`) — the shape whose GENUINE absence is a
/// divergence signal (a named code entity the note describes is gone), so a whole-tree miss on it
/// earns the authoritative [`NOT_FOUND`] rather than the uninformative "not a resolvable
/// identifier".
static SYMBOL_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*$").expect("static regex")
});
/// The character set of a file path (letters, digits, `_./@+-` — `@` for scoped package dirs like
/// `packages/@scope/app/src/index.ts`). Combined with a "has a `/` or a trailing `.ext`" check to
/// decide path-shapedness (so a bare `commit_fts` is judged by [`SYMBOL_PATH_RE`], not a path).
static PATH_SHAPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./@+-]+$").expect("static regex"));
/// A trailing file extension (`.rs`, `.md`, …) — the other half of path-shapedness for a bare
/// filename with no directory separator.
static FILE_EXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.[A-Za-z0-9]{1,8}$").expect("static regex"));

/// Runaway guard on candidate chunks a verbatim-text probe scans per identifier. The PHRASE
/// narrowing (adjacent tokens) keeps a real identifier's candidate set in the low hundreds, well
/// under this; the guard only bounds a degenerate phrase (e.g. a backticked English sentence). It
/// is NOT a correctness knob — a scan that reaches it yields `Capped` (indeterminate), never a
/// false `Absent`; the phrase set of a real identifier is exhausted long before the guard.
const TEXT_PRESENCE_SCAN_CAP: usize = 2000;

/// Version stamp of the verify/verdict prompt pack. Stamped into `memory_reality.prompt_version`
/// by the verdict pass and compared by the queue + surfacing gates: a bump re-queues every memory
/// (which is why prompt-observable changes ride version bumps backfill-free).
pub const VERDICT_PROMPT_VERSION: &str = "verify-pack-v6";
/// Version stamp of the compaction prompt, gating `memory_note_summaries` reuse the same way.
pub const COMPACT_PROMPT_VERSION: &str = "compact-v2";

/// Word ceiling on a compacted summary — the compaction acceptance guards accept nothing longer
/// (the prompt asks for well under it, leaving headroom for a rationale sentence). A note whose
/// body already fits IS a summary by that standard, so compaction never queues it: the rewrite
/// could come back no shorter while dropping a condition or the reason behind it.
pub const SUMMARY_MAX_WORDS: usize = 150;

/// Character ceiling on the same envelope — roughly the width [`SUMMARY_MAX_WORDS`] of ordinary
/// prose occupies. The word count alone does not bound the cost the surfaces are paying: 150 tokens
/// of absolute paths, URLs, a quoted stack line, or base64 run to kilobytes while a body cap of
/// 8000 chars lets them through. Without this bound such a note is skipped by compaction forever
/// and then emitted WHOLE on every attachment, which is what the summary surface exists to prevent.
pub const SUMMARY_MAX_CHARS: usize = 1200;

/// Whether compaction skips this body for already fitting the summary envelope — the ONE predicate
/// the compaction queue and both summary surfaces share. A skipped note never gets a
/// `memory_note_summaries` row and never will, so the summary surfaces must show it WHOLE (bounded
/// by [`SUMMARY_MAX_WORDS`] and [`SUMMARY_MAX_CHARS`]); letting the two sides disagree strands a
/// note in the gap, where it surfaces as a bare title forever.
pub fn note_is_shown_whole(body: &str) -> bool {
    body.split_whitespace().count() <= SUMMARY_MAX_WORDS
        && body.chars().count() <= SUMMARY_MAX_CHARS
}

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
    /// Whether this memory still has a live, non-`scip_moniker` binding in the active scope.
    #[serde(skip_serializing)]
    pub has_live_binding: bool,
}

impl EvidencePack {
    /// Whether the pack has any SUBSTANTIVE content a verdict could cite — at least one bound-file
    /// excerpt, at least one identifier that resolves to presence, or a live binding with an
    /// authoritative [`ResolutionKind::Absent`] identifier. Two memories look uncitable and must
    /// stay out of the model pass:
    ///   - a prose-only / conceptual note with no identifiers and no excerpts (only boilerplate);
    ///   - a note whose EVERY identifier resolves to `NOT_FOUND` and has no live binding — pass 0
    ///     already decides this `memory_unverifiable`, and counting those NOT_FOUND rows as citable
    ///     would let the model be asked anyway and accept a `diverged` verdict citing "x -> NOT
    ///     FOUND", opening `memory_divergence` and burning budget against the module contract that
    ///     unverifiable is never asked of the model.
    ///
    /// The verdict pass records a terminal (verdict-less) row for an uncitable pack instead of
    /// calling the model, so it churn-skips rather than re-queuing every run.
    ///
    /// "Citable" = carries at least one piece of PRESENCE evidence: a bound-file excerpt, or an
    /// identifier that resolved to a symbol / file / verbatim source text
    /// ([`ResolutionKind::is_present`]). A live-bound pack with an authoritative
    /// [`ResolutionKind::Absent`] identifier is also citable: it is the `note_ahead` candidate
    /// whose bound file exists but whose named code entity is gone. A pack whose every identifier
    /// is [`ResolutionKind::Absent`] with no live binding, or whose rows are all
    /// [`ResolutionKind::Unresolvable`] (a paraphrase / non-code span), carries no evidence — pass
    /// 0 already decides the former `memory_unverifiable`, so neither case must be asked of the
    /// model.
    pub fn is_citable(&self) -> bool {
        !self.excerpts.is_empty()
            || self.identifiers.iter().any(|id| id.kind.is_present())
            || (self.has_live_binding
                && self.identifiers.iter().any(|id| id.kind == ResolutionKind::Absent))
    }
}

/// How an extracted span resolves against the whole-tree index — the classification that decides
/// whether the span is PRESENCE evidence, a genuine absence (divergence-grade), or an uninformative
/// non-code span. Kept distinct from the rendered [`IdentifierResolution::resolution`] string so
/// the citability / "resolves" gates never string-match the human-facing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionKind {
    /// Resolved to ≥1 indexed symbol by name.
    Symbol,
    /// Resolved to ≥1 indexed file by path (exact or suffix).
    File,
    /// Not a symbol or file, but appears VERBATIM in indexed source text (a DB table/column name, a
    /// local variable, a common expression, an attribute). Presence evidence, NOT a divergence.
    TextPresent,
    /// An identifier- or path-shaped span that resolves to NOTHING — no symbol, no file, not even
    /// verbatim text. THE genuine divergence signal: a named code entity the note describes is
    /// gone.
    Absent,
    /// A span that is not shaped like a code symbol or path (whitespace, parens, brackets, quotes,
    /// operators — a paraphrased expression / SQL snippet / CLI flag) AND does not appear verbatim.
    /// Uninformative: its non-match is an artifact of the span shape, never evidence of divergence.
    Unresolvable,
}

impl ResolutionKind {
    /// Whether this resolution is POSITIVE presence evidence (symbol / file / verbatim text) — the
    /// citability + "any identifier resolves" gate. `Absent` and `Unresolvable` are not present.
    fn is_present(self) -> bool {
        matches!(self, Self::Symbol | Self::File | Self::TextPresent)
    }
}

/// How a `mem_`-prefixed identifier resolves against the minted memory-id shape.
///
/// `memory_create` mints `mem_<hex-timestamp>_<hex-suffix>` (`query::memory::validate::memory_id`)
/// or `mem_<hex>_<hex>` for a consolidated import (`index::consolidate`) — always TWO hex segments
/// — and agents commonly cite the timestamp segment ALONE. The two forms are disambiguated
/// differently downstream (see the tier-2.5 branch in [`resolve_identifier`]), so classification is
/// three-way:
/// - [`Full`](MemIdShape::Full): both segments present (`mem_<hex≥10>_<hex…>`). Decisive by shape —
///   a coincidental identifier of this exact form is vanishingly unlikely — so it is a
///   cross-reference without any record lookup, which keeps #678's DANGLING-reference property (a
///   cite of a deleted memory is uninformative, never a code absence) independent of the memory
///   table.
/// - [`Prefix`](MemIdShape::Prefix): one long hex segment, no suffix (`mem_<hex≥10>`).
///   Shape-AMBIGUOUS with a contiguous-hex code local, so shape alone cannot classify it — the
///   caller disambiguates by record-confirmation, then source presence.
/// - [`NotAnId`](MemIdShape::NotAnId): everything else. The first underscore-delimited segment must
///   be a long (≥10) contiguous hex run — checking that segment, NOT the aggregate hex count across
///   underscores, is what stops a segmented hex-word like `mem_dead_beef_ca` from being misread —
///   and the whole span must be hex/underscore, so a real symbol like `mem_19f2ad6cf90_lookup` is
///   rejected on its non-hex tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemIdShape {
    NotAnId,
    Full,
    Prefix,
}

fn memory_id_shape(ident: &str) -> MemIdShape {
    let Some(rest) = ident.strip_prefix("mem_") else {
        return MemIdShape::NotAnId;
    };
    let first_segment = rest.split('_').next().unwrap_or(rest);
    let shaped = first_segment.len() >= 10
        && first_segment.chars().all(|c| c.is_ascii_hexdigit())
        && rest.chars().all(|c| c.is_ascii_hexdigit() || c == '_');
    match (shaped, rest.contains('_')) {
        (false, _) => MemIdShape::NotAnId,
        (true, true) => MemIdShape::Full,
        (true, false) => MemIdShape::Prefix,
    }
}

/// The union predicate — is `ident` memory-id-shaped at all (Full OR Prefix)? Used only by the
/// excerpt filter in [`evidence_pack`], which must exclude BOTH forms from bound-file excerpt
/// windows once they resolve `Unresolvable`: `identifier_windows` matches by plain substring, so a
/// cross-ref left in the excerpt-ident list would window a source line and wrongly make a
/// cross-ref-only note citable. (#678)
fn is_memory_id_shaped(ident: &str) -> bool {
    !matches!(memory_id_shape(ident), MemIdShape::NotAnId)
}

/// Does a repo memory in the active scope have this bare timestamp `prefix` as its whole id, or as
/// the `<prefix>_<suffix>` timestamp segment of its id? Confirms a shape-ambiguous
/// [`MemIdShape::Prefix`] is a real cross-reference (vs a coincidental contiguous-hex code local).
/// Scoped exactly like the other `repo_memories` reads here. ALL statuses count — a cite of an
/// obsolete/superseded memory is still a cross-ref. Confirmation can only UPGRADE a prefix to a
/// cross-ref, so it can never turn one into a NOT_FOUND absence: #678's dangling-reference property
/// does not depend on the memory table.
fn memory_with_id_prefix_exists(conn: &Connection, prefix: &str) -> rusqlite::Result<bool> {
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let clause = schema::periphery_repo_scope_clause(&scope, "repo_memories");
    // `substr(id, 1, len)` byte-prefix equality, NOT LIKE — `_` is a LIKE single-char wildcard.
    // Input is ASCII hex, so SQLite's char-based `substr` is byte-equivalent.
    let pat = format!("{prefix}_");
    conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE (id = ?1 OR substr(id, 1, ?2) = \
             ?3){clause})"
        ),
        rusqlite::params![prefix, pat.len() as i64, pat],
        |r| r.get(0),
    )
}

/// One extracted identifier and where (if anywhere) it resolves in the whole-tree index.
#[derive(Debug, Clone, Serialize)]
pub struct IdentifierResolution {
    pub identifier: String,
    /// Human/model-facing text: `symbol <path>::<name>`, `file <path>`, a verbatim-text note, the
    /// authoritative [`NOT_FOUND`], or a "not a resolvable identifier" note. See
    /// [`ResolutionKind`] for the machine classification (which is what the gates read, never
    /// this string).
    pub resolution: String,
    pub kind: ResolutionKind,
}

/// A current-text excerpt window from a bound file, addressed by absolute line range.
#[derive(Debug, Clone, Serialize)]
pub struct FileExcerpt {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub text: String,
}

/// Active memories that need (re)verification, ranked (broken anchors first). Uncapped: the verdict
/// runner enforces its budget itself, after skipping entries a current model failure blocks. A
/// memory is enqueued when it has no `memory_reality` row, its note content changed
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
) -> rusqlite::Result<Vec<VerificationQueueEntry>> {
    let _ = now_ms;
    let scope = schema::periphery_repo_scope(conn, "repo_memories")?;
    let mem_clause = schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let reality_clause = schema::periphery_repo_scope_clause(&scope, "memory_reality");
    // Reuse the doctor's anchor predicate rather than re-inlining it here.
    let broken: HashSet<String> =
        crate::memory::memory_ids_with_broken_anchors(conn)?.into_iter().collect();

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
    // Deterministic order: rank desc, then memory_id asc.
    queue.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
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
    let prompt_changed = stored_prompt_version.as_deref() != Some(VERDICT_PROMPT_VERSION);
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
/// `memory_reality` / `memory_note_summaries`. It is `sha256(trim(title) + "\n" + trim(body))`,
/// covering EXACTLY what the verdict and compaction prompts render: the title and body, and nothing
/// else. So a title / body edit re-verifies / re-summarizes and drops the stale verdict / summary /
/// marker, while a change to a dimension the prompts don't render (kind, tags, payload) does NOT
/// churn the derived overlays. It becomes the §5.5 canonical [`content_hash`] (which folds the
/// payload) only when the prompts start rendering the payload — bundled with a [`PROMPT_VERSION`]
/// bump so that rollout is backfill-free (a bump re-queues every memory anyway). Distinct from the
/// raw create-time `memory_input_hash`, which also folds kind + tags (dimensions the prompts don't
/// audit) and reads the frozen-at-creation stored `input_hash`.
///
/// [`PROMPT_VERSION`]: dream verdict PROMPT_VERSION (engine)
///
/// `pub(crate)` so the surfacing hydrator recomputes it IDENTICALLY to the queue / verdict-pass
/// stamp.
pub fn note_content_hash(title: &str, body: &str) -> String {
    rag_rat_base::hash::hex_sha256(format!("{}\n{}", title.trim(), body.trim()).as_bytes())
}

/// sha256 fingerprint of a memory's ENTIRE deterministic evidence pack — the churn comparator that
/// beats a commit-ancestry walk. Named `checked_inputs_hash` after the column it is stamped into;
/// it covers all evidence dimensions that affect the verdict pass:
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
///     them fingerprints the whole pack without rebuilding excerpts;
///   - a live-binding marker ONLY for the newly citable shape: at least one authoritative absent
///     identifier, no presence evidence, no bound-file excerpt, and a live binding. The excerpt set
///     is checked directly because an indeterminate identifier probe can still have a bound source
///     excerpt. Every other shape uses the exact legacy files/identifiers preimage for upgrade
///     compatibility.
///
/// `pub(crate)` so the phase-B verdict pass (`verdict`) recomputes it EXACTLY as the queue's
/// comparator does when it stamps `memory_reality.checked_inputs_hash` — same function, so the next
/// run churn-skips instead of re-checking — and so the surfacing hydrator / divergence finder gate
/// a stale verdict on it the same way the queue does. Cost note: resolving identifiers loads
/// `indexed_file_paths` per call; acceptable for the deterministic pass (`unverifiable_findings`
/// already resolves every memory's identifiers each run) and the opt-in `surface = "summary"` read.
pub fn checked_inputs_hash(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<String> {
    let file_pairs: BTreeSet<String> = resolve_bound_files(conn, memory_id, scope)?
        .into_iter()
        .map(|(path, _, sha)| format!("{path}\u{1f}{sha}"))
        .collect();
    let ident_details = identifier_resolution_details(conn, memory_id, scope)?;
    let ident_pairs: BTreeSet<String> =
        ident_details.iter().map(|(ident, res, _)| format!("{ident}\u{1f}{res}")).collect();
    let files = file_pairs.into_iter().collect::<Vec<_>>().join("\u{1e}");
    let idents = ident_pairs.into_iter().collect::<Vec<_>>().join("\u{1e}");
    let excerpt_idents: Vec<String> = ident_details
        .iter()
        .filter(|(ident, _, kind)| {
            !(*kind == ResolutionKind::Unresolvable && is_memory_id_shaped(ident))
        })
        .map(|(ident, _, _)| ident.clone())
        .collect();
    // A live binding changes citability only for a pack with at least one authoritative absence,
    // no presence evidence, and no bound-file excerpt. Keep the legacy preimage for every other
    // shape so upgrading does not churn unrelated terminal rows.
    let binding_state_changes_citability = !ident_details.is_empty()
        && ident_details.iter().any(|(_, _, kind)| *kind == ResolutionKind::Absent)
        && ident_details.iter().all(|(_, _, kind)| !kind.is_present())
        && memory_has_live_binding(conn, memory_id, scope)?
        && bound_file_excerpts(conn, memory_id, scope, &excerpt_idents)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?
            .is_empty();
    let preimage = if binding_state_changes_citability {
        // `\u{1d}` (group separator) splits the legacy files/identifiers sections from the
        // selective binding marker; a marker is present only for the live-bound absent-only pack.
        format!("{files}\u{1d}{idents}\u{1d}live")
    } else {
        // This is the byte-for-byte legacy encoding. Do not append a not-live marker: it would
        // invalidate every pre-upgrade row whose citability cannot depend on binding state.
        format!("{files}\u{1d}{idents}")
    };
    Ok(rag_rat_base::hash::hex_sha256(preimage.as_bytes()))
}

/// The `(identifier, rendered resolution, kind)` rows of the evidence-pack fingerprint — the
/// memory's identifiers (from title+body) each resolved against the whole-tree index EXACTLY as
/// [`evidence_pack`] does, so the churn key matches what the model is actually shown. Empty when
/// the memory is not visible in scope or carries no identifiers.
fn identifier_resolution_details(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<Vec<(String, String, ResolutionKind)>> {
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
    let absence_is_authoritative = memory_binding_is_index_covered(conn, memory_id, scope)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let mut out = Vec::with_capacity(identifiers.len());
    for ident in &identifiers {
        // Fold the rendered resolution STRING. It is now path-independent for every tier —
        // `TextPresent` names no files, symbol/file carry stable identity, NOT_FOUND / unresolvable
        // are fixed — so the churn key re-verifies on a genuine tier flip or a symbol/file identity
        // change but stays stable against unrelated repo churn (adding a file carrying a cited
        // common token no longer re-queues the paid verdict).
        let (resolution, kind) =
            resolve_memory_identifier(conn, ident, &file_paths, absence_is_authoritative)?;
        out.push((ident.clone(), resolution, kind));
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
            has_live_binding: false,
        });
    };
    let has_live_binding = memory_has_live_binding(conn, memory_id, &scope)?;
    let identifiers = extract_identifiers(&title, &body);
    let file_paths = indexed_file_paths(conn)?;
    let absence_is_authoritative = memory_binding_is_index_covered(conn, memory_id, &scope)?;
    let mut resolutions = Vec::with_capacity(identifiers.len());
    for ident in &identifiers {
        let (resolution, kind) =
            resolve_memory_identifier(conn, ident, &file_paths, absence_is_authoritative)?;
        resolutions.push(IdentifierResolution { identifier: ident.clone(), resolution, kind });
    }
    // A `mem_<hex>` cross-reference is never source evidence, so it must not window an excerpt
    // either — else a note that merely mentions a memory id in its bound file stays citable via the
    // excerpt, even though the id itself resolved Unresolvable above. Real hex-named symbols (which
    // resolved as `Symbol`, not `Unresolvable`) are kept. (#678)
    let excerpt_idents: Vec<String> = resolutions
        .iter()
        .filter(|r| !(r.kind == ResolutionKind::Unresolvable && is_memory_id_shaped(&r.identifier)))
        .map(|r| r.identifier.clone())
        .collect();
    let excerpts = bound_file_excerpts(conn, memory_id, &scope, &excerpt_idents)?;
    Ok(EvidencePack {
        memory_id: memory_id.to_string(),
        identifiers: resolutions,
        excerpts,
        has_live_binding,
    })
}

/// The deterministic `memory_unverifiable` findings: active memories that WERE anchored but whose
/// bindings are now all gone (no live non-`scip_moniker` binding, yet ≥1 binding row) — OR a
/// zero-binding ORPHAN of a should-be-anchored kind — AND none of whose identifiers resolve
/// anywhere in the whole-tree index. An intentional UNANCHORED node (a `Task`/`Concept` with zero
/// binding rows — #463/#465) is EXCLUDED: it is anchorless BY DESIGN, not broken, so flagging it
/// would be spurious noise. Repo-scoped; the evidence names exactly what was checked. Folded into
/// the identity-keyed `dream_findings` lifecycle by `dream_run` (so a memory that becomes
/// verifiable again is resolved), which is why this runs over the full active population, not the
/// budget.
/// Whether `memory_id` has any live binding: a non-`scip_moniker` binding whose anchor is not
/// `gone` (`scip_moniker` self-heals on the next oracle run and is never rebind-actionable,
/// matching `doctor_report`). "Every binding gone/absent" is the negation.
pub fn memory_has_live_binding(
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

/// Whether `memory_id` has ANY binding row at all (any kind, any `anchor_status`). Distinguishes an
/// intentional UNANCHORED node (#463/#465 — zero rows, anchorless by design) from a broken memory
/// whose anchors all went `gone` (≥1 row): only the intentional case is excused from
/// `memory_unverifiable`, and only for the `Task`/`Concept` kinds.
pub fn memory_has_any_binding(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<bool> {
    let bind_clause = schema::periphery_repo_scope_clause(scope, "repo_memory_bindings");
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM repo_memory_bindings WHERE memory_id = ?1{bind_clause}"),
        [memory_id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Backticked spans + long snake_case tokens from title+body, trimmed, de-duplicated and SORTED (a
/// `BTreeSet`), so the identifier table is byte-stable across runs.
pub fn extract_identifiers(title: &str, body: &str) -> Vec<String> {
    let text = format!("{title}\n{body}");
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for cap in BACKTICK_RE.captures_iter(&text) {
        if let Some(span) = cap.get(1) {
            let span = span.as_str().trim();
            // A multi-line span would render its embedded newline INTO the identifier table,
            // splitting a row into free-standing attacker-controlled pack lines — reject it.
            if !span.is_empty() && !span.contains('\n') {
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

/// Resolve one extracted span against the whole-tree index through a four-tier ladder, returning
/// both the human/model-facing string and its [`ResolutionKind`]:
///   1. SYMBOL — the trailing `::` segment (call-args stripped) as a symbol name.
///   2. FILE — an exact or suffix path match.
///   3. VERBATIM TEXT — not a symbol/file, but present as literal source text (a DB table/column
///      name, a local variable, a common expression). PRESENCE evidence, not a divergence — but the
///      label states it is NOT a defined symbol, so a note claiming it is a live function can still
///      be judged diverged.
///   4. TERMINAL — nothing resolved. Split by span SHAPE: a symbol- or path-shaped span that is
///      genuinely absent is the real divergence signal ([`NOT_FOUND`] /
///      [`ResolutionKind::Absent`]); a non-code span (parens, brackets, quotes, operators,
///      whitespace — a paraphrase / snippet / flag) is uninformative
///      ([`ResolutionKind::Unresolvable`]), never evidence of divergence.
///
/// The old resolver conflated tiers 3 and 4 into a blanket [`NOT_FOUND`], so a memory citing a
/// table name, an attribute, or an expression was reported as "absent" and the verdict model
/// over-reported `diverged` — the root of the divergence false-positive class.
fn resolve_identifier(
    conn: &Connection,
    ident: &str,
    file_paths: &[String],
) -> rusqlite::Result<(String, ResolutionKind)> {
    // `norm` drops a Rust turbofish (`build_index::<Cfg>(cfg)` -> `build_index(cfg)`) for the
    // SYMBOL and SHAPE tiers, so a generic-argument `::` is never mistaken for a qualified path
    // (which would probe the generic args and hide a real absence). The FILE and TEXT tiers
    // keep the ORIGINAL span: the source carries the generic args verbatim, so a normalized
    // `HashMap::new` is NOT contiguous in the source's `HashMap::<T>::new()` and text-probing
    // it would miss a present call.
    let normalized = strip_turbofish(ident);
    let norm = normalized.as_ref();
    // 1. SYMBOL — probe the trailing name with any call-argument list (and macro bang) stripped, so
    //    a qualified call (`Mod::from_config(&x)`) still resolves its method instead of failing the
    //    bare-name gate on the parens. Only the trailing name (never the receiver type) is tried,
    //    so a deleted method on a surviving type still falls through to a genuine-absence signal
    //    below.
    if let Some(symbol_name) = symbol_lookup_name(norm) {
        // A macro invocation (`foo!`) resolves ONLY to a macro-kind symbol, so a removed macro is
        // not masked by a same-named non-macro (a surviving `fn foo`).
        let kind = is_macro_invocation(norm).then_some("macro");
        let locs = resolve_symbol(conn, &symbol_name, kind)?;
        match locs.as_slice() {
            [] => {},
            [one] => return Ok((format!("symbol {one}"), ResolutionKind::Symbol)),
            // AMBIGUOUS: more than one live definition shares this bare name. Present ALL of them
            // (sorted) rather than the first — the model must not audit against, and the churn key
            // must not be pinned to, an UNRELATED same-named symbol (so deleting/renaming the one
            // the note actually described re-verifies even when a namesake survives).
            many => {
                return Ok((
                    format!("symbols ({}): {}", many.len(), many.join(", ")),
                    ResolutionKind::Symbol,
                ));
            },
        }
    }
    // 2. FILE — a shorthand path (`lib.rs`, `src/lib.rs`) can suffix-match MORE than one indexed
    //    file; present ALL of them for the SAME reason as ambiguous symbols above. An exact path
    //    match is definitive (returned alone).
    let files = resolve_file_segment(ident, file_paths);
    match files.as_slice() {
        [] => {},
        [one] => return Ok((format!("file {one}"), ResolutionKind::File)),
        many => {
            return Ok((
                format!("files ({}): {}", many.len(), many.join(", ")),
                ResolutionKind::File,
            ));
        },
    }
    // 2.5. MEMORY CROSS-REFERENCE — a `mem_<hex>` id is a cross-reference to ANOTHER repo memory,
    // not      a code entity. Classified HERE — AFTER symbol/file (so a real symbol/file whose
    // name      matches the shape, `fn mem_deadbeefdead`, wins) — and split by shape so an
    // ambiguous prefix      can't strip source evidence from a coincidental code local:
    //      - FULL form (`mem_<hex>_<hex>`): unambiguous, so a cross-ref regardless of source
    //        presence or record existence — never NOT_FOUND (a dangling cite is not a code
    //        absence), never source presence (a note citing a full id in a comment stays
    //        `unverifiable`).
    //      - PREFIX form (`mem_<hex>`): shape-ambiguous with a contiguous-hex local. Confirm
    //        against the memory table first — a recorded memory's prefix is a cross-ref even when
    //        the bare id also appears in indexed text (a doc citing it is not code evidence). An
    //        UNRECORDED prefix defers to source: a verbatim token-boundary hit is a coincidental
    //        code identifier whose `TextPresent` evidence must survive; a miss is a dangling
    //        cross-ref, NEVER NOT_FOUND (the arm owns this terminal so a bare code-shaped prefix
    //        can't fall through to `Absent`). (#678)
    match memory_id_shape(ident) {
        MemIdShape::Full => {
            return Ok((MEM_XREF.to_string(), ResolutionKind::Unresolvable));
        },
        MemIdShape::Prefix => {
            if memory_with_id_prefix_exists(conn, ident)? {
                return Ok((MEM_XREF.to_string(), ResolutionKind::Unresolvable));
            }
            // Present at a token boundary → coincidental code identifier (keep its evidence); a
            // miss or an indeterminate (`Capped`) scan → dangling cross-ref, never
            // convict as `Absent`.
            return Ok(match text_probe(conn, ident)? {
                TextProbe::Present =>
                    (TEXT_PRESENT_SYMBOL.to_string(), ResolutionKind::TextPresent),
                _ => (MEM_XREF.to_string(), ResolutionKind::Unresolvable),
            });
        },
        MemIdShape::NotAnId => {},
    }
    // 3. VERBATIM TEXT — present in source but not a symbol/file (a DB table/column name, a local,
    //    a common expression). Present, so NOT a divergence — but "not a defined symbol" keeps the
    //    door open for a note claiming it is a live function. The resolution names NO files, so the
    //    rendered pack and the churn key stay stable as unrelated files gain/lose the token. Probe
    //    the NORMALIZED callee first (so a present turbofish call whose args/generics differ,
    //    `from_str::<Cfg>(payload)` vs a source `from_str(&body)`, is found by its contiguous
    //    name); for a turbofish whose QUALIFIED callee is split by generics in source, fall back to
    //    the ORIGINAL span's exact match.
    let text = match text_probe(conn, norm)? {
        TextProbe::Present => TextProbe::Present,
        // A turbofish's normalized callee was inconclusive (Exhausted or Capped, e.g. a common
        // `new`/`parse`) — try the EXACT original span, a much narrower query. Confirm Present if
        // it matches; else keep the normalized result, never downgrading an inconclusive
        // callee into a false absence via the exact miss.
        norm_result if norm != ident => match text_probe(conn, ident)? {
            TextProbe::Present => TextProbe::Present,
            _ => norm_result,
        },
        norm_result => norm_result,
    };
    match text {
        TextProbe::Present => {
            // A path-shaped span that reached here is NOT an indexed file (tier 2 missed) yet
            // appears verbatim — a FILE claim can still diverge, so it carries a file-specific
            // label symmetric to the symbol case; a name / expression keeps the
            // symbol-oriented one.
            let label = if is_file_path_shaped(ident, file_paths) {
                TEXT_PRESENT_FILE
            } else {
                TEXT_PRESENT_SYMBOL
            };
            return Ok((label.to_string(), ResolutionKind::TextPresent));
        },
        // Presence INDETERMINATE — the phrase matched more chunks than the scan cap, so the
        // verbatim chunk may rank beyond the window and go unchecked. Do NOT risk a false
        // `Absent` on that; treat the span as uninformative.
        TextProbe::Capped => return Ok((UNRESOLVABLE.to_string(), ResolutionKind::Unresolvable)),
        TextProbe::Exhausted => {},
    }
    // 4. TERMINAL — genuinely not present. A qualified CALL (`Type::method(args)`) or qualified
    //    MACRO (`mod::foo!`) is NOT ruled `Absent`: a method/macro not written verbatim and not
    //    resolved above is too ambiguous to convict — usually dot-called, external/std, imported,
    //    or a paraphrase — so the false-positive-averse posture drops that speculative signal. Only
    //    a code-shaped span that is neither, and resolves nowhere, is a genuine absence; anything
    //    else is uninformative.
    if span_is_code_shaped(norm, file_paths)
        && !norm.contains("::")
        && !is_qualified_call(norm)
        && !is_qualified_macro(norm)
    {
        Ok((NOT_FOUND.to_string(), ResolutionKind::Absent))
    } else {
        Ok((UNRESOLVABLE.to_string(), ResolutionKind::Unresolvable))
    }
}

/// Apply note-level index-coverage authority to one otherwise whole-tree resolution. A terminal
/// miss is only `Absent` when the note's own binding resolves inside the active index. If the
/// binding points at an excluded workflow/config/cookbook file, the index cannot honestly claim
/// the token is absent from that source domain, so downgrade the miss to `Unresolvable`.
fn resolve_memory_identifier(
    conn: &Connection,
    ident: &str,
    file_paths: &[String],
    absence_is_authoritative: bool,
) -> rusqlite::Result<(String, ResolutionKind)> {
    let (resolution, kind) = resolve_identifier(conn, ident, file_paths)?;
    if kind == ResolutionKind::Absent && !absence_is_authoritative {
        Ok((OUTSIDE_INDEX_COVERAGE.to_string(), ResolutionKind::Unresolvable))
    } else {
        Ok((resolution, kind))
    }
}

/// The three outcomes of a verbatim-text probe. `Capped` (the scan hit its guard before confirming)
/// is kept distinct from `Exhausted` (every phrase-match was checked, none matched) so the caller
/// never rules `Absent` on an indeterminate result — the sound fallback the raised scan cap needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextProbe {
    Present,
    Exhausted,
    Capped,
}

/// Whether a span is a QUALIFIED CALL — a `::`-qualified head followed by a call-arg list
/// (`Type::method(args)`). Such a span is never ruled `Absent` (the method may be dot-called,
/// external, or a paraphrase); presence for it comes only from a verbatim match of the qualified
/// name at tier 3.
fn is_qualified_call(ident: &str) -> bool {
    ident.split_once('(').is_some_and(|(head, _)| head.trim().contains("::"))
}

/// Whether a span is a `::`-QUALIFIED MACRO invocation (`crate::foo!`, `mod::foo!(x)`). Kept
/// conservative (never ruled `Absent`, like a qualified method call): the index tracks macros by
/// bare name only, so a qualified macro can't be told apart from an external / imported / moved /
/// namesake macro — forcing NOT_FOUND would false-positive on a live `tracing::info!()`.
fn is_qualified_macro(ident: &str) -> bool {
    macro_head(ident).is_some_and(|head| head.contains("::"))
}

/// The bare symbol name to probe for a span at tier 1, or `None` when the span is not name-shaped:
///   - BARE span (no `::`): strip a trailing call-arg list and a macro bang so `some_fn(x)` probes
///     `some_fn` and `my_macro!(x)` / `my_macro!` probe `my_macro` — safe, the note named the
///     callee bare, and resolving it to its (accurate) `Symbol` avoids a false absence for a
///     defined free function or macro.
///   - QUALIFIED span (`::`): the last `::` segment of the FULL span WITHOUT stripping args, so a
///     qualified CALL (`Type::method(x)`) keeps its parens and is NOT bare-probed — probing the
///     bare method would match an UNRELATED namesake and MASK the call's disappearance. A plain
///     qualified NAME (no args) still probes its last segment. A present qualified call resolves
///     through the verbatim-text tier (on its arg-stripped NAME), and the terminal is conservative
///     for it.
fn symbol_lookup_name(ident: &str) -> Option<String> {
    let last = if ident.contains("::") {
        ident.rsplit("::").next().unwrap_or(ident).trim()
    } else {
        call_head(ident)
    };
    BARE_NAME_RE.is_match(last).then(|| last.to_string())
}

/// The callee NAME at the head of a call or macro-invocation span — everything before the first
/// `(`, with a trailing macro bang stripped: `build_index(cfg)`, `my_macro!(x)`, and `my_macro!`
/// all yield the bare name (`build_index` / `my_macro`). A `::`-qualified head keeps its path
/// (`Type::method`). (Turbofish is already normalized away before this runs.)
fn call_head(ident: &str) -> &str {
    if let Some(mh) = macro_head(ident) {
        return mh.strip_suffix('!').unwrap_or(mh); // `vec!` -> `vec`
    }
    ident.split('(').next().unwrap_or(ident).trim() // `some_fn(x)` -> `some_fn`
}

/// The head of a Rust MACRO invocation WITH its bang — `foo!` for `foo!`, `foo!(x)`, `foo![x]`, and
/// `foo! { .. }` — or `None` when the span is not a macro invocation. The delimiter after the bang
/// may be `()`, `[]`, or `{}` (or absent); the trailing `!` is what distinguishes a macro
/// (`vec![x]`) from indexing (`arr[i]`).
fn macro_head(ident: &str) -> Option<&str> {
    let head = ident.split(['(', '[', '{']).next().unwrap_or(ident).trim();
    head.ends_with('!').then_some(head)
}

/// Whether a span is a Rust MACRO invocation (see [`macro_head`]). Constrains the symbol lookup to
/// macro-kind symbols so a removed macro is not masked by a same-named non-macro symbol.
fn is_macro_invocation(ident: &str) -> bool {
    macro_head(ident).is_some()
}

/// Strip Rust turbofish segments (`::<...>`, balanced) so `build_index::<Cfg>(cfg)` normalizes to
/// `build_index(cfg)` before any `::`/call logic — a turbofish `::` is a generic-argument marker,
/// not a path separator, and must not be read as a qualified path.
fn strip_turbofish(ident: &str) -> std::borrow::Cow<'_, str> {
    if !ident.contains("::<") {
        return std::borrow::Cow::Borrowed(ident);
    }
    let mut out = String::with_capacity(ident.len());
    let mut depth = 0usize; // turbofish `<`/`>` nesting
    let mut group = 0usize; // `()`/`[]`/`{}` nesting INSIDE the turbofish (fn-pointer args, arrays,
    // const-generic blocks) — `<`/`>` there are types/comparisons, not turbofish brackets
    let mut prev = '\0';
    let mut chars = ident.char_indices();
    while let Some((i, ch)) = chars.next() {
        if depth == 0 && ident[i..].starts_with("::<") {
            depth = 1;
            chars.next(); // consume the second ':'
            chars.next(); // consume the '<'
            prev = '<';
            continue;
        }
        if depth > 0 {
            match ch {
                '(' | '[' | '{' => group += 1,
                ')' | ']' | '}' => group = group.saturating_sub(1),
                '<' if group == 0 => depth += 1,
                // At the turbofish level, a `>` closes a level — EXCEPT a `->` (fn-pointer return);
                // inside a group (`{ N > 0 }`, `[T; N > 0]`) it is a comparison, never a close.
                '>' if group == 0 && prev != '-' => depth -= 1,
                _ => {},
            }
            prev = ch;
            continue;
        }
        out.push(ch);
        prev = ch;
    }
    std::borrow::Cow::Owned(out)
}

/// Whether a span is shaped like a code symbol or a file path — the shape whose genuine whole-tree
/// absence is a DIVERGENCE signal (a named entity the note describes is gone). A bare /
/// `::`-qualified name (with or without a stripped call-arg list), or a path (a `/`, or a bare
/// filename with a trailing extension), qualifies. Anything carrying whitespace, parens (that are
/// not a stripped call on a qualified name), brackets, quotes, or operators is a paraphrase /
/// snippet / flag, whose non-match is a shape artifact — not evidence of divergence.
fn span_is_code_shaped(ident: &str, file_paths: &[String]) -> bool {
    let ident = ident.trim();
    if SYMBOL_PATH_RE.is_match(ident) {
        return true;
    }
    if is_file_path_shaped(ident, file_paths) {
        return true;
    }
    // A CALL or MACRO span: the head before `(` (macro bang stripped) is name-shaped (a bare
    // `some_fn` / `my_macro`, or a `::`-qualified `Type::method`). A removed BARE call or macro is
    // a genuine absence — its arg-stripped name resolves nowhere, exactly as tiers 1 and 3
    // already probe it — so it must reach the NOT_FOUND terminal instead of falling to
    // Unresolvable and skipping the model (the deleted-function / note-ahead case). A qualified
    // call is still held back from the terminal by the caller's `!is_qualified_call` gate (the
    // namesake-masking guard); a PRESENT expression head (`Ok(None)`) is diverted to
    // TextPresent by tier 3 before this runs.
    SYMBOL_PATH_RE.is_match(call_head(ident))
}

/// Whether a span reads as a FILE PATH — a `/`-bearing or `stem.ext` shape whose extension an
/// indexed file actually uses (see [`looks_like_path`]). Shared by [`span_is_code_shaped`] (a
/// path's genuine whole-tree absence is a divergence signal) and the verbatim-text tier (a path
/// present only as source text gets a FILE-specific label, not the symbol one).
fn is_file_path_shaped(ident: &str, file_paths: &[String]) -> bool {
    let ident = ident.trim();
    PATH_SHAPE_RE.is_match(ident) && looks_like_path(ident, file_paths)
}

/// Whether a path-charset span reads as an INDEX-AUTHORITATIVE FILE PATH — one whose final segment
/// carries an extension that indexed files actually use. The extension gate is what makes a tier-2
/// miss INFORMATIVE: a `.rs`/`.md` path the index covers is a genuine file absence when gone, but a
/// path the index does NOT cover is a COVERAGE artifact, not an absence, and must not read as a
/// divergence:
///   - an unindexed extension (`.github/workflows/ci.yml` when only `crates`/`docs` are indexed),
///   - a DIRECTORY or extension-less path (`src/oplog/`, `src/oplog`) the index tracks no file for,
///   - dotted FIELD ACCESS (`DreamOptions.verify`, `config.dream.model`) — no indexed file's
///     extension, so not a path.
///
/// Any of these misses `Unresolvable`, never a false `Absent`. A `/` DISAMBIGUATES a real path from
/// field access: a slashed span is a file when its last segment's extension is covered, but a
/// NO-SLASH span must be a clean single-dot `stem.ext` — a bare MULTI-DOT span (`config.docs.md`)
/// is ambiguous with dotted field access, so the FP-averse call is to treat it as not-a-file.
fn looks_like_path(s: &str, file_paths: &[String]) -> bool {
    let last_segment = s.rsplit('/').next().unwrap_or(s);
    let Some(dot) = last_segment.rfind('.') else {
        return false; // a directory or extension-less name — the index tracks no such file
    };
    // No-slash multi-dot spans are ambiguous with field access; only a slash proves a path.
    if !s.contains('/') && last_segment.matches('.').count() != 1 {
        return false;
    }
    let ext = &last_segment[dot..]; // e.g. ".rs"
    FILE_EXT_RE.is_match(ext) && file_paths.iter().any(|p| p.ends_with(ext))
}

/// How the span appears VERBATIM in indexed source text (repo-scoped) — `Present`, `Exhausted`
/// (every phrase-match was checked, none contains it → genuine absence), or `Capped` (the scan hit
/// its guard first → INDETERMINATE, so the caller must not rule `Absent`). The probe target is the
/// span's arg-stripped NAME for a call (`Type::method(args)` -> `Type::method`, `some_fn(x)` ->
/// `some_fn`), else the full span (see `text_search_target`). `chunk_fts` (porter) narrows with a
/// PHRASE of the target's alphanumeric tokens: a chunk that contains the literal target has those
/// tokens ADJACENT (non-alphanumerics are token separators), so the phrase match cannot miss it — a
/// SOUND narrowing, unlike an AND + rank-capped one (empirically real for `clone_edges`: 1025
/// AND-matches ≫ 256). Candidates are decoded LAZILY in rank order; the FIRST confirmed hit settles
/// it, so a present token decodes ~one blob. Presence is FILE-INDEPENDENT (the resolution names no
/// files, so the pack and churn key stay stable as unrelated files gain/lose the token).
/// `TEXT_PRESENCE_SCAN_CAP` bounds the scan for a common phrase whose verbatim chunk ranks late —
/// exhausting FEWER rows than the cap is a definitive absence; hitting the cap is `Capped`
/// (indeterminate — never a false absence). A corrupt blob is skipped best-effort.
fn text_probe(conn: &Connection, ident: &str) -> rusqlite::Result<TextProbe> {
    let target = text_search_target(ident);
    let Some(query) = fts_token_query(target) else {
        return Ok(TextProbe::Exhausted);
    };
    let dicts = chunk_text_dict_bytes(conn)?;
    let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
    // Candidate chunks: contentless `chunk_fts.rowid` == `chunks.id`; the JOIN through the
    // repo-scoped `files` view keeps only the active repo's chunks (LIMIT is applied POST-join, so
    // a sibling repo's chunks never consume the guard budget). Rows are STEPPED lazily — the
    // return at the first confirmed hit stops the fetch, not just the decode.
    let mut stmt = conn.prepare(
        "SELECT ct.blob, ct.raw_len, ct.dict_version FROM chunk_fts JOIN chunks c ON c.id = \
         chunk_fts.rowid JOIN chunk_text ct ON ct.chunk_id = c.id JOIN files f ON f.id = \
         c.file_id WHERE chunk_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![query, TEXT_PRESENCE_SCAN_CAP as i64], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut seen = 0usize;
    for row in rows {
        seen += 1;
        let (blob, raw_len, dict_version) = row?;
        let Ok(bytes) = decoder.decompress(dict_version, &blob, raw_len.max(0) as usize) else {
            continue; // corrupt/undecodable chunk — best-effort skip
        };
        if std::str::from_utf8(&bytes).is_ok_and(|text| contains_at_token_boundary(text, target)) {
            return Ok(TextProbe::Present);
        }
    }
    // Fewer than the cap were scanned → the whole phrase set was checked (definitive absence);
    // reaching the cap leaves presence unknown beyond it.
    Ok(if seen >= TEXT_PRESENCE_SCAN_CAP { TextProbe::Capped } else { TextProbe::Exhausted })
}

/// Whether `needle` occurs in `haystack` bounded by non-token chars on any end that is itself a
/// token char — so a verbatim probe for `commit_fts` is NOT satisfied by a longer token that merely
/// contains it (`commit_fts_v2`), which would mask a rename/deletion of the non-symbol identifier
/// as still present. The boundary is required only where `needle` ends in a token char; a
/// punctuation-delimited span (`#[cfg(test)]`, `Ok(None)`) is self-delimiting and imposes no extra
/// constraint. The token alphabet is `[A-Za-z0-9_]`, PLUS `-` when the needle is kebab/flag-shaped
/// (contains `-`), so a cited `--config` is not "found" inside a longer `--config-file`, while an
/// identifier needle keeps `-` as a delimiter (a C `foo->bar` still finds `foo`). Neighbor lookups
/// are byte-wise and UTF-8-safe: `match_indices` yields char-boundary offsets, and any non-ASCII
/// neighbor byte is non-token (a boundary).
fn contains_at_token_boundary(haystack: &str, needle: &str) -> bool {
    let bytes = needle.as_bytes();
    let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) else {
        return false; // an empty target can't be a verbatim presence
    };
    let hyphen_is_token = needle.contains('-');
    let is_token = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || (hyphen_is_token && b == b'-');
    let guard_start = is_token(first);
    let guard_end = is_token(last);
    let hay = haystack.as_bytes();
    for (start, m) in haystack.match_indices(needle) {
        let end = start + m.len();
        let left_ok = !guard_start || start == 0 || !is_token(hay[start - 1]);
        let right_ok = !guard_end || end == hay.len() || !is_token(hay[end]);
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

/// The verbatim-text probe target: match the callee NAME so a present callee resolves regardless of
/// its specific (often paraphrased) arguments.
///   - CALL span (`Type::method(args)` -> `Type::method`, `some_fn(x)` -> `some_fn`).
///   - MACRO span (`my_macro!(x)` / `my_macro!` -> `my_macro!`): the target KEEPS the bang, so the
///     text probe requires the actual macro INVOCATION — a same-named non-macro (`fn my_macro`, no
///     bang) is not a false presence for a removed macro.
///   - Any other span — an attribute, an expression, a snippet — is matched in FULL, so a loose
///     prefix can't spuriously "confirm" it (e.g. `#[cfg(never)]` must not match on the `#[cfg`
///     prefix of a real `#[cfg(test)]`).
fn text_search_target(ident: &str) -> &str {
    let ident = ident.trim();
    if let Some(mh) = macro_head(ident) {
        // The invocation form WITH the bang (`foo!`, from any of `foo!(x)` / `foo![x]` /
        // `foo!{x}`); require a name-shaped callee so a non-code snippet falls through to
        // the full-span match.
        if SYMBOL_PATH_RE.is_match(mh.strip_suffix('!').unwrap_or(mh)) {
            return mh;
        }
    } else if ident.contains('(') {
        let head = call_head(ident);
        if SYMBOL_PATH_RE.is_match(head) {
            return head;
        }
    }
    ident
}

/// The `chunk_fts MATCH` PHRASE query for a verbatim-text probe: EVERY non-empty alphanumeric token
/// of `target`, in order, inside ONE quoted phrase (`"clone edges"`). Every token is kept, even a
/// 1-char one — dropping an interior token would break the adjacency the phrase relies on. `None`
/// when the target yields no token (so it can't be in the FTS index — the probe is skipped).
/// Builds a tokenized phrase. See `crate::impact::fts_phrase_query` for whole phrases
/// and impact historical’s `fts_escape` for OR-ed words.
fn fts_token_query(target: &str) -> Option<String> {
    let tokens: Vec<&str> =
        target.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).collect();
    (!tokens.is_empty()).then(|| format!("\"{}\"", tokens.join(" ")))
}

/// The resident `chunk_text` dictionaries as a `version -> bytes` map, read with plain SQL so the
/// verbatim-text probe stays on `rusqlite::Result` (no `anyhow` in the resolution path). Mirrors
/// `crate::chunk_text_dicts` without its `anyhow` return.
fn chunk_text_dict_bytes(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<i64, Vec<u8>>> {
    conn.prepare("SELECT version, dict FROM chunk_text_dict")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
        .collect()
}

/// Whether ANY extracted identifier is PRESENCE evidence (symbol / file / verbatim text) — the
/// "zero identifiers resolve" gate for `unverifiable_findings` (short-circuits on the first present
/// one). A span that is [`ResolutionKind::Absent`] or [`ResolutionKind::Unresolvable`] does not
/// count.
pub fn any_identifier_resolves(
    conn: &Connection,
    identifiers: &[String],
    file_paths: &[String],
) -> rusqlite::Result<bool> {
    for ident in identifiers {
        if resolve_identifier(conn, ident, file_paths)?.1.is_present() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// EVERY live symbol whose `name` matches (optionally constrained to a `kind`), as `path::name`,
/// path-sorted, through the `files` view (repo-scoped). Empty when the name is unknown anywhere in
/// the tree. Returns all matches (not `LIMIT 1`) so a common bare name is surfaced as ambiguous
/// rather than silently pinned to its first definition — see [`resolve_identifier`]. `DISTINCT`
/// collapses a symbol indexed twice at the same path (e.g. across chunks) but keeps genuinely
/// distinct same-named definitions. `kind` is `Some("macro")` for a macro invocation (`foo!`), so a
/// removed macro does NOT resolve to a same-named NON-macro (`fn foo`) and mask its absence.
fn resolve_symbol(
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.path, s.name FROM symbols s JOIN files f ON f.id = s.file_id WHERE \
         s.name = ?1 AND (?2 IS NULL OR s.kind = ?2) ORDER BY f.path, s.name",
    )?;
    stmt.query_map(rusqlite::params![name, kind], |r| {
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

/// Every indexed file path for the active repo (through the `files` view), sorted for deterministic
/// segment resolution.
pub fn indexed_file_paths(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT path FROM files ORDER BY path")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect()
}

/// Distinct, sorted, non-null binding paths for a memory (repo-scoped) — the memory's bound files.
/// `pub(super)` so the verdict pass can label a note by its first bound path.
pub fn bound_file_paths(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> rusqlite::Result<Vec<String>> {
    let bind_clause = schema::periphery_repo_scope_clause(scope, "repo_memory_bindings");
    conn.prepare(&format!(
        "SELECT DISTINCT {BINDING_CURRENT_PATH} AS path FROM repo_memory_bindings
         WHERE memory_id = ?1 AND {BINDING_CURRENT_PATH} IS NOT NULL{bind_clause}
         ORDER BY path"
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
            let dir_pattern = format!("{}/%", super::like_escape(&path));
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

/// Whether one bound path gives absence authority in the note's declared domain. Only an EXACT
/// FILE binding that resolves to a live indexed file qualifies. A repo-root or directory binding
/// can cover children excluded by `target_bindings`, so complete coverage is unprovable there.
fn bound_path_gives_absence_authority(conn: &Connection, path: &str) -> rusqlite::Result<bool> {
    if path.is_empty() {
        return Ok(false);
    }
    conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1 AND kind != '{}')",
            schema::TOMBSTONE_FILE_KIND
        ),
        [path],
        |r| r.get(0),
    )
}

/// Whether a server-derived CALL-PATH binding gives absence authority: every persisted edge must
/// currently resolve against the live graph — exact fingerprint, or loose name/kind/target for a
/// moved-line edge (mirroring `validate_call_path_binding`'s `current`/`relocated` outcomes,
/// recomputed live rather than read from the stored `anchor_status`). All candidate edges are
/// loaded in ONE bounded query — [`resolve::edge_by_fingerprint`] per persisted edge would
/// full-scan the live edge table once per edge, which the steady-state churn-key recomputation
/// cannot afford. A client-supplied hash with no persisted edges is unverifiable → NO authority.
fn call_path_gives_absence_authority(
    conn: &Connection,
    memory_id: &str,
    edge_sequence_hash: &str,
) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
         callee_logical_symbol_id, callee_identity_known FROM repo_memory_call_path_edges WHERE \
         memory_id = ?1 AND edge_sequence_hash = ?2 ORDER BY ordinal",
    )?;
    let edges = stmt
        .query_map(rusqlite::params![memory_id, edge_sequence_hash], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, i64>(6)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if edges.is_empty() {
        return Ok(false);
    }
    // A pre-V099 row cannot prove which callee it named. Validation may converge an exact
    // compatibility match first, but until then it grants no absence authority.
    if edges.iter().any(|edge| !edge.6) {
        return Ok(false);
    }
    let identities: Vec<EdgeLooseIdentity> = edges
        .iter()
        .map(|(_, from_name, to_name, edge_kind, target, callee, _)| EdgeLooseIdentity {
            from_name: from_name.clone(),
            to_name: to_name.clone().unwrap_or_default(),
            edge_kind: edge_kind.clone(),
            target_qualified_name: target.clone(),
            callee_logical_symbol_id: *callee,
        })
        .collect();
    let mut candidates = resolve::live_edges_matching_identities(conn, &identities)?;
    for (fingerprint, from_name, to_name, edge_kind, target, callee, _) in &edges {
        // Each persisted edge must CONSUME a distinct live candidate: with duplicate loose
        // identities in one path, a single surviving call site cannot vouch for all of them —
        // the sibling that fell out of the index must stay missing.
        let matched = candidates
            .iter()
            .position(|candidate| {
                candidate.fingerprint == *fingerprint
                    || candidate.legacy_fingerprint.as_deref() == Some(fingerprint.as_str())
            })
            .or_else(|| {
                candidates.iter().position(|candidate| {
                    (
                        candidate.from_name.as_deref().unwrap_or(""),
                        candidate.to_name.as_str(),
                        candidate.edge_kind.as_str(),
                        candidate.target_qualified_name.as_deref().unwrap_or(""),
                        candidate.callee_logical_symbol_id,
                    ) == (
                        from_name.as_deref().unwrap_or(""),
                        to_name.as_deref().unwrap_or(""),
                        edge_kind.as_str(),
                        target.as_deref().unwrap_or(""),
                        *callee,
                    )
                })
            });
        match matched {
            Some(pos) => {
                candidates.swap_remove(pos);
            },
            None => return Ok(false),
        }
    }
    Ok(true)
}

/// Whether terminal identifier misses are authoritative for this note. Absence authority requires
/// PROVABLE coverage of the note's domain, and on any partial index (for example only
/// `crates`/`docs` ingested) no binding-free or loosely-scoped domain is provable:
/// - an intentionally UNBOUND conceptual note (no binding rows at all) is checked against the whole
///   index, but the index is not the whole TREE — its misses stay indeterminate;
/// - ANY pathless binding that is not a server-derived call path (a commit or tracker anchor, `path
///   IS NULL`) — even alongside a covered file binding — keeps absence indeterminate, because
///   identifiers belonging to the historical or tracker side of the note live outside the indexed
///   tree;
/// - each call-path binding must independently resolve against the live graph;
/// - otherwise EVERY bound path must be an exact live indexed file; directory/root bindings remain
///   indeterminate because configured index coverage may omit children.
fn memory_binding_is_index_covered(
    conn: &Connection,
    memory_id: &str,
    scope: &Option<String>,
) -> anyhow::Result<bool> {
    let bind_clause = schema::periphery_repo_scope_clause(scope, "repo_memory_bindings");
    let has_bindings: bool = conn
        .prepare(&format!(
            "SELECT EXISTS(SELECT 1 FROM repo_memory_bindings WHERE memory_id = ?1{bind_clause})"
        ))?
        .query_row([memory_id], |r| r.get(0))?;
    if !has_bindings {
        return Ok(false);
    }
    let has_pathless: bool = conn
        .prepare(&format!(
            "SELECT EXISTS(SELECT 1 FROM repo_memory_bindings WHERE memory_id = ?1
                AND {BINDING_CURRENT_PATH} IS NULL
                AND binding_kind != 'call_path'{bind_clause})"
        ))?
        .query_row([memory_id], |r| r.get(0))?;
    if has_pathless {
        return Ok(false);
    }
    let mut call_path_stmt = conn.prepare(&format!(
        "SELECT {BINDING_CURRENT_BINDING_ID} FROM repo_memory_bindings
         WHERE memory_id = ?1 AND binding_kind = 'call_path'{bind_clause}"
    ))?;
    let call_path_hashes = call_path_stmt
        .query_map([memory_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for hash in &call_path_hashes {
        if !call_path_gives_absence_authority(conn, memory_id, hash)? {
            return Ok(false);
        }
    }
    for path in bound_file_paths(conn, memory_id, scope)? {
        if !bound_path_gives_absence_authority(conn, &path)? {
            return Ok(false);
        }
    }
    Ok(true)
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
    use rag_rat_db::text_compression::{ChunkTextDecoder, ChunkTextRow};
    let dicts = crate::chunk_text_dicts(conn)?;
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
#[path = "evidence/tests.rs"]
mod tests;
