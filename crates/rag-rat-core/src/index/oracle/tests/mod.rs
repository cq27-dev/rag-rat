//! End-to-end tests for the SCIP-oracle join. SCIP `Index` objects are built **programmatically**
//! via the `scip` crate's types (no rust-analyzer, no network) and serialized, then fed through the
//! real `run_oracle` path against a DB seeded with synthetic files/symbols/edges. This keeps the
//! join deterministic and exercises the exact code eval uses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ::protobuf::{EnumOrUnknown, Message};
use ::scip::types::{Document, Index, Occurrence, PositionEncoding, SymbolRole};
use rusqlite::{Connection, params};

use super::store::EdgeOracleRow;
use super::*;
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
// Model a REAL clean git checkout: a non-empty `commit_sha`, empty `worktree_id` (the
// `FileScope::commit` case — the dominant production shape). The earlier `"" / ""` pair only
// occurs in a non-git temp dir, where the active-checkout predicate degenerates and silently
// masked the #82 P0 (the AND-of-both-non-empty predicate matched zero rows on every real repo).
const COMMIT: &str = "deadbeefcafef00d";
const WORKTREE: &str = "";

/// The content-key fields of a live edge, owned so an [`EdgeOracleRow`] borrowing them outlives the
/// borrow (#248: verdicts are content-keyed, not rowid-keyed).
struct EdgeContentKey {
    source_path: String,
    source_start_byte: i64,
    source_end_byte: i64,
    callee_start_byte: i64,
    callee_end_byte: i64,
    edge_kind: String,
}

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
        // #224: qualified_name interned into name_strings (here name == qualified_name).
        self.conn
            .execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![name])
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, \
                 start_byte, end_byte, start_line, end_line)
                 VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?2), \
                 'function', ?3, ?4, 1, 1)",
                params![file_id, name, start_byte as i64, end_byte as i64],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    /// Insert a symbol with an explicit qualified name + kind (the production shape is
    /// path-qualified), returning its id. The moniker tests need the qualified name to CHANGE on a
    /// file move so the qualified-name relocation arm can't fire.
    fn add_symbol_qualified(
        &self,
        file_id: i64,
        name: &str,
        qualified_name: &str,
        kind: &str,
        start_byte: usize,
        end_byte: usize,
    ) -> i64 {
        self.conn
            .execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![
                qualified_name
            ])
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, \
                 start_byte, end_byte, start_line, end_line)
                 VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3), ?4, ?5, \
                 ?6, 1, 1)",
                params![file_id, name, qualified_name, kind, start_byte as i64, end_byte as i64],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    /// Insert a logical symbol group (explicit content-derived-style id) with one member.
    fn add_logical_symbol(
        &self,
        logical_symbol_id: i64,
        path: &str,
        name: &str,
        qualified_name: &str,
        symbol_id: i64,
    ) {
        self.conn
            .execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![
                qualified_name
            ])
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, \
                 kind, variant_count, group_reason)
                 VALUES (?1, 'rust', ?2, ?3, (SELECT id FROM name_strings WHERE value = ?4), \
                 'function', 1, 'single')",
                params![logical_symbol_id, path, name, qualified_name],
            )
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, cfg_expr, \
                 signature_hash, start_line, end_line) VALUES (?1, ?2, NULL, NULL, 1, 1)",
                params![logical_symbol_id, symbol_id],
            )
            .unwrap();
    }

    /// Insert a chunk for a symbol (the memory bind/validate path reads the symbol's chunk for its
    /// content hash and line span; a symbol row without one is not a shape the indexer produces).
    fn add_chunk(&self, file_id: i64, symbol_path: &str, text: &str) {
        self.conn
            .execute(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, \
                 start_line, end_line, text_hash) VALUES (?1, 'symbol', ?2, 0, ?3, 1, 1, ?4)",
                params![file_id, symbol_path, text.len() as i64, sha256_hex(text.as_bytes())],
            )
            .unwrap();
        // chunks.text is gone (#77 Phase 2); seed the compressed chunk_text blob readers INNER
        // JOIN.
        let chunk_id = self.conn.last_insert_rowid();
        crate::index::chunk_text_store::seed_chunk_text(&self.conn, chunk_id, text).unwrap();
    }

    /// The persisted moniker row for a logical symbol, if any.
    fn moniker(&self, logical_symbol_id: i64) -> Option<(String, String, String)> {
        self.conn
            .query_row(
                "SELECT moniker, tool, tool_version FROM logical_symbol_monikers WHERE \
                 logical_symbol_id = ?1",
                params![logical_symbol_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok()
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
        // `edges` is a view; `last_insert_rowid` does not survive its INSTEAD OF trigger (#79).
        self.conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
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

    /// The persisted oracle verdict for an edge, if any. Looks the row up by the edge's CONTENT key
    /// (#248: `edge_oracle` no longer carries `edge_id`) — joining the live edge by path + source +
    /// callee spans + edge_kind, the same key the production read join uses. NOTE: unlike the
    /// surfacing reads, this does NOT gate on `files.sha256 = file_sha`, so a test that wrote a
    /// non-matching `file_sha` still sees its row (the persisted-population view).
    fn verdict(&self, edge_id: i64) -> Option<(String, Option<i64>, String)> {
        self.conn
            .query_row(
                "SELECT eo.kind, eo.resolved_symbol_id, eo.scip_symbol
                 FROM edge_oracle eo
                 JOIN edges ON edges.source_start_byte = eo.source_start_byte
                           AND edges.source_end_byte = eo.source_end_byte
                           AND edges.callee_start_byte = eo.callee_start_byte
                           AND edges.callee_end_byte = eo.callee_end_byte
                           AND edges.edge_kind = eo.edge_kind
                 JOIN files ON files.id = edges.source_file_id AND files.path = eo.source_path
                 WHERE edges.id = ?1",
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

    /// The recorded `files.sha256` for a path in the active checkout — the "current content" sha a
    /// verdict must carry to be counted/surfaced (the scope join + current predicate gate on it).
    fn file_sha(&self, path: &str) -> String {
        self.conn
            .query_row("SELECT sha256 FROM files WHERE path = ?1", params![path], |r| r.get(0))
            .unwrap()
    }

    /// The recorded `files.sha256` for a path in a specific commit scope — for verdicts written
    /// against a sibling checkout's file (disambiguates the same path across two commits).
    fn file_sha_for_commit(&self, path: &str, commit: &str) -> String {
        self.conn
            .query_row(
                "SELECT sha256 FROM files WHERE path = ?1 AND commit_sha = ?2",
                params![path, commit],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The content-key fields of a live edge (`source_path`, source/callee byte spans,
    /// `edge_kind`), for building an [`EdgeOracleRow`] in tests that reference an edge by id.
    /// Mirrors what the production write path reads from each [`store::EdgeJoinCandidate`].
    fn edge_content_key(&self, edge_id: i64) -> EdgeContentKey {
        self.conn
            .query_row(
                "SELECT files.path, edges.source_start_byte, edges.source_end_byte,
                        edges.callee_start_byte, edges.callee_end_byte, edges.edge_kind
                 FROM edges JOIN files ON files.id = edges.source_file_id
                 WHERE edges.id = ?1",
                params![edge_id],
                |row| {
                    Ok(EdgeContentKey {
                        source_path: row.get(0)?,
                        source_start_byte: row.get(1)?,
                        source_end_byte: row.get(2)?,
                        callee_start_byte: row.get(3)?,
                        callee_end_byte: row.get(4)?,
                        edge_kind: row.get(5)?,
                    })
                },
            )
            .unwrap()
    }

    /// Write an `edge_oracle` verdict for a live edge, deriving the content key from the edge so
    /// the row matches the production read join by construction. The `EdgeContentKey` outlives
    /// the row.
    fn write_verdict(
        &self,
        edge_id: i64,
        file_sha: &str,
        resolved_symbol_id: Option<i64>,
        scip_symbol: &str,
        kind: OracleResolutionKind,
    ) {
        let key = self.edge_content_key(edge_id);
        store::write_edge_oracle(&self.conn, TOOL, VERSION, &EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha,
            resolved_symbol_id,
            scip_symbol,
            kind,
        })
        .unwrap();
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

// A sibling checkout sharing the same DB: a DIFFERENT commit, clean (empty worktree). Modelling the
// sibling as a distinct commit (rather than the same commit + a second worktree id) matches the
// real shape: two checkouts at the same HEAD is unusual, and under the active-checkout predicate a
// same-commit worktree overlay would shadow the clean row by path, which is not the
// cross-checkout-isolation property these tests mean to assert. Commit isolation is.
const OTHER_COMMIT: &str = "5ad1f1ce5ad1f1ce";
const OTHER_WORKTREE: &str = "";

/// Build a multi-document SCIP index, serialized.
fn scip_bytes_docs(docs: Vec<(&str, Vec<Occurrence>)>) -> Vec<u8> {
    let documents = docs
        .into_iter()
        .map(|(path, occurrences)| Document {
            relative_path: path.to_string(),
            occurrences,
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        })
        .collect();
    let index = Index { documents, ..Default::default() };
    index.write_to_bytes().unwrap()
}

const TARGET_MONIKER: &str = "rust-analyzer cargo test_crate 0.1.0 target().";

/// Create a memory bound to the harness symbol, asserting the automatic `scip_moniker` binding.
fn create_target_memory(h: &Harness, symbol_id: i64) -> String {
    use crate::query::memory::{RepoMemoryBindTarget, RepoMemoryCreate, create_memory};

    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target invariant".to_string(),
        body: "target must stay reentrant".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { symbol_id: Some(symbol_id), ..Default::default() },
    })
    .unwrap();
    assert!(!created.duplicate);
    let moniker_binding =
        created.memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect(
            "memory on a symbol with a known moniker gets the moniker binding automatically",
        );
    assert_eq!(moniker_binding.binding_id, TARGET_MONIKER);
    assert_eq!(moniker_binding.moniker_tool.as_deref(), Some(TOOL.as_db_str()));
    assert_eq!(moniker_binding.moniker_tool_version.as_deref(), Some(VERSION));
    created.memory.memory_id
}

/// Simulate a file move WITH a content edit: the old file/symbol/logical rows die, the new home
/// has a different path, qualified name, AND content — so neither the qualified-name arm nor the
/// name+content-hash arm can relocate, only the moniker can. Returns the new symbol id.
fn move_target_with_edit(h: &Harness, old_file: i64, new_kind: &str) -> i64 {
    h.conn.execute("DELETE FROM logical_symbol_members", []).unwrap();
    h.conn.execute("DELETE FROM logical_symbols", []).unwrap();
    h.conn.execute("DELETE FROM symbols", []).unwrap();
    h.conn.execute("DELETE FROM files WHERE id = ?1", params![old_file]).unwrap();
    std::fs::remove_file(h.root().join("defs.rs")).unwrap();

    let moved = h.add_file("moved.rs", "fn target(changed: u32) {}\n");
    let sym = h.add_symbol_qualified(moved, "target", "moved.rs::target", new_kind, 0, 26);
    h.add_chunk(moved, "moved.rs::target", "fn target(changed: u32) {}\n");
    h.add_logical_symbol(2002, "moved.rs", "target", "moved.rs::target", sym);
    sym
}

mod edge_view;
mod join_tests;
mod memory_bindings;
mod monikers;
mod persisted_enums;
mod pre_spawn;
mod production;
mod reports;
mod resolution;
mod run_eval;
mod schema_tests;
mod scip_parse;
mod scope;
mod status_tests;
mod store_io;
mod surfacing;
mod tool_defaults;
