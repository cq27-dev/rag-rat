//! Anchor-candidate mining for distilled records (#703).
//!
//! Anchors bind a record to the code its fix touched. They are mined MECHANICALLY from the files a
//! fixing commit changed and validated against the LIVE symbol index — never fuzzy-matched, never
//! basename-guessed. A symbol anchor carries the `sym_<hex>` logical-symbol handle (relocation-
//! compatible: it self-heals across cross-file moves) and its EXACT indexed path; a file anchor
//! carries the exact indexed path. Test and generated files are excluded — a fix's test churn is
//! not where its decision lives. #704's model selects from these candidates by index; it cannot
//! invent an anchor that mining did not surface.

use rag_rat_base::{path_class, serde_big_id};
use rusqlite::{Connection, OptionalExtension};

/// A single mined anchor. `resolved` means index-validated (a symbol with a logical id, or a path
/// present in the `files` scope) — only resolved anchors count toward `anchors_qualified_count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnchorCandidate {
    pub kind: AnchorKind,
    /// `sym_<hex>` for a resolved symbol anchor; `None` for file anchors or unresolved symbols.
    pub logical_symbol_id: Option<String>,
    /// The exact indexed path (source of the anchor); `None` only if a symbol lacks a path row.
    pub file_path: Option<String>,
    pub name: String,
    pub resolved: bool,
}

pub(crate) use rag_rat_papertrail::AnchorKind;

/// Caps that bound how much a single sprawling fixing commit can inflate the candidate pool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnchorCaps {
    pub max_files: usize,
    pub max_symbols_per_file: usize,
    pub max_total: usize,
}

impl Default for AnchorCaps {
    fn default() -> Self {
        Self { max_files: 15, max_symbols_per_file: 20, max_total: 40 }
    }
}

/// Mine anchor candidates from the changed paths of a record's fixing commits. `changed_paths` is
/// the deduped set of files those commits touched (from `git_file_changes`); this filters out test
/// and generated files, emits a file anchor per surviving source path present in the index, and a
/// symbol anchor per symbol defined in that path (logical id when the index has one). Symbol reads
/// go through the per-connection `files`/`symbols` scope views, so they are already repo- and
/// generation-scoped.
pub(crate) fn mine_anchor_candidates(
    conn: &Connection,
    changed_paths: &[String],
    caps: AnchorCaps,
) -> anyhow::Result<Vec<AnchorCandidate>> {
    let mut anchors = Vec::new();
    let mut files_used = 0usize;
    for path in changed_paths {
        if anchors.len() >= caps.max_total || files_used >= caps.max_files {
            break;
        }
        if path_class::is_test_path(path) || path_class::is_generated_path(path) {
            continue;
        }
        // A file anchor only when the path is actually indexed in this scope AND is not generated
        // (no basename fallback, no anchoring to a path the index has never seen).
        // `files.generated` is the repo's canonical generated classification — it folds
        // file kind + path heuristics, so it catches registry-classified generated files
        // the path heuristic above misses.
        let indexed = conn
            .query_row(
                "SELECT 1 FROM files WHERE path = ?1 AND generated = 0 LIMIT 1",
                [path],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !indexed {
            continue;
        }
        files_used += 1;
        anchors.push(AnchorCandidate {
            kind: AnchorKind::File,
            logical_symbol_id: None,
            file_path: Some(path.clone()),
            name: path.clone(),
            resolved: true,
        });
        for (name, logical) in symbols_in_file(conn, path, caps.max_symbols_per_file)? {
            if anchors.len() >= caps.max_total {
                break;
            }
            let logical_symbol_id = logical.map(serde_big_id::format_sym_handle);
            let resolved = logical_symbol_id.is_some();
            anchors.push(AnchorCandidate {
                kind: AnchorKind::Symbol,
                logical_symbol_id,
                file_path: Some(path.clone()),
                name,
                resolved,
            });
        }
    }
    Ok(anchors)
}

/// `(symbol_name, logical_symbol_id)` for the symbols defined in `path`, lowest symbol id first,
/// capped. The `files`/`symbols` reads ride the per-connection scope views; the LEFT JOIN yields a
/// `NULL` logical id for a symbol not yet folded into a logical group (an unresolved candidate).
fn symbols_in_file(
    conn: &Connection,
    path: &str,
    limit: usize,
) -> anyhow::Result<Vec<(String, Option<i64>)>> {
    let mut stmt = conn.prepare(
        "SELECT symbols.name, members.logical_symbol_id
         FROM symbols
         JOIN files ON files.id = symbols.file_id
         LEFT JOIN logical_symbol_members members ON members.symbol_id = symbols.id
         WHERE files.path = ?1
         ORDER BY symbols.id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![path, limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{AnchorCaps, AnchorKind, mine_anchor_candidates};

    /// Minimal `files` + `symbols` + `logical_symbol_members` fixture (real tables, not the scope
    /// views — the test binds paths directly, which the `WHERE files.path = ?` query reads the
    /// same).
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT, generated INTEGER NOT NULL \
             DEFAULT 0);
            CREATE TABLE symbols(id INTEGER PRIMARY KEY, file_id INTEGER, name TEXT);
            CREATE TABLE logical_symbol_members(symbol_id INTEGER, logical_symbol_id INTEGER);
            INSERT INTO files(id, path, generated) VALUES
                (1, 'crates/core/src/engine.rs', 0),
                (2, 'crates/core/tests/engine_test.rs', 0),
                (3, 'crates/core/src/generated/wire.rs', 0),
                -- a normal-looking source path the LANGUAGE REGISTRY flagged generated (the path
                -- heuristic would miss it; the `files.generated` flag must exclude it).
                (4, 'crates/core/src/bindings.rs', 1);
            INSERT INTO symbols(id, file_id, name) VALUES
                (10, 1, 'run_engine'),
                (11, 1, 'EngineState'),
                (20, 2, 'test_helper'),
                (40, 4, 'GeneratedBinding');
            -- run_engine is folded into a logical group; EngineState is not (unresolved \
             candidate).
            INSERT INTO logical_symbol_members(symbol_id, logical_symbol_id) VALUES (10, 123456);
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn mines_file_and_symbol_anchors_from_a_source_path() {
        let conn = fixture();
        let anchors = mine_anchor_candidates(
            &conn,
            &["crates/core/src/engine.rs".into()],
            AnchorCaps::default(),
        )
        .unwrap();
        // one file anchor + two symbol anchors.
        assert_eq!(anchors.len(), 3);
        assert_eq!(anchors[0].kind, AnchorKind::File);
        assert!(anchors[0].resolved);
        let run = anchors.iter().find(|a| a.name == "run_engine").unwrap();
        assert_eq!(run.kind, AnchorKind::Symbol);
        assert!(run.resolved, "a symbol with a logical id resolves");
        assert_eq!(run.logical_symbol_id.as_deref(), Some("sym_1e240")); // 123456 == 0x1e240
        let state = anchors.iter().find(|a| a.name == "EngineState").unwrap();
        assert!(!state.resolved, "no logical id → unresolved candidate");
        assert_eq!(state.logical_symbol_id, None);
    }

    #[test]
    fn skips_test_and_generated_and_unindexed_paths() {
        let conn = fixture();
        let anchors = mine_anchor_candidates(
            &conn,
            &[
                "crates/core/tests/engine_test.rs".into(), // test → skipped
                "crates/core/src/generated/wire.rs".into(), // generated (path) → skipped
                "crates/core/src/bindings.rs".into(),      // generated (files.generated) → skipped
                "crates/core/src/absent.rs".into(),        // not indexed → skipped
            ],
            AnchorCaps::default(),
        )
        .unwrap();
        assert!(anchors.is_empty(), "no anchors from test/generated/unindexed paths");
    }

    #[test]
    fn respects_the_total_cap() {
        let conn = fixture();
        let caps = AnchorCaps { max_files: 15, max_symbols_per_file: 20, max_total: 2 };
        let anchors =
            mine_anchor_candidates(&conn, &["crates/core/src/engine.rs".into()], caps).unwrap();
        assert_eq!(anchors.len(), 2, "capped at max_total");
    }
}
