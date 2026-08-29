//! Payload composition for the Claude Code grep-augmentation PreToolUse hook.
//!
//! Shared by the `rag-rat mcp` socket listener (with per-session dedupe) and the hook
//! client's direct read-only fallback (stateless). Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`. Never loads the embedding
//! model — symbol/FTS lanes only.

use std::collections::HashSet;

use rag_rat_query::{memory, symbol};
use rusqlite::{Connection, OptionalExtension};

use crate::search::lexical;

/// Hard cap on rendered context. Truncation drops whole items, never mid-item.
pub const MAX_CONTEXT_CHARS: usize = 1500;
const MAX_SYMBOLS: u32 = 3;
pub(crate) const MAX_MEMORIES: u32 = 4;
const MAX_LEXICAL_HITS: u32 = 3;
/// Lexical hits below this fraction of the best hit's score are dropped as low-relevance noise.
const LEXICAL_RELATIVE_FLOOR: f64 = 0.6;
/// FTS memory hits below this fraction of the best hit's bm25 magnitude are dropped as
/// low-relevance noise.
///
/// SCOPE: this catches ONE case — a hit whose only match is a token so common that fts5 clamps its
/// idf to 1e-6, which lands it ~6 orders of magnitude below the best hit. It is not a general
/// "did this answer the query" gate, and it is NOT comparable to [`LEXICAL_RELATIVE_FLOOR`], which
/// grades bounded reciprocal-rank scores.
///
/// The value is set by what a LEGITIMATE match can score, not by what noise scores, because bm25
/// magnitude does not separate the two once idf stays positive: it folds term coverage together
/// with term frequency and body length. Measured on a 43-memory corpus with a 3-term query, an
/// all-terms match carrying a ~1 300-char body scores 0.098 of the best hit, while a co-match on a
/// single token present in 10 of the 43 scores 0.126 — the noise ranks ABOVE the real match. Any
/// floor tight enough to cut that noise therefore drops exact matches for their body length, which
/// is strictly worse than the noise it removes. This one sits an order of magnitude under the
/// worst legitimate match measured and four orders above the clamped-idf case, so it can only fire
/// where it decides correctly. Moderately-common-token co-matches are left to [`MAX_MEMORIES`].
const MEMORY_RELATIVE_FLOOR: f64 = 0.01;

/// Maximum gist length in a rendered memory digest line — body or dream summary alike; longer text
/// is truncated with `…`.
const MAX_MEMORY_BODY_CHARS: usize = 240;

/// Strip regex syntax from a grep pattern, leaving plain query text. Metacharacters become
/// spaces (so alternation/group contents survive as separate words); runs of whitespace
/// collapse; result is trimmed.
///
/// Exception: a `.` (bare metachar) or `\.` (escaped) that sits directly between two ASCII
/// word characters is preserved as a literal `.` — this keeps `foo.bar`-style qualified names
/// intact. All other positions keep the space-substitution behavior.
pub fn normalize_pattern(pattern: &str) -> String {
    let chars_vec: Vec<char> = pattern.chars().collect();
    let n = chars_vec.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        let ch = chars_vec[i];
        match ch {
            '\\' if i + 1 < n => {
                let next = chars_vec[i + 1];
                if next == '.' {
                    // `\.` — check whether it's between two word chars in the *output* context.
                    // We look at the last non-space char pushed to `out` (prev) and the char
                    // after the escape sequence (lookahead).
                    let prev_word = out
                        .chars()
                        .rev()
                        .find(|c| *c != ' ')
                        .map(|c| c.is_ascii_alphanumeric() || c == '_')
                        .unwrap_or(false);
                    let next_word = chars_vec
                        .get(i + 2)
                        .map(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .unwrap_or(false);
                    if prev_word && next_word {
                        out.push('.');
                    } else {
                        out.push(' ');
                    }
                    i += 2;
                } else {
                    // All other escapes → space; consume both chars.
                    out.push(' ');
                    i += 2;
                }
            },
            '.' => {
                // Bare `.` metachar — preserve between word chars, else space.
                let prev_word = out
                    .chars()
                    .rev()
                    .find(|c| *c != ' ')
                    .map(|c| c.is_ascii_alphanumeric() || c == '_')
                    .unwrap_or(false);
                let next_word = chars_vec
                    .get(i + 1)
                    .map(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .unwrap_or(false);
                if prev_word && next_word {
                    out.push('.');
                } else {
                    out.push(' ');
                }
                i += 1;
            },
            '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                out.push(' ');
                i += 1;
            },
            _ => {
                out.push(ch);
                i += 1;
            },
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A normalized pattern that looks like one code identifier (optionally `::`/`.`-qualified):
/// the symbol-lane trigger. Multi-word or short patterns return `None`.
pub fn identifier_candidate(normalized: &str) -> Option<&str> {
    if normalized.len() < 3 || normalized.contains(' ') {
        return None;
    }
    let mut chars = normalized.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.')).then_some(normalized)
}

/// Definition/declaration keywords that commonly prefix the symbol in a grep pattern, across the
/// indexed languages. Stripped when isolating the one identifier a multi-word pattern targets.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "pub",
    "mut",
    "let",
    "const",
    "static",
    "struct",
    "enum",
    "trait",
    "impl",
    "type",
    "mod",
    "use",
    "async",
    "await",
    "return",
    "class",
    "def",
    "func",
    "function",
    "interface",
    "export",
    "import",
    "var",
    "val",
    "public",
    "private",
    "protected",
    "final",
    "override",
    "suspend",
    "void",
    "extern",
    "unsafe",
    "where",
    "dyn",
    // Swift. Without these, `protocol Fetcher` / `actor Store` / `extension Client` leave two
    // identifier-shaped tokens, so the pattern reads as ambiguous and drops to the lexical lane —
    // while `func Fetcher` (a keyword we already knew) correctly reached the symbol lane.
    "protocol",
    "actor",
    "extension",
    "init",
    "deinit",
    "subscript",
    "operator",
    "precedencegroup",
    "macro",
    "open",
    "internal",
    "fileprivate",
    "mutating",
    "nonmutating",
    "inout",
    "some",
    "any",
];

/// The single identifier a pattern targets, for the symbol lane. A lone identifier is used
/// directly; a definition-style multi-word pattern (`fn resolve_all_edges`, `pub struct
/// SymbolIndex`) is reduced by dropping definition keywords — if exactly one identifier-shaped
/// token remains, that is the target. Anything more ambiguous (two+ identifiers, or free text)
/// returns `None` and falls to the lexical lane, where multi-concept search is actually useful.
///
/// This is what stops a precise `grep "fn foo"` from getting a redundant lexical echo of results
/// grep already found: it routes to the symbol lane (symbol + bound memories) instead.
pub fn extract_symbol_identifier(normalized: &str) -> Option<&str> {
    if let Some(ident) = identifier_candidate(normalized) {
        return Some(ident);
    }
    let mut candidate: Option<&str> = None;
    for token in normalized.split(' ') {
        if DEFINITION_KEYWORDS.contains(&token) {
            continue;
        }
        if identifier_candidate(token).is_some() {
            if candidate.is_some() {
                return None; // more than one identifier — ambiguous; use the lexical lane
            }
            candidate = Some(token);
        } else {
            return None; // a non-keyword, non-identifier token → free text; use the lexical lane
        }
    }
    candidate
}

/// What the listener/fallback already injected for this session. Default = inject everything.
#[derive(Debug, Default, Clone)]
pub struct DedupeFilter {
    pub memory_ids: HashSet<String>,
    pub symbol_keys: HashSet<String>,
}

/// A rendered digest plus the IDs it contains, for the caller's dedupe bookkeeping.
#[derive(Debug)]
pub struct GrepAugment {
    pub context: String,
    pub memory_ids: Vec<String>,
    pub symbol_keys: Vec<String>,
}

/// Compose the grep-augmentation digest for one search. Lanes per the spec: symbol lane when
/// the pattern looks like an identifier, memory lane always, lexical lane only when the
/// symbol lane is empty. Returns `None` when nothing (new) is worth injecting.
pub fn compose(
    conn: &Connection,
    raw_pattern: &str,
    search_path: Option<&str>,
    dedupe: &DedupeFilter,
    surface: rag_rat_base::config::MemorySurface,
) -> anyhow::Result<Option<GrepAugment>> {
    let normalized = normalize_pattern(raw_pattern);
    if normalized.is_empty() {
        return Ok(None);
    }

    let mut memories = Vec::new();
    let mut symbol_items: Vec<SymbolItem> = Vec::new();
    // Track whether the symbol lane produced any raw hits (before dedup).
    // Lexical lane only runs when there were no symbol hits at all (not just all deduped).
    let mut symbol_lane_had_hits = false;

    // Seen set for memory dedup — preserves insertion order (symbol-bound first, then FTS,
    // then path-bound), unlike the old sort+dedup which ordered by creation-time ID.
    let mut seen_memory_ids: HashSet<String> = HashSet::new();
    // Within-call dedup of symbol hits by (path, qualified_name): `symbol::lookup` can return the
    // same logical symbol once per concrete row (overloads, multiple definitions, re-export rows),
    // which otherwise renders as N identical "Known symbols" lines — same defect as the lexical
    // lane (#139).
    let mut seen_symbol_keys: HashSet<String> = HashSet::new();

    if let Some(ident) = extract_symbol_identifier(&normalized) {
        // Symbol lane. Bare name for qualified queries: `Watcher::spawn` → `spawn`.
        let bare = ident.rsplit([':', '.']).next().unwrap_or(ident);
        for hit in symbol::lookup(conn, bare, None, MAX_SYMBOLS)? {
            symbol_lane_had_hits = true;
            let key = format!("{}:{}", hit.path, hit.qualified_name);
            if dedupe.symbol_keys.contains(&key) || !seen_symbol_keys.insert(key.clone()) {
                continue;
            }
            let (callers, callees) = edge_counts(conn, &hit)?;
            let start_line = line_for_symbol(conn, &hit)?;
            let line_suffix = match start_line {
                Some(l) => format!("{}:{}", hit.path, l),
                None => hit.path.clone(),
            };
            let rendered = format!(
                "- `{}` ({}) — {} — {} callers / {} callees{}",
                hit.qualified_name,
                hit.kind,
                line_suffix,
                callers,
                callees,
                hit.signature.as_deref().map(|s| format!(" — `{s}`")).unwrap_or_default(),
            );
            // Gather symbol-bound memories before adding them to the main list so they
            // come first (highest priority lane).
            for m in memory::memories_for_symbol(conn, &hit, MAX_MEMORIES)? {
                if seen_memory_ids.insert(m.memory_id.clone()) {
                    memories.push(m);
                }
            }
            symbol_items.push(SymbolItem { rendered, key });
        }
    }

    // Memory lane: always. FTS over the normalized pattern + path-bound memories. The FTS half is
    // relevance-gated (a corpus-wide token in the pattern otherwise drags in MAX_MEMORIES
    // unrelated memories); the path half is not — it is a structural binding, not a text match.
    // The gate runs over the FULL hit set, BEFORE session dedupe (the blanket retain below):
    // relevance is a property of the query, not of what this session happened to show. Dropping an
    // already-seen hit first would hand the reference score to the runner-up, and the weak tail
    // would pass the gate for the rest of the resurface window — exactly the noise it removes.
    let fts_hits = memory::memory_search_scored(conn, &normalized, MAX_MEMORIES)?;
    for m in memories_above_relative_floor(fts_hits) {
        if seen_memory_ids.insert(m.memory_id.clone()) {
            memories.push(m);
        }
    }
    if let Some(path) = search_path {
        for m in memory::memories_for_path(conn, path, MAX_MEMORIES)? {
            if seen_memory_ids.insert(m.memory_id.clone()) {
                memories.push(m);
            }
        }
    }
    // Apply session-level dedupe filter last (after insertion-order dedup above).
    memories.retain(|m| !dedupe.memory_ids.contains(&m.memory_id));
    // The lexical lane hydrates through plain `memory_by_id`, so a memory that reached this list
    // by matching prose carries no drift verdict while the same memory reached by path or symbol
    // does. Mark the assembled list, so one rendered lane cannot present a drifted anchor as
    // current just because of how the pattern happened to find it.
    memory::mark_drive_by_drift(conn, &mut memories)?;
    // Honor `[memory] surface`: under `Summary` each memory renders its dream summary + verdict
    // marker (title-only fallback) instead of the clamped body — the hook context stays terse and
    // the full body is one `memory show` away.
    memory::apply_memory_surface(conn, &mut memories, surface)?;

    // Lexical lane: only when the symbol lane found nothing (never had any raw hits). Relevance
    // gate: keep only hits within LEXICAL_RELATIVE_FLOOR of the best hit's score, so the weak tail
    // (e.g. an incidental match several ranks down) isn't injected as noise.
    let lexical_lines = if !symbol_lane_had_hits {
        lexical_lines_from_hits(lexical::search_lexical_only(
            conn,
            &normalized,
            MAX_LEXICAL_HITS,
            false,
        )?)
    } else {
        Vec::new()
    };

    if memories.is_empty() && symbol_items.is_empty() && lexical_lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(render(memories, symbol_items, lexical_lines)))
}

/// Floor-filter, dedup, and render the lexical-lane hits. Extracted so the dedup is unit-testable:
/// `search_lexical_only` can return the same chunk more than once (e.g. one row per matched FTS
/// term), which — capped at `MAX_LEXICAL_HITS` — otherwise rendered as N identical "Indexed hits"
/// lines (#139). Keeps the first occurrence of each `(path, start, end)`, preserving rank order,
/// after the relevance floor.
fn lexical_lines_from_hits(hits: Vec<lexical::SearchHit>) -> Vec<String> {
    let best = hits.iter().map(|hit| hit.score).fold(0.0_f64, f64::max);
    let floor = best * LEXICAL_RELATIVE_FLOOR;
    let mut seen: HashSet<(String, i64, i64)> = HashSet::new();
    hits.into_iter()
        .filter(|hit| hit.score >= floor)
        .filter(|hit| seen.insert((hit.path.clone(), hit.start_line, hit.end_line)))
        .map(|hit| format!("- {}:{}-{} — {}", hit.path, hit.start_line, hit.end_line, hit.summary))
        .collect()
}

/// Drop the weak tail of the FTS memory lane: keep only hits within [`MEMORY_RELATIVE_FLOOR`] of
/// the best hit's match strength.
///
/// INVARIANT: the paired score is SQLite's `bm25()`, which is NEGATIVE and lower-is-better — the
/// OPPOSITE sign convention from the lexical lane's positive scores. It is negated into a
/// higher-is-better strength before any comparison; filtering on the raw bm25 value would invert
/// the gate and keep exactly the irrelevant memories this drops. Strength is therefore never
/// negative, so the floor never rises above the best hit and a lone match always survives, however
/// weak in absolute terms.
fn memories_above_relative_floor(hits: Vec<(memory::RepoMemory, f64)>) -> Vec<memory::RepoMemory> {
    let best = hits.iter().map(|(_, bm25)| -bm25).fold(f64::NEG_INFINITY, f64::max);
    let floor = best * MEMORY_RELATIVE_FLOOR;
    hits.into_iter().filter(|(_, bm25)| -bm25 >= floor).map(|(m, _)| m).collect()
}

/// A single rendered symbol line plus the key that identifies it in the dedupe set.
struct SymbolItem {
    rendered: String,
    key: String,
}

/// A single renderable item in a section, with optional bookkeeping IDs. Shared with `read_augment`
/// via [`pack_sections`].
pub(crate) struct RenderItem {
    pub(crate) line: String,
    pub(crate) memory_id: Option<String>,
    pub(crate) symbol_key: Option<String>,
}

/// A section is a header line + a list of items. Header is only committed when at least one
/// item fits; the caller's ID is only appended to the output IDs when the item's line lands. Shared
/// with `read_augment` via [`pack_sections`].
pub(crate) struct Section {
    pub(crate) header: String,
    pub(crate) items: Vec<RenderItem>,
    /// An optional closing/footer line (not associated with an ID).
    pub(crate) footer: Option<String>,
}

/// Build the shared memory `RenderItem` (`- [Kind | status] title — gist verdict (rag-rat:
/// memory_search)`), so grep- and read-augment render bound memories identically. The gist is the
/// dream summary under `surface = "summary"`, else the body; a body the summary surface withheld is
/// a pointer rather than prose, so it renders title-only. Every source is clamped to the same
/// per-line budget: a digest line costs the same whichever slot its prose arrived in.
pub(crate) fn memory_render_item(m: memory::RepoMemory) -> RenderItem {
    let gist = match &m.summary {
        Some(summary) => clamp_body(summary),
        None if memory::body_is_elided(&m) => String::new(),
        None => clamp_body(&m.body),
    };
    let gist_part = if gist.is_empty() { String::new() } else { format!(" — {gist}") };
    let verdict_part = m.verdict.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
    // A synced memory anchored to text this checkout no longer holds reads in the status slot,
    // which is where a reader looks to decide how far to trust the line. These surfaces render a
    // raw list and never partition it, so without this the divergence would be computed and then
    // dropped on the way out.
    let status = if m.synced_anchor_drifted {
        format!("{} · anchor drifted", m.status)
    } else {
        m.status.clone()
    };
    RenderItem {
        line: format!(
            "- [{} | {}] {}{}{} (rag-rat: memory_search)",
            m.kind, status, m.title, gist_part, verdict_part,
        ),
        memory_id: Some(m.memory_id),
        symbol_key: None,
    }
}

/// Collapse all whitespace runs (including newlines) to single spaces and truncate to
/// `MAX_MEMORY_BODY_CHARS`, appending `…` when truncated.
pub(crate) fn clamp_body(body: &str) -> String {
    let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    // Compare char count (not byte length) so multibyte bodies don't get `…` with nothing removed.
    if collapsed.chars().count() <= MAX_MEMORY_BODY_CHARS {
        collapsed
    } else {
        // Truncate at exactly MAX_MEMORY_BODY_CHARS chars.
        let truncated: String = collapsed.chars().take(MAX_MEMORY_BODY_CHARS).collect();
        format!("{truncated}…")
    }
}

/// Memories first (the unique signal), then symbols, then lexical hits; whole-item truncation
/// against `MAX_CONTEXT_CHARS`. Section headers are committed ONLY together with their first
/// fitting item. IDs are appended to the returned vecs ONLY when their item line lands.
fn render(
    memories: Vec<memory::RepoMemory>,
    symbol_items: Vec<SymbolItem>,
    lexical_lines: Vec<String>,
) -> GrepAugment {
    let mut sections: Vec<Section> = Vec::new();

    if !memories.is_empty() {
        let items = memories.into_iter().map(memory_render_item).collect();
        sections.push(Section {
            header: "**Repo memories bound to this code:**".to_string(),
            items,
            footer: None,
        });
    }

    if !symbol_items.is_empty() {
        let items = symbol_items
            .into_iter()
            .map(|s| RenderItem { line: s.rendered, memory_id: None, symbol_key: Some(s.key) })
            .collect();
        sections.push(Section {
            header: "**Known symbols matching this pattern:**".to_string(),
            items,
            footer: Some("(rag-rat: impact_surface <name> before editing)".to_string()),
        });
    }

    if !lexical_lines.is_empty() {
        let items = lexical_lines
            .into_iter()
            .map(|line| RenderItem { line, memory_id: None, symbol_key: None })
            .collect();
        sections.push(Section {
            header: "**Indexed hits (rag-rat semantic_search has more):**".to_string(),
            items,
            footer: None,
        });
    }

    pack_sections("rag-rat index context for this search:", sections)
}

/// Pack `sections` under `intro` into a char-budget-bounded digest (whole-item truncation against
/// [`MAX_CONTEXT_CHARS`]; a section header is committed only with its first fitting item; an item's
/// bookkeeping id is recorded only when its line lands). Shared by grep- and read-augment so both
/// obey the same budget and dedup-bookkeeping rules.
pub(crate) fn pack_sections(intro: &str, sections: Vec<Section>) -> GrepAugment {
    let mut context = format!("{intro}\n");
    let mut memory_ids: Vec<String> = Vec::new();
    let mut symbol_keys: Vec<String> = Vec::new();

    'section: for section in sections {
        // We only know if the header fits once we find the first fitting item.
        // Speculatively account for: header + '\n' + first item + '\n'.
        let mut section_committed = false;

        for item in section.items {
            // Space needed: item line + newline. If the section header hasn't been
            // committed yet, include it too.
            let needed = if section_committed {
                item.line.len() + 1
            } else {
                section.header.len() + 1 + item.line.len() + 1
            };

            if context.len() + needed > MAX_CONTEXT_CHARS {
                // Whole-item truncation: stop at the first item that doesn't fit.
                break 'section;
            }

            if !section_committed {
                context.push_str(&section.header);
                context.push('\n');
                section_committed = true;
            }
            context.push_str(&item.line);
            context.push('\n');

            // Record IDs only for items whose lines actually landed.
            if let Some(mid) = item.memory_id {
                memory_ids.push(mid);
            }
            if let Some(key) = item.symbol_key {
                symbol_keys.push(key);
            }
        }

        // Footer is best-effort: append only if section was committed and it fits.
        if section_committed
            && let Some(footer) = section.footer
            && context.len() + footer.len() < MAX_CONTEXT_CHARS
        {
            context.push_str(&footer);
            context.push('\n');
        }
    }

    GrepAugment { context: context.trim_end().to_string(), memory_ids, symbol_keys }
}

/// Caller/callee edge counts. Callers resolve by `to_symbol_id` or qualified-name match;
/// callees are edges leaving any of the symbol's concrete rows.
pub(crate) fn edge_counts(
    conn: &Connection,
    hit: &symbol::SymbolHit,
) -> anyhow::Result<(i64, i64)> {
    // GENERATION-SCOPED via the `files` view (batch 6, count-scoping class; `compose` installs the
    // worktree scope view before calling in). The `to_symbol_id = ?1` arm keys on a LIVE rowid, but
    // the interned-name arm matches callers purely by NAME and so double-counts dead-generation
    // edges during a dead-generation window, inflating the "{N} callers" line.
    // #692: the name arm compares the raw `target_qualified_name_id` against an interned-id lookup,
    // not the value-joined `target_qualified_name`, so the planner drives idx_edges_to_symbol +
    // idx_edges_target_qname (a MULTI-INDEX OR) instead of full-scanning edges_data — this count
    // runs on every grep-augmented hit. Same matching semantics; same class as #682.
    let callers: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges
         JOIN files source_files ON source_files.id = edges.source_file_id
         WHERE edges.to_symbol_id = ?1
            OR edges.target_qualified_name_id = (SELECT id FROM name_strings WHERE value = ?2)",
        rusqlite::params![hit.symbol_id, hit.qualified_name],
        |row| row.get(0),
    )?;
    let callees: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE from_symbol_id = ?1",
        [hit.symbol_id],
        |row| row.get(0),
    )?;
    Ok((callers, callees))
}

/// Start line for a symbol hit (line spans live on chunks).
/// Returns `None` when no matching chunk is found; callers render `{path}` without `:{line}`
/// rather than a confidently-wrong `:1`.
pub(crate) fn line_for_symbol(
    conn: &Connection,
    hit: &symbol::SymbolHit,
) -> anyhow::Result<Option<i64>> {
    conn.query_row(
        "SELECT start_line FROM chunks
         WHERE file_id = ?1 AND start_byte <= ?2 AND end_byte >= ?2
         ORDER BY (end_byte - start_byte) ASC LIMIT 1",
        rusqlite::params![hit.file_id, hit.start_byte],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rag_rat_db::schema;
    use rag_rat_query::SearchHit;
    use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
    use rusqlite::Connection;

    use super::*;

    fn lexical_hit(path: &str, start: i64, end: i64, score: f64) -> SearchHit {
        SearchHit {
            chunk_id: 0,
            path: path.to_string(),
            language: "rust".to_string(),
            kind: "chunk".to_string(),
            start_line: start,
            end_line: end,
            symbol_path: None,
            score,
            retrieval_mode: "lexical".to_string(),
            summary: format!("{path} summary"),
            graph: None,
            score_components: None,
            importance: None,
            distilled_records: Vec::new(),
        }
    }

    /// #139: the same chunk returned more than once (capped at MAX_LEXICAL_HITS) rendered as N
    /// identical "Indexed hits" lines. The dedup keeps one line per (path, start, end); the
    /// relevance floor still drops weak hits.
    #[test]
    fn lexical_lines_dedup_chunks_and_apply_floor() {
        let hits = vec![
            lexical_hit("a.rs", 1, 9, 1.0),
            lexical_hit("a.rs", 1, 9, 1.0), // exact duplicate chunk
            lexical_hit("a.rs", 1, 9, 1.0), // and again — would have filled all 3 slots
            lexical_hit("b.rs", 2, 3, 0.9), // distinct, above floor (0.6 * 1.0)
            lexical_hit("c.rs", 4, 5, 0.1), // below floor → dropped
        ];
        let lines = lexical_lines_from_hits(hits);
        assert_eq!(lines.len(), 2, "a.rs deduped to one, c.rs floored out: {lines:?}");
        assert!(lines[0].contains("a.rs:1-9"), "first line is a.rs once: {lines:?}");
        assert!(lines[1].contains("b.rs:2-3"), "second is b.rs: {lines:?}");
        assert!(!lines.iter().any(|l| l.contains("c.rs")), "weak hit dropped: {lines:?}");
    }

    /// #139 (symbol lane): two symbol rows sharing (path, qualified_name) — overloads / cfg
    /// variants / re-export rows — must render once, not once per row.
    #[test]
    fn symbol_lane_dedups_duplicate_rows() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/a.rs', 'rust', 'source', 'h', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('a::foo')", []).unwrap();
        for _ in 0..3 {
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', 'foo', (SELECT id FROM name_strings WHERE value = 'a::foo'),
                         'function', 0, 10, 'fn foo()', NULL)",
                [],
            )
            .unwrap();
        }
        let out = compose(
            &conn,
            "foo",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("symbol lane augments");
        assert_eq!(
            out.context.matches("`a::foo`").count(),
            1,
            "duplicate symbol rows must render once:\n{}",
            out.context
        );
    }

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('watch::watcher_main')", [
        ])
        .unwrap();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                 end_byte, signature, docs)
             VALUES (1, 'rust', 'watcher_main',
                     (SELECT id FROM name_strings WHERE value = 'watch::watcher_main'),
                     'function', 0, 100, 'fn watcher_main(config: Config)', NULL)",
            [],
        )
        .unwrap();
        let chunk_text = "fn watcher_main() { /* election retry loop */ }";
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text_hash)
             VALUES (1, 'symbol', 'watch::watcher_main', 0, 100, 1, 20, 'h1')",
            [],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
        // chunks.text is gone (#77 Phase 2): seed the compressed chunk_text blob (readers INNER
        // JOIN it) and the contentless chunk_fts tokens.
        rag_rat_db::chunk_text_store::seed_chunk_text(&conn, chunk_id, chunk_text).unwrap();
        conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", rusqlite::params![
            chunk_id, chunk_text
        ])
        .unwrap();
        // One caller edge and one callee edge for the counts line.
        conn.execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence)
             VALUES (1, NULL, 1, 'watcher_main', 'watch::watcher_main', 'calls_name', 'exact')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence)
             VALUES (1, 1, NULL, 'maintenance_pass', NULL, 'calls_name', 'name_only')",
            [],
        )
        .unwrap();
        crate::memory_write::create_memory(&conn, RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "One watcher per worktree".to_string(),
            body: "The election lock guarantees a single watcher; never bind without it."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            payload_json: None,
            bind: RepoMemoryBindTarget {
                symbol_id: Some(1),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                tracker: None,
                project: None,
                item_key: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
        // Sync chunk_fts directly — external-content FTS5 needs explicit INSERT.
        conn.execute(
            "INSERT INTO chunk_fts(rowid, text)
             VALUES (?1, 'fn watcher_main() { /* election retry loop */ }')",
            [chunk_id],
        )
        .unwrap();
        conn
    }

    /// A memory reachable ONLY through the FTS lane: bound to the seeded file's path, which
    /// `compose` never consults here because these tests pass `search_path: None`.
    #[test]
    fn a_drifted_memory_renders_its_drift_in_the_status_slot() {
        // grep- and read-augment render a raw list without partitioning it, so the status slot is
        // the only place a reader learns the anchor moved on. Kept next to the persisted status
        // rather than replacing it: the row really is active, and this says what is untrustworthy.
        let memory = memory::RepoMemory {
            memory_id: "m1".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            summary: None,
            verdict: None,
            confidence: "high".to_string(),
            status: "active".to_string(),
            created_by: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            source: "agent".to_string(),
            payload_json: None,
            source_text_hash: None,
            input_hash: None,
            memory_version: "v1".to_string(),
            synced_anchor_drifted: true,
            bindings: Vec::new(),
            call_paths: Vec::new(),
            tags: Vec::new(),
        };
        let rendered = memory_render_item(memory).line;
        assert!(
            rendered.contains("anchor drifted"),
            "the drift must reach the rendered line: {rendered}"
        );
    }

    #[test]
    fn a_memory_found_only_by_prose_still_renders_its_anchor_drift() {
        // The lexical lane hydrates through plain `memory_by_id`, so a memory the pattern reaches
        // by prose alone arrives unmarked. Rendering it beside the path/symbol lanes would present
        // the same memory as current or drifted purely by how it was found.
        let conn = seeded_conn();
        let memory = seed_fts_memory(
            &conn,
            "Zebraglyph routing pins quokkaform",
            "zebraglyph quokkaform lorikeetwise — the zebraglyph router pins quokkaform.",
        );
        // Make it a synced memory whose stamped text this checkout no longer holds. The pattern
        // below matches its prose only: nothing names `src/watch.rs` or a symbol in it.
        conn.execute(
            "UPDATE repo_memories SET origin = 'synced', source_text_hash = 'stamped-then' WHERE \
             id = ?1",
            rusqlite::params![memory.memory_id],
        )
        .unwrap();

        let out = compose(
            &conn,
            "zebraglyph quokkaform lorikeetwise",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");
        assert!(
            out.context.contains("Zebraglyph routing pins quokkaform"),
            "the prose match surfaces: {}",
            out.context
        );
        assert!(
            out.context.contains("anchor drifted"),
            "and carries its drift, though no drive-by reader hydrated it: {}",
            out.context
        );
    }

    fn seed_fts_memory(conn: &Connection, title: &str, body: &str) -> memory::RepoMemory {
        crate::memory_write::create_memory(conn, RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            payload_json: None,
            bind: RepoMemoryBindTarget {
                path: Some("src/watch.rs".to_string()),
                ..RepoMemoryBindTarget::default()
            },
        })
        .unwrap()
        .memory
    }

    /// #1200: the FTS memory lane had no relevance gate, so one common token in the normalized
    /// pattern surfaced up to `MAX_MEMORIES` unrelated memories. The magnitudes are the ones
    /// `compose_memory_lane_drops_weak_fts_co_matches` actually produces — the co-match's only
    /// token is corpus-wide, so fts5 clamps its idf to 1e-6. bm25 is NEGATIVE and lower-is-better,
    /// so a sign slip here would keep exactly the memory this drops.
    #[test]
    fn memory_floor_keeps_the_strong_match_and_drops_the_weak_tail() {
        let conn = seeded_conn();
        let strong = seed_fts_memory(&conn, "Strong bm25 match", "matches every query term");
        let weak = seed_fts_memory(&conn, "Weak bm25 match", "matches one corpus-wide term");
        let kept = memories_above_relative_floor(vec![(strong, -1.63), (weak, -1.02e-6)]);
        assert_eq!(kept.len(), 1, "only the strong match survives: {kept:?}");
        assert_eq!(kept[0].title, "Strong bm25 match");
    }

    /// The floor's VALUE is load-bearing, not just its existence: two orders of magnitude below the
    /// best hit, so the tf/body-length spread between equally-relevant matches never reaches it.
    #[test]
    fn memory_floor_cuts_two_orders_of_magnitude_below_the_best_hit() {
        let conn = seeded_conn();
        let best = seed_fts_memory(&conn, "Best bm25 match", "the strongest match");
        let above = seed_fts_memory(&conn, "Just above the floor", "same terms, a longer body");
        let below = seed_fts_memory(&conn, "Just below the floor", "an incidental co-match");
        let kept = memories_above_relative_floor(vec![(best.clone(), -10.0), (above, -0.11)]);
        assert_eq!(kept.len(), 2, "0.011 of the best hit survives: {kept:?}");
        let kept = memories_above_relative_floor(vec![(best, -10.0), (below, -0.09)]);
        assert_eq!(kept.len(), 1, "0.009 of the best hit is dropped: {kept:?}");
    }

    /// The false-drop guard on the constant. These are the magnitudes measured on a 43-memory
    /// corpus for the 3-term query `zebraglyph quokkaform rebuildpass`: the best hit is a short
    /// all-terms memory, the second is an all-terms memory whose ~1 300-char body dilutes it to
    /// 0.098 of the best. A floor anywhere near a tenth of the best hit discards that real match,
    /// which is worse than the noise a tighter floor would remove.
    #[test]
    fn memory_floor_keeps_a_length_diluted_exact_match() {
        let conn = seeded_conn();
        let best = seed_fts_memory(&conn, "Short exact match", "every query term, briefly");
        let diluted =
            seed_fts_memory(&conn, "Long-bodied exact match", "every query term, at length");
        let kept = memories_above_relative_floor(vec![(best, -10.005), (diluted, -0.981)]);
        assert_eq!(kept.len(), 2, "body length must not disqualify an exact match: {kept:?}");
    }

    /// The gate is a RELATIVE floor, never a minimum score: the best hit is always its own
    /// reference, so a lone match surfaces however weak it is in absolute terms.
    #[test]
    fn memory_floor_keeps_a_lone_hit_however_weak() {
        let conn = seeded_conn();
        let weak = seed_fts_memory(&conn, "Weak bm25 match", "matches one incidental term");
        assert_eq!(memories_above_relative_floor(vec![(weak, -0.01)]).len(), 1);
    }

    /// End-to-end: a pattern whose terms one memory answers fully and another merely brushes
    /// injects only the former.
    #[test]
    fn compose_memory_lane_drops_weak_fts_co_matches() {
        let conn = seeded_conn();
        seed_fts_memory(
            &conn,
            "Zebraglyph routing pins quokkaform",
            "zebraglyph quokkaform lorikeetwise — the zebraglyph router pins quokkaform to \
             lorikeetwise on every zebraglyph rebuild.",
        );
        seed_fts_memory(
            &conn,
            "Unrelated cache eviction note",
            "The cache evicts on write; it happens to mention lorikeetwise once.",
        );
        let out = compose(
            &conn,
            "zebraglyph quokkaform lorikeetwise",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");
        assert!(
            out.context.contains("Zebraglyph routing pins quokkaform"),
            "the strong match surfaces: {}",
            out.context
        );
        assert!(
            !out.context.contains("Unrelated cache eviction note"),
            "the weak co-match is floored out: {}",
            out.context
        );
    }

    /// End-to-end in the regime the two small-corpus tests never reach: a shared token present in
    /// 10 of 43 memories keeps a POSITIVE idf, so the co-matches land at ~0.13 of the best hit
    /// rather than the ~1e-6 a corpus-wide token clamps to. The gate is scoped to the clamped case
    /// and deliberately does not fire here — bm25 magnitude cannot separate these co-matches from
    /// a length-diluted exact match, so `MAX_MEMORIES` is what bounds them. This pins that scope:
    /// a future tightening that starts dropping hits here is dropping real matches with them.
    #[test]
    fn compose_memory_lane_gate_is_scoped_to_corpus_wide_tokens() {
        let conn = seeded_conn();
        seed_fts_memory(
            &conn,
            "Zebraglyph routing pins quokkaform",
            "zebraglyph quokkaform rebuildpass — the zebraglyph router pins quokkaform on every \
             rebuildpass.",
        );
        for i in 0..10 {
            seed_fts_memory(
                &conn,
                &format!("Rebuildpass note {i}"),
                "The rebuildpass drains the queue before the next tick; unrelated to routing.",
            );
        }
        for i in 0..30 {
            seed_fts_memory(&conn, &format!("Filler note {i}"), "Nothing relevant lives here.");
        }
        let out = compose(
            &conn,
            "zebraglyph quokkaform rebuildpass",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");
        assert!(
            out.context.contains("Zebraglyph routing pins quokkaform"),
            "the all-terms match surfaces: {}",
            out.context
        );
        assert!(
            out.context.contains("Rebuildpass note"),
            "a positive-idf co-match is NOT floored out — only the cap bounds it: {}",
            out.context
        );
    }

    /// End-to-end counterpart: the same weak memory is the ONLY hit for its own rare token, so it
    /// still surfaces — proof the floor stayed relative rather than becoming an absolute gate.
    #[test]
    fn compose_memory_lane_surfaces_a_lone_weak_match() {
        let conn = seeded_conn();
        seed_fts_memory(
            &conn,
            "Unrelated cache eviction note",
            "The cache evicts on write; it happens to mention kestrelmark once.",
        );
        let out = compose(
            &conn,
            "kestrelmark",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");
        assert!(
            out.context.contains("Unrelated cache eviction note"),
            "a lone weak match still surfaces: {}",
            out.context
        );
    }

    /// Session dedupe must not move the floor: the reference is the best hit for the QUERY, so the
    /// weak co-match stays floored out even while the strong hit is suppressed as already-seen.
    /// Gating the survivors instead would re-admit the weak tail for the whole resurface window.
    #[test]
    fn memory_floor_ignores_session_dedupe() {
        let conn = seeded_conn();
        let strong = seed_fts_memory(
            &conn,
            "Zebraglyph routing pins quokkaform",
            "zebraglyph quokkaform lorikeetwise — the zebraglyph router pins quokkaform to \
             lorikeetwise on every zebraglyph rebuild.",
        );
        seed_fts_memory(
            &conn,
            "Unrelated cache eviction note",
            "The cache evicts on write; it happens to mention lorikeetwise once.",
        );
        let dedupe = DedupeFilter {
            memory_ids: HashSet::from([strong.memory_id.clone()]),
            symbol_keys: HashSet::new(),
        };
        let out = compose(
            &conn,
            "zebraglyph quokkaform lorikeetwise",
            None,
            &dedupe,
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap();
        // Nothing else in the seeded corpus answers this pattern, so suppressing both the
        // already-seen hit and the floored co-match legitimately leaves no payload at all.
        let context = out.map(|payload| payload.context).unwrap_or_default();
        assert!(
            !context.contains("Unrelated cache eviction note"),
            "the weak co-match stays floored out while the strong hit is deduped: {context}"
        );
    }

    #[test]
    fn compose_identifier_pattern_yields_symbol_and_memory() {
        let conn = seeded_conn();
        let out = compose(
            &conn,
            r"watcher_main\b",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");
        assert!(out.context.contains("src/watch.rs"), "symbol location present");
        assert!(out.context.contains("One watcher per worktree"), "memory title present");
        let memory_pos = out.context.find("One watcher per worktree").unwrap();
        let symbol_pos = out.context.find("src/watch.rs").unwrap();
        assert!(memory_pos < symbol_pos, "memories render before symbols");
        assert_eq!(out.memory_ids.len(), 1);
        assert_eq!(out.symbol_keys.len(), 1);
        assert!(out.context.len() <= MAX_CONTEXT_CHARS);
    }

    #[test]
    fn compose_summary_surface_renders_the_summary_and_verdict_not_the_full_body() {
        let conn = seeded_conn();
        // Seed a dream summary + verdict for the seeded memory, keyed on its id, repo scope, and
        // current content_hash / prompt versions — exactly what the surfacing hydrator gates on.
        let (id, repo_id): (String, String) = conn
            .query_row("SELECT id, repo_id FROM repo_memories LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let title = "One watcher per worktree";
        let body = "The election lock guarantees a single watcher; never bind without it.";
        conn.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
             prompt_version, generated_at_ms) VALUES (?1,?2,?3,?4,?5,0)",
            rusqlite::params![
                id,
                repo_id,
                rag_rat_query::memory::evidence::note_content_hash(title, body),
                "Election lock ensures exactly one watcher; bind only under it.",
                rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION
            ],
        )
        .unwrap();
        let inputs = rag_rat_query::memory::evidence::checked_inputs_hash(
            &conn,
            &id,
            &Some(repo_id.clone()),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
             checked_against_commit, checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
             (?1,?2,?3,'diverged',NULL,?4,?5,0)",
            rusqlite::params![
                id,
                repo_id,
                rag_rat_query::memory::evidence::note_content_hash(title, body),
                inputs,
                rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION
            ],
        )
        .unwrap();

        let out = compose(
            &conn,
            r"watcher_main\b",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Summary,
        )
        .unwrap()
        .expect("payload expected");
        assert!(
            out.context.contains("Election lock ensures exactly one watcher"),
            "the summary renders in place of the body: {}",
            out.context
        );
        assert!(
            !out.context.contains("never bind without it"),
            "the full body is deferred under summary: {}",
            out.context
        );
        assert!(out.context.contains("diverged"), "the verdict marker renders: {}", out.context);
        assert!(out.context.contains(title), "the title still renders: {}", out.context);
    }

    /// The default surface with NO summary rows — dream disabled (the default), never run, or every
    /// summary invalidated by a prompt-version bump. The digest gives a memory ONE line, so it has
    /// one prose slot: a body the surface withheld is a pointer to `memory_show`, and spending that
    /// slot on the marker costs the budget of a real gist to say nothing.
    #[test]
    fn compose_summary_surface_shows_a_short_body_and_renders_a_deferred_one_title_only() {
        let conn = seeded_conn();
        // A second memory on the same symbol, one word OVER the summary envelope, so the surface
        // defers its body instead of showing it whole.
        let long_body =
            vec!["padding"; rag_rat_query::memory::evidence::SUMMARY_MAX_WORDS + 1].join(" ");
        crate::memory_write::create_memory(&conn, RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Deferred until compaction runs".to_string(),
            body: long_body,
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            payload_json: None,
            bind: RepoMemoryBindTarget { symbol_id: Some(1), ..Default::default() },
        })
        .unwrap();

        let out = compose(
            &conn,
            r"watcher_main\b",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Summary,
        )
        .unwrap()
        .expect("payload expected");
        assert!(
            out.context.contains("The election lock guarantees a single watcher"),
            "a note inside the envelope will never be summarized, so its body is its gist: {}",
            out.context
        );
        assert!(
            out.context.contains("Deferred until compaction runs"),
            "the deferred memory still renders its title: {}",
            out.context
        );
        assert!(
            !out.context.contains("body elided"),
            "the elision marker is a pointer, never a gist: {}",
            out.context
        );
        assert!(
            !out.context.contains("padding"),
            "the deferred body itself never renders: {}",
            out.context
        );
    }

    /// A summary is bounded in WORDS (150), which is worth ~1 kB — six times the per-line gist
    /// budget the hook renders bodies under. Both sources are prose in the same slot, so both are
    /// clamped: the digest costs the same whichever one filled it.
    #[test]
    fn compose_summary_surface_clamps_a_long_summary_to_the_line_budget() {
        let conn = seeded_conn();
        let (id, repo_id): (String, String) = conn
            .query_row("SELECT id, repo_id FROM repo_memories LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let title = "One watcher per worktree";
        let body = "The election lock guarantees a single watcher; never bind without it.";
        let long_summary = format!("Election lock first. {} Tailmarker.", "filler ".repeat(60));
        assert!(long_summary.chars().count() > MAX_MEMORY_BODY_CHARS, "the clamp must engage");
        conn.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
             prompt_version, generated_at_ms) VALUES (?1,?2,?3,?4,?5,0)",
            rusqlite::params![
                id,
                repo_id,
                rag_rat_query::memory::evidence::note_content_hash(title, body),
                long_summary,
                rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION
            ],
        )
        .unwrap();

        let out = compose(
            &conn,
            r"watcher_main\b",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Summary,
        )
        .unwrap()
        .expect("payload expected");
        assert!(out.context.contains("Election lock first."), "the head renders: {}", out.context);
        assert!(
            !out.context.contains("Tailmarker"),
            "the tail past the clamp does not: {}",
            out.context
        );
        assert!(out.context.contains('…'), "the truncation is marked: {}", out.context);
    }

    #[test]
    fn compose_respects_dedupe_filter_and_returns_none_when_everything_filtered() {
        let conn = seeded_conn();
        let first = compose(
            &conn,
            "watcher_main",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("first payload");
        let filter = DedupeFilter {
            memory_ids: first.memory_ids.iter().cloned().collect::<HashSet<_>>(),
            symbol_keys: first.symbol_keys.iter().cloned().collect::<HashSet<_>>(),
        };
        assert!(
            compose(
                &conn,
                "watcher_main",
                None,
                &filter,
                rag_rat_base::config::MemorySurface::Full
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn extract_symbol_identifier_handles_definition_patterns() {
        // Lone identifier passes through.
        assert_eq!(extract_symbol_identifier("watcher_main"), Some("watcher_main"));
        // Definition keywords are stripped, leaving the one target identifier.
        assert_eq!(extract_symbol_identifier("fn watcher_main"), Some("watcher_main"));
        assert_eq!(extract_symbol_identifier("pub struct SymbolIndex"), Some("SymbolIndex"));
        assert_eq!(
            extract_symbol_identifier("pub async fn resolve_all_edges"),
            Some("resolve_all_edges")
        );
        // Two real identifiers → ambiguous → lexical lane.
        assert_eq!(extract_symbol_identifier("election retry loop"), None);
        // Free text token (not keyword, not identifier-shaped) → lexical lane.
        assert_eq!(extract_symbol_identifier("foo == bar"), None);
    }

    /// Swift's declaration keywords reach the symbol lane like every other language's. Before they
    /// were known keywords, `protocol Fetcher` read as two identifiers and fell to the lexical lane
    /// — even though `func Fetcher` did not, which is the tell that the list, not the pattern, was
    /// wrong.
    #[test]
    fn extract_symbol_identifier_handles_swift_definition_patterns() {
        assert_eq!(extract_symbol_identifier("protocol Fetcher"), Some("Fetcher"));
        assert_eq!(extract_symbol_identifier("actor Store"), Some("Store"));
        assert_eq!(extract_symbol_identifier("extension Client"), Some("Client"));
        assert_eq!(extract_symbol_identifier("public actor SyncStore"), Some("SyncStore"));
        assert_eq!(extract_symbol_identifier("mutating func reset"), Some("reset"));
        assert_eq!(extract_symbol_identifier("open class ViewModel"), Some("ViewModel"));
        assert_eq!(extract_symbol_identifier("macro stringify"), Some("stringify"));
    }

    #[test]
    fn compose_definition_pattern_routes_to_symbol_lane_not_lexical() {
        let conn = seeded_conn();
        let out = compose(
            &conn,
            r"fn watcher_main",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");
        // Resolves to the symbol + its bound memory; the redundant lexical echo is suppressed.
        assert!(out.context.contains("watch::watcher_main"), "symbol lane fired");
        assert!(out.context.contains("One watcher per worktree"), "bound memory surfaced");
        assert!(
            !out.context.contains("Indexed hits"),
            "lexical lane must be suppressed when the symbol lane has hits: {}",
            out.context
        );
        assert!(!out.symbol_keys.is_empty());
    }

    #[test]
    fn compose_non_identifier_pattern_uses_lexical_lane() {
        let conn = seeded_conn();
        let out = compose(
            &conn,
            "election retry loop",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("lexical payload");
        assert!(out.context.contains("src/watch.rs"));
    }

    #[test]
    fn compose_unknown_pattern_yields_none() {
        let conn = seeded_conn();
        assert!(
            compose(
                &conn,
                "zzqqyyxx_nothing",
                None,
                &DedupeFilter::default(),
                rag_rat_base::config::MemorySurface::Full
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn normalize_strips_regex_metacharacters_and_anchors() {
        assert_eq!(normalize_pattern(r"^fn\s+watcher_main\b"), "fn watcher_main");
        assert_eq!(
            normalize_pattern(r"Watcher::spawn(_with_fleet)?"),
            "Watcher::spawn _with_fleet"
        );
        assert_eq!(normalize_pattern("plain words"), "plain words");
        assert_eq!(normalize_pattern(r".*[]()|+?^$\\"), "");
    }

    #[test]
    fn normalize_preserves_dot_between_word_chars() {
        assert_eq!(normalize_pattern("foo.bar"), "foo.bar");
        assert_eq!(normalize_pattern(r"foo\.bar"), "foo.bar");
        // Leading/trailing dot is NOT between word chars → space.
        assert_eq!(normalize_pattern(".foo"), "foo");
        assert_eq!(normalize_pattern("foo."), "foo");
        // Dot between non-word chars → space.
        assert_eq!(normalize_pattern("foo. bar"), "foo bar");
    }

    #[test]
    fn identifier_candidate_accepts_identifier_shapes_only() {
        assert_eq!(identifier_candidate("watcher_main"), Some("watcher_main"));
        assert_eq!(identifier_candidate("Watcher::spawn"), Some("Watcher::spawn"));
        assert_eq!(identifier_candidate("foo.bar"), Some("foo.bar"));
        assert_eq!(identifier_candidate("fn watcher_main"), None); // two words
        assert_eq!(identifier_candidate("ab"), None); // too short
        assert_eq!(identifier_candidate("1abc"), None); // leading digit
        assert_eq!(identifier_candidate(""), None);
    }

    #[test]
    fn normalize_and_identifier_candidate_compose_for_dot_qualified() {
        // End-to-end: a grep pattern `foo.bar` reaches the symbol lane.
        let norm = normalize_pattern("foo.bar");
        assert_eq!(norm, "foo.bar");
        assert_eq!(identifier_candidate(&norm), Some("foo.bar"));

        // `r"foo\.bar"` (escaped) also reaches the symbol lane.
        let norm2 = normalize_pattern(r"foo\.bar");
        assert_eq!(norm2, "foo.bar");
        assert_eq!(identifier_candidate(&norm2), Some("foo.bar"));
    }

    #[test]
    fn render_truncation_respects_cap_no_dangling_headers_ids_match() {
        // ── Setup ──────────────────────────────────────────────────────────────────────
        // seeded_conn() already contains one memory ("One watcher per worktree") bound to
        // symbol_id=1.  We add FOUR more memories, each with a body long enough to survive
        // the 240-char clamp as a full 241-char string (body trimmed = 300 ASCII words ≈
        // 1499 chars, collapses to 1499 chars, clamped to exactly 240 chars + `…`).
        //
        // Each rendered memory line is:
        //   "- [Invariant | active] <title≈80chars> — <241-char-body>\n"
        //   ≈ 24 + 80 + 4 + 241 + 1 = ~350 chars
        //
        // With four such lines:
        //   preamble(41) + header(39) + 4×350(1400) = 1480 chars for memories alone.
        //
        // The symbol section header+line needs ~151 chars more → 1480+151 = 1631 > 1500.
        // Therefore the render loop MUST drop the symbol section entirely, giving us a
        // genuine truncation scenario.  We assert below that the candidate total exceeds
        // the cap so the test is self-verifying.
        let conn = seeded_conn();

        // Body: 300 distinct English words × ~5 chars = ~1499 chars → collapses to 1499
        // chars → clamped to 240 chars + `…`.  All ASCII so char count == byte count.
        let long_body: String =
            (0u32..300).map(|i| format!("word{i:04}")).collect::<Vec<_>>().join(" ");
        assert!(long_body.len() > MAX_MEMORY_BODY_CHARS, "body must survive clamp");
        assert!(long_body.len() < 4000, "must not exceed validation cap");

        // Titles are ~80 chars — recognizable and unique, long enough to push each rendered
        // line to ~350 chars.
        let titles = [
            "Truncation memory one — extra padding words fill the title field here ok",
            "Truncation memory two — extra padding words fill the title field here ok",
            "Truncation memory three — extra padding words fill the title field here",
            "Truncation memory four — extra padding words fill the title field here ok",
        ];

        let mut created_ids: Vec<String> = Vec::new();
        for title in &titles {
            let result = crate::memory_write::create_memory(&conn, RepoMemoryCreate {
                kind: "Invariant".to_string(),
                title: title.to_string(),
                body: long_body.clone(),
                confidence: "high".to_string(),
                created_by: Some("test".to_string()),
                source: None,
                tags: vec![],
                payload_json: None,
                bind: RepoMemoryBindTarget {
                    symbol_id: Some(1),
                    logical_symbol_id: None,
                    chunk_id: None,
                    edge_id: None,
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    tracker: None,
                    project: None,
                    item_key: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                    edge_path: None,
                    dir: None,
                },
            })
            .unwrap();
            created_ids.push(result.memory.memory_id);
        }
        assert_eq!(created_ids.len(), 4, "all four memories must be created");

        // ── Sanity-check: verify the cap path triggers ─────────────────────────────────
        // A single memory render line ≈ 350 chars (conservative lower bound: 24+70+4+241+1).
        // Four lines + preamble + mem-header = min ~1480 chars; symbol section adds ~151.
        // Assert total candidate content exceeds MAX_CONTEXT_CHARS so truncation is forced.
        let per_mem_line_min: usize = "- [Invariant | active] ".len()  // 24
            + titles[0].len()                                            // ≥70
            + " — ".len()                                                // 4
            + MAX_MEMORY_BODY_CHARS + 1; // 241 (clamped+…)
        let preamble_len = "rag-rat index context for this search:\n".len();
        let mem_header_len = "**Repo memories bound to this code:**\n".len();
        let symbol_section_min: usize = "**Known symbols matching this pattern:**\n".len() + 80; // header + short line
        let candidate_total =
            preamble_len + mem_header_len + 4 * per_mem_line_min + symbol_section_min;
        assert!(
            candidate_total > MAX_CONTEXT_CHARS,
            "candidate_total={candidate_total} must exceed MAX_CONTEXT_CHARS={MAX_CONTEXT_CHARS} \
             for truncation to trigger",
        );

        // ── Run compose ────────────────────────────────────────────────────────────────
        let out = compose(
            &conn,
            "watcher_main",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("payload expected");

        // (a) Context must not exceed the cap.
        assert!(
            out.context.len() <= MAX_CONTEXT_CHARS,
            "context.len()={} > MAX_CONTEXT_CHARS={}",
            out.context.len(),
            MAX_CONTEXT_CHARS,
        );

        // (b) No section header is the last line / every committed header is followed by
        //     at least one item line.
        let section_headers = [
            "**Repo memories bound to this code:**",
            "**Known symbols matching this pattern:**",
            "**Indexed hits (rag-rat semantic_search has more):**",
        ];
        let lines: Vec<&str> = out.context.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let is_header = section_headers.iter().any(|h| line.trim() == *h);
            if is_header {
                assert!(
                    idx + 1 < lines.len(),
                    "section header '{line}' is the last line — dangling header",
                );
            }
        }

        // (c) Exact two-way correspondence for every seeded memory:
        //     context.contains(title)  ⟺  memory_ids.contains(that_id)
        for (title, id) in titles.iter().zip(created_ids.iter()) {
            let in_context = out.context.contains(*title);
            let id_present = out.memory_ids.contains(id);
            assert_eq!(
                in_context, id_present,
                "mismatch for '{title}': in_context={in_context}, id_present={id_present}",
            );
        }

        // (d) Two-way correspondence for the symbol: symbol_keys non-empty ⟺
        //     "watch::watcher_main" appears in context.
        let sym_in_context = out.context.contains("watch::watcher_main");
        let sym_keys_non_empty = !out.symbol_keys.is_empty();
        assert_eq!(
            sym_in_context, sym_keys_non_empty,
            "symbol context/key mismatch: sym_in_context={sym_in_context}, \
             sym_keys_non_empty={sym_keys_non_empty}",
        );

        // (e) Truncation actually occurred: at least one seeded memory title OR the symbol
        //     must be absent from context (we have more content than the cap allows).
        let all_titles_present = titles.iter().all(|t| out.context.contains(*t));
        let symbol_present = out.context.contains("watch::watcher_main");
        assert!(
            !all_titles_present || !symbol_present,
            "no truncation detected: all memory titles and the symbol section all fit within \
             MAX_CONTEXT_CHARS — increase body/title size so the cap is actually exercised",
        );
    }

    #[test]
    fn clamp_body_truncates_long_bodies_and_collapses_whitespace() {
        let short = "hello world";
        assert_eq!(clamp_body(short), "hello world");

        // Whitespace collapse.
        let multiline = "line one\nline two\n  indented";
        assert_eq!(clamp_body(multiline), "line one line two indented");

        // Long body truncation.
        let long = "x".repeat(300);
        let clamped = clamp_body(&long);
        assert!(clamped.ends_with('…'), "truncated body must end with ellipsis");
        // The char count of the non-ellipsis prefix must be exactly MAX_MEMORY_BODY_CHARS.
        let without_ellipsis: String = clamped.chars().take(MAX_MEMORY_BODY_CHARS).collect();
        assert_eq!(without_ellipsis.len(), MAX_MEMORY_BODY_CHARS);
    }
}
