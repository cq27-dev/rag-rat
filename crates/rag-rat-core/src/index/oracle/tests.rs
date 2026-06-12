//! End-to-end tests for the SCIP-oracle join. SCIP `Index` objects are built **programmatically**
//! via the `scip` crate's types (no rust-analyzer, no network) and serialized, then fed through the
//! real `run_oracle` path against a DB seeded with synthetic files/symbols/edges. This keeps the
//! join deterministic and exercises the exact code eval uses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ::protobuf::{EnumOrUnknown, Message};
use ::scip::types::{Document, Index, Occurrence, PositionEncoding, SymbolRole};
use rusqlite::{Connection, params};

use super::{OracleResolutionKind, OracleTool, RecallCalls, run_oracle};
use crate::index::schema;

/// A unique temp directory under the system temp root (no external crate; matches the repo's
/// `std::env::temp_dir` + atomic-counter convention). Cleaned up on `Drop`.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rag-rat-oracle-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempRoot { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const TOOL: OracleTool = OracleTool::RustAnalyzer;
const VERSION: &str = "test";
const COMMIT: &str = "";
const WORKTREE: &str = "";

/// A test corpus written to a temp checkout + an index DB seeded to match.
struct Harness {
    conn: Connection,
    root: TempRoot,
}

impl Harness {
    fn new() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        // Match production (storage.rs) so FK cascades fire — `edge_oracle.edge_id` cascades off
        // `edges` (V018), and the oracle tests assert that cascade.
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::apply(&conn).unwrap();
        let root = TempRoot::new();
        Harness { conn, root }
    }

    /// Write a source file to the checkout and insert its `files` row, returning the file id. The
    /// `files.sha256` is the REAL hash of the written bytes (matching production), so the oracle's
    /// content-integrity gate (finding 2) sees no drift — the run hashes the same disk bytes. Tests
    /// that want to model drift override the row's `sha256` afterward (see `set_file_sha`).
    fn add_file(&self, path: &str, contents: &str) -> i64 {
        std::fs::write(self.root.path().join(path), contents).unwrap();
        let sha = sha256_hex(contents.as_bytes());
        self.conn
            .execute(
                "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
                 commit_sha, worktree_id) VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, ?4)",
                params![path, sha, COMMIT, WORKTREE],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    /// Force a file's recorded `sha256` to a value that does NOT match its disk bytes, modelling
    /// content drift between the index build and the `.scip` — the candidate's `file_sha` then
    /// disagrees with the disk-byte hash and the oracle skips it (finding 2).
    fn set_file_sha(&self, file_id: i64, sha: &str) {
        self.conn
            .execute("UPDATE files SET sha256 = ?2 WHERE id = ?1", params![file_id, sha])
            .unwrap();
    }

    /// Insert a symbol with a byte span, returning its id.
    fn add_symbol(&self, file_id: i64, name: &str, start_byte: usize, end_byte: usize) -> i64 {
        self.conn
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, \
                 end_byte, start_line, end_line) VALUES (?1, 'rust', ?2, ?2, 'function', ?3, ?4, \
                 1, 1)",
                params![file_id, name, start_byte as i64, end_byte as i64],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    /// Insert a `calls_name` edge carrying a callee identifier byte range, returning its id.
    #[allow(clippy::too_many_arguments)]
    fn add_edge(
        &self,
        source_file_id: i64,
        to_name: &str,
        callee_start_byte: usize,
        callee_end_byte: usize,
        confidence: &str,
        to_symbol_id: Option<i64>,
    ) -> i64 {
        self.add_edge_with_kind(
            source_file_id,
            to_name,
            callee_start_byte,
            callee_end_byte,
            "calls_name",
            confidence,
            to_symbol_id,
        )
    }

    /// Insert an edge of an explicit `edge_kind` (e.g. `references_type`) carrying a callee byte
    /// range, returning its id. Non-call kinds still join against SCIP occurrences (they carry a
    /// callee range) but must not count toward the covered side of recall.
    #[allow(clippy::too_many_arguments)]
    fn add_edge_with_kind(
        &self,
        source_file_id: i64,
        to_name: &str,
        callee_start_byte: usize,
        callee_end_byte: usize,
        edge_kind: &str,
        confidence: &str,
        to_symbol_id: Option<i64>,
    ) -> i64 {
        let resolution = if to_symbol_id.is_some() { "exact" } else { "unresolved" };
        self.conn
            .execute(
                "INSERT INTO edges(source_file_id, to_name, callee_start_byte, callee_end_byte, \
                 edge_kind, confidence, resolution, to_symbol_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, \
                 ?7, ?8)",
                params![
                    source_file_id,
                    to_name,
                    callee_start_byte as i64,
                    callee_end_byte as i64,
                    edge_kind,
                    confidence,
                    resolution,
                    to_symbol_id,
                ],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    /// Insert a `files` row in an explicit `(commit_sha, worktree_id)` scope (no checkout write —
    /// these rows model another checkout sharing the same DB). Returns the file id.
    fn add_file_in_scope(&self, path: &str, commit: &str, worktree: &str) -> i64 {
        let sha = format!("sha-{worktree}-{path}");
        self.conn
            .execute(
                "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
                 commit_sha, worktree_id) VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, ?4)",
                params![path, sha, commit, worktree],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    /// The persisted oracle verdict for an edge, if any.
    fn verdict(&self, edge_id: i64) -> Option<(String, Option<i64>, String)> {
        self.conn
            .query_row(
                "SELECT kind, resolved_symbol_id, scip_symbol FROM edge_oracle WHERE edge_id = ?1",
                params![edge_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok()
    }

    /// The heuristic resolution on the `edges` row (must never change).
    fn heuristic_resolution(&self, edge_id: i64) -> (String, Option<i64>) {
        self.conn
            .query_row(
                "SELECT resolution, to_symbol_id FROM edges WHERE id = ?1",
                params![edge_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .unwrap()
    }
}

/// A single-line occurrence: `range = [line, start_char, end_char]` in the document's encoding.
fn occurrence(line: i32, start_char: i32, end_char: i32, symbol: &str, roles: i32) -> Occurrence {
    Occurrence {
        range: vec![line, start_char, end_char],
        symbol: symbol.to_string(),
        symbol_roles: roles,
        ..Default::default()
    }
}

/// Build a single-document SCIP index over `path` with the given occurrences + encoding,
/// serialized.
fn scip_bytes(path: &str, encoding: PositionEncoding, occurrences: Vec<Occurrence>) -> Vec<u8> {
    let document = Document {
        relative_path: path.to_string(),
        occurrences,
        position_encoding: EnumOrUnknown::new(encoding),
        ..Default::default()
    };
    let index = Index { documents: vec![document], ..Default::default() };
    index.write_to_bytes().unwrap()
}

/// Hex SHA-256 of bytes — the same hash `files.sha256` carries, so a test file's recorded sha
/// matches the disk-byte hash the oracle's content-integrity gate computes (finding 2).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A def inside the corpus upgrades an unresolved edge; the `edge_oracle` row is written and the
/// heuristic `edges` row is untouched.
#[test]
fn def_inside_corpus_upgrades_unresolved_edge() {
    let h = Harness::new();
    // `caller.rs` calls `target` defined in `defs.rs`.
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    // `target` definition spans bytes 3..9 ("target") in defs.rs.
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    // The callee identifier `target` in caller.rs is at bytes 14..20.
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let same = "scip-rust crate v1 `target`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        // Reference occurrence at the call site: chars 14..20 on line 0 (ASCII → bytes).
        occurrence(0, 14, 20, same, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);
    // Definition occurrence lives in defs.rs.
    let mut full = Index::parse_from_bytes(&bytes).unwrap();
    full.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, same, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = full.write_to_bytes().unwrap();

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    let (kind, resolved, _scip) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    // Heuristic row untouched.
    assert_eq!(h.heuristic_resolution(edge), ("unresolved".to_string(), None));
}

/// A def outside the corpus resolves to `resolved-external(<package>)`.
#[test]
fn def_outside_corpus_resolves_external() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { spawn(); }\n");
    let edge = h.add_edge(caller, "spawn", 14, 19, "NameOnly", None);

    // A SCIP symbol with a package component, no definition occurrence in the corpus → external.
    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 19, external, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    let (kind, resolved, scip) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved, None);
    assert_eq!(scip, external);
}

/// The position-encoding correctness test: a non-ASCII identifier resolves correctly under BOTH
/// UTF-8 and UTF-16 `position_encoding`. The multibyte prefix shifts the byte offset away from the
/// char offset; only correct per-encoding conversion lands on the identifier.
#[test]
fn non_ascii_identifier_resolves_under_both_encodings() {
    for encoding in [
        PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
    ] {
        let h = Harness::new();
        // `café` is the receiver prefix: 'é' is 2 UTF-8 bytes (1 UTF-16 unit). The call is
        // `café.naïve()`. We want the callee identifier `naïve`.
        //   bytes:  c a f é(2) . n a ï(2) v e ( ) ;
        //   "café." = c(1)a(1)f(1)é(2).(1) = 6 bytes → `naïve` starts at byte 6.
        //   `naïve` = n a ï(2) v e = 6 bytes → ends at byte 12.
        let src = "café.naïve();\n";
        let caller = h.add_file("caller.rs", src);
        let defs = h.add_file("defs.rs", "fn naïve() {}\n");
        // `naïve` definition in defs.rs: "fn " = 3 bytes, then `naïve` (6 bytes) → 3..9.
        let target_sym = h.add_symbol(defs, "naïve", 3, 9);
        let edge = h.add_edge(caller, "naïve", 6, 12, "NameOnly", None);

        // Char offsets of `naïve` on line 0 differ by encoding:
        //   UTF-8 : "café." = c,a,f,é(2),. = 6 code units → start 6; `naïve` = 6 units → end 12.
        //   UTF-16: "café." = c,a,f,é(1),. = 5 code units → start 5; `naïve` (ï=1) = 5 units → 10.
        let (start_char, end_char) = match encoding {
            PositionEncoding::UTF8CodeUnitOffsetFromLineStart => (6, 12),
            PositionEncoding::UTF16CodeUnitOffsetFromLineStart => (5, 10),
            _ => unreachable!(),
        };
        let (def_start, def_end) = match encoding {
            PositionEncoding::UTF8CodeUnitOffsetFromLineStart => (3, 9),
            PositionEncoding::UTF16CodeUnitOffsetFromLineStart => (3, 8),
            _ => unreachable!(),
        };

        let symbol = "scip-rust crate v1 `naïve`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    start_char,
                    end_char,
                    symbol,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(encoding),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                def_start,
                def_end,
                symbol,
                SymbolRole::Definition as i32,
            )],
            position_encoding: EnumOrUnknown::new(encoding),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();

        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

        let (kind, resolved, _) =
            h.verdict(edge).unwrap_or_else(|| panic!("verdict written for encoding {encoding:?}"));
        assert_eq!(
            kind,
            OracleResolutionKind::Upgrade.as_db_str(),
            "encoding {encoding:?}: expected in-corpus upgrade"
        );
        assert_eq!(resolved, Some(target_sym), "encoding {encoding:?}: wrong symbol");
    }
}

/// `local N` symbols are skipped entirely — they never produce a verdict.
#[test]
fn local_symbols_are_skipped() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { helper(); }\n");
    let edge = h.add_edge(caller, "helper", 14, 20, "NameOnly", None);

    // A `local 0` occurrence covers the call site — must be ignored.
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 20, "local 0", SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    assert!(h.verdict(edge).is_none(), "local symbol must not produce a verdict");
}

/// An Exact/Syntactic edge the oracle contradicts is recorded as a disagreement; the heuristic
/// `edges` row is unchanged.
#[test]
fn exact_edge_contradiction_recorded_not_applied() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nfn other() {}\n");
    // Two candidate symbols: heuristic resolved to `wrong`, oracle says `right`.
    let wrong_sym = h.add_symbol(defs, "other", 18, 23);
    let right_sym = h.add_symbol(defs, "target", 3, 9);
    // Heuristic edge is Exact → resolved to the WRONG symbol.
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(wrong_sym));

    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        // Definition of `target` at bytes 3..9.
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Contradict.as_db_str());
    assert_eq!(resolved, Some(right_sym), "oracle resolved to the correct symbol");
    // Heuristic row STILL points at the wrong symbol — never auto-applied.
    assert_eq!(h.heuristic_resolution(edge), ("exact".to_string(), Some(wrong_sym)));
}

/// An Exact edge the oracle agrees with is recorded as a confirmation (the precision signal).
#[test]
fn exact_edge_agreement_recorded_as_confirm() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));

    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Confirm.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    assert_eq!(report.confirmed, 1);
}

/// The migration creates the side tables with the expected columns + STRICT mode.
#[test]
fn migration_creates_oracle_side_tables() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    for table in ["oracle_runs", "edge_oracle"] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{table} table must exist");
    }

    // STRICT mode (repo convention for new tables).
    let edge_oracle_sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'edge_oracle'", [], |row| row.get(0))
        .unwrap();
    assert!(edge_oracle_sql.contains("STRICT"), "edge_oracle must be STRICT");

    let columns = table_columns(&conn, "edge_oracle");
    for expected in [
        "edge_id",
        "file_sha",
        "tool",
        "tool_version",
        "resolved_symbol_id",
        "scip_symbol",
        "kind",
        "computed_at",
    ] {
        assert!(columns.contains(&expected.to_string()), "edge_oracle missing {expected}");
    }

    assert_eq!(schema::LATEST_SCHEMA_VERSION, 18);
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Deleting an `edges` row cascades to its `edge_oracle` verdict (FK `ON DELETE CASCADE`, V018), so
/// no orphan verdict survives for `status`/eval to keep counting. The rebuild +
/// `remove_file_in_scope` edge deletes reinsert edges with new ids and recompute the oracle, so
/// cascading old verdicts away is correct.
#[test]
fn deleting_an_edge_cascades_away_its_oracle_verdict() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let edge = h.add_edge(f, "target", 14, 20, "NameOnly", None);
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();
    assert!(h.verdict(edge).is_some(), "verdict present before delete");

    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge]).unwrap();

    assert!(h.verdict(edge).is_none(), "verdict cascaded away with its edge");
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 0, "no orphan edge_oracle rows survive the edge delete");
}

// ---------------------------------------------------------------------------
// store.rs — side-table I/O round trips, candidate scoping, staleness key.
// ---------------------------------------------------------------------------

use super::join::{self, names_external_package, package_of};
use super::scip::ScipIndex;
use super::store::{self, EdgeOracleRow, SymbolSpan};

/// `edge_join_candidates` returns only edges carrying a callee byte range, scoped to the active
/// commit/worktree, ordered by `(path, callee_start_byte)`. Edges with a NULL callee range and
/// edges in another worktree are excluded.
#[test]
fn edge_join_candidates_filters_null_range_and_scopes_by_worktree() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { a(); b(); }\n");
    // Two call edges with callee ranges (out of source order so we can assert ORDER BY).
    let edge_b = h.add_edge(caller, "b", 19, 20, "NameOnly", None);
    let edge_a = h.add_edge(caller, "a", 14, 15, "NameOnly", None);
    // A non-call edge with NULL callee range must be excluded.
    h.conn
        .execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
             (?1, 'mod', 'contains', 'Exact', 'exact')",
            params![caller],
        )
        .unwrap();

    let candidates = store::edge_join_candidates(&h.conn, COMMIT, WORKTREE).unwrap();
    let ids: Vec<i64> = candidates.iter().map(|c| c.edge_id).collect();
    // Only the two call edges, ordered by callee_start_byte (a at 14 before b at 19).
    assert_eq!(ids, vec![edge_a, edge_b]);
    // `file_sha` is the file's real content hash (what production records), so the candidate's
    // `edge_kind` and `file_sha` round-trip from the `files`/`edges` rows.
    assert_eq!(candidates[0].file_sha, sha256_hex("fn caller() { a(); b(); }\n".as_bytes()));
    assert_eq!(candidates[0].edge_kind, "calls_name");
    assert_eq!(candidates[0].source_path, "caller.rs");

    // A candidate in a different worktree is out of scope.
    assert!(store::edge_join_candidates(&h.conn, COMMIT, "other-worktree").unwrap().is_empty());
}

/// `symbol_spans_for_path` returns the file's symbols ordered by start byte, scoped to the path +
/// commit/worktree, and empty for an unknown path.
#[test]
fn symbol_spans_for_path_returns_scoped_ordered_spans() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn a() {}\nfn b() {}\n");
    let a = h.add_symbol(defs, "a", 3, 4);
    let b = h.add_symbol(defs, "b", 13, 14);

    let spans = store::symbol_spans_for_path(&h.conn, "defs.rs", COMMIT, WORKTREE).unwrap();
    assert_eq!(spans.iter().map(|s| s.symbol_id).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!(spans[0].start_byte, 3);
    assert_eq!(spans[1].end_byte, 14);

    assert!(
        store::symbol_spans_for_path(&h.conn, "missing.rs", COMMIT, WORKTREE).unwrap().is_empty()
    );
}

/// Writing an `edge_oracle` row and reading it back round-trips every field; re-writing the same
/// `(edge_id, tool, tool_version)` upserts (new file_sha/kind) rather than inserting a duplicate.
/// The matching `edges` row is never touched (side-table invariant).
#[test]
fn write_edge_oracle_round_trips_and_upserts_without_touching_edges() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));

    store::write_edge_oracle(&h.conn, TOOL, VERSION, &EdgeOracleRow {
        edge_id: edge,
        file_sha: "sha-v1",
        resolved_symbol_id: Some(7),
        scip_symbol: "scip `target`().",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    let (kind, resolved, scip) = h.verdict(edge).expect("row written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(7));
    assert_eq!(scip, "scip `target`().");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );

    // Re-write the same key with a new sha + verdict → upsert, still one row.
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &EdgeOracleRow {
        edge_id: edge,
        file_sha: "sha-v2",
        resolved_symbol_id: None,
        scip_symbol: "scip cargo tokio `target`().",
        kind: OracleResolutionKind::ResolvedExternal,
    })
    .unwrap();
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );
    let (kind2, resolved2, _) = h.verdict(edge).expect("row still present");
    assert_eq!(kind2, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved2, None);
    let file_sha: String = h
        .conn
        .query_row("SELECT file_sha FROM edge_oracle WHERE edge_id = ?1", params![edge], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(file_sha, "sha-v2", "upsert refreshed the staleness sha");

    // The heuristic edges row is untouched by either write.
    assert_eq!(h.heuristic_resolution(edge), ("exact".to_string(), Some(target_sym)));
}

/// The `(file_sha, tool, tool_version)` staleness key is a real composite: rows written under one
/// sha are findable by that sha, and a different sha (changed file bytes) does not match — the
/// content-addressing property the staleness index guards.
#[test]
fn staleness_key_distinguishes_rows_by_file_sha_tool_and_version() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    let e1 = h.add_edge(f, "x", 1, 2, "NameOnly", None);
    let e2 = h.add_edge(f, "y", 3, 4, "NameOnly", None);

    let row = |edge: i64, sha: &'static str| EdgeOracleRow {
        edge_id: edge,
        file_sha: sha,
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind: OracleResolutionKind::Upgrade,
    };
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &row(e1, "sha-fresh")).unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &row(e2, "sha-old")).unwrap();

    let count_for_sha = |sha: &str| -> i64 {
        h.conn
            .query_row(
                "SELECT COUNT(*) FROM edge_oracle WHERE file_sha = ?1 AND tool = ?2 AND \
                 tool_version = ?3",
                params![sha, TOOL.as_db_str(), VERSION],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(count_for_sha("sha-fresh"), 1, "row matches its own sha");
    assert_eq!(count_for_sha("sha-old"), 1);
    assert_eq!(count_for_sha("sha-changed"), 0, "a changed file's sha matches no rows (stale)");
    // Wrong tool_version also fails the key.
    let other_version: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM edge_oracle WHERE tool = ?1 AND tool_version = ?2",
            params![TOOL.as_db_str(), "other-version"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_version, 0);
}

/// `record_oracle_run` inserts a run row and returns its id; `count_edge_oracle_scoped` tallies
/// only the requested kind for the tool/version within the active checkout.
#[test]
fn record_oracle_run_and_count_by_kind() {
    let h = Harness::new();
    let id1 = store::record_oracle_run(&h.conn, TOOL, VERSION, "abc", WORKTREE, "Completed", "{}")
        .unwrap();
    let id2 = store::record_oracle_run(&h.conn, TOOL, VERSION, "abc", WORKTREE, "Completed", "{}")
        .unwrap();
    assert!(id2 > id1, "row id increments");

    let f = h.add_file("a.rs", "x\n");
    let e_up = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    let e_conf = h.add_edge(f, "c", 1, 2, "Exact", None);
    let mk = |edge, kind| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind,
    };
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e_up, OracleResolutionKind::Upgrade))
        .unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e_conf, OracleResolutionKind::Confirm))
        .unwrap();

    assert_eq!(
        store::count_edge_oracle_scoped(
            &h.conn,
            TOOL,
            VERSION,
            COMMIT,
            WORKTREE,
            Some(OracleResolutionKind::Upgrade)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        store::count_edge_oracle_scoped(
            &h.conn,
            TOOL,
            VERSION,
            COMMIT,
            WORKTREE,
            Some(OracleResolutionKind::Contradict)
        )
        .unwrap(),
        0
    );
}

// ---------------------------------------------------------------------------
// status.rs — status construction + serialization, last-run metadata.
// ---------------------------------------------------------------------------

/// `oracle_status` reflects the persisted verdict counts and the most recent run's status/commit;
/// it serializes to JSON with the documented field names.
#[test]
fn oracle_status_reports_counts_and_last_run() {
    let h = Harness::new();
    // No verdicts, no runs yet → zeros and `None` last run.
    let empty = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(empty.total_verdicts, 0);
    assert_eq!(empty.last_run_status, None);
    assert_eq!(empty.last_run_commit_sha, None);

    // Two runs in THIS checkout; the later one wins for "last run". Both recorded under the active
    // `(COMMIT, WORKTREE)` so the worktree-scoped `last_run_meta` can see them (finding 3).
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "Completed", "{}").unwrap();
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "Blocked", "{}").unwrap();

    let f = h.add_file("a.rs", "x\n");
    let e1 = h.add_edge(f, "a", 0, 1, "NameOnly", None);
    let e2 = h.add_edge(f, "b", 1, 2, "Exact", None);
    let mk = |edge, kind| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind,
    };
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e1, OracleResolutionKind::Upgrade))
        .unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e2, OracleResolutionKind::Contradict))
        .unwrap();

    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.tool, "rust-analyzer");
    assert_eq!(status.tool_version, VERSION);
    assert_eq!(status.total_verdicts, 2);
    assert_eq!(status.upgraded, 1);
    assert_eq!(status.contradicted, 1);
    assert_eq!(status.confirmed, 0);
    assert_eq!(status.last_run_status.as_deref(), Some("Blocked"));
    assert_eq!(status.last_run_commit_sha.as_deref(), Some(COMMIT));

    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["total_verdicts"], 2);
    assert_eq!(json["last_run_commit_sha"], COMMIT);
}

// ---------------------------------------------------------------------------
// run.rs — report aggregation, empty/no-scip path, eval metric ratios.
// ---------------------------------------------------------------------------

/// A run with no `.scip` documents (empty index) examines its candidates but writes no verdicts and
/// returns cleanly with `status = "Completed"` — the no-data path is not an error.
#[test]
fn run_with_empty_scip_completes_with_no_verdicts() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let empty = Index::default().write_to_bytes().unwrap();
    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &empty, h.root()).unwrap();

    assert_eq!(report.edges_examined, 1);
    assert_eq!(report.no_occurrence, 1, "no document → no occurrence bucket");
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.oracle_only_calls, 0);
    assert_eq!(report.status, "Completed");
    assert!(h.verdict(edge).is_none());
    // The run is still recorded.
    let runs: i64 = h.conn.query_row("SELECT COUNT(*) FROM oracle_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(runs, 1);
}

/// A candidate whose callee byte range falls outside every occurrence lands in the `no_occurrence`
/// bucket with no verdict written, even though the document has occurrences.
#[test]
fn candidate_outside_any_occurrence_counts_no_occurrence() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    // Callee range 14..20 in source, but the only occurrence covers bytes 0..5 (the `fn ca`
    // prefix).
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(
            0,
            0,
            5,
            "scip-rust crate v1 `other`().",
            SymbolRole::UnspecifiedSymbolRole as i32,
        ),
    ]);

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
    assert_eq!(report.edges_examined, 1);
    assert_eq!(report.no_occurrence, 1);
    assert_eq!(report.rows_written, 0);
    assert!(h.verdict(edge).is_none());
}

/// A run aggregates per-kind counts across multiple edges and computes the recall gap: an in-corpus
/// reference occurrence no edge covered increments `oracle_only_calls`.
#[test]
fn run_aggregates_counts_and_recall_gap() {
    let h = Harness::new();
    // Two call sites of `target` in caller.rs; only one is emitted as an edge → recall gap of 1.
    let caller = h.add_file("caller.rs", "fn caller() { target(); target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    // Edge covers the FIRST call site (bytes 14..20). Second call site (24..30) has no edge.
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let sym = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![
                occurrence(0, 14, 20, sym, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(0, 24, 30, sym, SymbolRole::UnspecifiedSymbolRole as i32),
            ],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
    assert_eq!(report.edges_examined, 1);
    assert_eq!(report.upgraded, 1);
    assert_eq!(report.rows_written, 1);
    assert_eq!(report.oracle_only_calls, 1, "the uncovered second call site is the recall gap");
    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(target_sym));
}

/// A rerun is AUTHORITATIVE for its `(tool, tool_version)`: when the new `.scip` no longer yields a
/// verdict for an edge the prior run covered, that edge's stale verdict must be GONE after the
/// rerun (not left behind by the per-edge upsert). The run clears the scope before writing.
#[test]
fn rerun_clears_stale_verdict_for_dropped_edge() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    // First run: a `.scip` that covers the edge → an Upgrade verdict is written.
    let sym = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(0, 14, 20, sym, SymbolRole::UnspecifiedSymbolRole as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
    assert_eq!(
        h.verdict(edge).map(|(k, _, _)| k).as_deref(),
        Some("upgrade"),
        "first run wrote it"
    );
    assert_eq!(target_sym, target_sym); // (binds target_sym so the def mapping is exercised)

    // Rerun with a `.scip` that has NO occurrence for that edge (the document lost the call site).
    let empty_doc =
        scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
            occurrence(
                0,
                0,
                5,
                "scip-rust crate v1 `other`().",
                SymbolRole::UnspecifiedSymbolRole as i32,
            ),
        ]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &empty_doc, h.root()).unwrap();

    assert!(h.verdict(edge).is_none(), "the dropped edge's stale verdict was cleared on rerun");
    let total: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 0, "rerun left no stale rows for this (tool, version)");
}

/// The recall gap counts only CALL-LIKE occurrences: an `Import`-role reference and a non-callable
/// (`Type`-descriptor) reference are excluded even though they have in-corpus definitions, while an
/// uncovered callable reference IS counted. This keeps imports / type refs from falsely lowering
/// recall (`oracle_only_calls`).
#[test]
fn recall_gap_counts_only_call_like_occurrences() {
    let h = Harness::new();
    // One source file with three uncovered references (no edges emitted for any of them):
    //   - a callable (`Method` suffix) → counts toward the recall gap,
    //   - an import of that same callable (Import role) → excluded,
    //   - a type reference (`Type` suffix) → excluded.
    let src = h.add_file("src.rs", "fn caller() {}\nstruct Thing;\n");
    let _ = h.add_edge(src, "noop", 0, 1, "NameOnly", None); // unrelated edge, distinct occurrence
    // The callable's SCIP definition (line 1, bytes 15..21) must map to one of OUR indexed symbols,
    // otherwise it is (correctly) excluded as not-in-rag-rat's-set. Seed a symbol spanning it.
    h.add_symbol(src, "target", 15, 21);

    let callable = "scip-rust crate v1 `target`().";
    let ty = "scip-rust crate v1 `Thing`#"; // `#` is the Type descriptor suffix in SCIP symbol text
    let index = Index {
        documents: vec![Document {
            relative_path: "src.rs".to_string(),
            occurrences: vec![
                // Uncovered callable reference + its definition (in-corpus) → 1 recall-gap call.
                occurrence(0, 5, 11, callable, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(1, 0, 6, callable, SymbolRole::Definition as i32),
                // An IMPORT of the callable → excluded (Import role).
                occurrence(0, 12, 13, callable, SymbolRole::Import as i32),
                // A type reference + its definition → excluded (not callable).
                occurrence(1, 7, 12, ty, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(1, 7, 12, ty, SymbolRole::Definition as i32),
            ],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    let bytes = index.write_to_bytes().unwrap();
    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    // Only the one uncovered callable reference is the recall gap; the import + type ref are not.
    assert_eq!(report.oracle_only_calls, 1, "only the call-like uncovered reference counts");
}

/// `oracle_eval_metrics` derives precision / recall / recovery rates from the persisted verdicts.
/// One confirm + one contradict → precision 0.5; one upgrade among the low-confidence edges →
/// recovery 1.0.
#[test]
fn eval_metrics_derive_rates_from_persisted_verdicts() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    // Exact edges judged: one confirmed, one contradicted. (to_symbol_id NULL keeps the FK happy;
    // precision is derived from the persisted edge_oracle rows, not the edges row.)
    let e_conf = h.add_edge(f, "c", 0, 1, "Exact", None);
    let e_contra = h.add_edge(f, "d", 1, 2, "Exact", None);
    // A NameOnly edge the oracle upgraded.
    let e_up = h.add_edge(f, "u", 2, 3, "NameOnly", None);

    let mk = |edge, kind, resolved| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: resolved,
        scip_symbol: "s",
        kind,
    };
    store::write_edge_oracle(
        &h.conn,
        TOOL,
        VERSION,
        &mk(e_conf, OracleResolutionKind::Confirm, Some(1)),
    )
    .unwrap();
    store::write_edge_oracle(
        &h.conn,
        TOOL,
        VERSION,
        &mk(e_contra, OracleResolutionKind::Contradict, Some(3)),
    )
    .unwrap();
    store::write_edge_oracle(
        &h.conn,
        TOOL,
        VERSION,
        &mk(e_up, OracleResolutionKind::Upgrade, Some(4)),
    )
    .unwrap();

    // Recall sides come from the run, occurrence-counted over the call population: 3 covered call
    // occurrences + 1 oracle-only gap. (Recall no longer derives from the per-kind verdict sum.)
    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: 3,
        oracle_only: 1,
    })
    .unwrap();
    assert_eq!(m.confirmed, 1);
    assert_eq!(m.contradicted, 1);
    assert_eq!(m.upgraded, 1);
    assert_eq!(m.oracle_only_calls, 1);
    assert_eq!(m.covered_calls, 3);
    // precision = confirm / (confirm + contradict) = 1/2.
    assert!((m.precision - 0.5).abs() < 1e-9);
    // recovery = upgrades / low-confidence-edges-with-oracle = 1/1.
    assert!((m.name_only_recovery_rate - 1.0).abs() < 1e-9);
    // recall = covered_calls / (covered_calls + oracle_only) = 3/4.
    assert!((m.recall - 0.75).abs() < 1e-9, "recall was {}", m.recall);
    // oracle_upgradeable_fraction = (upgrade + external) / unresolved candidates.
    // Only `e_up` is a NameOnly edge carrying a callee range → denominator 1, numerator 1.
    assert!((m.oracle_upgradeable_fraction - 1.0).abs() < 1e-9);
}

/// `oracle_upgradeable_fraction` stays bounded by 1.0 even when the oracle resolved an
/// already-Exact edge to an external dependency. Numerator and denominator must both range over the
/// low-confidence (`NameOnly`/`Ambiguous`) population: a `resolved-external` verdict on an Exact
/// edge is NOT in the denominator, so counting it in the numerator (the old bug) let the fraction
/// exceed 1.0.
#[test]
fn oracle_upgradeable_fraction_is_bounded_by_one() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    // The only low-confidence candidate (denominator = 1): a NameOnly edge the oracle upgraded.
    let e_low = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    // An already-Exact edge the oracle placed to an EXTERNAL dep — NOT in the low-conf denominator.
    let e_exact = h.add_edge(f, "x", 1, 2, "Exact", None);

    let mk = |edge, kind| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind,
    };
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e_low, OracleResolutionKind::Upgrade))
        .unwrap();
    store::write_edge_oracle(
        &h.conn,
        TOOL,
        VERSION,
        &mk(e_exact, OracleResolutionKind::ResolvedExternal),
    )
    .unwrap();

    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    // Old numerator (upgraded 1 + resolved_external 1 = 2) over denominator 1 would be 2.0.
    // Scoped numerator counts only the low-conf upgrade → 1/1 = 1.0.
    assert!(
        m.oracle_upgradeable_fraction <= 1.0,
        "fraction {} exceeds 1.0",
        m.oracle_upgradeable_fraction
    );
    assert!((m.oracle_upgradeable_fraction - 1.0).abs() < 1e-9);
    // The raw counts still report both verdicts for transparency.
    assert_eq!(m.upgraded, 1);
    assert_eq!(m.resolved_external, 1);
}

/// Vacuous denominators yield 1.0 across the board (nothing to get wrong) — the documented
/// hit-rate convention.
#[test]
fn eval_metrics_are_vacuously_perfect_with_no_verdicts() {
    let h = Harness::new();
    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    assert_eq!(m.precision, 1.0);
    assert_eq!(m.recall, 1.0);
    assert_eq!(m.name_only_recovery_rate, 1.0);
    assert_eq!(m.oracle_upgradeable_fraction, 1.0);
}

// ---------------------------------------------------------------------------
// scip.rs — reader branches: empty/malformed bytes, multi-line, encoding fallback.
// ---------------------------------------------------------------------------

/// Malformed protobuf bytes return a clean parse error (not a panic), naming the SCIP index.
#[test]
fn parse_malformed_bytes_returns_clean_error() {
    // A long run of 0xFF is not a valid protobuf wire stream.
    let err = ScipIndex::parse(&[0xFF; 64], |_| Some(Vec::new())).unwrap_err();
    assert!(err.to_string().contains("SCIP"), "error mentions SCIP: {err}");
}

/// Empty bytes parse to an empty index (zero documents) — not an error.
#[test]
fn parse_empty_bytes_yields_empty_maps() {
    let idx = ScipIndex::parse(&[], |_| Some(Vec::new())).unwrap();
    assert!(idx.occurrences_by_path.is_empty());
    assert!(idx.definitions.is_empty());
}

/// A document whose source can't be read is skipped entirely — its occurrences never enter the
/// maps (the correct degradation; the join just finds no oracle data for those edges).
#[test]
fn parse_skips_documents_with_unreadable_source() {
    let bytes =
        scip_bytes("gone.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occurrence(
            0,
            0,
            3,
            "scip-rust crate v1 `f`().",
            SymbolRole::UnspecifiedSymbolRole as i32,
        )]);
    // read_document_source returns None → document dropped.
    let idx = ScipIndex::parse(&bytes, |_| None).unwrap();
    assert!(idx.occurrences_by_path.is_empty());
}

/// A document with no occurrences still registers an (empty) path entry; definitions stay empty.
#[test]
fn parse_document_with_no_occurrences_registers_empty_entry() {
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![]);
    let idx = ScipIndex::parse(&bytes, |_| Some(b"fn a() {}\n".to_vec())).unwrap();
    assert_eq!(idx.occurrences_by_path.get("a.rs").map(Vec::len), Some(0));
    assert!(idx.definitions.is_empty());
}

/// A multi-line occurrence range (`[start_line, start_char, end_line, end_char]`) parses to the
/// byte span crossing the newline.
#[test]
fn parse_multi_line_occurrence_range() {
    // Source: line 0 = "fn a(\n", line 1 = "   b) {}\n". A range from (0,3) to (1,4) spans
    // bytes 3 .. (line1_start 6 + 4) = 3..10.
    let source = b"fn a(\n   b) {}\n".to_vec();
    let occ = Occurrence {
        range: vec![0, 3, 1, 4],
        symbol: "scip-rust crate v1 `span`().".to_string(),
        symbol_roles: SymbolRole::UnspecifiedSymbolRole as i32,
        ..Default::default()
    };
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!(occs.len(), 1);
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (3, 10));
}

/// An unspecified `position_encoding` falls back to one-byte-per-code-unit on ASCII (it behaves
/// like UTF-32: one unit per scalar), so an ASCII identifier resolves to the same span a UTF-8
/// document would.
#[test]
fn parse_unspecified_encoding_falls_back_for_ascii() {
    let source = b"fn a() { foo(); }\n".to_vec();
    // `foo` sits at chars/bytes 9..12 on line 0 (ASCII → all encodings agree).
    let occ = occurrence(
        0,
        9,
        12,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UnspecifiedPositionEncoding, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (9, 12));
}

/// A malformed occurrence range (wrong arity) is dropped, not fatal — the rest of the document
/// still parses.
#[test]
fn parse_drops_malformed_occurrence_range() {
    let source = b"fn a() { foo(); }\n".to_vec();
    let bad = Occurrence {
        range: vec![0, 9], // arity 2 — neither single- nor multi-line shape.
        symbol: "scip-rust crate v1 `foo`().".to_string(),
        symbol_roles: SymbolRole::UnspecifiedSymbolRole as i32,
        ..Default::default()
    };
    let good = occurrence(
        0,
        9,
        12,
        "scip-rust crate v1 `bar`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes =
        scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![bad, good]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    // Only the well-formed range survives.
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].symbol, "scip-rust crate v1 `bar`().");
}

/// A 3-byte UTF-8 scalar (a BMP CJK character) is one UTF-16 unit; an ASCII identifier after it
/// lands at the right byte offset under UTF-16 — exercises the 3-byte `utf8_char_len` arm and the
/// BMP (non-astral) UTF-16 branch.
#[test]
fn parse_utf16_three_byte_bmp_character_offsets_correctly() {
    // Line 0: "中x foo()\n". '中' (U+4E2D) is 3 UTF-8 bytes / 1 UTF-16 unit.
    //   bytes: 中(3 → 0..3) x(byte 3) ' '(byte 4) f o o → `foo` at bytes 5..8.
    //   UTF-16 units before `foo`: 中=1, x=1, space=1 = 3 → `foo` is units 3..6.
    let source = "中x foo()\n".as_bytes().to_vec();
    assert_eq!(&source[5..8], b"foo", "byte offset sanity check");
    let occ = occurrence(
        0,
        3,
        6,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF16CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (5, 8));
}

/// A range whose end column lands BEFORE its start column is malformed and dropped (the
/// `end < start` guard in `byte_range`), not surfaced as an inverted span.
#[test]
fn parse_drops_range_with_end_before_start() {
    let source = b"fn a() { foo(); }\n".to_vec();
    // Single-line range [line 0, start_char 12, end_char 9] → end byte < start byte.
    let occ = occurrence(
        0,
        12,
        9,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!(occs.len(), 0, "inverted range dropped");
}

/// A column that overruns the line is clamped to the line end (the `byte >= line_end` branch in
/// `byte_at`), so an over-long end column resolves to the newline boundary instead of spilling.
#[test]
fn parse_clamps_column_overrun_to_line_end() {
    // Line 0 = "ab\n": line_starts = [0, 3]. An end column of 9 overruns the line → the walk hits
    // `byte >= line_end` and clamps to the line-end boundary (byte 3, the start of line 1).
    let source = b"ab\ncd\n".to_vec();
    let occ =
        occurrence(0, 0, 9, "scip-rust crate v1 `ab`().", SymbolRole::UnspecifiedSymbolRole as i32);
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!(occs[0].start_byte, 0);
    assert_eq!(occs[0].end_byte, 3, "overrun clamped to the line-end boundary");
}

// ---------------------------------------------------------------------------
// join.rs — package extraction, def-range → symbol overlap mapping.
// ---------------------------------------------------------------------------

/// `package_of` / `names_external_package` extract the crate/package name from a package-bearing
/// SCIP symbol and return `None`/false for a local symbol.
#[test]
fn package_extraction_from_scip_symbol() {
    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    assert_eq!(package_of(external).as_deref(), Some("tokio"));
    assert!(names_external_package(external));

    // A `local …` symbol has no package component.
    assert_eq!(package_of("local 0"), None);
    assert!(!names_external_package("local 0"));
}

/// `map_definition_to_symbol` picks the tightest span that REALLY contains the whole definition
/// range (a method beats its enclosing impl) and returns `None` when no span contains it.
#[test]
fn map_definition_to_symbol_prefers_tightest_span() {
    let spans = vec![
        SymbolSpan { symbol_id: 1, start_byte: 0, end_byte: 100 }, // enclosing impl
        SymbolSpan { symbol_id: 2, start_byte: 10, end_byte: 20 }, // tight method
    ];
    // Def 12..16 is contained by both → the tighter (id 2) wins.
    assert_eq!(join::map_definition_to_symbol(&spans, 12, 16), Some(2));
    // Def 50..50 is past the tight method's end (20) → only the enclosing impl contains it.
    assert_eq!(join::map_definition_to_symbol(&spans, 50, 50), Some(1));
    // Def 200..200 is past every span → no containment.
    assert_eq!(join::map_definition_to_symbol(&spans, 200, 200), None);
}

/// REAL containment: a symbol span whose `end_byte` falls BEFORE the definition must NOT match,
/// even though its `start_byte <= def_start`. This is the corrupting case the old
/// `end_byte.max(def_end)` predicate got wrong — a short helper preceding the real target would win
/// via `min_by_key` and record the verdict against the WRONG symbol. The fix requires `def_end <=
/// span.end_byte`.
#[test]
fn map_definition_to_symbol_requires_real_containment_not_just_start() {
    let spans = vec![
        // A short helper that ENDS before the definition (10..20), but starts before def_start.
        SymbolSpan { symbol_id: 1, start_byte: 10, end_byte: 20 },
        // The real enclosing target that actually contains the def (0..100).
        SymbolSpan { symbol_id: 2, start_byte: 0, end_byte: 100 },
    ];
    // Def 50..60 is contained ONLY by id 2; the preceding helper (ends at 20) must be rejected even
    // though 10 <= 50. Under the old `max(def_end)` predicate the tighter helper (10..20) wrongly
    // won. Now only id 2 contains it.
    assert_eq!(join::map_definition_to_symbol(&spans, 50, 60), Some(2));

    // A def whose END spills past the only candidate span (30..40, def 35..50) is NOT contained —
    // partial overlap is not containment, so it returns None rather than a wrong match.
    let one = vec![SymbolSpan { symbol_id: 5, start_byte: 30, end_byte: 40 }];
    assert_eq!(join::map_definition_to_symbol(&one, 35, 50), None);
    // The same span DOES contain a def that fits entirely inside it.
    assert_eq!(join::map_definition_to_symbol(&one, 32, 38), Some(5));
}

// ---------------------------------------------------------------------------
// mod.rs — persisted-enum round trips.
// ---------------------------------------------------------------------------

/// Both persisted enums round-trip through `as_db_str` / `from_db_str` for every variant, and
/// reject an unknown string — the `rust-modern-style` closed-enum contract.
#[test]
fn persisted_enums_round_trip_through_db_strings() {
    assert_eq!(OracleTool::from_db_str(OracleTool::RustAnalyzer.as_db_str()), Some(TOOL));
    assert_eq!(OracleTool::from_db_str("scip-typescript"), None);

    for kind in [
        OracleResolutionKind::Upgrade,
        OracleResolutionKind::ResolvedExternal,
        OracleResolutionKind::Confirm,
        OracleResolutionKind::Contradict,
    ] {
        assert_eq!(OracleResolutionKind::from_db_str(kind.as_db_str()), Some(kind));
    }
    assert_eq!(OracleResolutionKind::from_db_str("nonsense"), None);
}

/// A 4-byte UTF-8 scalar (an astral-plane character) counts as **2** UTF-16 code units; an ASCII
/// identifier after it lands at the right byte offset only when the reader applies that surrogate
/// width — the UTF-16 astral branch in `code_units_for` plus the 4-byte `utf8_char_len` arm.
#[test]
fn parse_utf16_astral_character_shifts_offset_by_surrogate_width() {
    // Source line 0: "𝛂x foo()\n". '𝛂' (U+1D6C2) is 4 UTF-8 bytes / 2 UTF-16 units.
    //   bytes:  𝛂(4 → 0..4) x(byte 4) ' '(byte 5) f o o → `foo` starts at byte 6, ends at 9.
    //   UTF-16 units before `foo`: 𝛂=2, x=1, space=1 = 4 → `foo` is units 4..7.
    let source = "𝛂x foo()\n".as_bytes().to_vec();
    assert_eq!(&source[6..9], b"foo", "byte offset sanity check");
    let occ = occurrence(
        0,
        4,
        7,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF16CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    // Correct surrogate accounting lands the byte range on `foo` (bytes 6..9).
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (6, 9));
}

// ---------------------------------------------------------------------------
// Multi-checkout scoping — the four PR-#81 review findings (#68). Every
// metric/clear/recall read must scope to the run's (commit_sha, worktree_id),
// mirroring `edge_join_candidates`, so a sibling checkout in the same DB can't
// leak into (or be erased by) the active run.
// ---------------------------------------------------------------------------

const OTHER_WORKTREE: &str = "other-wt";

/// Finding 1: `clear_edge_oracle_for_tool` must delete ONLY the active checkout's verdicts. With
/// two worktrees' verdicts for the same `(tool, tool_version)` in one DB, clearing one leaves the
/// other's intact (the clear is scoped via `edge_oracle -> edges -> files`).
#[test]
fn clear_edge_oracle_is_scoped_to_active_checkout() {
    let h = Harness::new();
    // Active-checkout edge (commit_sha="" / worktree_id="") + a verdict.
    let active_file = h.add_file("a.rs", "fn caller() { target(); }\n");
    let active_edge = h.add_edge(active_file, "target", 14, 20, "NameOnly", None);
    // Another worktree's edge (same DB) + a verdict for the SAME tool/version.
    let other_file = h.add_file_in_scope("a.rs", COMMIT, OTHER_WORKTREE);
    let other_edge = h.add_edge(other_file, "target", 14, 20, "NameOnly", None);

    let mk = |edge| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind: OracleResolutionKind::Upgrade,
    };
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(active_edge)).unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(other_edge)).unwrap();
    // Whole-table count (both worktrees) — the production count helper is intentionally scoped to
    // ONE checkout, so this test reads the raw total directly to prove the cross-checkout state.
    let total_rows = || -> i64 {
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |row| row.get(0)).unwrap()
    };
    assert_eq!(total_rows(), 2);
    // The scoped count sees ONLY the active checkout's verdict.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );

    // Clear the ACTIVE checkout's scope only.
    store::clear_edge_oracle_for_tool(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();

    assert!(h.verdict(active_edge).is_none(), "active checkout's verdict cleared");
    assert!(h.verdict(other_edge).is_some(), "the other worktree's verdict is untouched");
    assert_eq!(total_rows(), 1, "only the other worktree's verdict remains in the table");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "active checkout's scoped count is now zero"
    );
}

/// Finding 2: a low-confidence edge in ANOTHER worktree must not inflate the current run's
/// `oracle_upgradeable_fraction` denominator. With one upgraded low-conf edge in-scope and an
/// extra unresolved low-conf edge out-of-scope, the scoped fraction is 1/1 = 1.0 (not 1/2).
#[test]
fn upgradeable_fraction_denominator_is_scoped_to_active_checkout() {
    let h = Harness::new();
    // Active checkout: one NameOnly edge the oracle upgraded → numerator 1, denominator 1.
    let active = h.add_file("a.rs", "fn caller() {}\n");
    let e_low = h.add_edge(active, "u", 0, 1, "NameOnly", None);
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &EdgeOracleRow {
        edge_id: e_low,
        file_sha: "s",
        resolved_symbol_id: Some(1),
        scip_symbol: "s",
        kind: OracleResolutionKind::Upgrade,
    })
    .unwrap();

    // Another worktree: an unresolved NameOnly candidate carrying a callee range, NO verdict. If
    // the denominator weren't scoped, it would count → fraction 1/2.
    let other = h.add_file_in_scope("a.rs", COMMIT, OTHER_WORKTREE);
    let _ = h.add_edge(other, "v", 0, 1, "NameOnly", None);

    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    assert!(
        (m.oracle_upgradeable_fraction - 1.0).abs() < 1e-9,
        "scoped fraction is 1/1, not diluted to 1/2 by the other worktree; got {}",
        m.oracle_upgradeable_fraction
    );
}

/// Finding 3: a `.scip` definition in a file rag-rat did NOT index (no matching DB symbol) is
/// excluded from the recall gap — counting calls to un-indexed tests/examples/generated/dependency
/// sources as misses is a false negative. The same callable, when its def maps to an indexed
/// symbol, IS counted; without an indexed symbol it drops to zero.
#[test]
fn recall_gap_excludes_definitions_in_unindexed_files() {
    // The `.scip` references `target`, defined in `gen.rs` — a file with occurrences but whose
    // definition byte range maps to NO indexed symbol (rag-rat never indexed gen.rs's symbols).
    let build = |seed_symbol: bool| -> u64 {
        let h = Harness::new();
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        let _ = h.add_edge(caller, "noop", 0, 1, "NameOnly", None); // unrelated, distinct occurrence
        let gen_file = h.add_file("gen.rs", "fn target() {}\n");
        if seed_symbol {
            // Seed the def symbol so the callable resolves to OUR corpus → counted.
            h.add_symbol(gen_file, "target", 3, 9);
        }

        let callable = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                // Uncovered callable reference (no edge covers it) at bytes 14..20.
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    callable,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "gen.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, callable, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root())
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(false), 0, "a def in an un-indexed file is NOT a recall gap");
    assert_eq!(build(true), 1, "the same callable IS a recall gap once its def maps to a symbol");
}

/// Round-3 finding (occurrence side of the recall gap): a `.scip` occurrence whose *call site*
/// lives in a SOURCE document rag-rat did NOT index in this checkout is excluded from the recall
/// gap, even when the callee *definition* resolves to an indexed symbol. No edge candidate can ever
/// cover a call from an unindexed file (`edge_join_candidates` only emits candidates for indexed
/// files), so counting it as an uncovered call is a false miss. The same callable, when its call
/// site IS an indexed file, IS counted — proving the filter is the occurrence path, not the
/// definition.
#[test]
fn recall_gap_excludes_occurrences_in_unindexed_source_files() {
    // `target` is defined in an INDEXED file with a seeded symbol (so the def-side filter passes).
    // The uncovered call occurrence lives in `caller.rs`; we vary whether rag-rat indexed it.
    let build = |index_caller: bool| -> u64 {
        let h = Harness::new();
        // The def file is always indexed + its symbol seeded → the def-side filter never trips.
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        h.add_symbol(defs, "target", 3, 9);
        if index_caller {
            // caller.rs IS an indexed file in this checkout → the call site is in scope.
            h.add_file("caller.rs", "fn caller() { target(); }\n");
        }
        // else: caller.rs exists in the `.scip` but is NOT a `files` row → out-of-scope call site.

        let callable = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                // Uncovered callable reference (no edge covers it) at bytes 14..20.
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    callable,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, callable, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root())
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(false), 0, "a call from an un-indexed source file is NOT a recall gap");
    assert_eq!(build(true), 1, "the same call IS a recall gap once its source file is indexed");
}

/// Round-3 finding (the comprehensive audit): a sibling checkout's `edge_oracle` rows for the SAME
/// `(tool, tool_version)` must not perturb the active checkout's `verdict_counts`-derived metrics
/// (precision/recall) OR its status. Checkout A sees 1 confirm + 1 contradict (precision 0.5);
/// checkout B has its own confirm/confirm/upgrade rows. Before the round-3 fix `verdict_counts` was
/// a global `(tool, tool_version)` count, so B's confirms would inflate A's numerator and break
/// both precision and recall. This pins the whole metric path to the active checkout.
#[test]
fn verdict_counts_and_metrics_ignore_sibling_checkout_rows() {
    let h = Harness::new();
    let mk = |edge, kind| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind,
    };

    // Active checkout A: one confirmed + one contradicted Exact edge → precision 1/2.
    let a_file = h.add_file("a.rs", "fn caller() {}\n");
    let a_conf = h.add_edge(a_file, "c", 0, 1, "Exact", None);
    let a_contra = h.add_edge(a_file, "d", 1, 2, "Exact", None);
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(a_conf, OracleResolutionKind::Confirm))
        .unwrap();
    store::write_edge_oracle(
        &h.conn,
        TOOL,
        VERSION,
        &mk(a_contra, OracleResolutionKind::Contradict),
    )
    .unwrap();

    // Sibling checkout B (same DB, same tool/version): TWO confirms + an upgrade. If A's counts
    // leaked B's rows, A's precision would jump to 3/4 and its recall would change.
    let b_file = h.add_file_in_scope("a.rs", COMMIT, OTHER_WORKTREE);
    let b_conf1 = h.add_edge(b_file, "e", 0, 1, "Exact", None);
    let b_conf2 = h.add_edge(b_file, "f", 1, 2, "Exact", None);
    let b_up = h.add_edge(b_file, "g", 2, 3, "NameOnly", None);
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(b_conf1, OracleResolutionKind::Confirm))
        .unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(b_conf2, OracleResolutionKind::Confirm))
        .unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(b_up, OracleResolutionKind::Upgrade))
        .unwrap();

    // A's metrics: precision = 1 confirm / (1 confirm + 1 contradict) = 0.5; recall over A's
    // covered call set (2 covered call occurrences) and 0 oracle-only = 2/2 = 1.0. The recall
    // counts come from the run; B's three verdict rows never enter A's precision either.
    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: 2,
        oracle_only: 0,
    })
    .unwrap();
    assert_eq!(m.confirmed, 1, "only A's confirm counts");
    assert_eq!(m.contradicted, 1);
    assert_eq!(m.upgraded, 0, "B's upgrade does not leak into A");
    assert!(
        (m.precision - 0.5).abs() < 1e-9,
        "precision unperturbed by B's confirms; got {}",
        m.precision
    );
    assert!((m.recall - 1.0).abs() < 1e-9, "recall is A's covered set only; got {}", m.recall);

    // The status read shares the same scoped `verdict_counts`, so it is scoped identically.
    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.total_verdicts, 2, "status counts only A's two verdicts");
    assert_eq!(status.confirmed, 1);
    assert_eq!(status.contradicted, 1);
    assert_eq!(status.upgraded, 0);

    // Sanity: checkout B's own scoped status sees its three rows, proving the rows really exist.
    let status_b = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, OTHER_WORKTREE).unwrap();
    assert_eq!(status_b.total_verdicts, 3);
    assert_eq!(status_b.confirmed, 2);
    assert_eq!(status_b.upgraded, 1);
}

/// Finding 4: an already-resolved (`Exact`) edge pointing at an IN-CORPUS target, when SCIP
/// resolves the same call to an EXTERNAL definition, is a CONTRADICTION (the heuristic picked the
/// wrong target) — not `resolved-external`. It must count in `confirm + contradict` and lower
/// precision.
#[test]
fn exact_in_corpus_edge_contradicted_by_external_scip_resolution() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { spawn(); }\n");
    let defs = h.add_file("defs.rs", "fn spawn() {}\n");
    // The heuristic resolved `spawn` to an IN-CORPUS symbol (Exact).
    let in_corpus = h.add_symbol(defs, "spawn", 3, 8);
    let edge = h.add_edge(caller, "spawn", 14, 19, "Exact", Some(in_corpus));

    // SCIP says `spawn` is the external tokio::spawn — a package-bearing symbol with NO in-corpus
    // definition.
    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 19, external, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    let (kind, resolved, scip) = h.verdict(edge).expect("verdict written");
    assert_eq!(
        kind,
        OracleResolutionKind::Contradict.as_db_str(),
        "external-vs-in-corpus disagreement is a contradiction, not resolved-external"
    );
    assert_eq!(resolved, None, "no in-corpus symbol — the real target is external");
    assert_eq!(scip, external);
    assert_eq!(report.contradicted, 1);
    assert_eq!(report.resolved_external, 0, "it is NOT counted as resolved-external");
    // Heuristic row untouched.
    assert_eq!(h.heuristic_resolution(edge), ("exact".to_string(), Some(in_corpus)));

    // Precision counts it honestly: 0 confirmed / (0 + 1 contradicted) = 0.0.
    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    assert!(
        (m.precision - 0.0).abs() < 1e-9,
        "contradiction lowers precision; got {}",
        m.precision
    );
    assert_eq!(m.contradicted, 1);
}

/// Finding 4 (counterpart): an UNRESOLVED / `NameOnly` edge SCIP places externally is still
/// `resolved-external` — there is no in-corpus claim to contradict. This pins the boundary so the
/// contradiction rule doesn't swallow the legitimate external-recovery case.
#[test]
fn name_only_edge_with_external_scip_stays_resolved_external() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { spawn(); }\n");
    // NameOnly + unresolved (no heuristic symbol) → no in-corpus claim.
    let edge = h.add_edge(caller, "spawn", 14, 19, "NameOnly", None);

    let external = "scip-rust cargo tokio 1.0 `spawn`().";
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 19, external, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();

    let (kind, _, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(report.resolved_external, 1);
    assert_eq!(report.contradicted, 0);
}

/// The `(None, Some(_))` join arm: SCIP HAS a definition document for the callee, but that
/// definition's byte range maps to NO indexed symbol (the def lives in an un-indexed file) → the
/// callee is external. A NameOnly edge there is `resolved-external`. Distinct from the package-only
/// `(None, None)` path covered above — this exercises the in-`.scip`-but-out-of-corpus branch.
#[test]
fn scip_definition_outside_indexed_corpus_resolves_external() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    // `defs.rs` is indexed as a file but we seed NO symbol for `target`, so the def maps to
    // nothing.
    let _defs = h.add_file("defs.rs", "fn target() {}\n");
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    // A package-bearing symbol WITH a definition occurrence in defs.rs → goes through (None, Some).
    let symbol = "scip-rust crate v1 `target`().";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                20,
                symbol,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved, None, "def maps to no indexed symbol → external");
    assert_eq!(report.resolved_external, 1);
}

/// A reference occurrence whose symbol has NO SCIP definition and names NO package is the
/// no-actionable-data `(None, None)` drop: the join returns `None`, no verdict is written. (A
/// non-`local`, non-package, definition-less symbol — e.g. a malformed/synthetic one.)
#[test]
fn reference_without_definition_or_package_yields_no_verdict() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { mystery(); }\n");
    let edge = h.add_edge(caller, "mystery", 14, 21, "NameOnly", None);

    // A bare symbol with no `Definition` occurrence anywhere and no package component.
    let bare = "scip-rust  `mystery`().";
    assert!(!join::names_external_package(bare), "fixture must have no package");
    let bytes = scip_bytes("caller.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 14, 21, bare, SymbolRole::UnspecifiedSymbolRole as i32),
    ]);

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
    assert!(h.verdict(edge).is_none(), "no definition + no package → no verdict");
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.no_occurrence, 1, "dropped into the no-actionable bucket");
}

// ---------------------------------------------------------------------------
// #81 review fixes — recall population, content integrity, worktree scope, tombstones.
// ---------------------------------------------------------------------------

/// Finding 1 (gap side): an in-corpus FIELD/CONST read — a SCIP `Term` descriptor ending in a bare
/// `.` (no `()`), with an in-corpus definition — is NOT a callable, so it must NOT count as a
/// missed call in the recall gap. Before the `symbol_is_callable` tightening (`).` not bare `.`) it
/// slipped through and deflated recall. The same occurrence as a genuine `Method` (`).`) IS
/// counted, proving the suffix is what gates it.
#[test]
fn recall_gap_excludes_field_const_term_reads() {
    let build = |callable: bool| -> u64 {
        let h = Harness::new();
        let src = h.add_file("src.rs", "fn caller() {}\n");
        // Seed the def symbol so the def-side filter passes — the ONLY thing left deciding the gap
        // is callability.
        h.add_symbol(src, "VALUE", 3, 8);
        // A `Term` (bare `.`) is a const/field read; a `Method` (`).`) is a call.
        let symbol =
            if callable { "scip-rust crate v1 `VALUE`()." } else { "scip-rust crate v1 `VALUE`." };
        let index = Index {
            documents: vec![Document {
                relative_path: "src.rs".to_string(),
                occurrences: vec![
                    // Uncovered reference (no edge covers it) at bytes 5..10.
                    occurrence(0, 5, 10, symbol, SymbolRole::UnspecifiedSymbolRole as i32),
                    // Its in-corpus definition (line 0, bytes 3..8).
                    occurrence(0, 3, 8, symbol, SymbolRole::Definition as i32),
                ],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root())
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(false), 0, "a bare-`.` Term (field/const read) is NOT a missed call");
    assert_eq!(build(true), 1, "the same occurrence as a `).` Method IS a missed call");
}

/// Finding 1 (covered side): the covered side of recall counts ONLY `calls_name`-edge occurrences.
/// A `references_type` edge that the oracle CONFIRMS carries a callee byte range (so it joins and
/// gets a verdict), but it is not a *call* — it must NOT inflate `covered_calls`. A `calls_name`
/// edge over a DIFFERENT occurrence does count. So with one confirmed type-ref edge and one covered
/// call, `covered_calls == 1`, not 2.
#[test]
fn covered_side_ignores_references_type_confirmation() {
    let h = Harness::new();
    // `caller.rs`: a call to `target` at 14..20 and a type reference `Thing` at 24..29.
    let caller = h.add_file("caller.rs", "fn caller() { target(); Thing::new(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nstruct Thing;\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let thing_sym = h.add_symbol(defs, "Thing", 22, 27);
    // A CALL edge (covers the call occurrence) and a TYPE-REF edge (covers the type occurrence).
    let call_edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    let type_edge =
        h.add_edge_with_kind(caller, "Thing", 24, 29, "references_type", "Exact", Some(thing_sym));

    let call_sym = "scip-rust crate v1 `target`().";
    let type_sym = "scip-rust crate v1 `Thing`#";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![
                occurrence(0, 14, 20, call_sym, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(0, 24, 29, type_sym, SymbolRole::UnspecifiedSymbolRole as i32),
            ],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    index.documents.push(Document {
        relative_path: "defs.rs".to_string(),
        occurrences: vec![
            occurrence(0, 3, 9, call_sym, SymbolRole::Definition as i32),
            occurrence(1, 7, 12, type_sym, SymbolRole::Definition as i32),
        ],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report = run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
    // BOTH edges got verdicts (both carry callee ranges and join)…
    assert!(h.verdict(call_edge).is_some(), "call edge verdicted");
    assert!(h.verdict(type_edge).is_some(), "type-ref edge verdicted");
    // …but only the CALL occurrence counts toward the covered side of recall.
    assert_eq!(
        report.covered_calls, 1,
        "the references_type confirmation does NOT inflate covered"
    );
    assert_eq!(report.oracle_only_calls, 0, "both call-like occurrences were covered");

    let m = super::oracle_eval_metrics(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, RecallCalls {
        covered: report.covered_calls,
        oracle_only: report.oracle_only_calls,
    })
    .unwrap();
    assert!(
        (m.recall - 1.0).abs() < 1e-9,
        "recall over the call population only; got {}",
        m.recall
    );
    assert_eq!(m.covered_calls, 1);
}

/// Finding 2: a candidate whose recorded `file_sha` no longer matches the disk bytes (content drift
/// between the index build and the `.scip`) is SKIPPED — no verdict is emitted from mismatched
/// content — and tallied in `skipped_drifted`. The same edge, with a matching `file_sha`, IS
/// verdicted, proving the gate is the sha comparison.
#[test]
fn drifted_file_sha_is_skipped_not_verdicted() {
    // (verdict row: kind, resolved_symbol_id, scip_symbol; skipped_drifted; examined)
    type DriftProbe = (Option<(String, Option<i64>, String)>, u64, u64);
    let build = |drift: bool| -> DriftProbe {
        let h = Harness::new();
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let target_sym = h.add_symbol(defs, "target", 3, 9);
        let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);
        if drift {
            // The edge was indexed against a DIFFERENT sha than the file on disk now.
            h.set_file_sha(
                caller,
                "0000000000000000000000000000000000000000000000000000000000000000",
            );
        }

        let sym = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    sym,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let _ = target_sym;
        let bytes = index.write_to_bytes().unwrap();
        let report =
            run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root()).unwrap();
        (h.verdict(edge), report.skipped_drifted, report.rows_written)
    };

    let (verdict, skipped, written) = build(true);
    assert!(verdict.is_none(), "a drifted candidate must NOT be verdicted");
    assert_eq!(skipped, 1, "the drifted candidate is tallied in skipped_drifted");
    assert_eq!(written, 0, "nothing written from drifted content");

    let (verdict, skipped, written) = build(false);
    assert!(verdict.is_some(), "the same candidate with a matching file_sha IS verdicted");
    assert_eq!(skipped, 0, "no drift when the sha matches");
    assert_eq!(written, 1);
}

/// Finding 3: a run recorded for ANOTHER worktree (same tool/version/commit) does NOT surface as
/// this checkout's last run. `oracle_runs` now carries `worktree_id` and `last_run_meta` filters on
/// it, so the status read describes only the active checkout — consistent with its worktree-scoped
/// verdict counts.
#[test]
fn last_run_meta_is_scoped_to_active_worktree() {
    let h = Harness::new();
    // A run in a SIBLING worktree (same tool/version/commit). It must not be THIS checkout's last.
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, OTHER_WORKTREE, "Completed", "{}")
        .unwrap();

    // This checkout has no run yet → no last run, despite the sibling's row existing.
    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.last_run_status, None, "the sibling worktree's run is not ours");
    assert_eq!(status.last_run_commit_sha, None);

    // Record a run in THIS checkout → now it's the last run.
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, "Blocked", "{}").unwrap();
    let status = super::oracle_status(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(status.last_run_status.as_deref(), Some("Blocked"));
}

/// Finding 5: a file marked deleted (a `kind='deleted'` tombstone left by `mark_file_deleted`) but
/// still present in the `.scip` must NOT have its occurrences inflate the recall gap. The tombstone
/// is not "indexed in scope", so an uncovered call from it is out of scope, not a miss. A live file
/// with the same occurrence IS counted.
#[test]
fn deleted_file_occurrences_do_not_inflate_gap() {
    let build = |deleted: bool| -> u64 {
        let h = Harness::new();
        // `target` is defined in an indexed, live file (def-side filter passes).
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        h.add_symbol(defs, "target", 3, 9);
        // The call site lives in caller.rs.
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        if deleted {
            // Tombstone it: a `kind='deleted'` row, as `mark_file_deleted` leaves.
            h.conn
                .execute("UPDATE files SET kind = 'deleted' WHERE id = ?1", params![caller])
                .unwrap();
        }

        let sym = "scip-rust crate v1 `target`().";
        let mut index = Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                occurrences: vec![occurrence(
                    0,
                    14,
                    20,
                    sym,
                    SymbolRole::UnspecifiedSymbolRole as i32,
                )],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        index.documents.push(Document {
            relative_path: "defs.rs".to_string(),
            occurrences: vec![occurrence(0, 3, 9, sym, SymbolRole::Definition as i32)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        });
        let bytes = index.write_to_bytes().unwrap();
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root())
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(true), 0, "a tombstoned source file's occurrence is NOT a recall gap");
    assert_eq!(build(false), 1, "the same call from a live indexed file IS a recall gap");
}

/// Finding 6a: the oracle's checkout scope is leak-proof BY CONSTRUCTION — every raw `edge_oracle`
/// query lives in `store.rs` (which owns the scoped helpers + the `EDGE_ORACLE_SCOPE_JOIN`
/// predicate). This source-scan test fails CI if a future unscoped `FROM edge_oracle` query is
/// added to any other oracle module (run.rs / status.rs / join.rs / scip.rs), forcing it back
/// through the scoped helper.
#[test]
fn raw_edge_oracle_queries_live_only_in_store() {
    let oracle_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/index/oracle");
    for entry in std::fs::read_dir(&oracle_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        // store.rs owns the scoped helpers; tests.rs exercises them with direct assertions.
        if name == "store.rs" || name == "tests.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("FROM edge_oracle"),
            "{name} contains a raw `FROM edge_oracle` query — route it through \
             store::count_edge_oracle_scoped / EDGE_ORACLE_SCOPE_JOIN so it can't drop the \
             checkout scope",
        );
    }
}

/// Finding 6b: `name_only_recovery_rate` cannot exceed 1.0 even if a writer bug stamps an `upgrade`
/// verdict on an `Exact`-confidence edge (e.g. an Exact edge with a NULL `to_symbol_id` that
/// `classify_resolved` would treat as unresolved). The numerator is now scoped to `upgrade`
/// verdicts on `NameOnly`/`Ambiguous` edges only, so the stray Exact upgrade is excluded — the rate
/// stays over the low-confidence population the denominator counts.
#[test]
fn name_only_recovery_rate_excludes_exact_upgrades() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");
    // The legitimate low-confidence population: one NameOnly edge the oracle upgraded (1/1 = 1.0).
    let e_low = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    // A pathological Exact edge ALSO stamped `upgrade` (the writer-bug shape). It is NOT in the
    // low-confidence denominator, so admitting it in the numerator (the old raw `counts.upgraded`)
    // would push the rate to 2/1 = 2.0.
    let e_exact = h.add_edge(f, "x", 1, 2, "Exact", None);

    let mk = |edge, kind| EdgeOracleRow {
        edge_id: edge,
        file_sha: "s",
        resolved_symbol_id: None,
        scip_symbol: "s",
        kind,
    };
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e_low, OracleResolutionKind::Upgrade))
        .unwrap();
    store::write_edge_oracle(&h.conn, TOOL, VERSION, &mk(e_exact, OracleResolutionKind::Upgrade))
        .unwrap();

    let m = super::oracle_eval_metrics(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        RecallCalls::default(),
    )
    .unwrap();
    // Raw count still reports both upgrade rows for transparency…
    assert_eq!(m.upgraded, 2);
    // …but the rate is scoped to the low-confidence population: 1 low-conf upgrade / 1 low-conf
    // edge-with-oracle = 1.0, never 2.0.
    assert!(
        m.name_only_recovery_rate <= 1.0,
        "recovery rate {} exceeds 1.0",
        m.name_only_recovery_rate
    );
    assert!((m.name_only_recovery_rate - 1.0).abs() < 1e-9);
}
