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
    let (symbol_items, symbol_bound, symbol_lane_had_hits) =
        symbol_lane(conn, &normalized, dedupe)?;
    let memories = memory_lane(conn, &normalized, search_path, dedupe, surface, symbol_bound)?;
    let lexical_lines =
        if symbol_lane_had_hits { Vec::new() } else { lexical_lane(conn, &normalized)? };

    if memories.is_empty() && symbol_items.is_empty() && lexical_lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(render(memories, symbol_items, lexical_lines)))
}

/// Symbol lane: runs only when the pattern targets one identifier. Returns the rendered symbols,
/// the memories bound to them (the highest-priority memories), and whether the lookup produced ANY
/// raw hits — counted before dedupe, because the lexical lane runs only when there were no symbol
/// hits at all, not merely when every hit was already shown.
fn symbol_lane(
    conn: &Connection,
    normalized: &str,
    dedupe: &DedupeFilter,
) -> anyhow::Result<(Vec<SymbolItem>, Vec<memory::RepoMemory>, bool)> {
    let mut symbol_items = Vec::new();
    let mut bound_memories = Vec::new();
    let mut had_hits = false;
    let Some(ident) = extract_symbol_identifier(normalized) else {
        return Ok((symbol_items, bound_memories, had_hits));
    };
    let mut seen_memory_ids = HashSet::new();
    // Within-call dedup of symbol hits by (path, qualified_name): `symbol::lookup` can return the
    // same logical symbol once per concrete row (overloads, multiple definitions, re-export rows),
    // which otherwise renders as N identical "Known symbols" lines — same defect as the lexical
    // lane (#139).
    let mut seen_symbol_keys: HashSet<String> = HashSet::new();
    // Bare name for qualified queries: `Watcher::spawn` → `spawn`.
    let bare = ident.rsplit([':', '.']).next().unwrap_or(ident);
    for hit in symbol::lookup(conn, bare, None, MAX_SYMBOLS)? {
        had_hits = true;
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
        extend_new_memories(
            &mut bound_memories,
            &mut seen_memory_ids,
            memory::memories_for_symbol(conn, &hit, MAX_MEMORIES)?,
        );
        symbol_items.push(SymbolItem { rendered, key });
    }
    Ok((symbol_items, bound_memories, had_hits))
}

/// Memory lane: runs always. The symbol-bound memories lead, then relevance-gated FTS hits over the
/// normalized pattern, then path-bound memories; session dedupe, drift marking and the surface
/// projection apply to the assembled list.
fn memory_lane(
    conn: &Connection,
    normalized: &str,
    search_path: Option<&str>,
    dedupe: &DedupeFilter,
    surface: rag_rat_base::config::MemorySurface,
    symbol_bound: Vec<memory::RepoMemory>,
) -> anyhow::Result<Vec<memory::RepoMemory>> {
    let mut seen_memory_ids: HashSet<String> =
        symbol_bound.iter().map(|m| m.memory_id.clone()).collect();
    let mut memories = symbol_bound;
    // The FTS half is relevance-gated (a corpus-wide token in the pattern otherwise drags in
    // MAX_MEMORIES unrelated memories); the path half is not — it is a structural binding, not a
    // text match. The gate runs over the FULL hit set, BEFORE session dedupe (the blanket retain
    // below): relevance is a property of the query, not of what this session happened to show.
    // Dropping an already-seen hit first would hand the reference score to the runner-up, and the
    // weak tail would pass the gate for the rest of the resurface window — exactly the noise it
    // removes.
    let fts_hits = memory::memory_search_scored(conn, normalized, MAX_MEMORIES)?;
    extend_new_memories(
        &mut memories,
        &mut seen_memory_ids,
        memories_above_relative_floor(fts_hits),
    );
    if let Some(path) = search_path {
        extend_new_memories(
            &mut memories,
            &mut seen_memory_ids,
            memory::memories_for_path(conn, path, MAX_MEMORIES)?,
        );
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
    Ok(memories)
}

/// Lexical lane: runs only when the symbol lane had no raw hits. Relevance gate: keep only hits
/// within LEXICAL_RELATIVE_FLOOR of the best hit's score, so the weak tail (e.g. an incidental
/// match several ranks down) isn't injected as noise.
fn lexical_lane(conn: &Connection, normalized: &str) -> anyhow::Result<Vec<String>> {
    Ok(lexical_lines_from_hits(lexical::search_lexical_only(
        conn,
        normalized,
        MAX_LEXICAL_HITS,
        false,
    )?))
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

/// Append each memory in `incoming` whose id is not yet in `seen`, keeping `incoming`'s order. Both
/// hook composers assemble their memory list lane by lane through this, so the order they call it
/// in IS the rendered priority order.
pub(crate) fn extend_new_memories(
    dst: &mut Vec<memory::RepoMemory>,
    seen: &mut HashSet<String>,
    incoming: impl IntoIterator<Item = memory::RepoMemory>,
) {
    for m in incoming {
        if seen.insert(m.memory_id.clone()) {
            dst.push(m);
        }
    }
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
#[path = "grep_augment_tests.rs"]
mod tests;
