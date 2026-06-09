//! Payload composition for the Claude Code grep-augmentation PreToolUse hook.
//!
//! Shared by the `rag-rat mcp` socket listener (with per-session dedupe) and the hook
//! client's direct read-only fallback (stateless). Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`. Never loads the embedding
//! model — symbol/FTS lanes only.

/// Strip regex syntax from a grep pattern, leaving plain query text. Metacharacters become
/// spaces (so alternation/group contents survive as separate words); runs of whitespace
/// collapse; result is trimmed.
pub fn normalize_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Drop regex escapes entirely: \s/\b/\w and \\, \., \-, etc. all become a
                // space — punctuation and class shorthands are not query signal.
                if chars.peek().is_some() {
                    chars.next(); // consume the escaped char
                }
                out.push(' ');
            },
            '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                out.push(' ');
            },
            _ => out.push(ch),
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

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension};

use crate::query::{memory, symbol};
use crate::search::lexical;

/// Hard cap on rendered context. Truncation drops whole items, never mid-item.
pub const MAX_CONTEXT_CHARS: usize = 1500;
const MAX_SYMBOLS: u32 = 3;
const MAX_MEMORIES: u32 = 4;
const MAX_LEXICAL_HITS: u32 = 3;

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
) -> anyhow::Result<Option<GrepAugment>> {
    let normalized = normalize_pattern(raw_pattern);
    if normalized.is_empty() {
        return Ok(None);
    }

    let mut memories = Vec::new();
    let mut symbol_lines = Vec::new();
    let mut symbol_keys = Vec::new();
    // Track whether the symbol lane produced any raw hits (before dedup).
    // Lexical lane only runs when there were no symbol hits at all (not just all deduped).
    let mut symbol_lane_had_hits = false;

    if let Some(ident) = identifier_candidate(&normalized) {
        // Symbol lane. Bare name for qualified queries: `Watcher::spawn` → `spawn`.
        let bare = ident.rsplit([':', '.']).next().unwrap_or(ident);
        for hit in symbol::lookup(conn, bare, None, MAX_SYMBOLS)? {
            symbol_lane_had_hits = true;
            let key = format!("{}:{}", hit.path, hit.qualified_name);
            if dedupe.symbol_keys.contains(&key) {
                continue;
            }
            let (callers, callees) = edge_counts(conn, &hit)?;
            let start_line = line_for_symbol(conn, &hit)?;
            symbol_lines.push(format!(
                "- `{}` ({}) — {}:{} — {} callers / {} callees{}",
                hit.qualified_name,
                hit.kind,
                hit.path,
                start_line,
                callers,
                callees,
                hit.signature.as_deref().map(|s| format!(" — `{s}`")).unwrap_or_default(),
            ));
            memories.extend(memory::memories_for_symbol(conn, &hit, MAX_MEMORIES)?);
            symbol_keys.push(key);
        }
    }

    // Memory lane: always. FTS over the normalized pattern + path-bound memories.
    memories.extend(memory::memory_search(conn, &normalized, MAX_MEMORIES)?);
    if let Some(path) = search_path {
        memories.extend(memory::memories_for_path(conn, path, MAX_MEMORIES)?);
    }
    memories.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    memories.dedup_by(|a, b| a.memory_id == b.memory_id);
    memories.retain(|m| !dedupe.memory_ids.contains(&m.memory_id));

    // Lexical lane: only when the symbol lane found nothing (never had any raw hits).
    let lexical_lines = if !symbol_lane_had_hits {
        lexical::search_lexical_only(conn, &normalized, MAX_LEXICAL_HITS, false)?
            .into_iter()
            .map(|hit| {
                format!("- {}:{}-{} — {}", hit.path, hit.start_line, hit.end_line, hit.summary)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    if memories.is_empty() && symbol_lines.is_empty() && lexical_lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(render(memories, symbol_lines, symbol_keys, lexical_lines)))
}

/// Caller/callee edge counts. Callers resolve by `to_symbol_id` or qualified-name match;
/// callees are edges leaving any of the symbol's concrete rows.
fn edge_counts(conn: &Connection, hit: &symbol::SymbolHit) -> anyhow::Result<(i64, i64)> {
    let callers: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE to_symbol_id = ?1 OR target_qualified_name = ?2",
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

/// 1-based start line for a symbol hit (line spans live on chunks; fall back to 1).
fn line_for_symbol(conn: &Connection, hit: &symbol::SymbolHit) -> anyhow::Result<i64> {
    let line: Option<i64> = conn
        .query_row(
            "SELECT start_line FROM chunks
             WHERE file_id = ?1 AND start_byte <= ?2 AND end_byte >= ?2
             ORDER BY (end_byte - start_byte) ASC LIMIT 1",
            rusqlite::params![hit.file_id, hit.start_byte],
            |row| row.get(0),
        )
        .optional()?;
    Ok(line.unwrap_or(1))
}

/// Memories first (the unique signal), then symbols, then lexical hits; whole-item truncation
/// against `MAX_CONTEXT_CHARS`.
fn render(
    memories: Vec<memory::RepoMemory>,
    symbol_lines: Vec<String>,
    symbol_keys: Vec<String>,
    lexical_lines: Vec<String>,
) -> GrepAugment {
    let mut sections = Vec::new();
    let mut memory_ids = Vec::new();
    if !memories.is_empty() {
        let mut lines = vec!["**Repo memories bound to this code:**".to_string()];
        for m in &memories {
            lines.push(format!(
                "- [{} | {}] {} — {} (rag-rat: memory_search)",
                m.kind, m.status, m.title, m.body
            ));
            memory_ids.push(m.memory_id.clone());
        }
        sections.push(lines);
    }
    if !symbol_lines.is_empty() {
        let mut lines = vec!["**Known symbols matching this pattern:**".to_string()];
        lines.extend(symbol_lines);
        lines.push("(rag-rat: impact_surface <name> before editing)".to_string());
        sections.push(lines);
    }
    if !lexical_lines.is_empty() {
        let mut lines = vec!["**Indexed hits (rag-rat semantic_search has more):**".to_string()];
        lines.extend(lexical_lines);
        sections.push(lines);
    }
    let mut context = String::from("rag-rat index context for this search:\n");
    'outer: for section in sections {
        for line in section {
            if context.len() + line.len() + 1 > MAX_CONTEXT_CHARS {
                break 'outer;
            }
            context.push_str(&line);
            context.push('\n');
        }
    }
    GrepAugment { context: context.trim_end().to_string(), memory_ids, symbol_keys }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rusqlite::Connection;

    use super::*;
    use crate::index::schema;
    use crate::query::memory::{self, RepoMemoryBindTarget, RepoMemoryCreate};

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                                 end_byte, signature, docs)
             VALUES (1, 'rust', 'watcher_main', 'watch::watcher_main', 'function', 0, 100,
                     'fn watcher_main(config: Config)', NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text, text_hash)
             VALUES (1, 'symbol', 'watch::watcher_main', 0, 100, 1, 20,
                     'fn watcher_main() { /* election retry loop */ }', 'h1')",
            [],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
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
        memory::create_memory(
            &conn,
            RepoMemoryCreate {
                kind: "Invariant".to_string(),
                title: "One watcher per worktree".to_string(),
                body: "The election lock guarantees a single watcher; never bind without it."
                    .to_string(),
                confidence: "high".to_string(),
                created_by: Some("test".to_string()),
                source: None,
                tags: vec![],
                bind: RepoMemoryBindTarget {
                    symbol_id: Some(1),
                    logical_symbol_id: None,
                    chunk_id: None,
                    edge_id: None,
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                },
            },
        )
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

    #[test]
    fn compose_identifier_pattern_yields_symbol_and_memory() {
        let conn = seeded_conn();
        let out = compose(&conn, r"watcher_main\b", None, &DedupeFilter::default())
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
    fn compose_respects_dedupe_filter_and_returns_none_when_everything_filtered() {
        let conn = seeded_conn();
        let first = compose(&conn, "watcher_main", None, &DedupeFilter::default())
            .unwrap()
            .expect("first payload");
        let filter = DedupeFilter {
            memory_ids: first.memory_ids.iter().cloned().collect::<HashSet<_>>(),
            symbol_keys: first.symbol_keys.iter().cloned().collect::<HashSet<_>>(),
        };
        assert!(compose(&conn, "watcher_main", None, &filter).unwrap().is_none());
    }

    #[test]
    fn compose_non_identifier_pattern_uses_lexical_lane() {
        let conn = seeded_conn();
        let out = compose(&conn, "election retry loop", None, &DedupeFilter::default())
            .unwrap()
            .expect("lexical payload");
        assert!(out.context.contains("src/watch.rs"));
    }

    #[test]
    fn compose_unknown_pattern_yields_none() {
        let conn = seeded_conn();
        assert!(
            compose(&conn, "zzqqyyxx_nothing", None, &DedupeFilter::default()).unwrap().is_none()
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
    fn identifier_candidate_accepts_identifier_shapes_only() {
        assert_eq!(identifier_candidate("watcher_main"), Some("watcher_main"));
        assert_eq!(identifier_candidate("Watcher::spawn"), Some("Watcher::spawn"));
        assert_eq!(identifier_candidate("foo.bar"), Some("foo.bar"));
        assert_eq!(identifier_candidate("fn watcher_main"), None); // two words
        assert_eq!(identifier_candidate("ab"), None); // too short
        assert_eq!(identifier_candidate("1abc"), None); // leading digit
        assert_eq!(identifier_candidate(""), None);
    }
}
