//! Payload composition for the `Read`-augmentation PreToolUse hook (#756).
//!
//! When an agent opens a file, inject the repo memories bound to that file and its directory, plus
//! the load-bearing symbols DEFINED in the file (ranked by caller fan-in) — the drive-by context
//! that grep-augment surfaces on a pattern search, extended to the read path. Shares grep-augment's
//! renderer, dedup filter, and per-symbol helpers so the two hooks stay consistent; like it, this
//! NEVER loads the embedding model — memory + symbol lanes only.
//!
//! `path` is REPO-ROOT-RELATIVE (the hook client relativizes the absolute `file_path` against the
//! config root before composing / sending), matching how indexed bindings and symbols are keyed.

use std::collections::HashSet;

use rag_rat_query::{memory, symbol};
use rusqlite::Connection;

use crate::query::grep_augment::{
    self, DedupeFilter, GrepAugment, MAX_MEMORIES, RenderItem, Section,
};

/// Symbols in the file to actually surface, after ranking by caller fan-in. A file being opened
/// usually has a small handful of genuinely load-bearing definitions; more is noise.
const MAX_FILE_SYMBOLS: usize = 3;
/// Upper bound on the symbols whose caller counts we compute for ranking. Bounds the per-`Read`
/// cost on a huge file (each candidate is a pair of indexed COUNT queries); a symbol past the cap
/// is simply not considered.
const CANDIDATE_CAP: u32 = 80;

/// Compose the read-augmentation digest for `path` (root-relative), or `None` when nothing (new) is
/// worth injecting. Memory lane: file-bound + directory-bound memories, plus memories on the file's
/// load-bearing symbols. Symbol lane: those load-bearing symbols with caller/callee counts.
pub fn compose(
    conn: &Connection,
    path: &str,
    dedupe: &DedupeFilter,
    surface: rag_rat_base::config::MemorySurface,
) -> anyhow::Result<Option<GrepAugment>> {
    // Rank the file's symbols by caller fan-in first: the top few are the symbol lane, and their
    // bound memories join the memory lane at the front (highest-priority signal, like
    // grep-augment).
    let ranked = rank_file_symbols(conn, path)?;

    let mut memories = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Symbol-bound memories first (the load-bearing symbols the agent is about to touch).
    for (hit, _, _) in &ranked {
        for m in memory::memories_for_symbol(conn, hit, MAX_MEMORIES)? {
            if seen.insert(m.memory_id.clone()) {
                memories.push(m);
            }
        }
    }
    // Then the file's own path-bound memories, then its directory memories.
    for m in memory::memories_for_path(conn, path, MAX_MEMORIES)? {
        if seen.insert(m.memory_id.clone()) {
            memories.push(m);
        }
    }
    for m in memory::memories_for_path(conn, parent_dir(path), MAX_MEMORIES)? {
        if seen.insert(m.memory_id.clone()) {
            memories.push(m);
        }
    }
    // Session-level dedupe (what this agent was already shown), then the memory-surface projection.
    memories.retain(|m| !dedupe.memory_ids.contains(&m.memory_id));
    memory::apply_memory_surface(conn, &mut memories, surface)?;

    // Symbol lane items, minus anything this session already saw (shared key with grep-augment).
    let symbol_items: Vec<RenderItem> = ranked
        .iter()
        .map(|(hit, callers, callees)| symbol_render_item(conn, hit, *callers, *callees))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter(|item| item.symbol_key.as_ref().is_none_or(|k| !dedupe.symbol_keys.contains(k)))
        .collect();

    if memories.is_empty() && symbol_items.is_empty() {
        return Ok(None);
    }

    let mut sections: Vec<Section> = Vec::new();
    if !memories.is_empty() {
        sections.push(Section {
            header: "**Repo memories bound to this file:**".to_string(),
            items: memories.into_iter().map(grep_augment::memory_render_item).collect(),
            footer: None,
        });
    }
    if !symbol_items.is_empty() {
        sections.push(Section {
            header: "**Load-bearing symbols in this file:**".to_string(),
            items: symbol_items,
            footer: Some("(rag-rat: impact_surface <name> before editing)".to_string()),
        });
    }
    Ok(Some(grep_augment::pack_sections("rag-rat index context for this file:", sections)))
}

/// The file's symbols ranked by caller fan-in, top [`MAX_FILE_SYMBOLS`], callers > 0 only. Reuses
/// grep-augment's exact `edge_counts` (both caller arms, index-driven) so the counts shown here
/// agree with `impact_surface`; a symbol with no in-repo callers is not "load-bearing" and is
/// dropped rather than shown as a zero.
fn rank_file_symbols(
    conn: &Connection,
    path: &str,
) -> anyhow::Result<Vec<(symbol::SymbolHit, i64, i64)>> {
    let mut ranked = Vec::new();
    // Dedup by qualified name: a file can hold many concrete rows for one logical symbol —
    // overloads, cfg variants, re-exports, or Python's per-class `__init__` (thousands in one
    // dummy-objects file) — and without this the three slots fill with repeats of one symbol,
    // hiding the rest (#756 review; the same defect grep-augment dedups). All rows here share
    // the queried path, so the qualified name alone keys the logical symbol.
    let mut seen: HashSet<String> = HashSet::new();
    for id in symbol::symbol_ids_in_file(conn, path, CANDIDATE_CAP)? {
        let Some(hit) = symbol::lookup_by_id(conn, id)? else { continue };
        if !seen.insert(hit.qualified_name.clone()) {
            continue;
        }
        let (callers, callees) = grep_augment::edge_counts(conn, &hit)?;
        if callers > 0 {
            ranked.push((hit, callers, callees));
        }
    }
    // Most-depended-on first; a stable tie-break on the qualified name keeps output deterministic.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.qualified_name.cmp(&b.0.qualified_name)));
    ranked.truncate(MAX_FILE_SYMBOLS);
    Ok(ranked)
}

/// Render one load-bearing-symbol line: `- \`name\` (kind) at line L — C callers / C callees —
/// \`sig\``. The symbol key matches grep-augment's (`path:qualified_name`) so a symbol surfaced via
/// a read and a grep dedups against one shared session filter.
fn symbol_render_item(
    conn: &Connection,
    hit: &symbol::SymbolHit,
    callers: i64,
    callees: i64,
) -> anyhow::Result<RenderItem> {
    let line_suffix = match grep_augment::line_for_symbol(conn, hit)? {
        Some(l) => format!(" at line {l}"),
        None => String::new(),
    };
    let sig = hit.signature.as_deref().map(|s| format!(" — `{s}`")).unwrap_or_default();
    let line = format!(
        "- `{}` ({}){} — {} callers / {} callees{}",
        hit.qualified_name, hit.kind, line_suffix, callers, callees, sig,
    );
    Ok(RenderItem {
        line,
        memory_id: None,
        symbol_key: Some(format!("{}:{}", hit.path, hit.qualified_name)),
    })
}

/// The immediate parent directory of a root-relative file path, to look up its directory memories.
/// A top-level file's directory is the REPO ROOT, anchored as the empty path — the memory API's
/// `dir = ""` repo-root binding — so return `""` there rather than skipping it (#756 review).
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
    use rusqlite::Connection;

    use super::*;

    #[test]
    fn parent_dir_is_the_immediate_directory_or_the_repo_root_at_top_level() {
        assert_eq!(
            parent_dir("crates/rag-rat-core/src/query/read_augment.rs"),
            "crates/rag-rat-core/src/query"
        );
        // A top-level file's directory is the repo root, keyed as the empty path.
        assert_eq!(parent_dir("README.md"), "");
        assert_eq!(parent_dir("src/lib.rs"), "src");
    }

    fn empty_bind() -> RepoMemoryBindTarget {
        RepoMemoryBindTarget {
            symbol_id: None,
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
        }
    }

    fn create(conn: &Connection, title: &str, target: RepoMemoryBindTarget) {
        crate::memory_write::create_memory(conn, RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: format!("body of {title}"),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            payload_json: None,
            bind: target,
        })
        .unwrap();
    }

    /// A file `src/watch.rs` with a load-bearing `watcher_main` (id 1, one caller) and a leaf
    /// `helper` (id 2, no callers), a path-bound memory, and a directory memory on `src`.
    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
            [],
        )
        .unwrap();
        for (name, qn) in [("watcher_main", "watch::watcher_main"), ("helper", "watch::helper")] {
            conn.execute(
                "INSERT OR IGNORE INTO name_strings(value) VALUES (?1)",
                rusqlite::params![qn],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', ?1, (SELECT id FROM name_strings WHERE value = ?2),
                         'function', 0, 10, NULL, NULL)",
                rusqlite::params![name, qn],
            )
            .unwrap();
        }
        // A SECOND concrete row for the SAME `watch::watcher_main` (a re-export/overload). The
        // ranker must fold it into one line, not spend two slots on it.
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                 end_byte, signature, docs)
             VALUES (1, 'rust', 'watcher_main',
                     (SELECT id FROM name_strings WHERE value = 'watch::watcher_main'),
                     'function', 20, 30, NULL, NULL)",
            [],
        )
        .unwrap();
        // One caller of watcher_main (symbol id 1) → callers=1; helper (id 2) has none.
        conn.execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence)
             VALUES (1, NULL, 1, 'watcher_main', 'watch::watcher_main', 'calls_name', 'exact')",
            [],
        )
        .unwrap();
        create(&conn, "Watch invariant", RepoMemoryBindTarget {
            path: Some("src/watch.rs".to_string()),
            ..empty_bind()
        });
        create(&conn, "Src directory note", RepoMemoryBindTarget {
            dir: Some("src".to_string()),
            ..empty_bind()
        });
        // A repo-root directory memory (`dir = ""`), surfaced only for TOP-LEVEL reads.
        create(&conn, "Repo root note", RepoMemoryBindTarget {
            dir: Some(String::new()),
            ..empty_bind()
        });
        conn
    }

    #[test]
    fn read_augment_surfaces_file_and_dir_memories_plus_load_bearing_symbols() {
        let conn = seeded_conn();
        let out = compose(
            &conn,
            "src/watch.rs",
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("a file with memories + a load-bearing symbol augments");

        assert!(
            out.context.contains("Watch invariant"),
            "path-bound memory surfaces:\n{}",
            out.context
        );
        assert!(
            out.context.contains("Src directory note"),
            "directory memory surfaces:\n{}",
            out.context
        );
        assert!(
            out.context.contains("`watch::watcher_main`") && out.context.contains("1 callers"),
            "the load-bearing symbol surfaces with its caller count:\n{}",
            out.context
        );
        assert!(
            !out.context.contains("watch::helper"),
            "a symbol with no callers is not load-bearing and is omitted:\n{}",
            out.context
        );
        assert_eq!(
            out.context.matches("`watch::watcher_main`").count(),
            1,
            "duplicate concrete rows of one symbol render once, not once per row:\n{}",
            out.context
        );
    }

    #[test]
    fn read_augment_surfaces_the_repo_root_directory_memory_for_a_top_level_file() {
        let conn = seeded_conn();
        // A top-level file's directory is the repo root (`dir = ""`) — that memory must surface.
        let out = compose(
            &conn,
            "README.md",
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("a top-level file with a repo-root directory memory augments");
        assert!(
            out.context.contains("Repo root note"),
            "the repo-root (dir = \"\") memory surfaces for a top-level read:\n{}",
            out.context
        );
        // The `src` directory memory must NOT leak into a top-level read.
        assert!(!out.context.contains("Src directory note"), "src-dir memory stays scoped to src/");
    }

    #[test]
    fn read_augment_dedups_against_the_session_filter() {
        let conn = seeded_conn();
        // First surface records ids; feed them back as the dedupe filter — nothing new remains.
        let first = compose(
            &conn,
            "src/watch.rs",
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()
        .expect("first surface augments");
        let filter = DedupeFilter {
            memory_ids: first.memory_ids.iter().cloned().collect(),
            symbol_keys: first.symbol_keys.iter().cloned().collect(),
        };
        let second =
            compose(&conn, "src/watch.rs", &filter, rag_rat_base::config::MemorySurface::Full)
                .unwrap();
        assert!(
            second.is_none(),
            "everything already surfaced this session → nothing new to inject"
        );
    }

    #[test]
    fn read_augment_is_silent_for_a_file_with_nothing_indexed() {
        let conn = seeded_conn();
        // A file in a directory with no memory of its own — no path/dir memory, no indexed symbols.
        // (`src/…` would still pick up the `src` directory memory, which is the feature working.)
        let out = compose(
            &conn,
            "docs/unknown.md",
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap();
        assert!(out.is_none(), "a file with no memories and no symbols yields no context");
    }
}
