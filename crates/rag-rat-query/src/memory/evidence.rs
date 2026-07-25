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

use super::resolve;

/// The authoritative "resolves nowhere" verdict, emitted only when the note's binding proves the
/// searched domain is live and covered.
const NOT_FOUND: &str = "NOT FOUND anywhere in the source tree";
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
const TEXT_PRESENT_SYMBOL: &str = "not a defined symbol; appears verbatim as source text";
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
/// Version stamp of the compaction prompt, gating `memory_summaries` reuse the same way.
pub const COMPACT_PROMPT_VERSION: &str = "compact-v1";

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
    ///
    /// "Citable" = carries at least one piece of PRESENCE evidence: a bound-file excerpt, or an
    /// identifier that resolved to a symbol / file / verbatim source text
    /// ([`ResolutionKind::is_present`]). A pack whose every identifier is
    /// [`ResolutionKind::Absent`] (a real gone-symbol) or [`ResolutionKind::Unresolvable`] (a
    /// paraphrase / non-code span) carries no positive evidence — pass 0 already decides such a
    /// memory `memory_unverifiable`, so it must not be asked of the model (asking would invite
    /// a `diverged` verdict cited off a bare NOT-FOUND / unresolvable row).
    pub fn is_citable(&self) -> bool {
        !self.excerpts.is_empty() || self.identifiers.iter().any(|id| id.kind.is_present())
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
/// `memory_reality` / `memory_summaries`. It is `sha256(trim(title) + "\n" + trim(body))`, covering
/// EXACTLY what the verdict and compaction prompts render: the title and body, and nothing else. So
/// a title / body edit re-verifies / re-summarizes and drops the stale verdict / summary / marker,
/// while a change to a dimension the prompts don't render (kind, tags, payload) does NOT churn the
/// derived overlays. It becomes the §5.5 canonical [`content_hash`] (which folds the payload) only
/// when the prompts start rendering the payload — bundled with a [`PROMPT_VERSION`] bump so that
/// rollout is backfill-free (a bump re-queues every memory anyway). Distinct from the raw
/// create-time `memory_input_hash`, which also folds kind + tags (dimensions the prompts don't
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
pub fn checked_inputs_hash(
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
    Ok(rag_rat_base::hash::hex_sha256(format!("{files}\u{1d}{idents}").as_bytes()))
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
    let absence_is_authoritative = memory_binding_is_index_covered(conn, memory_id, scope)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let mut out = Vec::with_capacity(identifiers.len());
    for ident in &identifiers {
        // Fold the rendered resolution STRING. It is now path-independent for every tier —
        // `TextPresent` names no files, symbol/file carry stable identity, NOT_FOUND / unresolvable
        // are fixed — so the churn key re-verifies on a genuine tier flip or a symbol/file identity
        // change but stays stable against unrelated repo churn (adding a file carrying a cited
        // common token no longer re-queues the paid verdict).
        out.push((
            ident.clone(),
            resolve_memory_identifier(conn, ident, &file_paths, absence_is_authoritative)?.0,
        ));
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
    Ok(EvidencePack { memory_id: memory_id.to_string(), identifiers: resolutions, excerpts })
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
                "not an indexed file; appears verbatim only as source text"
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
    let Some(query) = fts_phrase_query(target) else {
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
fn fts_phrase_query(target: &str) -> Option<String> {
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

/// Escape a string for use as a SQLite `LIKE` pattern under `ESCAPE '\'` — the three special chars
/// `\`, `%`, `_` are backslash-escaped so a bound path with a literal `_`/`%` matches literally.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
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

/// Whether one bound path gives absence authority in the note's declared domain. Only an EXACT
/// FILE binding that resolves to a live indexed file qualifies. A repo-root or directory binding
/// can cover children excluded by `target_bindings`, so complete coverage is unprovable there.
fn bound_path_gives_absence_authority(conn: &Connection, path: &str) -> rusqlite::Result<bool> {
    if path.is_empty() {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1 AND kind != 'deleted')",
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
        "SELECT edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name FROM \
         repo_memory_call_path_edges WHERE memory_id = ?1 AND edge_sequence_hash = ?2 ORDER BY \
         ordinal",
    )?;
    let edges = stmt
        .query_map(rusqlite::params![memory_id, edge_sequence_hash], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if edges.is_empty() {
        return Ok(false);
    }
    let identities: Vec<(Option<String>, String, String, Option<String>)> = edges
        .iter()
        .map(|(_, from_name, to_name, edge_kind, target)| {
            (
                from_name.clone(),
                to_name.clone().unwrap_or_default(),
                edge_kind.clone(),
                target.clone(),
            )
        })
        .collect();
    let mut candidates = resolve::live_edges_matching_identities(conn, &identities)?;
    for (fingerprint, from_name, to_name, edge_kind, target) in &edges {
        // Each persisted edge must CONSUME a distinct live candidate: with duplicate loose
        // identities in one path, a single surviving call site cannot vouch for all of them —
        // the sibling that fell out of the index must stay missing.
        let matched = candidates
            .iter()
            .position(|candidate| candidate.fingerprint == *fingerprint)
            .or_else(|| {
                candidates.iter().position(|candidate| {
                    (
                        candidate.from_name.as_deref().unwrap_or(""),
                        candidate.to_name.as_str(),
                        candidate.edge_kind.as_str(),
                        candidate.target_qualified_name.as_deref().unwrap_or(""),
                    ) == (
                        from_name.as_deref().unwrap_or(""),
                        to_name.as_deref().unwrap_or(""),
                        edge_kind.as_str(),
                        target.as_deref().unwrap_or(""),
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
            "SELECT EXISTS(SELECT 1 FROM repo_memory_bindings WHERE memory_id = ?1 AND path IS \
             NULL AND binding_kind != 'call_path'{bind_clause})"
        ))?
        .query_row([memory_id], |r| r.get(0))?;
    if has_pathless {
        return Ok(false);
    }
    let mut call_path_stmt = conn.prepare(&format!(
        "SELECT binding_id FROM repo_memory_bindings WHERE memory_id = ?1 AND binding_kind = \
         'call_path'{bind_clause}"
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
mod tests {
    /// A fresh in-memory index at the current schema. `MigrationHooks::noop()` is the
    /// documented-sound choice on a fresh scratch DB, keeping these tests engine-free.
    fn mem_db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &rag_rat_db::MigrationHooks::noop()).unwrap();
        c
    }

    /// Point the connection's periphery scope at `repo_id` — mirrors the scope-context write the
    /// production open installs.
    fn set_repo(c: &Connection, repo_id: &str) {
        c.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [repo_id],
        )
        .unwrap();
    }
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
        rag_rat_db::chunk_text_store::seed_chunk_text(c, chunk_id, text).unwrap();
        // Mirror production: the chunk is FTS-searchable, so `resolve_identifier`'s verbatim-text
        // tier can narrow to it (`seed_chunk_text` alone populates only `chunk_text`).
        c.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", rusqlite::params![
            chunk_id, text
        ])
        .unwrap();
        file_id
    }

    fn content_hash(title: &str, body: &str) -> String {
        note_content_hash(title, body)
    }

    #[test]
    fn checked_inputs_hash_folds_child_files_of_a_directory_binding() {
        // Regression (PR #428): a `--dir` binding stores a directory in
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
        // Regression (PR #428): a `--dir .` binding normalizes to an empty path, which
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
        // Regression (PR #428): a bound dir path with a SQLite LIKE wildcard (`_`) must be
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
        // Regression (PR #428): hashing only unique shas is blind to a rebind that keeps
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
        // Regression (PR #428): the churn key must fingerprint the WHOLE evidence pack,
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
        // Regression (PR #428): `identifier_windows` merges adjacent hits into ONE range,
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
        // Regression (PR #428): a note whose every identifier resolves to NOT_FOUND and
        // has no bound-file excerpts is the pass-0 unverifiable case and must NOT be
        // citable, or the model would be asked and could open a divergence citing the
        // NOT_FOUND rows.
        let c = mem_db();
        set_repo(&c, "r");
        // One unrelated indexed file, bound: the exact file declares the note's source domain.
        seed_file(&c, "src/lib.rs", "fn unrelated() {}\n", "r");
        seed_memory(&c, "m1", "t", "a note about `never_defined_symbol` and nothing else", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/lib.rs','src/lib.rs','current',0,'r')",
            [],
        )
        .unwrap();
        let pack = evidence_pack(&c, "m1").unwrap();
        assert!(!pack.identifiers.is_empty(), "the identifier was extracted");
        assert!(pack.identifiers.iter().all(|id| id.resolution == NOT_FOUND));
        assert!(!pack.is_citable(), "an all-NOT_FOUND, no-excerpt pack is uncitable");
    }

    #[test]
    fn queue_re_enqueues_when_the_verdict_prompt_version_changes() {
        // Regression (PR #428): a stored verdict from an older PROMPT_VERSION is not
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
    fn resolve_bound_files_is_path_sorted_not_rowid_order() {
        // Regression (PR #428): the excerpt-budget cap consumes files in THIS order, so it
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
        // Regression (PR #428): a common bare name must surface as AMBIGUOUS (all
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
        let locs = resolve_symbol(&c, "shared_name", None).unwrap();
        assert_eq!(
            locs,
            vec!["src/a.rs::shared_name", "src/b.rs::shared_name"],
            "all, path-sorted"
        );
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "shared_name", &files).unwrap();
        assert!(res.starts_with("symbols (2):"), "renders ambiguous: {res}");
        assert_eq!(kind, ResolutionKind::Symbol);
    }

    #[test]
    fn resolve_file_segment_returns_all_same_suffix_matches_as_ambiguous() {
        // Regression (PR #428): a shorthand path (`lib.rs`) suffix-matching more than one
        // indexed file must surface as AMBIGUOUS, not pinned to the first — same class as ambiguous
        // symbols, so deleting the file the note meant re-verifies even when a same-suffix file
        // survives.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "a note about `lib.rs`", "r");
        seed_file(&c, "crates/b/lib.rs", "fn f() {}\n", "r");
        seed_file(&c, "crates/a/lib.rs", "fn f() {}\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "lib.rs", &files).unwrap();
        assert!(res.starts_with("files (2):"), "renders ambiguous: {res}");
        assert!(
            res.contains("crates/a/lib.rs") && res.contains("crates/b/lib.rs"),
            "lists both matches: {res}"
        );
        assert_eq!(kind, ResolutionKind::File);
    }

    #[test]
    fn resolve_identifier_ladder_classifies_symbol_file_text_absent_and_unresolvable() {
        // The divergence-false-positive fix: a span that is present in source as a NON-symbol (a DB
        // table name, a local var) resolves to `TextPresent`, NOT the authoritative NOT_FOUND that
        // the model over-read as divergence; a non-code-shaped span that matches nothing is
        // `Unresolvable` (uninformative), never NOT_FOUND; only a name-shaped genuine absence keeps
        // NOT_FOUND.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(
            &c,
            "src/db.rs",
            "fn real_fn() {\n    let local_root_named = 1;\n    conn.execute(\"CREATE TABLE \
             commit_fts(x)\");\n}\n",
            "r",
        );
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','real_fn','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        let resolve = |id: &str| resolve_identifier(&c, id, &files).unwrap();

        assert_eq!(resolve("real_fn").1, ResolutionKind::Symbol, "1: a defined symbol");
        // A BARE call to a defined function arg-strips to its name and resolves as a Symbol — NOT a
        // false Absent / mislabeled TextPresent.
        assert_eq!(resolve("real_fn(&db, 1)").1, ResolutionKind::Symbol, "1b: a bare call");
        assert_eq!(resolve("src/db.rs").1, ResolutionKind::File, "2: an indexed file");

        // 3: a DB table name and a local variable — present as literal text, not defined symbols.
        // The resolution is file-INDEPENDENT (names no files) so the pack/churn key stay stable.
        let (res, kind) = resolve("commit_fts");
        assert_eq!(kind, ResolutionKind::TextPresent, "table name in DDL text: {res}");
        assert!(res.contains("appears verbatim"), "labeled present-but-not-a-symbol: {res}");
        assert_eq!(resolve("local_root_named").1, ResolutionKind::TextPresent, "a local variable");

        // 4a: a name-shaped identifier that exists nowhere → the genuine divergence signal.
        let (res, kind) = resolve("no_such_symbol_anywhere");
        assert_eq!(kind, ResolutionKind::Absent);
        assert_eq!(res, NOT_FOUND);

        // A qualified name may denote a field/variant/member the symbol index does not cover.
        // Without a call or a resolvable trailing symbol, its miss is not authoritative absence.
        let (res, kind) = resolve("ConfigSnapshot::trusted_path");
        assert_eq!(kind, ResolutionKind::Unresolvable, "qualified member: {res}");
        assert_ne!(res, NOT_FOUND);

        // 4b: an attribute span, absent from text and not name-shaped → uninformative, NOT
        // NOT_FOUND.
        let (res, kind) = resolve("#[cfg(never_seen_flag)]");
        assert_eq!(kind, ResolutionKind::Unresolvable, "attribute span: {res}");
        assert_ne!(res, NOT_FOUND);
    }

    #[test]
    fn resolve_identifier_treats_memory_id_cross_references_as_non_code() {
        // #678: a memory body that cross-references ANOTHER memory by id (`mem_<hex>_<hex>`, or the
        // common shorthand PREFIX) is not a CODE entity. It resolves `Unresolvable` —
        // uninformative, hidden from the pack — so it is NEITHER the NOT_FOUND
        // code-divergence signal NOR source presence. The presence point matters: a note
        // whose ONLY resolving token is a cross-ref has no code evidence, so it must stay
        // `unverifiable`, not be promoted to the verdict model on the strength of another
        // note existing.
        let c = mem_db();
        set_repo(&c, "r");
        // The referenced memory exists here, but existence does not change the classification — a
        // cross-ref is never code evidence, dangling or not.
        seed_memory(
            &c,
            "mem_19f2ad6cf90_2feb75f29ff8",
            "title",
            "a cross-referenced decision",
            "r",
        );
        let files = indexed_file_paths(&c).unwrap();
        let resolve = |id: &str| resolve_identifier(&c, id, &files).unwrap();

        for id in [
            "mem_19f2ad6cf90_2feb75f29ff8",  // an existing memory, full id
            "mem_19f2ad6cf90",               // the timestamp-prefix form agents usually cite
            "mem_deadbeefdead_cafebabecafe", // a dangling reference
        ] {
            let (res, kind) = resolve(id);
            assert_eq!(
                kind,
                ResolutionKind::Unresolvable,
                "{id}: a cross-ref is uninformative: {res}"
            );
            assert!(!kind.is_present(), "{id}: a cross-ref is not source presence: {res}");
            assert_ne!(res, NOT_FOUND, "{id}: a cross-ref is never the code-absence signal: {res}");
        }
    }

    #[test]
    fn resolve_identifier_prefers_a_real_symbol_over_the_memory_id_heuristic() {
        // #678 review: `is_memory_id_shaped` is a SHAPE heuristic — a real code symbol whose name
        // happens to be all-hex (`mem_deadbeefdead`) must still resolve as that SYMBOL, not be
        // hidden as a memory cross-reference. Code resolution runs FIRST; the memory-ref heuristic
        // only reclassifies a span that would otherwise be a genuine NOT_FOUND absence.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(&c, "src/m.rs", "fn mem_deadbeefdead() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','mem_deadbeefdead','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "mem_deadbeefdead", &files).unwrap();
        assert_eq!(kind, ResolutionKind::Symbol, "a real hex-named symbol resolves as code: {res}");
    }

    #[test]
    fn resolve_identifier_treats_a_source_mentioned_memory_id_as_a_cross_ref_not_text() {
        // #678 review: even when a `mem_<hex>` id appears VERBATIM in indexed source (a doc comment
        // or a test fixture), it is still a cross-reference to another memory, not code evidence —
        // so it classifies `Unresolvable`, NOT `TextPresent`. The memory-id check runs
        // after symbol/file resolution (real symbols win) but BEFORE the verbatim-text
        // probe.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(
            &c,
            "src/x.rs",
            "// see mem_19f2ad6cf90_2feb75f29ff8 for the rationale\nfn f() {}\n",
            "r",
        );
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "mem_19f2ad6cf90_2feb75f29ff8", &files).unwrap();
        assert_eq!(
            kind,
            ResolutionKind::Unresolvable,
            "a source-mentioned mem-id is a cross-ref, not verbatim text: {res}"
        );
        assert!(!kind.is_present(), "a cross-ref is not source presence even in source: {res}");
    }

    #[test]
    fn resolve_identifier_does_not_mistake_a_segmented_hex_word_for_a_memory_id() {
        // #678 review: `is_memory_id_shaped` keys on the FIRST underscore-delimited segment being a
        // long contiguous hex run (the minted `mem_<hex-timestamp>_<suffix>` shape), NOT the
        // aggregate hex count across underscores. Otherwise an ordinary identifier built from short
        // hex-word chunks — `mem_dead_beef_ca` (4+4+2 = 10 hex) — is misread as a memory cross-ref,
        // and if it appears as source text its legitimate `TextPresent` evidence gets suppressed.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/y.rs", "let mem_dead_beef_ca = 1;\n", "r");
        let files = indexed_file_paths(&c).unwrap();

        // A segmented hex-word local is NOT a memory id: its verbatim source presence survives.
        let (res, kind) = resolve_identifier(&c, "mem_dead_beef_ca", &files).unwrap();
        assert_eq!(
            kind,
            ResolutionKind::TextPresent,
            "a segmented hex word keeps its source presence: {res}"
        );
        assert!(kind.is_present(), "a segmented hex word is not suppressed as a cross-ref: {res}");

        // A genuinely minted id (long first-segment hex run) is still classified as a cross-ref.
        let (_, mem_kind) = resolve_identifier(&c, "mem_19f2ad6cf90_2feb75f29ff8", &files).unwrap();
        assert_eq!(
            mem_kind,
            ResolutionKind::Unresolvable,
            "a real mem-id (long first-segment hex run) still classifies as a cross-ref"
        );
    }

    #[test]
    fn memory_id_shape_splits_full_prefix_and_non_ids() {
        use MemIdShape::{Full, NotAnId, Prefix};
        // Full — two hex segments: both mint sites (`mem_{now:x}_{suffix}`, consolidate `13+12`),
        // plus a trailing-underscore edge. Decisive by shape, no record lookup.
        for id in
            ["mem_19f2ad6cf90_2feb75f29ff8", "mem_deadbeefdead0_cafebabecafe", "mem_deadbeefdead_"]
        {
            assert_eq!(memory_id_shape(id), Full, "{id} is a full two-segment id");
        }
        // Prefix — one long hex segment, no suffix: the shorthand agents cite; ambiguous with a
        // local.
        for id in ["mem_19f2ad6cf90", "mem_deadbeefdead"] {
            assert_eq!(memory_id_shape(id), Prefix, "{id} is a bare timestamp prefix");
        }
        // NotAnId — short first segment, non-hex tail, or no `mem_` prefix.
        for id in ["mem_dead_beef_ca", "mem_19f2ad6cf90_lookup", "mem_copy", "memcpy", "other"] {
            assert_eq!(memory_id_shape(id), NotAnId, "{id} is not a memory id");
        }
    }

    #[test]
    fn resolve_identifier_lets_a_coincidental_hex_local_keep_its_text_presence() {
        // #678 review (Codex P2): a PREFIX-shaped token that is neither a defined symbol nor a
        // recorded memory, but appears verbatim in source, is a coincidental code identifier — a
        // memory-shaped LOCAL misses the symbol tier, so ONLY its source presence carries it. Its
        // `TextPresent` evidence must survive, not be suppressed as a memory cross-reference.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/y.rs", "let mem_deadbeefdead = 1;\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "mem_deadbeefdead", &files).unwrap();
        assert_eq!(
            kind,
            ResolutionKind::TextPresent,
            "a coincidental hex local keeps its source presence: {res}"
        );
        assert!(
            kind.is_present(),
            "a coincidental hex local is not suppressed as a cross-ref: {res}"
        );
    }

    #[test]
    fn resolve_identifier_keeps_a_recorded_memorys_prefix_a_cross_ref_even_when_source_mentions_it()
    {
        // #678 review: a prefix that IS the timestamp of a RECORDED memory stays a cross-reference
        // even when the bare prefix also appears verbatim in indexed text (an ADR/plan doc citing
        // the id) — a cross-ref is never code evidence. Record-confirmation is what
        // distinguishes this from a coincidental hex local (no matching memory), which
        // keeps its text presence above.
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "mem_19f2ad6cf90_2feb75f29ff8", "title", "a decision", "r");
        seed_file(&c, "docs/adr.md", "// relates to mem_19f2ad6cf90\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "mem_19f2ad6cf90", &files).unwrap();
        assert_eq!(
            kind,
            ResolutionKind::Unresolvable,
            "a recorded memory's prefix is a cross-ref, not source text: {res}"
        );
        assert!(!kind.is_present(), "a cross-ref is not source presence: {res}");
        assert_ne!(res, NOT_FOUND, "a cross-ref is never the code-absence signal: {res}");
    }

    #[test]
    fn resolve_identifier_treats_a_dangling_memory_prefix_as_a_cross_ref_not_an_absence() {
        // #678: a prefix matching NO recorded memory, appearing in source only as a SUBSTRING of a
        // longer full id (not at a token boundary), is a dangling cross-reference — `Unresolvable`,
        // and NEVER the NOT_FOUND code-absence signal (a bare code-shaped prefix would otherwise
        // reach the `Absent` terminal). The Prefix arm owns this terminal so the property
        // can't regress.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/z.rs", "// see mem_19f2ad6cf90_2feb75f29ff8\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "mem_19f2ad6cf90", &files).unwrap();
        assert_eq!(kind, ResolutionKind::Unresolvable, "a dangling prefix is a cross-ref: {res}");
        assert_ne!(res, NOT_FOUND, "a dangling prefix is never a code absence: {res}");
    }

    #[test]
    fn resolve_identifier_never_masks_a_deleted_qualified_method_via_a_namesake() {
        // A qualified call is NOT resolved by bare method name (which would match an unrelated
        // NAMESAKE and hide the method's deletion). A PRESENT qualified call resolves through the
        // verbatim-text tier on its arg-stripped NAME. A qualified call is NEVER ruled Absent (a
        // dot-called / external / paraphrased method is too ambiguous to convict) — an absent one
        // is Unresolvable, so it can't false-diverge.
        let c = mem_db();
        set_repo(&c, "r");
        // `from_config` exists as a bare free function (the potential namesake), and the qualified
        // `Present::from_config` appears verbatim in source; `Gone::from_config` does not.
        let fid = seed_file(
            &c,
            "src/m.rs",
            "fn from_config() {}\nlet x = Present::from_config(&cfg);\n",
            "r",
        );
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','from_config','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();

        // Present qualified call → verbatim text of the qualified NAME (args paraphrased away).
        let (res, kind) =
            resolve_identifier(&c, "Present::from_config(&config.dream.model)", &files).unwrap();
        assert_eq!(kind, ResolutionKind::TextPresent, "present qualified call resolves: {res}");

        // A qualified call whose qualified NAME is not written verbatim is NEVER false-Absented,
        // whether its bare method survives elsewhere (`Elsewhere::from_config` — `from_config` is a
        // symbol) or vanished entirely (`Gone::vanished_method_xyz`): both are Unresolvable, so a
        // dot-called / external / paraphrased qualified reference can't produce a false divergence.
        // This drops the speculative deleted-qualified-method true-positive to kill a common false
        // positive.
        for call in ["Elsewhere::from_config(x)", "Gone::vanished_method_xyz(y)"] {
            let (res, kind) = resolve_identifier(&c, call, &files).unwrap();
            assert_eq!(kind, ResolutionKind::Unresolvable, "{call} is not a false absence: {res}");
        }
    }

    #[test]
    fn resolver_is_language_neutral_and_never_false_absents_dotted_references() {
        // The resolver must not be Rust-centric in a way that manufactures false divergences for
        // other languages. `::`-qualified handling is Rust/C++-specific, but a `.`-qualified
        // reference (TS/Kotlin/Python/Java `Class.method`, `module.func`) is NEVER ruled Absent:
        // present → verbatim TextPresent, absent → Unresolvable. Bare names (any language,
        // camelCase included) still resolve as Symbol / Absent correctly. So no language
        // gets a false divergence; the only cross-language gap is precision (a
        // `.`-qualified name isn't resolved to a Symbol), which costs at most a missed
        // divergence — the safe direction.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(
            &c,
            "src/svc.ts",
            "class UserService { getUserById(id) { return this.repo.findById(id); } }\n",
            "r",
        );
        // A camelCase symbol (as any indexed language yields) resolves by bare name.
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'typescript','getUserById','method',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        let kind = |id: &str| resolve_identifier(&c, id, &files).unwrap().1;

        assert_eq!(kind("getUserById"), ResolutionKind::Symbol, "camelCase symbol resolves");
        assert_eq!(kind("getUserDeleted"), ResolutionKind::Absent, "a gone bare name is absent");
        // `.`-qualified references: present verbatim → TextPresent; absent → Unresolvable. NEVER
        // Absent — the FP-averse guarantee holds for non-`::` languages.
        assert_eq!(
            kind("UserService.getUserById(id)"),
            ResolutionKind::Unresolvable,
            "a dotted method call is never a false absence"
        );
        assert_eq!(
            kind("this.repo.findById"),
            ResolutionKind::TextPresent,
            "a dotted reference present verbatim is text-present, not absent"
        );
    }

    #[test]
    fn text_presence_is_sound_past_the_rank_cap() {
        // Soundness regression: the old AND-of-tokens + `LIMIT 256` narrowing was UNSOUND — the
        // chunk that literally contains the identifier could rank below the cap among many
        // token-co-occurring chunks (empirically `clone_edges`: 1025 AND-matches ≫ 256) and
        // be dropped → a false `Absent` → a false `memory_divergence` on the very token
        // class the fix targets. The phrase narrowing + scan guard must find the verbatim
        // chunk even when it ranks last behind a flood of higher-ranked phrase-matches that
        // do NOT contain it.
        let c = mem_db();
        set_repo(&c, "r");
        // 300 decoys that phrase-match `"poison token"` twice each (higher bm25) but never contain
        // the underscored identifier; one chunk with the verbatim `poison_token`, ranked last.
        for i in 0..300 {
            seed_file(&c, &format!("src/decoy_{i}.rs"), "poison token poison token\n", "r");
        }
        seed_file(&c, "src/real.rs", "let poison_token = 1;\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "poison_token", &files).unwrap();
        assert_eq!(
            kind,
            ResolutionKind::TextPresent,
            "found the verbatim chunk past the 256 cap: {res}"
        );
    }

    #[test]
    fn checked_inputs_hash_is_stable_against_unrelated_text_presence_churn() {
        // Churn-stability regression: the churn hash must NOT fold the TextPresent path
        // enumeration, or adding an UNRELATED file that happens to carry a cited common
        // token re-keys the memory and re-runs the paid verdict for no reason. A
        // `TextPresent` token contributes a KIND marker only.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/a.rs", "let commit_fts_seen = 1;\n", "r");
        seed_memory(&c, "m1", "t", "note about the `commit_fts_seen` local", "r");
        let before = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        // Another file carrying the same token — unrelated to this memory. Hash must not move.
        seed_file(&c, "src/b.rs", "let commit_fts_seen = 2;\n", "r");
        let after = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        assert_eq!(before, after, "unrelated text-presence file must not re-key the churn hash");
    }

    #[test]
    fn dotted_field_access_is_unresolvable_not_a_false_file_absence() {
        // Regression: field access reads as NOT a file — interior dots
        // (`config.dream.model`) OR a single dot whose extension no indexed file uses
        // (`DreamOptions.verify` — `.verify` is not a source extension) → `Unresolvable`, never
        // `Absent`/NOT_FOUND (a false divergence). A single-extension filename whose extension IS
        // indexed (`.rs`) stays code-shaped, so a genuinely gone one is a real absence.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/only.rs", "fn f() {}\n", "r"); // an indexed `.rs`; nothing else below
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "config.dream.model", &files).unwrap().1,
            ResolutionKind::Unresolvable,
            "interior-dot field access is not a file"
        );
        assert_eq!(
            resolve_identifier(&c, "DreamOptions.verify", &files).unwrap().1,
            ResolutionKind::Unresolvable,
            "single-dot field access with a non-source extension is not a file"
        );
        assert_eq!(
            resolve_identifier(&c, "deleted_module.rs", &files).unwrap().1,
            ResolutionKind::Absent,
            "a bare filename with an indexed extension is a genuine file absence"
        );
    }

    #[test]
    fn deleted_bare_call_is_a_genuine_absence_not_unresolvable() {
        // A note citing a function in CALL form whose function was REMOVED must surface the
        // genuine-absence signal (the deleted-function / note-ahead case), not be silently dropped
        // as Unresolvable. The arg-stripped bare name is name-shaped, so its whole-tree absence is
        // a divergence signal — consistent with tiers 1 and 3, which both arg-strip a bare
        // call. The text tier still diverts a PRESENT expression head (`Ok(None)`) to
        // TextPresent first, so this does not reintroduce a false NOT_FOUND on illustrative
        // expressions.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/x.rs", "fn other() {}\n", "r"); // does not mention `new_helper` at all
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "new_helper()", &files).unwrap();
        assert_eq!(kind, ResolutionKind::Absent, "a removed bare call is a genuine absence: {res}");
        assert_eq!(res, NOT_FOUND);
        assert_eq!(
            resolve_identifier(&c, "build_index(cfg)", &files).unwrap().1,
            ResolutionKind::Absent,
            "a removed bare call carrying args is still a genuine absence"
        );
        // GUARD: an enum-constructor / expression whose head is PRESENT as text stays TextPresent
        // (the text tier resolves it before the terminal), so bare-call absence does not
        // manufacture a false NOT_FOUND on an illustrative expression.
        seed_file(&c, "src/y.rs", "let v = Ok(None);\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "Ok(None)", &files).unwrap().1,
            ResolutionKind::TextPresent,
            "a present expression head is text-present, not a false absence"
        );
    }

    #[test]
    fn renamed_identifier_superstring_does_not_mask_absence() {
        // Token-boundary soundness: when a NON-symbol identifier was renamed to a longer token
        // (`commit_fts` -> `commit_fts_v2`), the old name survives only as a SUBSTRING of the new
        // one. A raw substring match would mask the rename as TextPresent and suppress the absence
        // signal; the verbatim probe must confirm the match at token boundaries, so the gone name
        // resolves as a genuine absence while the renamed-TO token is still found.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/db.rs", "let commit_fts_v2 = open();\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "commit_fts", &files).unwrap();
        assert_eq!(kind, ResolutionKind::Absent, "a rename to a superstring is not masked: {res}");
        assert_eq!(res, NOT_FOUND);
        assert_eq!(
            resolve_identifier(&c, "commit_fts_v2", &files).unwrap().1,
            ResolutionKind::TextPresent,
            "the bounded token itself is still found (boundaries don't reject real matches)"
        );
    }

    #[test]
    fn deleted_file_present_only_as_text_gets_a_file_specific_label() {
        // A path-shaped span that is NOT an indexed file but appears verbatim (in a comment/string)
        // must carry a FILE-specific text-present label, so a memory claiming the file exists can
        // still diverge — the symbol-oriented label only contradicts SYMBOL claims. Symmetry with
        // the "not a defined symbol" case for names.
        let c = mem_db();
        set_repo(&c, "r");
        // An indexed `.md` file establishes `.md` as a COVERED extension (so a `.md` miss is
        // index-authoritative); a comment mentions a DELETED `.md` file verbatim.
        seed_file(&c, "docs/current.md", "# current docs\n", "r");
        seed_file(&c, "src/note.rs", "// see docs/old.md for the legacy format\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "docs/old.md", &files).unwrap();
        assert_eq!(kind, ResolutionKind::TextPresent, "present as text: {res}");
        assert!(res.contains("not an indexed file"), "file-specific label: {res}");
        // A NON-path present token still gets the symbol-oriented label.
        seed_file(&c, "src/t.rs", "let commit_fts = 1;\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, _) = resolve_identifier(&c, "commit_fts", &files).unwrap();
        assert!(res.contains("not a defined symbol"), "a name keeps the symbol label: {res}");
    }

    #[test]
    fn scoped_package_path_with_at_sign_is_recognized_as_a_path() {
        // A scoped package directory (`@scope/`) uses `@`, a valid path char — a deleted but
        // index-covered `.ts` file under it is a genuine file absence, not Unresolvable.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "packages/@scope/app/src/index.ts", "export const x = 1;\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "packages/@scope/app/src/gone.ts", &files).unwrap().1,
            ResolutionKind::Absent,
            "a deleted scoped-package path is a genuine file absence"
        );
    }

    #[test]
    fn unindexed_path_reference_is_unresolvable_not_a_false_absence() {
        // A tier-2 file MISS is a genuine absence only where the index has AUTHORITY over the path:
        // a path whose extension no indexed file uses (`.yml` when only `.rs`/`.md` are indexed), a
        // DIRECTORY path (no file extension), or an out-of-root path is a coverage artifact — the
        // file may well exist on disk, just outside the index — so its miss is Unresolvable, never
        // the NOT_FOUND that reads as a genuine deletion / divergence.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "crates/x/src/lib.rs", "fn f() {}\n", "r"); // `.rs` indexed
        seed_file(&c, "docs/guide.md", "# guide\n", "r"); // `.md` indexed (for the field-access case)
        let files = indexed_file_paths(&c).unwrap();
        for p in [
            ".github/workflows/release.yml", // unindexed extension, out of root
            "crates/x/src/oplog/",           // a directory (trailing slash, no file extension)
            "crates/x/src/oplog",            // a directory (no file extension)
            "/workspace/rag-rat.toml",       // out-of-root, unindexed extension
            "config.docs.md",                /* a bare MULTI-DOT dotted ref (field access), not
                                              * a file */
        ] {
            let (res, kind) = resolve_identifier(&c, p, &files).unwrap();
            assert_eq!(
                kind,
                ResolutionKind::Unresolvable,
                "{p} is not a false file absence: {res}"
            );
            assert_ne!(res, NOT_FOUND);
        }
        // CONTRAST: paths with an INDEXED extension that are genuinely gone stay real absences,
        // including a slashed MULTI-DOT filename — the slash disambiguates a path from the bare
        // multi-dot field access above.
        for gone in ["crates/x/src/deleted.rs", "crates/x/src/schema.test.rs", "gone_module.rs"] {
            assert_eq!(
                resolve_identifier(&c, gone, &files).unwrap().1,
                ResolutionKind::Absent,
                "a gone path with an indexed extension is still a genuine file absence: {gone}"
            );
        }
    }

    #[test]
    fn turbofish_generic_call_resolves_by_callee_name_not_its_generic_args() {
        // A Rust turbofish (`build_index::<Cfg>(cfg)`) must resolve by its CALLEE name — its
        // `::<...>` is a generic-argument marker, not a qualified-path separator. A present
        // one resolves to its symbol; a removed one is a genuine absence (NOT_FOUND), not
        // hidden as Unresolvable.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(&c, "src/x.rs", "fn build_index() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','build_index','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "build_index::<Cfg>(cfg)", &files).unwrap().1,
            ResolutionKind::Symbol,
            "a present generic call resolves to its callee symbol"
        );
        assert_eq!(
            resolve_identifier(&c, "gone_index::<Cfg>(cfg)", &files).unwrap(),
            (NOT_FOUND.to_string(), ResolutionKind::Absent),
            "a removed generic call is a genuine absence, not hidden"
        );
        // A turbofish argument carrying a `->` (fn-pointer return type) or a const-generic
        // comparison (`{ N > 0 }`) must not have its inner `>` read as the end of the generic list.
        for gone in ["gone_index::<fn() -> u8>(x)", "gone_index::<{ N > 0 }>()"] {
            assert_eq!(
                resolve_identifier(&c, gone, &files).unwrap(),
                (NOT_FOUND.to_string(), ResolutionKind::Absent),
                "a turbofish arg with `->`/const-generic `>` is stripped cleanly: {gone}"
            );
        }
        // A PRESENT bare turbofish call whose callee appears with different args/generics must not
        // be a false absence — the normalized callee `from_str` is found by its contiguous
        // name.
        seed_file(&c, "src/y.rs", "let v = from_str::<Real>(&body);\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        assert_ne!(
            resolve_identifier(&c, "from_str::<Cfg>(payload)", &files).unwrap().1,
            ResolutionKind::Absent,
            "a present bare turbofish callee (different args) is not a false absence"
        );
    }

    #[test]
    fn qualified_macros_are_conservative_never_a_false_absence() {
        // A `::`-qualified macro can't be disambiguated from an external / imported / moved /
        // namesake macro (the index tracks macros by BARE name only), so it stays CONSERVATIVE
        // (Unresolvable), never NOT_FOUND: a live `tracing::info!()` must not be falsely marked
        // absent, a possibly-removed `crate::gone_macro!()` is uninformative rather than a
        // fabricated divergence, and `old_mod::foo!` is not silently masked by an unrelated
        // same-named macro.
        let c = mem_db();
        set_repo(&c, "r");
        // The macro is external: the source imports and invokes it BARE (`info!`), with no local
        // macro symbol and no verbatim `tracing::info!` spelling.
        seed_file(&c, "src/x.rs", "use tracing::info;\nfn f() { info!(\"hi\"); }\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        for cited in
            ["tracing::info!()", "crate::gone_macro!(x)", "crate::gone_macro![x]", "old_mod::foo!"]
        {
            assert_eq!(
                resolve_identifier(&c, cited, &files).unwrap().1,
                ResolutionKind::Unresolvable,
                "a qualified macro is conservative, never a false absence: {cited}"
            );
        }
    }

    #[test]
    fn present_qualified_turbofish_call_is_text_present_not_hidden() {
        // A PRESENT qualified turbofish call must stay citable. The source carries the generic args
        // verbatim, so the TEXT tier probes the ORIGINAL span — a normalized `HashMap::new` is NOT
        // contiguous in `HashMap::<T>::new()` and would falsely miss (Unresolvable/uncitable).
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/x.rs", "let m = HashMap::<String, Vec<u8>>::new();\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "HashMap::<String, Vec<u8>>::new()", &files).unwrap().1,
            ResolutionKind::TextPresent,
            "a present qualified turbofish call is found verbatim, not hidden"
        );
    }

    #[test]
    fn removed_macro_invocation_is_a_genuine_absence() {
        // A macro is a named code entity: a removed `my_macro!` / `my_macro!(x)` must surface
        // NOT_FOUND, not be hidden as Unresolvable because of the `!`. A present one resolves.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(&c, "src/x.rs", "macro_rules! live_macro { () => {}; }\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','live_macro','macro',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "live_macro!", &files).unwrap().1,
            ResolutionKind::Symbol,
            "a present macro resolves to its symbol"
        );
        for gone in ["gone_macro!", "gone_macro!(x)"] {
            assert_eq!(
                resolve_identifier(&c, gone, &files).unwrap(),
                (NOT_FOUND.to_string(), ResolutionKind::Absent),
                "a removed macro invocation is a genuine absence: {gone}"
            );
        }
    }

    #[test]
    fn removed_macro_is_not_masked_by_a_same_named_non_macro_symbol() {
        // A removed macro `gone_macro!` with a surviving same-named NON-macro (`fn gone_macro`)
        // must surface as a genuine absence, not be masked: the symbol lookup is
        // macro-kind-constrained (no false Symbol), and the text probe searches the
        // INVOCATION form `gone_macro!` (with the bang), which the bare `fn gone_macro`
        // does not satisfy — so it resolves NOT_FOUND.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(&c, "src/x.rs", "fn gone_macro() {}\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','gone_macro','function',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "gone_macro!", &files).unwrap(),
            (NOT_FOUND.to_string(), ResolutionKind::Absent),
            "a removed macro surfaces as absent, not masked by a non-macro namesake"
        );
    }

    #[test]
    fn macro_invocations_with_bracket_or_brace_delimiters_resolve() {
        // Rust macros invoke with `()`, `[]`, or `{}` delimiters. A present `live_vec![x]` resolves
        // to its macro symbol; a removed `gone_vec![x]` / `gone_tl! { .. }` surfaces NOT_FOUND
        // rather than falling through as Unresolvable.
        let c = mem_db();
        set_repo(&c, "r");
        let fid = seed_file(&c, "src/x.rs", "macro_rules! live_vec { () => {}; }\n", "r");
        c.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES \
             (?1,'rust','live_vec','macro',0,0)",
            rusqlite::params![fid],
        )
        .unwrap();
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "live_vec![x]", &files).unwrap().1,
            ResolutionKind::Symbol,
            "a present macro invoked with [] resolves to its symbol"
        );
        for gone in ["gone_vec![x]", "gone_tl! { a }"] {
            assert_eq!(
                resolve_identifier(&c, gone, &files).unwrap(),
                (NOT_FOUND.to_string(), ResolutionKind::Absent),
                "a removed bracket/brace macro is a genuine absence: {gone}"
            );
        }
    }

    #[test]
    fn present_invoked_macro_without_a_symbol_is_text_present() {
        // A macro invoked in source but not indexed as a symbol (an external macro) resolves via
        // its INVOCATION form in text — present, not a false absence.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/x.rs", "fn f() { ext_macro!(1); }\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        assert_eq!(
            resolve_identifier(&c, "ext_macro!", &files).unwrap().1,
            ResolutionKind::TextPresent,
            "a present invoked macro is found via its invocation form"
        );
    }

    #[test]
    fn flag_prefix_rename_is_not_masked_as_verbatim_presence() {
        // A cited CLI flag whose only source occurrence is a LONGER flag (`--config` vs
        // `--config-file`) must NOT be reported present — `-` is a token char for a flag needle, so
        // the prefix is not a boundary match. The exact longer flag itself is still found.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/cli.rs", "let f = value_of(\"--config-file\");\n", "r");
        let files = indexed_file_paths(&c).unwrap();
        let (res, kind) = resolve_identifier(&c, "--config", &files).unwrap();
        assert_ne!(
            kind,
            ResolutionKind::TextPresent,
            "a flag prefix must not be masked as verbatim presence: {res}"
        );
        assert_eq!(
            resolve_identifier(&c, "--config-file", &files).unwrap().1,
            ResolutionKind::TextPresent,
            "the exact flag is present at a boundary"
        );
    }

    #[test]
    fn is_citable_requires_presence_evidence_not_bare_absence_or_unresolvable() {
        let id = |kind| IdentifierResolution {
            identifier: "i".to_string(),
            resolution: "r".to_string(),
            kind,
        };
        let pack = |ids: Vec<IdentifierResolution>| EvidencePack {
            memory_id: "m".to_string(),
            identifiers: ids,
            excerpts: Vec::new(),
        };
        assert!(
            !pack(vec![id(ResolutionKind::Absent), id(ResolutionKind::Unresolvable)]).is_citable(),
            "a pack of only genuine-absence + uninformative rows carries no evidence — uncitable"
        );
        assert!(
            pack(vec![id(ResolutionKind::Absent), id(ResolutionKind::TextPresent)]).is_citable(),
            "verbatim-text presence is citable evidence"
        );
        assert!(pack(vec![id(ResolutionKind::Symbol)]).is_citable(), "a symbol is citable");
    }

    #[test]
    fn checked_inputs_hash_flips_when_a_name_gains_verbatim_text_presence() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_memory(&c, "m1", "t", "note about the `commit_fts` table", "r");
        let before = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        // The name now appears as source text → its resolution flips NOT_FOUND → verbatim-text,
        // which the churn key must reflect so a re-verification is queued.
        seed_file(&c, "src/schema.rs", "conn.execute(\"CREATE TABLE commit_fts(id)\");\n", "r");
        let after = checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        assert_ne!(before, after, "the resolution-tier flip re-keys the churn hash");
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
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','crates/x/src/thing.rs','crates/x/src/thing.rs','current',0,'r')",
            [],
        )
        .unwrap();
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
        assert_eq!(missing.resolution, NOT_FOUND, "an exact-file-domain miss is authoritative");
    }

    #[test]
    fn evidence_pack_downgrades_absence_when_the_binding_is_outside_index_coverage() {
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "release config", "the workflow uses `git_release_enable`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','release-plz.toml','release-plz.toml','current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "git_release_enable")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_when_only_some_bindings_are_index_covered() {
        // A note bound to BOTH an indexed source file and an excluded config file: the covered
        // binding must not lend absence authority to an identifier that lives only in the
        // uncovered one — a whole-tree miss for `git_release_enable` stays indeterminate.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "release config", "the workflow uses `git_release_enable`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/lib.rs','src/lib.rs','current',0,'r'), \
             ('m1','path','release-plz.toml','release-plz.toml','current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "git_release_enable")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_a_pathless_commit_binding() {
        // A commit/tracker binding stores `path = NULL`: the note is anchored to history or an
        // issue, not to the indexed tree, so a whole-tree miss must not read as an authoritative
        // NOT FOUND just because the pack is otherwise citable.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "historical", "the refactor removed `gone_helper`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','commit','deadbeef',NULL,'current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_keeps_absence_indeterminate_for_an_unbound_note() {
        // An intentionally unbound note is checked against the whole index, but the index is not
        // the whole TREE on a partial index — its misses stay indeterminate whether or not the
        // index is empty (here: non-empty).
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "t", "the note describes `ghost_symbol`", "r");
        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "ghost_symbol")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_an_unpersisted_call_path_binding() {
        // A client-supplied call-path hash has no persisted edges to re-resolve against the live
        // graph — unverifiable, so absence stays indeterminate.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','call_path','client-hash',NULL,'current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_a_call_path_binding_with_dead_edges() {
        // Persisted edges that no longer resolve against the live graph (fingerprint AND loose
        // identity both miss) mean the path's domain drifted out of the index — indeterminate.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','call_path','hash1',NULL,'current',0,'r')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
             edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name) VALUES \
             ('m1','hash1',0,'fp-unknown','deleted_caller','deleted_callee','calls_name',NULL)",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_a_call_path_with_a_missing_duplicate_edge() {
        // Two persisted edges share one loose identity, but only ONE live call site remains: the
        // multiset match must not let the survivor vouch for both — the path is incomplete and
        // absence stays indeterminate.
        let c = mem_db();
        set_repo(&c, "r");
        let file_id = seed_file(&c, "src/lib.rs", "fn caller_fn() {}\n", "r");
        c.execute(
            "INSERT INTO edges(from_name, to_name, edge_kind, confidence, target_qualified_name, \
             source_file_id, source_start_line, source_end_line) VALUES \
             ('caller_fn','gone_helper','calls_name','exact',NULL,?1,1,1)",
            [file_id],
        )
        .unwrap();
        seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','call_path','hash1',NULL,'current',0,'r')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
             edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name) VALUES \
             ('m1','hash1',0,'fp-a','caller_fn','gone_helper','calls_name',NULL), \
             ('m1','hash1',1,'fp-b','caller_fn','gone_helper','calls_name',NULL)",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_call_path_binding_with_live_edges_keeps_absence_authoritative() {
        // A server-derived call path whose persisted edges still resolve against the live graph
        // (here by loose name/kind/target identity — the fingerprint is unknown) IS index-covered:
        // its identifiers were resolved from this index, so a whole-tree miss is a real absence.
        let c = mem_db();
        set_repo(&c, "r");
        let file_id = seed_file(&c, "src/lib.rs", "fn caller_fn() {}\n", "r");
        c.execute(
            "INSERT INTO edges(from_name, to_name, edge_kind, confidence, target_qualified_name, \
             source_file_id, source_start_line, source_end_line) VALUES \
             ('caller_fn','gone_helper','calls_name','exact',NULL,?1,1,1)",
            [file_id],
        )
        .unwrap();
        seed_memory(&c, "m1", "t", "the note describes `gone_helper`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','call_path','hash1',NULL,'current',0,'r')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
             edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name) VALUES \
             ('m1','hash1',0,'fp-unknown','caller_fn','gone_helper','calls_name',NULL)",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(
            resolution.resolution, NOT_FOUND,
            "a live call path keeps whole-tree absence authoritative"
        );
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_a_mixed_file_and_commit_binding() {
        // An indexed file binding PLUS a pathless commit anchor: the file alone would give
        // absence authority, but identifiers belonging to the historical side of the note live
        // outside the indexed tree — any pathless row keeps absence indeterminate.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "mixed", "the refactor removed `gone_helper`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/lib.rs','src/lib.rs','current',0,'r'), \
             ('m1','commit','deadbeef',NULL,'current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_a_repo_root_binding_under_a_partial_index() {
        // A `--dir .` binding in a partially indexed repository (only `crates`/`docs` ingested)
        // must not grant absence authority: an identifier living only in an excluded root TOML or
        // workflow would otherwise read as an authoritative NOT FOUND.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/lib.rs", "fn indexed() {}\n", "r");
        seed_memory(&c, "m1", "release config", "the workflow uses `git_release_enable`", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES ('m1','dir','','','current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "git_release_enable")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
    }

    #[test]
    fn evidence_pack_downgrades_absence_for_a_directory_binding() {
        // A directory binding can hold children the index never ingested (excluded by
        // `target_bindings`), and the index cannot enumerate what it does not contain — so a
        // directory NEVER gives absence authority even when every discovered child is indexed.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/dir/a.rs", "fn a() {}\n", "r");
        seed_file(&c, "src/dir/b.rs", "fn b() {}\n", "r");
        seed_memory(&c, "m1", "module note", "the module keeps `gone_helper` available", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','dir','src/dir','src/dir','current',0,'r')",
            [],
        )
        .unwrap();

        let pack = evidence_pack(&c, "m1").unwrap();
        let resolution = pack
            .identifiers
            .iter()
            .find(|identifier| identifier.identifier == "gone_helper")
            .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unresolvable);
        assert_eq!(resolution.resolution, OUTSIDE_INDEX_COVERAGE);
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
    fn a_memory_id_in_a_bound_file_is_not_citable_source_evidence() {
        // #678 review: a note whose only identifier is a `mem_<hex>` cross-ref must NOT become
        // citable just because that id appears verbatim in its bound file. The mem-id is excluded
        // from excerpt windowing (it is Unresolvable, never source evidence), so a cross-ref-only
        // note does not leak to the verdict model via a `path:line:` excerpt.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(
            &c,
            "src/x.rs",
            "// rationale: see mem_19f2ad6cf90_2feb75f29ff8\nfn f() {}\n",
            "r",
        );
        // The note's ONLY identifier is the memory cross-reference.
        seed_memory(&c, "m1", "t", "background: mem_19f2ad6cf90_2feb75f29ff8", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/x.rs','src/x.rs','current',0,'r')",
            [],
        )
        .unwrap();
        let pack = evidence_pack(&c, "m1").unwrap();
        assert!(
            pack.excerpts.is_empty(),
            "a memory cross-ref must not window a bound-file excerpt: {:?}",
            pack.excerpts
        );
        assert!(!pack.is_citable(), "a cross-ref-only note stays non-citable");
    }

    #[test]
    fn a_coincidental_hex_local_is_citable_and_windows_an_excerpt() {
        // #678 review (Codex P2): the mirror of the cross-ref case above — a note whose only
        // identifier is a coincidental hex LOCAL (`mem_deadbeefdead`, a prefix-shaped token that is
        // no recorded memory) present in its bound file IS real source evidence. It
        // resolves `TextPresent`, so it is NOT excluded from excerpt windowing and the note
        // stays citable — otherwise a genuinely-verifiable memory would be wrongly held
        // back as `unverifiable`.
        let c = mem_db();
        set_repo(&c, "r");
        seed_file(&c, "src/x.rs", "let mem_deadbeefdead = compute();\nfn f() {}\n", "r");
        seed_memory(&c, "m1", "t", "the guard reads mem_deadbeefdead each pass", "r");
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/x.rs','src/x.rs','current',0,'r')",
            [],
        )
        .unwrap();
        let pack = evidence_pack(&c, "m1").unwrap();
        assert!(
            pack.excerpts.iter().any(|e| e.text.contains("mem_deadbeefdead")),
            "a coincidental hex local windows its bound-file excerpt: {:?}",
            pack.excerpts
        );
        assert!(pack.is_citable(), "a note anchored by a real source token stays citable");
    }
}
