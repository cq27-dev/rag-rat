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

#[test]
fn total_occurrences_counts_data_and_distinguishes_an_empty_shell() {
    use super::scip::ScipIndex;
    // A real index with occurrences → the raw count; gates the diagnostic-exit tolerance in
    // produce_scip_with_tool (#198 review) so an empty shell from an early-bailing tool isn't
    // accepted on a non-zero exit.
    let with_data = scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![
        occurrence(0, 0, 3, "scip x `a`/foo().", SymbolRole::Definition as i32),
        occurrence(1, 4, 7, "scip x `a`/foo().", 0),
    ]);
    assert_eq!(ScipIndex::total_occurrences(&with_data).unwrap(), 2);
    // A parseable index with documents but ZERO occurrences — the empty shell the gate must reject.
    let empty_shell = scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![]);
    assert_eq!(ScipIndex::total_occurrences(&empty_shell).unwrap(), 0);
    // Non-SCIP bytes don't parse → Err (the caller treats that as 0 / unusable).
    assert!(ScipIndex::total_occurrences(b"not a scip index").is_err());
}

#[test]
fn accept_produced_index_tolerates_diagnostic_exit_only_with_usable_data() {
    use std::path::Path;

    use super::accept_produced_index;
    let p = Path::new("/tmp/out.scip");
    let data =
        scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occurrence(
            0,
            0,
            3,
            "scip x `a`/foo().",
            SymbolRole::Definition as i32,
        )]);
    let empty_shell = scip_bytes("a.py", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![]);

    // Clean exit: needs only non-empty bytes (join + health gate validate the rest); an empty doc
    // shell is fine here (a 0-occurrence run trips the health gate downstream, not this). The
    // `tolerate_diagnostic_exit` flag is irrelevant on a clean exit.
    assert!(accept_produced_index(true, true, &data, "scip-python", p).unwrap().is_none());
    assert!(
        accept_produced_index(true, false, &empty_shell, "rust-analyzer", p).unwrap().is_none()
    );
    assert!(
        accept_produced_index(true, true, b"", "scip-python", p).is_err(),
        "clean exit, no bytes → bail"
    );

    // Non-zero exit, DIAGNOSTIC-exit tool (scip-python): tolerated ONLY with usable occurrences.
    let note = accept_produced_index(false, true, &data, "scip-python", p).unwrap();
    assert!(note.expect("tolerated note").contains("1 occurrences"));
    assert!(
        accept_produced_index(false, true, &empty_shell, "scip-python", p).is_err(),
        "non-zero exit + empty doc shell (0 occurrences) → bail"
    );
    assert!(
        accept_produced_index(false, true, b"", "scip-python", p).is_err(),
        "non-zero exit + no index → bail"
    );

    // Non-zero exit, NON-diagnostic tool (rust-analyzer/scip-clang): a real failure — bail even
    // with a parseable, occurrence-bearing index (it could be a crashed run's partial output).
    assert!(
        accept_produced_index(false, false, &data, "rust-analyzer", p).is_err(),
        "non-diagnostic backend's non-zero exit is a real failure → bail regardless of index"
    );
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

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::Confirm.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    assert_eq!(report.confirmed, 1);
}

/// Decl-vs-def is a CONFIRM, not a contradiction. C/C++ index a function's prototype declaration
/// and its definition as separate concrete symbols (`parser.rs`: `function_definition` +
/// `declaration` with a `function_declarator`). The heuristic may bind a call to the declaration
/// row while the oracle maps `scip-clang`'s definition occurrence to the definition row — same
/// function, two concrete `symbol_id`s under one `logical_symbol_id`. Comparing concrete ids alone
/// scored this as a contradiction and ~halved measured precision (#61 follow-up); the join now
/// folds to the logical symbol, so this is a confirm.
#[test]
fn decl_and_def_of_same_logical_symbol_is_confirm_not_contradiction() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    // The declaration (a prototype, e.g. in a header) and the definition are SEPARATE concrete
    // symbols, both named `target`, grouped under one logical symbol.
    let decl_file = h.add_file("target.h", "fn target();\n");
    let def_file = h.add_file("target.c", "fn target() {}\n");
    let decl_sym = h.add_symbol(decl_file, "target", 3, 9);
    let def_sym = h.add_symbol(def_file, "target", 3, 9);
    h.add_logical_symbol(1000, "target.c", "target", "target", def_sym);
    h.conn
        .execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, cfg_expr, \
             signature_hash, start_line, end_line) VALUES (1000, ?1, NULL, NULL, 1, 1)",
            params![decl_sym],
        )
        .unwrap();
    // Heuristic is Exact but resolved to the DECLARATION row (the wrong concrete row, right fn).
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(decl_sym));

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
    // The compiler's DEFINITION occurrence lands in target.c → maps to the definition row.
    index.documents.push(Document {
        relative_path: "target.c".to_string(),
        occurrences: vec![occurrence(0, 3, 9, symbol, SymbolRole::Definition as i32)],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(
        kind,
        OracleResolutionKind::Confirm.as_db_str(),
        "decl-row heuristic vs def-row oracle of the SAME logical function must confirm"
    );
    assert_eq!(resolved, Some(def_sym), "oracle resolves to the definition row");
    assert_eq!(report.confirmed, 1);
    assert_eq!(report.contradicted, 0, "same logical symbol is not a contradiction");
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

    // STRICT mode (repo convention for new tables); NO FK to edges_data (#248 — the V018 cascade
    // wiped verdicts on reindex).
    let edge_oracle_sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'edge_oracle'", [], |row| row.get(0))
        .unwrap();
    assert!(edge_oracle_sql.contains("STRICT"), "edge_oracle must be STRICT");
    assert!(
        !edge_oracle_sql.to_uppercase().contains("FOREIGN KEY"),
        "edge_oracle must NOT carry an FK to edges_data — a reindex CASCADE would wipe verdicts"
    );

    // Content-key columns (#248) replace the volatile `edge_id`.
    let columns = table_columns(&conn, "edge_oracle");
    for expected in [
        "source_path",
        "source_start_byte",
        "source_end_byte",
        "callee_start_byte",
        "callee_end_byte",
        "edge_kind",
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
    assert!(!columns.contains(&"edge_id".to_string()), "edge_id column was dropped");

    assert_eq!(schema::LATEST_SCHEMA_VERSION, 31);
}

/// The V019 moniker migration: the `logical_symbol_monikers` table (STRICT, NO foreign key — see
/// the migration's invariant comment: an FK would cascade-wipe monikers on every
/// `rebuild_logical_symbols` DELETE-all pass) plus the moniker provenance + relocation-reason
/// columns on `repo_memory_bindings`.
#[test]
fn migration_creates_moniker_table_and_binding_columns() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'logical_symbol_monikers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("STRICT"), "logical_symbol_monikers must be STRICT");
    assert!(
        !sql.to_uppercase().contains("FOREIGN KEY"),
        "logical_symbol_monikers must NOT carry an FK to logical_symbols — the DELETE-all \
         logical-symbol rebuild would cascade-wipe monikers on every index pass"
    );

    let columns = table_columns(&conn, "logical_symbol_monikers");
    for expected in ["logical_symbol_id", "tool", "tool_version", "moniker", "computed_at"] {
        assert!(
            columns.contains(&expected.to_string()),
            "logical_symbol_monikers missing {expected}"
        );
    }

    let binding_columns = table_columns(&conn, "repo_memory_bindings");
    for expected in ["moniker_tool", "moniker_tool_version", "relocation_reason"] {
        assert!(
            binding_columns.contains(&expected.to_string()),
            "repo_memory_bindings missing {expected}"
        );
    }
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// #248: deleting an `edges` row no longer cascades to its `edge_oracle` verdict (the FK is gone —
/// it CASCADE-wiped every verdict on reindex). Instead the verdict stops RESOLVING: the content
/// join finds no live edge, so the surfacing/metric reads return nothing — the moniker model
/// (dangling never resolves). The physical row persists until the next run's clear or gc sweeps it.
#[test]
fn deleting_an_edge_leaves_a_dangling_verdict_that_does_not_resolve() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let edge = h.add_edge(f, "target", 14, 20, "NameOnly", None);
    let file_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    h.write_verdict(edge, &file_sha, None, "s", OracleResolutionKind::Upgrade);
    assert!(h.verdict(edge).is_some(), "verdict resolves before delete");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "the live verdict is counted before delete"
    );

    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge]).unwrap();

    assert!(h.verdict(edge).is_none(), "no live edge → the verdict no longer resolves");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "the dangling verdict is excluded from the scoped count (live-edge join)"
    );
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(
        remaining, 1,
        "no FK cascade: the physical row survives the edge delete (swept later)"
    );
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

    // A candidate scoped to a DIFFERENT commit is out of scope. (Under clean-checkout semantics a
    // commit-scoped file is visible from any worktree-overlay query as long as the commit matches,
    // so the isolation that actually matters is the commit, not the overlay id.)
    assert!(store::edge_join_candidates(&h.conn, "other-commit-sha", WORKTREE).unwrap().is_empty());
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

/// #248: writing an `edge_oracle` row keyed by the edge's CONTENT key round-trips every field;
/// re-writing the SAME content key upserts (new file_sha/kind) rather than inserting a duplicate.
/// The row resolves through the live-edge content join, and the matching `edges` row is never
/// touched (side-table invariant). The write uses the real `files.sha256` so the count path (which
/// now gates on current content via the scope join) tallies it.
#[test]
fn write_edge_oracle_round_trips_and_upserts_without_touching_edges() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    let caller_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE path = 'caller.rs'", [], |r| r.get(0))
        .unwrap();

    h.write_verdict(
        edge,
        &caller_sha,
        Some(target_sym),
        "scip `target`().",
        OracleResolutionKind::Upgrade,
    );

    let (kind, resolved, scip) = h.verdict(edge).expect("row written");
    assert_eq!(kind, OracleResolutionKind::Upgrade.as_db_str());
    assert_eq!(resolved, Some(target_sym));
    assert_eq!(scip, "scip `target`().");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1
    );

    // Re-write the SAME content key (same edge) with a new sha + verdict → upsert, still one row.
    // Use the same `caller_sha` so the count path keeps it (a different sha would read as stale).
    h.write_verdict(
        edge,
        &caller_sha,
        None,
        "scip cargo tokio `target`().",
        OracleResolutionKind::ResolvedExternal,
    );
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "upsert overwrote the row by content key — no duplicate"
    );
    let (kind2, resolved2, _) = h.verdict(edge).expect("row still present");
    assert_eq!(kind2, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved2, None);
    let physical_rows: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(physical_rows, 1, "the upsert kept a single physical row");
    let file_sha: String = h
        .conn
        .query_row(
            "SELECT file_sha FROM edge_oracle WHERE source_path = 'caller.rs' AND \
             callee_start_byte = 14 AND callee_end_byte = 20 AND edge_kind = 'calls_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file_sha, caller_sha, "upsert refreshed the staleness sha");

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

    h.write_verdict(e1, "sha-fresh", None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e2, "sha-old", None, "s", OracleResolutionKind::Upgrade);

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

/// #145: `record_oracle_run_at` persists the PASSED start time, not `now_ms()`/completion — so the
/// auto-run staleness gate keys on when the run BEGAN, not when it finished (a run that overlapped
/// a watcher reindex must not look fresher than the edits it skipped).
#[test]
fn record_oracle_run_at_persists_the_passed_start_time() {
    let h = Harness::new();
    // Deliberately far in the past: if the impl stamped `now_ms()` instead, this would not match.
    let started_at_ms = 1_000_000_i64;
    store::record_oracle_run_at(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        started_at_ms,
        "Completed",
        "{}",
    )
    .unwrap();
    assert_eq!(
        store::latest_run_started_at(&h.conn, TOOL, COMMIT, WORKTREE).unwrap(),
        Some(started_at_ms),
        "started_at must be the value the caller passed, not completion time"
    );
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
    let sha = h.file_sha("a.rs");
    let e_up = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    let e_conf = h.add_edge(f, "c", 1, 2, "Exact", None);
    h.write_verdict(e_up, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e_conf, &sha, None, "s", OracleResolutionKind::Confirm);

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
    let sha = h.file_sha("a.rs");
    let e1 = h.add_edge(f, "a", 0, 1, "NameOnly", None);
    let e2 = h.add_edge(f, "b", 1, 2, "Exact", None);
    h.write_verdict(e1, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e2, &sha, None, "s", OracleResolutionKind::Contradict);

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
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &empty, h.root(), None, None).unwrap();

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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
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
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
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
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &empty_doc, h.root(), None, None).unwrap();

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
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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
    let sha = h.file_sha("a.rs");
    // Exact edges judged: one confirmed, one contradicted. (to_symbol_id NULL keeps the FK happy;
    // precision is derived from the persisted edge_oracle rows, not the edges row.)
    let e_conf = h.add_edge(f, "c", 0, 1, "Exact", None);
    let e_contra = h.add_edge(f, "d", 1, 2, "Exact", None);
    // A NameOnly edge the oracle upgraded.
    let e_up = h.add_edge(f, "u", 2, 3, "NameOnly", None);

    h.write_verdict(e_conf, &sha, Some(1), "s", OracleResolutionKind::Confirm);
    h.write_verdict(e_contra, &sha, Some(3), "s", OracleResolutionKind::Contradict);
    h.write_verdict(e_up, &sha, Some(4), "s", OracleResolutionKind::Upgrade);

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
    let sha = h.file_sha("a.rs");
    // The only low-confidence candidate (denominator = 1): a NameOnly edge the oracle upgraded.
    let e_low = h.add_edge(f, "u", 0, 1, "NameOnly", None);
    // An already-Exact edge the oracle placed to an EXTERNAL dep — NOT in the low-conf denominator.
    let e_exact = h.add_edge(f, "x", 1, 2, "Exact", None);

    h.write_verdict(e_low, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e_exact, &sha, None, "s", OracleResolutionKind::ResolvedExternal);

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

/// scip-typescript leaves `position_encoding` UNSET (Unspecified) but emits UTF-16 column offsets.
/// `parse_with_default(UTF16, …)` must read those columns as UTF-16 so an identifier after an
/// astral character lands on the right bytes; the historical Unspecified→UTF-32 fallback misaligns
/// it.
#[test]
fn unspecified_encoding_uses_the_supplied_default() {
    // "😀foo\n": the emoji is 4 UTF-8 bytes = 2 UTF-16 units = 1 UTF-32 unit; "foo" is bytes 4..7.
    let source = "😀foo\n".as_bytes().to_vec();
    // UTF-16 columns [2,5] (past the 2-unit emoji).
    let occ =
        occurrence(0, 2, 5, "scip-typescript npm p 1 `a.ts`/foo().", SymbolRole::Definition as i32);
    // Document with NO position_encoding set (serialized as the protobuf default = Unspecified).
    let bytes = scip_bytes("a.ts", PositionEncoding::UnspecifiedPositionEncoding, vec![occ]);

    let s = source.clone();
    let idx = ScipIndex::parse_with_default(
        &bytes,
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
        move |_| Some(s.clone()),
    )
    .unwrap();
    let occ16 = &idx.occurrences_by_path.get("a.ts").unwrap()[0];
    assert_eq!((occ16.start_byte, occ16.end_byte), (4, 7));
    assert_eq!(&source[occ16.start_byte..occ16.end_byte], b"foo");

    // The plain 2-arg parse (Unspecified → UTF-32 fallback) misaligns onto the wrong bytes.
    let s = source.clone();
    let idx32 = ScipIndex::parse(&bytes, move |_| Some(s.clone())).unwrap();
    let occ32 = &idx32.occurrences_by_path.get("a.ts").unwrap()[0];
    assert_ne!(&source[occ32.start_byte..occ32.end_byte], b"foo", "UTF-32 fallback misaligns");
}

/// `symbol_is_module` recognizes the SCIP namespace/module/package suffix (`/`) and nothing else.
#[test]
fn symbol_is_module_matches_only_namespace_suffix() {
    use super::scip::symbol_is_module;
    assert!(symbol_is_module("scip-typescript npm p 1 `a.ts`/"));
    assert!(!symbol_is_module("scip-typescript npm p 1 `a.ts`/foo().")); // method
    assert!(!symbol_is_module("scip-typescript npm p 1 `a.ts`/Bar#")); // type
    assert!(!symbol_is_module("local 1"));
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
    for &tool in OracleTool::ALL {
        assert_eq!(OracleTool::from_db_str(tool.as_db_str()), Some(tool));
    }
    assert_eq!(OracleTool::from_db_str("no-such-tool"), None);

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

// A sibling checkout sharing the same DB: a DIFFERENT commit, clean (empty worktree). Modelling the
// sibling as a distinct commit (rather than the same commit + a second worktree id) matches the
// real shape — two checkouts at the same HEAD is unusual, and under the active-checkout predicate a
// same-commit worktree overlay would (correctly) *shadow* the clean row by path, which is not the
// cross-checkout-isolation property these tests mean to assert. Commit isolation is.
const OTHER_COMMIT: &str = "5ad1f1ce5ad1f1ce";
const OTHER_WORKTREE: &str = "";

/// Finding 1 + #248: `clear_edge_oracle_for_tool` must delete ONLY the active checkout's verdicts.
/// With two worktrees' verdicts for the same `(tool, tool_version)` in one DB, clearing one leaves
/// the other's intact — the clear is scoped via a CONTENT join to the live edges in the active
/// checkout (path + source/callee spans + edge_kind), not the old `edge_id` rowid subquery. The two
/// checkouts use DISTINCT callee ranges so their content keys differ (a content-key COLLISION
/// across checkouts is the intentional "same resolution → shared verdict" case, which would defeat
/// the per-checkout isolation this test asserts).
#[test]
fn clear_edge_oracle_for_tool_scopes_by_checkout_content() {
    let h = Harness::new();
    // Active-checkout edge + a verdict (real file sha so the scoped count tallies it).
    let active_file = h.add_file("a.rs", "fn caller() { target(); other(); }\n");
    let active_sha = h.file_sha("a.rs");
    let active_edge = h.add_edge(active_file, "target", 14, 20, "NameOnly", None);
    // Another worktree's edge (same DB, same path, DIFFERENT callee range → distinct content key) +
    // a verdict for the SAME tool/version.
    let other_file = h.add_file_in_scope("a.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let other_sha = h.file_sha_for_commit("a.rs", OTHER_COMMIT);
    let other_edge = h.add_edge(other_file, "other", 22, 27, "NameOnly", None);

    h.write_verdict(active_edge, &active_sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(other_edge, &other_sha, None, "s", OracleResolutionKind::Upgrade);
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

/// #248 THE killer test: an `edge_oracle` verdict SURVIVES reindex for an UNCHANGED file. A reindex
/// rewrites `edges_data` (DELETE + reinsert with NEW rowids); before the content-key fix the
/// `ON DELETE CASCADE` FK wiped every verdict and the opt-in oracle never repopulated. Now the
/// verdict is content-keyed with no FK, so the reindexed edge (same path + spans + edge_kind, same
/// `files.sha256`) RE-ANCHORS the verdict by content — `count_edge_oracle_scoped` /
/// `current_oracle_comparisons` still return it, re-projected onto the NEW edge id.
#[test]
fn edge_oracle_survives_reindex_for_unchanged_file() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha = h.file_sha("a.rs");
    let target = h.add_symbol(f, "target", 3, 9);
    // An Exact edge the heuristic resolved to `target`; the oracle CONTRADICTS it (so it shows up
    // in `current_oracle_comparisons`, which keeps Contradict rows).
    let edge_v1 = h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    h.write_verdict(
        edge_v1,
        &sha,
        None,
        "scip-rust cargo other 1.0 `target`().",
        OracleResolutionKind::Contradict,
    );

    // Sanity before reindex: counted + surfaced in compare.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "verdict counted before reindex"
    );
    assert_eq!(
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap().len(),
        1
    );
    let physical_before: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();

    // --- Simulate a reindex of the UNCHANGED file: DELETE the edge (rewriting edges_data) and
    // re-insert the SAME edge content, which mints a NEW edges_data rowid. files.sha256 unchanged.
    // ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v1]).unwrap();
    let edge_v2 = h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    assert_ne!(edge_v2, edge_v1, "reindex minted a new edge rowid");

    // The verdict was NOT touched (no cascade) — same physical row count.
    let physical_after: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(physical_after, physical_before, "no FK cascade wiped the verdict on reindex");

    // And it RE-ANCHORS to the new edge by content key: still counted, still in compare, now keyed
    // on the NEW edge id.
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "verdict still counted after reindex (re-anchored by content)"
    );
    let comparisons =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(comparisons.len(), 1, "verdict re-surfaces in compare after reindex");
    assert_eq!(
        comparisons[0].edge_id, edge_v2,
        "the comparison is re-projected onto the LIVE edge id"
    );
    assert_eq!(
        h.verdict(edge_v2).map(|(kind, _, _)| kind),
        Some(OracleResolutionKind::Contradict.as_db_str().to_string()),
        "the reindexed edge resolves the re-anchored verdict by content"
    );
}

/// #248: a verdict for a CHANGED file (its `files.sha256` no longer matches the verdict's
/// `file_sha`) is NOT counted — the scope join gates `files.sha256 = edge_oracle.file_sha`, so a
/// stale verdict drops out of the metrics until the next run rewrites it.
#[test]
fn edge_oracle_stale_after_file_change_not_counted() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha = h.file_sha("a.rs");
    let edge = h.add_edge(f, "target", 14, 20, "Exact", None);
    h.write_verdict(edge, &sha, None, "scip x `target`().", OracleResolutionKind::Confirm);
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "current verdict counted"
    );

    // The file content changed: its recorded sha no longer matches the verdict's file_sha.
    h.conn.execute("UPDATE files SET sha256 = 'changed-sha' WHERE id = ?1", params![f]).unwrap();

    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "a changed file's verdict is stale → not counted (file_sha mismatch)"
    );
    assert!(
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE)
            .unwrap()
            .is_empty(),
        "a stale verdict does not surface in compare either"
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
    let active_sha = h.file_sha("a.rs");
    let e_low = h.add_edge(active, "u", 0, 1, "NameOnly", None);
    h.write_verdict(e_low, &active_sha, Some(1), "s", OracleResolutionKind::Upgrade);

    // Another worktree: an unresolved NameOnly candidate carrying a callee range, NO verdict. If
    // the denominator weren't scoped, it would count → fraction 1/2.
    let other = h.add_file_in_scope("a.rs", OTHER_COMMIT, OTHER_WORKTREE);
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
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
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
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
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

    // Active checkout A: one confirmed + one contradicted Exact edge → precision 1/2.
    let a_file = h.add_file("a.rs", "fn caller() {}\n");
    let a_sha = h.file_sha("a.rs");
    let a_conf = h.add_edge(a_file, "c", 0, 1, "Exact", None);
    let a_contra = h.add_edge(a_file, "d", 1, 2, "Exact", None);
    h.write_verdict(a_conf, &a_sha, None, "s", OracleResolutionKind::Confirm);
    h.write_verdict(a_contra, &a_sha, None, "s", OracleResolutionKind::Contradict);

    // Sibling checkout B (same DB, same tool/version): TWO confirms + an upgrade. Uses a DISTINCT
    // path ("b.rs") so the content keys never collide with A's (#248: the content key omits
    // commit/worktree, so a same-path same-span edge in B would SHARE A's verdict row — that is the
    // intentional "same resolution" case; this test asserts SCOPE isolation, so it keeps the
    // populations physically distinct). If A's counts leaked B's rows, A's precision would jump.
    let b_file = h.add_file_in_scope("b.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let b_sha = h.file_sha_for_commit("b.rs", OTHER_COMMIT);
    let b_conf1 = h.add_edge(b_file, "e", 0, 1, "Exact", None);
    let b_conf2 = h.add_edge(b_file, "f", 1, 2, "Exact", None);
    let b_up = h.add_edge(b_file, "g", 2, 3, "NameOnly", None);
    h.write_verdict(b_conf1, &b_sha, None, "s", OracleResolutionKind::Confirm);
    h.write_verdict(b_conf2, &b_sha, None, "s", OracleResolutionKind::Confirm);
    h.write_verdict(b_up, &b_sha, None, "s", OracleResolutionKind::Upgrade);

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
    let status_b =
        super::oracle_status(&h.conn, TOOL, VERSION, OTHER_COMMIT, OTHER_WORKTREE).unwrap();
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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
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
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
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

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
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

/// #176 (covered side): the covered side requires the matched SCIP symbol be CALLABLE (`).`) — the
/// same filter `count_uncovered_calls` applies. A `calls_name` edge a verdict matched to a CLASS
/// symbol (`…Thing#`, e.g. scip-python's `Thing()` constructor, which our extractor emits as
/// `CallsName` but SCIP records as a reference to the class) must NOT inflate `covered_calls`.
/// Otherwise the two sides measure different populations and a MISSED constructor — invisible to
/// the callable-filtered uncovered side — would never offset a covered one, inflating recall.
#[test]
fn covered_side_requires_a_callable_scip_symbol() {
    let h = Harness::new();
    // `caller.rs`: a method call `target` at 14..20 and a constructor call `Thing` at 24..29.
    let caller = h.add_file("caller.rs", "fn caller() { target(); Thing(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\nstruct Thing;\n");
    let target_sym = h.add_symbol(defs, "target", 3, 9);
    let thing_sym = h.add_symbol(defs, "Thing", 22, 27);
    // BOTH are `calls_name` edges (a constructor call is a `CallsName` in our extractor).
    let call_edge = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    let ctor_edge = h.add_edge(caller, "Thing", 24, 29, "Exact", Some(thing_sym));

    let call_sym = "scip-rust crate v1 `target`().";
    // Class symbol — ends `#`, NOT `).`: not callable (how scip-python records a constructor ref).
    let class_sym = "scip-rust crate v1 `Thing`#";
    let mut index = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: vec![
                occurrence(0, 14, 20, call_sym, SymbolRole::UnspecifiedSymbolRole as i32),
                occurrence(0, 24, 29, class_sym, SymbolRole::UnspecifiedSymbolRole as i32),
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
            occurrence(1, 7, 12, class_sym, SymbolRole::Definition as i32),
        ],
        position_encoding: EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart),
        ..Default::default()
    });
    let bytes = index.write_to_bytes().unwrap();

    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    // Both edges still get verdicts (both join + resolve in-corpus)…
    assert!(h.verdict(call_edge).is_some(), "call edge verdicted");
    assert!(h.verdict(ctor_edge).is_some(), "constructor edge verdicted");
    // …but only the callable-symbol call counts as covered; the class-symbol constructor does not,
    // and the uncovered side excludes it too → no phantom recall gap.
    assert_eq!(report.covered_calls, 1, "constructor (class symbol) must NOT inflate covered");
    assert_eq!(report.oracle_only_calls, 0);
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
            run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
                .unwrap();
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

/// #82 TOCTOU: the scip-vs-disk gate. A tool-driven run carries a per-document `production_sha`
/// snapshot — the disk hashes captured the instant the subprocess finished. The join verdicts a
/// candidate only when that snapshot STILL equals the disk bytes it reads; if the snapshot no
/// longer matches (the watcher reindexed the call-site file in the lock-free window after the
/// `.scip` was built, so index-vs-disk agrees on the NEW content while the `.scip` describes the
/// OLD), the candidate is skipped as drifted instead of writing a spurious Compiler verdict. A
/// document absent from the snapshot (unreadable at production) also fails the gate. The pre-built
/// `--scip` path (`None`) keeps only the index-vs-disk gate — proven by
/// `drifted_file_sha_is_skipped_not_verdicted`.
#[test]
fn stale_production_snapshot_is_skipped_not_verdicted() {
    // What the production snapshot says about the documents a verdict depends on: the call-site
    // file `caller.rs` and the definition file `defs.rs`.
    enum Snapshot {
        MatchesDisk,
        StaleCaller,
        MissingCaller,
        StaleDefs,
    }
    // (verdict row; skipped_drifted; rows_written)
    type Probe = (Option<(String, Option<i64>, String)>, u64, u64);
    let build = |snapshot: Snapshot| -> Probe {
        let h = Harness::new();
        let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let _target_sym = h.add_symbol(defs, "target", 3, 9);
        let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

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

        // Hash the disk bytes the way the join does, so MatchesDisk pins the actual current
        // content.
        let disk_hash =
            |rel: &str| super::run::hex_sha256(&std::fs::read(h.root().join(rel)).unwrap());
        let stale = "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        let mut production: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // Default: both documents match disk. Each arm then drifts exactly one (or omits it).
        production.insert("caller.rs".to_string(), disk_hash("caller.rs"));
        production.insert("defs.rs".to_string(), disk_hash("defs.rs"));
        match snapshot {
            Snapshot::MatchesDisk => {},
            // The subprocess saw older CALL-SITE content than what's on disk at join time.
            Snapshot::StaleCaller => {
                production.insert("caller.rs".to_string(), stale);
            },
            Snapshot::MissingCaller => {
                production.remove("caller.rs"); // unreadable at production → absent from the snapshot
            },
            // The call site is pristine, but the watcher reindexed the DEFINITION file in the
            // window: the resolved-symbol byte range is converted against drifted bytes, so the
            // verdict must be skipped even though `caller.rs` passes the call-site gate.
            Snapshot::StaleDefs => {
                production.insert("defs.rs".to_string(), stale);
            },
        }

        let report = run_oracle(
            &h.conn,
            TOOL,
            VERSION,
            COMMIT,
            WORKTREE,
            &bytes,
            h.root(),
            Some(&production),
            None,
        )
        .unwrap();
        (h.verdict(edge), report.skipped_drifted, report.rows_written)
    };

    let (verdict, skipped, written) = build(Snapshot::MatchesDisk);
    assert!(verdict.is_some(), "a snapshot matching disk IS verdicted");
    assert_eq!(skipped, 0);
    assert_eq!(written, 1);

    let (verdict, skipped, written) = build(Snapshot::StaleCaller);
    assert!(verdict.is_none(), "a stale production snapshot must NOT be verdicted (TOCTOU)");
    assert_eq!(skipped, 1, "the stale-snapshot candidate is tallied in skipped_drifted");
    assert_eq!(written, 0, "nothing written from a `.scip` describing superseded content");

    let (verdict, skipped, written) = build(Snapshot::MissingCaller);
    assert!(
        verdict.is_none(),
        "a candidate absent from the production snapshot must NOT be verdicted"
    );
    assert_eq!(skipped, 1);
    assert_eq!(written, 0);

    // The def-document leg of the gate: call site pristine, definition file drifted from the
    // snapshot → the resolved-target conversion is untrustworthy, so the verdict is skipped too.
    let (verdict, skipped, written) = build(Snapshot::StaleDefs);
    assert!(
        verdict.is_none(),
        "a verdict whose DEFINITION document drifted must NOT be verdicted (def-doc TOCTOU)"
    );
    assert_eq!(skipped, 1, "the stale-def candidate is tallied in skipped_drifted");
    assert_eq!(written, 0);
}

/// Finding 3: a run recorded for ANOTHER worktree (same tool/version/commit) does NOT surface as
/// this checkout's last run. `oracle_runs` now carries `worktree_id` and `last_run_meta` filters on
/// it, so the status read describes only the active checkout — consistent with its worktree-scoped
/// verdict counts.
#[test]
fn last_run_meta_is_scoped_to_active_worktree() {
    let h = Harness::new();
    // A run in a SIBLING worktree (same tool/version/commit, distinct worktree id). It must not be
    // THIS checkout's last. `oracle_runs.worktree_id` scoping is orthogonal to the file-predicate
    // fix, so this uses a non-empty sibling worktree id directly rather than the file-level
    // `OTHER_*` constants.
    store::record_oracle_run(&h.conn, TOOL, VERSION, COMMIT, "sibling-wt", "Completed", "{}")
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
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap()
            .oracle_only_calls
    };

    assert_eq!(build(true), 0, "a tombstoned source file's occurrence is NOT a recall gap");
    assert_eq!(build(false), 1, "the same call from a live indexed file IS a recall gap");
}

/// Finding 6a: the oracle's checkout scope is leak-proof BY CONSTRUCTION — every raw `edge_oracle`
/// query lives in `store.rs` (which owns the scoped helpers + the `edge_oracle_scope_join`
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
             store::count_edge_oracle_scoped / edge_oracle_scope_join so it can't drop the \
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
    let sha = h.file_sha("a.rs");

    h.write_verdict(e_low, &sha, None, "s", OracleResolutionKind::Upgrade);
    h.write_verdict(e_exact, &sha, None, "s", OracleResolutionKind::Upgrade);

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

// ---------------------------------------------------------------------------
// Phase 2 (#69): read-side surfacing helpers — `Compiler` tier, staleness/dirty
// revert, `resolved-external`, `compare_graph_to_scip` data, and gc pruning of
// `oracle_runs`. All deterministic against the synthetic harness conn (no
// rust-analyzer, no `.scip` subprocess).
// ---------------------------------------------------------------------------

/// Seed one edge with a written verdict and return `(harness, edge_id, file_sha)`. The verdict's
/// `file_sha` matches the file's recorded sha (current), so it surfaces; tests that want staleness
/// drift the edge's file sha afterward. When `in_corpus` is set, a real `target` symbol is inserted
/// and the verdict resolves to its id — so the def-drift gate (`resolved_symbol_id` must still
/// EXIST in `symbols`) is satisfied for current verdicts. `None` (external) verdicts skip that
/// gate.
fn seed_verdict(
    kind: OracleResolutionKind,
    scip_symbol: &str,
    in_corpus: bool,
) -> (Harness, i64, String) {
    let (h, edge, file_sha, _resolved) = seed_verdict_full(kind, scip_symbol, in_corpus);
    (h, edge, file_sha)
}

/// Like [`seed_verdict`] but also returns the in-corpus `resolved_symbol_id` (when any), so a test
/// can delete/reindex that definition symbol and assert the verdict stops surfacing (#82 finding
/// 3).
fn seed_verdict_full(
    kind: OracleResolutionKind,
    scip_symbol: &str,
    in_corpus: bool,
) -> (Harness, i64, String, Option<i64>) {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let file_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    // An in-corpus resolution must point at a symbol that EXISTS — the def-drift gate in
    // `edge_oracle_current_predicate` filters a dangling `resolved_symbol_id`.
    let resolved_symbol_id = in_corpus.then(|| h.add_symbol(f, "target", 3, 9));
    let edge = h.add_edge(f, "target", 14, 20, "NameOnly", None);
    h.write_verdict(edge, &file_sha, resolved_symbol_id, scip_symbol, kind);
    (h, edge, file_sha, resolved_symbol_id)
}

/// A CURRENT verdict (its `file_sha` matches `files.sha256`) is returned by the surfacing read —
/// the `Compiler` tier data.
#[test]
fn current_verdict_is_surfaced_for_edge() {
    let (h, edge, _sha) = seed_verdict(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    let verdicts =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    let verdict = verdicts.get(&edge).expect("current verdict surfaced");
    assert_eq!(verdict.kind, OracleResolutionKind::Upgrade);
    assert_eq!(verdict.resolution_reason(), format!("scip:{}@{VERSION}", TOOL.as_db_str()));
}

/// Staleness revert: drift the edge's file sha so it no longer matches the verdict's `file_sha`.
/// The surfacing read excludes it — the edge reverts to heuristic display, never `Compiler`.
#[test]
fn drifted_file_verdict_is_not_surfaced() {
    let (h, edge, _sha) = seed_verdict(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    // The file's content changed since the verdict was computed: its sha now differs from
    // `edge_oracle.file_sha`, so the current-content predicate filters the verdict out.
    h.conn.execute("UPDATE files SET sha256 = 'drifted-sha' WHERE path = 'a.rs'", []).unwrap();
    let verdicts =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    assert!(verdicts.is_empty(), "a drifted file's verdict must not surface as Compiler");
}

/// The whole-graph scan ([`store::current_oracle_verdicts_all`], used by symbol-importance ranking)
/// returns the same current+in-scope verdicts as the per-edge read — `(kind, resolved_symbol_id)`
/// keyed by edge id — and applies the same currency gate (a drifted callsite drops out).
#[test]
fn current_oracle_verdicts_all_returns_scoped_current() {
    let (h, edge, _sha, resolved) =
        seed_verdict_full(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    let all = store::current_oracle_verdicts_all(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(
        all.get(&edge),
        Some(&(OracleResolutionKind::Upgrade, resolved)),
        "the whole-graph scan returns the current verdict with its resolved symbol id"
    );

    // Drift the callsite file: the currency gate must drop the verdict from the whole-graph scan
    // too, exactly as it does for the per-edge read.
    h.conn.execute("UPDATE files SET sha256 = 'drifted-sha' WHERE path = 'a.rs'", []).unwrap();
    let after =
        store::current_oracle_verdicts_all(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert!(after.is_empty(), "a drifted file's verdict must not surface in the whole-graph scan");
}

/// Def-drift revert (#82 finding 3): an in-corpus verdict keeps its callsite file unchanged (so the
/// `file_sha` gate still matches), but its resolved DEFINITION symbol is deleted/reinserted by
/// incremental reindexing — the old `resolved_symbol_id` dangles. The surfacing read must drop the
/// verdict (the def the compiler resolved to no longer exists), reverting to heuristic display.
#[test]
fn resolved_def_drift_verdict_is_not_surfaced() {
    let (h, edge, _sha, resolved) =
        seed_verdict_full(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    let resolved = resolved.expect("in-corpus verdict has a resolved symbol id");
    // Sanity: while the resolved def symbol exists, the verdict surfaces.
    let before =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    assert!(before.contains_key(&edge), "current in-corpus verdict surfaces before def drift");
    // The def file was reindexed: AUTOINCREMENT mints new ids, so the old resolved symbol id is
    // gone. Model that by deleting the resolved symbol row.
    h.conn.execute("DELETE FROM symbols WHERE id = ?1", params![resolved]).unwrap();
    let after =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    assert!(after.is_empty(), "a verdict whose resolved definition drifted must not surface");
}

/// Overlay def-drift (#82 P2): when the *def* file goes dirty, the indexer inserts a
/// worktree-scoped overlay row and leaves the old commit-scoped symbols shadowed-but-PRESENT (not
/// deleted). A raw `EXISTS (symbols.id = resolved_symbol_id)` would still find the stale id and
/// keep surfacing a `Compiler` verdict pointing at the pre-edit target (the CALLSITE file is
/// untouched, so its sha still matches). The scope-aware def-drift EXISTS — which joins `symbols ->
/// files` and applies the active-checkout predicate — must treat the shadowed commit-scoped def as
/// out of scope, reverting the verdict to heuristic. Callsite and def are in SEPARATE files so only
/// the def's scope changes.
#[test]
fn overlay_shadowed_def_verdict_is_not_surfaced() {
    let h = Harness::new();
    // Callsite in `caller.rs` (stays committed); def in `defs.rs` (will get an overlay).
    // The active context for a dirty checkout carries a real worktree id (the root path) alongside
    // the HEAD commit — `resolve_git_context` always returns the root as `worktree_id`. Both the
    // committed (clean) caller row and the dirty overlay use that id.
    let active_wt = "/some/checkout/root";
    let caller = h.add_file_in_scope("caller.rs", COMMIT, "");
    h.conn
        .execute("UPDATE files SET sha256 = 'caller-sha' WHERE id = ?1", params![caller])
        .unwrap();
    let defs = h.add_file_in_scope("defs.rs", COMMIT, "");
    let resolved = h.add_symbol(defs, "target", 3, 9);
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);
    h.write_verdict(
        edge,
        "caller-sha",
        Some(resolved),
        "scip x `target`().",
        OracleResolutionKind::Upgrade,
    );

    // Sanity: with no overlay, the committed def is in scope → the verdict surfaces.
    let before =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, active_wt, &[
            edge,
        ])
        .unwrap();
    assert!(before.contains_key(&edge), "verdict surfaces before the def file goes dirty");

    // The def file goes dirty: a worktree-scoped overlay row for `defs.rs` is inserted; the
    // committed `defs.rs` row (and its `target` symbol) stay shadowed-but-present. The active
    // worktree id matches the overlay, so the committed def is now shadowed out of scope.
    h.add_file_in_scope("defs.rs", "", active_wt);

    let after =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, active_wt, &[
            edge,
        ])
        .unwrap();
    assert!(
        after.is_empty(),
        "a verdict whose resolved def is shadowed by a dirty overlay must not keep surfacing"
    );
}

/// A verdict whose edge lives in a different checkout never surfaces for the active one: querying
/// the seeded (clean-checkout) edge under a DIFFERENT commit is out of the active-checkout scope
/// join, so the verdict is excluded. (Commit, not worktree id, is the isolation boundary for a
/// commit-scoped row — a clean file is visible from any worktree-overlay query at the same commit,
/// so a sibling-worktree query at the SAME commit would correctly still see it; the genuine
/// out-of-scope case is a different commit.)
#[test]
fn out_of_scope_verdict_is_not_surfaced() {
    let (h, edge, sha) = seed_verdict(OracleResolutionKind::Upgrade, "scip x `target`().", true);
    // Query the SAME edge under a different commit's scope: the scope join excludes it.
    let verdicts = store::current_oracle_verdicts_for_edges(
        &h.conn,
        TOOL,
        VERSION,
        "a-different-commit-sha",
        WORKTREE,
        &[edge],
    )
    .unwrap();
    assert!(verdicts.is_empty(), "a verdict outside the active checkout must not surface");
    let _ = sha;
}

/// #82 P3: the `--scip` run-id fingerprint is a stable 12-hex-char content hash, distinct for
/// distinct bytes — so two indexes sharing a basename don't collide onto one `tool_version`.
#[test]
fn scip_content_fingerprint_is_stable_and_content_distinct() {
    let a = super::scip_content_fingerprint(b"index-A-bytes");
    let b = super::scip_content_fingerprint(b"index-B-bytes");
    assert_eq!(a.len(), 12, "12 hex chars");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(a, super::scip_content_fingerprint(b"index-A-bytes"), "stable for identical bytes");
    assert_ne!(a, b, "distinct bytes → distinct fingerprint (no basename collision)");
}

/// A `resolved-external` verdict surfaces a `resolved-external(<package>)` label derived from the
/// SCIP symbol's package component.
#[test]
fn resolved_external_label_surfaces_package() {
    let (h, edge, _sha) = seed_verdict(
        OracleResolutionKind::ResolvedExternal,
        "scip-rust cargo tokio 1.0 `spawn`().",
        false,
    );
    let verdicts =
        store::current_oracle_verdicts_for_edges(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &[edge])
            .unwrap();
    let verdict = verdicts.get(&edge).expect("verdict surfaced");
    assert_eq!(verdict.resolved_external_label().as_deref(), Some("resolved-external(tokio)"));
}

/// `current_oracle_comparisons` returns CURRENT, in-scope verdicts joined to the heuristic edge —
/// the `compare_graph_to_scip` data. A `Contradict` verdict appears with its scip symbol; a drifted
/// row does not.
#[test]
fn comparisons_return_current_contradictions_only() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    let target = h.add_symbol(f, "target", 3, 9);
    // An Exact edge the heuristic resolved to `target`; the oracle CONTRADICTS it (points
    // elsewhere).
    let edge = h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    h.write_verdict(
        edge,
        &sha,
        None,
        "scip-rust cargo other 1.0 `target`().",
        OracleResolutionKind::Contradict,
    );

    let comparisons =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert_eq!(comparisons.len(), 1);
    let c = &comparisons[0];
    assert_eq!(c.kind, OracleResolutionKind::Contradict);
    assert_eq!(c.edge_id, edge);
    assert_eq!(c.heuristic_confidence, "Exact");
    assert_eq!(c.scip_symbol, "scip-rust cargo other 1.0 `target`().");

    // Drift the file → the comparison drops out (no stale contradiction surfaced).
    h.conn.execute("UPDATE files SET sha256 = 'drift' WHERE id = ?1", params![f]).unwrap();
    let after =
        store::current_oracle_comparisons(&h.conn, TOOL, VERSION, COMMIT, WORKTREE).unwrap();
    assert!(after.is_empty(), "a drifted file's contradiction must not surface");
}

/// `latest_run_tool_version` returns the most recent run's version for the active checkout, and
/// `None` when there is no run.
#[test]
fn latest_run_tool_version_tracks_active_checkout() {
    let h = Harness::new();
    assert_eq!(store::latest_run_tool_version(&h.conn, TOOL, COMMIT, WORKTREE).unwrap(), None);
    store::record_oracle_run(&h.conn, TOOL, "v1", COMMIT, WORKTREE, "Completed", "{}").unwrap();
    store::record_oracle_run(&h.conn, TOOL, "v2", COMMIT, WORKTREE, "Completed", "{}").unwrap();
    assert_eq!(
        store::latest_run_tool_version(&h.conn, TOOL, COMMIT, WORKTREE).unwrap().as_deref(),
        Some("v2")
    );
    // A sibling worktree's run does not leak in.
    store::record_oracle_run(&h.conn, TOOL, "v3", COMMIT, "other", "Completed", "{}").unwrap();
    assert_eq!(
        store::latest_run_tool_version(&h.conn, TOOL, COMMIT, WORKTREE).unwrap().as_deref(),
        Some("v2")
    );
}

/// gc: `prune_oracle_runs_outside_scope` drops runs whose `(commit, worktree)` is dead, keeps live
/// ones, and refuses to prune when both live sets are empty (so a missing live set never wipes all
/// run history).
#[test]
fn prune_oracle_runs_drops_dead_contexts_only() {
    let h = Harness::new();
    store::record_oracle_run(&h.conn, TOOL, "v1", "live-commit", "live-wt", "Completed", "{}")
        .unwrap();
    store::record_oracle_run(&h.conn, TOOL, "v1", "dead-commit", "dead-wt", "Completed", "{}")
        .unwrap();
    // A run whose commit is dead but whose worktree overlay is live survives (OR rule).
    store::record_oracle_run(&h.conn, TOOL, "v1", "dead-commit", "live-wt", "Completed", "{}")
        .unwrap();

    let live_commits = vec!["live-commit".to_string()];
    let live_worktrees = vec!["live-wt".to_string()];

    // Empty live sets are a no-op (never wipe everything).
    assert_eq!(store::prune_oracle_runs_outside_scope(&h.conn, &[], &[]).unwrap(), 0);

    let deleted =
        store::prune_oracle_runs_outside_scope(&h.conn, &live_commits, &live_worktrees).unwrap();
    assert_eq!(deleted, 1, "only the (dead-commit, dead-wt) run is pruned");
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM oracle_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 2);
}

/// gc (#248): `prune_edge_oracle_without_live_edge` is a GLOBAL sweep — it deletes a verdict whose
/// content key matches NO live edge in ANY scope, but KEEPS one that matches a live edge anywhere
/// (so a sibling worktree's still-live verdict is never swept by a sweep run in another checkout).
/// This is the gc hygiene that replaces the dropped FK cascade; correctness never depended on it
/// (dangling verdicts already never resolve via the live join — see
/// `edge_oracle_survives_reindex_for_unchanged_file`), so this only guards unbounded growth.
#[test]
fn gc_prunes_edge_oracle_rows_with_no_live_edge() {
    let h = Harness::new();

    // (A) A verdict whose edge is LIVE in the active checkout — must be kept.
    let live_file = h.add_file("live.rs", "fn caller() { target(); }\n");
    let live_sha = h.file_sha("live.rs");
    let live_edge = h.add_edge(live_file, "target", 14, 20, "Exact", None);
    h.write_verdict(
        live_edge,
        &live_sha,
        None,
        "scip x `target`().",
        OracleResolutionKind::Confirm,
    );

    // (B) A verdict whose edge lives in a SIBLING checkout (another commit). The global sweep does
    // NOT apply the active-checkout predicate, so it must still see this edge as live and keep its
    // verdict — a sweep in THIS checkout must never delete a sibling's live verdict.
    let sibling_file = h.add_file_in_scope("sibling.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let sibling_sha = h.file_sha_for_commit("sibling.rs", OTHER_COMMIT);
    let sibling_edge = h.add_edge(sibling_file, "thing", 14, 19, "Exact", None);
    h.write_verdict(
        sibling_edge,
        &sibling_sha,
        None,
        "scip x `thing`().",
        OracleResolutionKind::Confirm,
    );

    // (C) A DANGLING verdict — content key matches no live edge anywhere (the edge was deleted in a
    // reindex, leaving the FK-less verdict behind). Build it by writing a verdict, then deleting
    // its edge (simulating `remove_file_in_scope` dropping a changed file's edges).
    let dangling_file = h.add_file("dangling.rs", "fn caller() { gone(); }\n");
    let dangling_sha = h.file_sha("dangling.rs");
    let dangling_edge = h.add_edge(dangling_file, "gone", 14, 18, "Exact", None);
    h.write_verdict(
        dangling_edge,
        &dangling_sha,
        None,
        "scip x `gone`().",
        OracleResolutionKind::Confirm,
    );
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![dangling_edge]).unwrap();

    let before: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(before, 3, "three verdicts before the sweep (live, sibling, dangling)");

    let deleted = store::prune_edge_oracle_without_live_edge(&h.conn).unwrap();
    assert_eq!(deleted, 1, "only the dangling verdict (no live edge anywhere) is swept");

    let after: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(after, 2, "the live + sibling verdicts survive the global sweep");
    // The two survivors still resolve to their live edges by content key.
    assert!(h.verdict(live_edge).is_some(), "the active-checkout verdict is kept");
    assert!(
        h.verdict(sibling_edge).is_some(),
        "the sibling-checkout verdict is kept (global sweep)"
    );
}

// ---------------------------------------------------------------------------
// Moniker anchors (#70, phase 3): oracle-run moniker pass + memory relocation.
// ---------------------------------------------------------------------------

use crate::query::memory::{
    RepoMemoryBindTarget, RepoMemoryCreate, create_memory, doctor_attention_count, memory_by_id,
    split_active_stale, validate_memories,
};

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

/// A definition that maps to an in-corpus symbol writes that logical symbol's moniker; a
/// definition in a document rag-rat never indexed writes nothing.
#[test]
fn oracle_run_writes_monikers_for_in_corpus_defs() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    // A dependency source the `.scip` covers but rag-rat never indexed (on disk, no `files` row).
    std::fs::write(h.root().join("dep.rs"), "fn external_fn() {}\n").unwrap();

    let bytes = scip_bytes_docs(vec![
        // `target` identifier at line 0, chars 3..9.
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
        ("dep.rs", vec![occurrence(
            0,
            3,
            14,
            "rust-analyzer cargo dep 1.0.0 external_fn().",
            SymbolRole::Definition as i32,
        )]),
    ]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "only the in-corpus def writes a moniker");
    let (moniker, tool, tool_version) = h.moniker(1001).expect("moniker row written");
    assert_eq!(moniker, TARGET_MONIKER);
    assert_eq!(tool, TOOL.as_db_str());
    assert_eq!(tool_version, VERSION);
    let total: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM logical_symbol_monikers", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "the unindexed dep def must not write a row");
}

/// A re-run is authoritative for the tool: monikers the current `.scip` no longer defines are
/// cleared, not left stale.
#[test]
fn oracle_rerun_clears_prior_monikers_for_tool() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);

    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert!(h.moniker(1001).is_some());

    // Second run: a `.scip` with no definitions at all.
    let empty = scip_bytes_docs(vec![("defs.rs", vec![])]);
    run_oracle(&h.conn, TOOL, "v2", COMMIT, WORKTREE, &empty, h.root(), None, None).unwrap();
    assert!(h.moniker(1001).is_none(), "authoritative clear removed the stale moniker");
}

/// Create a memory bound to the harness symbol, asserting the automatic `scip_moniker` binding.
fn create_target_memory(h: &Harness, symbol_id: i64) -> String {
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

/// The #70 acceptance test: a memory bound to a symbol survives a file move (with a content edit
/// the hash fallback can't survive) via moniker relocation — `relocated`, reason `moniker-match`.
#[test]
fn memory_survives_file_move_via_moniker_relocation() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let memory_id = create_target_memory(&h, sym);

    move_target_with_edit(&h, defs, "function");
    // The next oracle run sees the same moniker defined at its new home.
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let report = validate_memories(&h.conn, None).unwrap();
    assert!(report.relocated >= 1, "expected a relocation, got {report:?}");

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
    assert_eq!(symbol_binding.anchor_status, "relocated");
    assert_eq!(symbol_binding.binding_id, "moved.rs::target");
    assert_eq!(symbol_binding.path.as_deref(), Some("moved.rs"));
    assert_eq!(symbol_binding.relocation_reason.as_deref(), Some("moniker-match"));
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(moniker_binding.logical_symbol_id, Some(2002));
}

/// A moniker match under a DIFFERENT current tool_version is lower confidence: it relocates only
/// when the stored `symbol_kind` corroborates the candidate.
#[test]
fn cross_version_moniker_match_requires_kind_corroboration() {
    for (new_kind, expect_status) in [("function", "relocated"), ("struct", "gone")] {
        let h = Harness::new();
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
        h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
        h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
        let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
            0,
            3,
            9,
            TARGET_MONIKER,
            SymbolRole::Definition as i32,
        )])]);
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
        let memory_id = create_target_memory(&h, sym);

        move_target_with_edit(&h, defs, new_kind);
        // The re-run comes from an UPGRADED tool: same moniker, different tool_version.
        let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
            0,
            3,
            9,
            TARGET_MONIKER,
            SymbolRole::Definition as i32,
        )])]);
        run_oracle(&h.conn, TOOL, "v-newer", COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap();

        validate_memories(&h.conn, None).unwrap();
        let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
        let symbol_binding =
            memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
        assert_eq!(
            symbol_binding.anchor_status, expect_status,
            "cross-version match with new_kind={new_kind}"
        );
    }
}

/// #154: a `logical_symbol` binding must stay `current` across a reindex that merely SHIFTS the
/// symbol's lines (an edit elsewhere in the file). The logical symbol's id is content-derived and
/// stable, but chunk ids are reassigned on every re-chunk — so the stored `chunk_id` goes stale.
/// Before the fix the stable-id arm called `validate_bound_chunk`, which found the churned chunk_id
/// missing and returned `gone`; it must instead re-derive the chunk from the live logical symbol.
#[test]
fn logical_symbol_binding_survives_chunk_id_churn_on_reindex() {
    let h = Harness::new();
    let file = h.add_file("a.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(file, "target", "a.rs::target", "function", 0, 14);
    h.add_chunk(file, "a.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "a.rs", "target", "a.rs::target", sym);

    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target invariant".to_string(),
        body: "target stays reentrant".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { logical_symbol_id: Some(1001), ..Default::default() },
    })
    .unwrap();
    let memory_id = created.memory.memory_id;
    let original_chunk_id = created
        .memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "logical_symbol")
        .expect("logical_symbol binding")
        .chunk_id
        .expect("chunk_id bound");

    // Re-chunk the file: chunk + symbol rows get NEW rowids (as a reindex reassigns them), but the
    // logical symbol keeps its content-derived id 1001 (the symbol is unchanged, just shifted). The
    // content is byte-identical, so the only thing that moved is the chunk_id.
    h.conn.execute("DELETE FROM logical_symbol_members", []).unwrap();
    h.conn.execute("DELETE FROM chunks", []).unwrap();
    h.conn.execute("DELETE FROM symbols", []).unwrap();
    let new_sym = h.add_symbol_qualified(file, "target", "a.rs::target", "function", 0, 14);
    h.add_chunk(file, "a.rs::target", "fn target() {}\n");
    h.conn
        .execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, cfg_expr, \
             signature_hash, start_line, end_line) VALUES (1001, ?1, NULL, NULL, 1, 1)",
            params![new_sym],
        )
        .unwrap();

    validate_memories(&h.conn, None).unwrap();

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let binding = memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "logical_symbol")
        .expect("logical_symbol binding");
    assert_eq!(
        binding.anchor_status, "current",
        "a logical_symbol binding must survive chunk_id churn on reindex (#154)"
    );
    assert_ne!(
        binding.chunk_id,
        Some(original_chunk_id),
        "the binding's chunk_id should be refreshed to the re-chunked symbol's new chunk"
    );
}

/// The memory body cap is 8000 chars (raised from 4000 so detailed Invariant/Decision/BugPattern
/// memories aren't forced to drop content). Boundary: 8000 accepted, 8001 rejected.
#[test]
fn memory_body_cap_is_8000_chars() {
    let h = Harness::new();
    let make = |body: String| RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "cap test".to_string(),
        body,
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
    };
    assert!(create_memory(&h.conn, make("x".repeat(8000))).is_ok(), "8000 chars is accepted");
    let err = create_memory(&h.conn, make("x".repeat(8001))).unwrap_err();
    assert!(err.to_string().contains("body exceeds 8000"), "8001 rejected with the cap: {err}");
}

/// `doctor_attention_count` (behind the MCP staleness nudge) counts active bindings whose anchor is
/// gone/stale, excludes obsolete memories, and matches the population `memory_doctor` lists.
#[test]
fn doctor_attention_count_counts_active_gone_and_stale_bindings() {
    let h = Harness::new();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "drift test".to_string(),
        body: "x".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
    })
    .unwrap();
    let id = created.memory.memory_id;
    let set_status = |status: &str| {
        h.conn
            .execute(
                "UPDATE repo_memory_bindings SET anchor_status = ?2 WHERE memory_id = ?1",
                params![id, status],
            )
            .unwrap();
    };

    set_status("current");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 0, "current is not counted");
    set_status("gone");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 1, "gone is counted");
    set_status("stale");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 1, "stale is counted");
    // An obsolete memory drops out even with a gone binding.
    h.conn.execute("UPDATE repo_memories SET status = 'obsolete' WHERE id = ?1", [&id]).unwrap();
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 0, "obsolete is excluded");
}

/// The public `memory_attention_count` (the MCP staleness nudge's source) reads from a file DB via
/// a bare read-only open and fails open to 0 on a missing DB — it must never block a tool call.
#[test]
fn memory_attention_count_reads_file_db_and_fails_open() {
    let dir = std::env::temp_dir().join(format!("ragrat-attn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("index.sqlite");

    // Missing DB → 0 (fail-open).
    assert_eq!(crate::memory_attention_count(&db_path), 0, "missing DB is 0, never an error");

    // A real file DB with one gone binding → 1.
    {
        let rw = crate::storage::IndexConnection::open(&db_path).unwrap();
        crate::index::schema::apply(rw.connection()).unwrap();
        let created = create_memory(rw.connection(), RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "drift".to_string(),
            body: "x".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
        })
        .unwrap();
        rw.connection()
            .execute(
                "UPDATE repo_memory_bindings SET anchor_status = 'gone' WHERE memory_id = ?1",
                [&created.memory.memory_id],
            )
            .unwrap();
    }
    assert_eq!(crate::memory_attention_count(&db_path), 1, "counts the gone binding from disk");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Point the index `source_root` meta at the harness checkout so the off-index filesystem
/// existence fallback (#98) has a root to resolve a binding path against.
fn set_source_root(h: &Harness) {
    h.conn
        .execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('source_root', ?1)",
            params![h.root().to_string_lossy()],
        )
        .unwrap();
}

/// Create a bare path-bound memory and return its id + the validated anchor status of the `path`
/// binding.
fn path_binding_status_after_validate(h: &Harness, path: &str) -> String {
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: format!("note about {path}"),
        body: "this guidance is still valid".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { path: Some(path.to_string()), ..Default::default() },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "path")
        .expect("path binding")
        .anchor_status
        .clone()
}

/// #98: a memory bound to a NON-INDEXED file that exists on disk (a Containerfile, shell script,
/// `.yml`, `.toml` — anything outside the indexed language set, so it has no `files` row by
/// construction) must validate `current`, not `gone`. Acting on a false `gone` would delete valid
/// guidance.
#[test]
fn path_binding_to_unindexed_file_present_on_disk_is_current() {
    let h = Harness::new();
    set_source_root(&h);
    std::fs::create_dir_all(h.root().join("tools")).unwrap();
    std::fs::write(h.root().join("tools/bench.Containerfile"), "FROM scratch\n").unwrap();
    // Deliberately NO `files` row — this file type is outside the indexed set.
    assert_eq!(
        path_binding_status_after_validate(&h, "tools/bench.Containerfile"),
        "current",
        "a path bound to a real-but-unindexed file is an area anchor, not gone"
    );
}

/// #98: a path binding whose target is absent from BOTH the index and the filesystem is genuinely
/// `gone`.
#[test]
fn path_binding_to_missing_file_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    assert_eq!(
        path_binding_status_after_validate(&h, "tools/deleted.Containerfile"),
        "gone",
        "a path that exists nowhere is genuinely gone"
    );
}

/// #98: a SPANNED path binding (`path:start-end`) to a non-indexed file has no chunk to hash, so it
/// is `unverified` rather than `gone` — it can't be content-validated but its target is alive.
#[test]
fn spanned_path_binding_to_unindexed_file_is_unverified() {
    let h = Harness::new();
    set_source_root(&h);
    std::fs::create_dir_all(h.root().join("tools")).unwrap();
    std::fs::write(h.root().join("tools/build.sh"), "#!/bin/sh\necho hi\n").unwrap();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "spanned note".to_string(),
        body: "lines 1-2 matter".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("tools/build.sh".to_string()),
            start_line: Some(1),
            end_line: Some(2),
            ..Default::default()
        },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let pb = memory.bindings.iter().find(|b| b.binding_kind == "path").expect("path binding");
    assert_eq!(
        pb.anchor_status, "unverified",
        "a spanned binding to a non-indexed file can't be hashed but isn't gone"
    );
}

/// #98 (dir analogue): a `dir` binding to a directory that exists on disk but holds only
/// non-indexed files (so `dir_has_files` finds nothing) must validate `current`, not `gone`.
#[test]
fn dir_binding_to_unindexed_dir_present_on_disk_is_current() {
    let h = Harness::new();
    set_source_root(&h);
    std::fs::create_dir_all(h.root().join("scripts")).unwrap();
    std::fs::write(h.root().join("scripts/deploy.sh"), "#!/bin/sh\n").unwrap();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "scripts dir note".to_string(),
        body: "deploy scripts live here".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { dir: Some("scripts".to_string()), ..Default::default() },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let db = memory.bindings.iter().find(|b| b.binding_kind == "dir").expect("dir binding");
    assert_eq!(
        db.anchor_status, "current",
        "a dir present on disk with only non-indexed files is current, not gone"
    );
}

/// #98 (dir analogue): a `dir` binding to a directory absent from index and filesystem is `gone`.
#[test]
fn dir_binding_to_missing_dir_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "ghost dir note".to_string(),
        body: "nothing here".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            dir: Some("does/not/exist".to_string()),
            ..Default::default()
        },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let db = memory.bindings.iter().find(|b| b.binding_kind == "dir").expect("dir binding");
    assert_eq!(db.anchor_status, "gone", "a dir that exists nowhere is genuinely gone");
}

/// #98 review (Codex): a `path` binding names a FILE. If the file is deleted and a DIRECTORY now
/// occupies that name, the file is genuinely `gone` — the off-index fallback must use `is_file`,
/// not `exists`, so a directory at the path can't keep the file anchor alive.
#[test]
fn path_binding_to_a_dir_replacing_the_file_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    // A directory sits where the bound file used to be.
    std::fs::create_dir_all(h.root().join("tools/build.sh")).unwrap();
    assert_eq!(
        path_binding_status_after_validate(&h, "tools/build.sh"),
        "gone",
        "a directory occupying a file-bound path does not keep the file anchor alive"
    );
}

/// #98 review (Codex): path bindings are repo-root-relative by contract. An absolute path or one
/// with `..` could resolve OUTSIDE `source_root` (`root.join(abs)` replaces the root), letting an
/// unrelated out-of-repo file mark the anchor alive. Such a binding must be treated as `gone`.
#[test]
fn path_binding_escaping_source_root_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    // A real file OUTSIDE the source_root, reachable only by escaping it via `..`.
    let outside = h.root().parent().unwrap().join(format!("escape-{}.sh", std::process::id()));
    std::fs::write(&outside, "#!/bin/sh\n").unwrap();
    let traversal = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    let status = path_binding_status_after_validate(&h, &traversal);
    let _ = std::fs::remove_file(&outside);
    assert_eq!(
        status, "gone",
        "a `..`-escaping path must not validate against an out-of-repo file"
    );
}

/// #98 review (Codex): under a shared DB across git worktrees, `index_meta.source_root` holds
/// whichever worktree last indexed. `validate_memories` must prefer the caller-supplied ACTIVE
/// checkout root so a sibling worktree checks its OWN filesystem, not the last indexer's.
#[test]
fn validate_prefers_active_root_over_persisted_meta() {
    let h = Harness::new();
    // Persisted meta points at a bogus root (a stale/sibling worktree); the active checkout is
    // h.root(), where the file actually lives.
    h.conn
        .execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('source_root', ?1)",
            params![h.root().join("nonexistent-worktree").to_string_lossy()],
        )
        .unwrap();
    std::fs::create_dir_all(h.root().join("tools")).unwrap();
    std::fs::write(h.root().join("tools/notes.Containerfile"), "FROM scratch\n").unwrap();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "worktree note".to_string(),
        body: "still valid".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("tools/notes.Containerfile".to_string()),
            ..Default::default()
        },
    })
    .unwrap();
    validate_memories(&h.conn, Some(h.root())).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let pb = memory.bindings.iter().find(|b| b.binding_kind == "path").expect("path binding");
    assert_eq!(
        pb.anchor_status, "current",
        "the active checkout root must win over the stale persisted source_root"
    );
}

/// `scip_moniker` binding statuses: `unverified` when the tool has no data at all, `gone` when
/// current data lacks the moniker, `stale` when the moniker's row dangles (its content-derived
/// logical id died after the symbol changed).
#[test]
fn moniker_binding_validation_statuses() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let memory_id = create_target_memory(&h, sym);

    let moniker_status = |h: &Harness| -> String {
        validate_memories(&h.conn, None).unwrap();
        let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
        memory
            .bindings
            .iter()
            .find(|b| b.binding_kind == "scip_moniker")
            .expect("moniker binding")
            .anchor_status
            .clone()
    };

    assert_eq!(moniker_status(&h), "current");

    // Dangling: the symbol changed — its content-derived logical id died, the row points nowhere.
    h.conn.execute("DELETE FROM logical_symbols WHERE id = 1001", []).unwrap();
    assert_eq!(moniker_status(&h), "stale");

    // A lagging moniker anchor must NOT demote the memory's evidence — the symbol binding is
    // intact, and the moniker self-heals on the next oracle run.
    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_status = memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "symbol")
        .map(|b| b.anchor_status.clone())
        .unwrap();
    assert_eq!(symbol_status, "current");
    let (direct, stale) = split_active_stale(vec![memory]);
    assert_eq!(direct.len(), 1, "stale moniker anchor must not demote the memory");
    assert!(stale.is_empty());

    // ...and must not count toward the anchor-health totals that drive the "run memory doctor"
    // warnings — doctor hides moniker rows, so counting them would warn about nothing visible.
    let health = crate::query::memory::anchor_health_counts(&h.conn).unwrap();
    assert_eq!(health.stale, 0, "auxiliary moniker anchors are excluded from health counts");

    // Gone: the tool has current data, but not this moniker.
    h.conn
        .execute(
            "UPDATE logical_symbol_monikers SET moniker = 'rust-analyzer cargo test_crate 0.1.0 \
             other().' WHERE logical_symbol_id = 1001",
            [],
        )
        .unwrap();
    assert_eq!(moniker_status(&h), "gone");

    // Unverified: no oracle data for the tool at all.
    h.conn.execute("DELETE FROM logical_symbol_monikers", []).unwrap();
    assert_eq!(moniker_status(&h), "unverified");
}

/// Several defs containment-map to one logical symbol (a struct's fields have no symbol row, so
/// they map up to the enclosing struct alongside its own def). The stored moniker must be the
/// DETERMINISTIC best — shortest, then lexicographic — i.e. the symbol's own moniker, never an
/// arbitrary member's (HashMap-order last-writer would silently break relocation between runs).
#[test]
fn moniker_for_symbol_with_member_defs_is_the_symbols_own() {
    let h = Harness::new();
    // `struct Config { db: u32 }` — one symbol row spanning the whole struct.
    let defs = h.add_file("defs.rs", "struct Config { db: u32 }\n");
    let sym = h.add_symbol_qualified(defs, "Config", "defs.rs::Config", "struct", 0, 25);
    h.add_logical_symbol(3003, "defs.rs", "Config", "defs.rs::Config", sym);

    let struct_moniker = "rust-analyzer cargo test_crate 0.1.0 Config#";
    let field_moniker = "rust-analyzer cargo test_crate 0.1.0 Config#db.";
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![
        // Field def first so a naive insertion-order winner would pick it.
        occurrence(0, 16, 18, field_moniker, SymbolRole::Definition as i32),
        occurrence(0, 7, 13, struct_moniker, SymbolRole::Definition as i32),
    ])]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "one row per logical symbol, not per def");
    let (moniker, ..) = h.moniker(3003).expect("moniker row written");
    assert_eq!(moniker, struct_moniker, "the symbol's own (shortest) moniker wins");
}

/// A synthetic zero-width per-file definition (scip-typescript emits one ending in `/` at byte
/// `0..0`) must NOT become a symbol's moniker: it containment-maps to the first symbol (whose span
/// starts at 0) and, being shorter, would win shortest-moniker selection and clobber the real one.
/// The namespace-suffix filter in the moniker pass drops it.
#[test]
fn synthetic_file_definition_does_not_overwrite_a_symbols_moniker() {
    let h = Harness::new();
    // `greet`'s span starts at byte 0, so a 0..0 file def would containment-map to it.
    let defs = h.add_file("a.ts", "function greet() {}\n");
    let sym = h.add_symbol_qualified(defs, "greet", "a.ts::greet", "function", 0, 19);
    h.add_logical_symbol(4004, "a.ts", "greet", "a.ts::greet", sym);

    let greet_moniker = "rust-analyzer cargo test_crate 0.1.0 greet().";
    let file_moniker = "rust-analyzer cargo test_crate 0.1.0 `a.ts`/"; // shorter; ends in `/`
    let bytes = scip_bytes_docs(vec![("a.ts", vec![
        // File def first + at 0..0 so a naive containment+shortest winner would pick it.
        occurrence(0, 0, 0, file_moniker, SymbolRole::Definition as i32),
        occurrence(0, 9, 14, greet_moniker, SymbolRole::Definition as i32),
    ])]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "the namespace symbol must not add a second row");
    let (moniker, ..) = h.moniker(4004).expect("moniker row written");
    assert_eq!(moniker, greet_moniker, "the symbol's own moniker wins, not the file's");
}

/// Moniker-STRING drift (Codex P2): rust-analyzer monikers embed the Cargo package version, so a
/// routine version bump rewrites every string while no symbol changes. The binding's stored
/// content-derived logical id is still live, so validation re-anchors the binding to the new
/// string (`relocated`, reason `moniker-refresh`) instead of marking it gone forever — and a
/// LATER file move can still relocate via the refreshed moniker.
#[test]
fn moniker_string_drift_rebinds_via_live_logical_symbol_then_survives_move() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let memory_id = create_target_memory(&h, sym);

    // Crate version bump: same symbol, same location, NEW moniker string + tool version.
    let bumped_moniker = "rust-analyzer cargo test_crate 0.2.0 target().";
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        bumped_moniker,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, "v-bumped", COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(moniker_binding.binding_id, bumped_moniker, "rebound to the current string");
    assert_eq!(moniker_binding.moniker_tool_version.as_deref(), Some("v-bumped"));
    assert_eq!(moniker_binding.relocation_reason.as_deref(), Some("moniker-refresh"));

    // The refreshed anchor still does its job: a later move + content edit relocates via it.
    move_target_with_edit(&h, defs, "function");
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        bumped_moniker,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, "v-bumped", COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
    assert_eq!(symbol_binding.anchor_status, "relocated");
    assert_eq!(symbol_binding.path.as_deref(), Some("moved.rs"));
    assert_eq!(symbol_binding.relocation_reason.as_deref(), Some("moniker-match"));
}

/// The string-resolution path must NOT refresh the bind-time `moniker_tool_version` (Codex P1):
/// the cross-version corroboration gate compares the CURRENT data's version against bind-time
/// provenance, and a "last verified" refresh would silently downgrade a real cross-version match
/// to same-version.
#[test]
fn string_resolution_preserves_bind_time_tool_version() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let memory_id = create_target_memory(&h, sym);

    // Move with the SAME moniker string but a NEWER tool version: the stored logical id is dead,
    // so validation takes the string-resolution path.
    move_target_with_edit(&h, defs, "function");
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, "v-newer", COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    validate_memories(&h.conn, None).unwrap();

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(
        moniker_binding.moniker_tool_version.as_deref(),
        Some(VERSION),
        "bind-time provenance must survive a string-resolution relocate"
    );
}

/// Bare `path` bindings are AREA anchors (like `dir` bindings): a file edit must not stale them —
/// before this, every commit permanently staled every area-level note bound to a touched file,
/// burying real staleness signals. A SPANNED `path:start-end` binding claims specific content and
/// keeps the content-hash staleness.
#[test]
fn bare_path_binding_survives_file_edit_spanned_goes_stale() {
    let h = Harness::new();
    let file = h.add_file("notes.rs", "fn a() {}\nfn b() {}\n");

    let bind_path = |start: Option<i64>, end: Option<i64>, title: &str| {
        create_memory(&h.conn, RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: title.to_string(),
            body: "area note".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            bind: RepoMemoryBindTarget {
                path: Some("notes.rs".to_string()),
                start_line: start,
                end_line: end,
                ..Default::default()
            },
        })
        .unwrap()
        .memory
        .memory_id
    };
    let bare = bind_path(None, None, "bare path note");
    let spanned = bind_path(Some(1), Some(1), "spanned path note");

    // Edit the file: new content, new sha on the files row.
    h.set_file_sha(file, "edited-sha");
    validate_memories(&h.conn, None).unwrap();

    let status =
        |id: &str| memory_by_id(&h.conn, id).unwrap().unwrap().bindings[0].anchor_status.clone();
    assert_eq!(
        status(&bare),
        "current",
        "bare path binding is an area anchor — never content-stale"
    );
    assert_eq!(status(&spanned), "stale", "spanned path binding still claims content");

    // Deleting the file row sends both to gone.
    h.conn.execute("DELETE FROM files WHERE id = ?1", [file]).unwrap();
    validate_memories(&h.conn, None).unwrap();
    assert_eq!(status(&bare), "gone");
    assert_eq!(status(&spanned), "gone");
}

// ---------------------------------------------------------------------------
// Pre-spawn gate (#83): the mid-subprocess TOCTOU the post-exit snapshot can't see.
// ---------------------------------------------------------------------------

/// Build the standard caller/defs corpus + scip, then run the oracle with explicit
/// production/pre-spawn maps. Returns (verdict for the edge, report, defs logical id).
fn run_with_pins(
    pre_spawn: impl Fn(&str, &str) -> std::collections::HashMap<String, String>,
) -> (Option<(String, Option<i64>, String)>, super::OracleReport) {
    let h = Harness::new();
    let caller_text = "fn caller() { target(); }\n";
    let defs_text = "fn target() {}\n";
    let caller = h.add_file("caller.rs", caller_text);
    let defs = h.add_file("defs.rs", defs_text);
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let edge = h.add_edge(caller, "target", 14, 20, "NameOnly", None);

    let bytes = scip_bytes_docs(vec![
        ("caller.rs", vec![occurrence(0, 14, 20, TARGET_MONIKER, 0)]),
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
    ]);
    // The post-exit production snapshot agrees with disk (and disk agrees with the index): every
    // #82 gate passes. Only the PRE-SPAWN snapshot distinguishes the mid-subprocess scenarios.
    let caller_sha = sha256_hex(caller_text.as_bytes());
    let defs_sha = sha256_hex(defs_text.as_bytes());
    let production: std::collections::HashMap<String, String> =
        [("caller.rs".to_string(), caller_sha.clone()), ("defs.rs".to_string(), defs_sha.clone())]
            .into();
    let pre = pre_spawn(&caller_sha, &defs_sha);
    let report = run_oracle(
        &h.conn,
        TOOL,
        VERSION,
        COMMIT,
        WORKTREE,
        &bytes,
        h.root(),
        Some(&production),
        Some(&pre),
    )
    .unwrap();
    let moniker_written = h.moniker(1001).is_some();
    assert_eq!(
        moniker_written,
        pre.get("defs.rs").map(String::as_str) == Some(defs_sha.as_str()),
        "moniker write must follow the def document's pre-spawn gate"
    );
    (h.verdict(edge), report)
}

/// Control: a pre-spawn snapshot matching the indexed shas changes nothing — the verdict lands.
#[test]
fn pre_spawn_gate_passes_when_nothing_reindexed() {
    let (verdict, report) = run_with_pins(|caller_sha, defs_sha| {
        [
            ("caller.rs".to_string(), caller_sha.to_string()),
            ("defs.rs".to_string(), defs_sha.to_string()),
        ]
        .into()
    });
    assert!(verdict.is_some(), "matching pre-spawn snapshot must not block the verdict");
    assert_eq!(report.skipped_drifted, 0);
    assert_eq!(report.oracle_only_calls, 0, "the covered call is no recall gap");
}

/// CALL-SITE document edited during the subprocess: index/disk/production all carry the NEW
/// content (every #82 gate passes), but the pre-spawn snapshot still has the OLD sha — the
/// `.scip` was built from bytes nobody can verify, so the candidate is skipped, never verdicted.
#[test]
fn pre_spawn_gate_skips_call_site_reindexed_mid_subprocess() {
    let (verdict, report) = run_with_pins(|_caller_sha, defs_sha| {
        [
            ("caller.rs".to_string(), "pre-spawn-old-sha".to_string()),
            ("defs.rs".to_string(), defs_sha.to_string()),
        ]
        .into()
    });
    assert!(verdict.is_none(), "mid-subprocess call-site reindex must skip the verdict");
    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.rows_written, 0);
    // A skipped-as-drifted candidate is an ABSTENTION, not a heuristic miss: its occurrence must
    // not count as a recall gap (#88 review).
    assert_eq!(report.oracle_only_calls, 0, "drifted call-site doc is excluded from recall");
}

/// DEFINITION document edited during the subprocess: the call-site gate passes, but the resolved
/// symbol came from converting the def occurrence against bytes the pre-spawn snapshot can't
/// confirm — the verdict (and the moniker, asserted in the helper) is skipped.
#[test]
fn pre_spawn_gate_skips_definition_reindexed_mid_subprocess() {
    let (verdict, report) = run_with_pins(|caller_sha, _defs_sha| {
        [
            ("caller.rs".to_string(), caller_sha.to_string()),
            ("defs.rs".to_string(), "pre-spawn-old-sha".to_string()),
        ]
        .into()
    });
    assert!(verdict.is_none(), "mid-subprocess def reindex must skip the verdict");
    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.rows_written, 0);
    // The call site is clean but its DEF document drifted: the occurrence resolving into it must
    // not count as a recall gap either (#88 review).
    assert_eq!(report.oracle_only_calls, 0, "drifted def doc is excluded from recall");
}

// ---------------------------------------------------------------------------
// Edge string interning (#79): compat view shape, round-trip writes, dedup, V020 conversion.
// ---------------------------------------------------------------------------

/// The V020 shape: `edges` is a VIEW over `edges_data` + the `name_strings` dictionary, with
/// INSTEAD OF triggers; both backing tables are STRICT; the int indexes replaced the TEXT ones.
#[test]
fn edges_is_a_compat_view_over_interned_tables() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    let object_type = |name: &str| -> String {
        conn.query_row("SELECT type FROM sqlite_master WHERE name = ?1", [name], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(object_type("edges"), "view");
    assert_eq!(object_type("edges_data"), "table");
    assert_eq!(object_type("name_strings"), "table");
    for trigger in ["edges_view_insert", "edges_view_update", "edges_view_delete"] {
        assert_eq!(object_type(trigger), "trigger", "{trigger} must exist");
    }
    let index_table: String = conn
        .query_row("SELECT tbl_name FROM sqlite_master WHERE name = 'idx_edges_to_name'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(index_table, "edges_data", "TEXT indexes were replaced by int indexes");
}

/// Writes through the view round-trip with the legacy semantics: defaults for omitted columns,
/// UPDATE rewrites, DELETE, and shared strings deduplicate in the dictionary.
#[test]
fn view_writes_round_trip_and_dedup() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); helper(); }\n");
    // Two edges sharing from_name/edge_kind/confidence; insert via the view, omitting the
    // defaulted columns (source_* spans, resolution) like legacy SQL could.
    h.conn
        .execute(
            "INSERT INTO edges(source_file_id, from_name, to_name, edge_kind, confidence) VALUES \
             (?1, 'a.rs::caller', 'target', 'calls_name', 'NameOnly')",
            params![f],
        )
        .unwrap();
    h.conn
        .execute(
            "INSERT INTO edges(source_file_id, from_name, to_name, edge_kind, confidence) VALUES \
             (?1, 'a.rs::caller', 'helper', 'calls_name', 'NameOnly')",
            params![f],
        )
        .unwrap();

    let (resolution, start_line): (String, i64) = h
        .conn
        .query_row(
            "SELECT resolution, source_start_line FROM edges WHERE to_name = 'target'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(resolution, "unresolved", "legacy DEFAULT applies through the trigger");
    assert_eq!(start_line, 0, "legacy DEFAULT applies through the trigger");

    // Shared strings appear once: from_name, edge_kind, confidence, resolution are common.
    let shared: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM name_strings WHERE value IN ('a.rs::caller', 'calls_name', \
             'NameOnly', 'unresolved')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(shared, 4, "each shared string interned exactly once across both edges");

    // UPDATE through the view rewrites the row (the maintenance/migration path).
    h.conn
        .execute("UPDATE edges SET confidence = 'Syntactic' WHERE to_name = 'target'", [])
        .unwrap();
    let confidence: String = h
        .conn
        .query_row("SELECT confidence FROM edges WHERE to_name = 'target'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(confidence, "Syntactic");

    // DELETE through the view removes the backing row.
    h.conn.execute("DELETE FROM edges WHERE to_name = 'helper'", []).unwrap();
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edges_data", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 1);
}

/// V020 conversion: a legacy `edges` TABLE (pre-interning shape, with rows) converts into the
/// dictionary + `edges_data` behind the view, byte-equal through the view, ids preserved, and the
/// `edge_oracle` FK re-pointed so verdict cascade still fires.
#[test]
fn v020_converts_a_legacy_edges_table() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Recreate the LEGACY world: drop the view shape and install a real old-format table with a
    // row, plus an edge_oracle row referencing it.
    conn.execute_batch(
        "
        DROP TRIGGER edges_view_insert;
        DROP TRIGGER edges_view_update;
        DROP TRIGGER edges_view_delete;
        DROP VIEW edges;
        DELETE FROM edges_data;
        DELETE FROM name_strings;
        CREATE TABLE edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_file_id INTEGER,
            from_symbol_id INTEGER,
            to_symbol_id INTEGER,
            from_name TEXT,
            to_name TEXT NOT NULL,
            source_start_line INTEGER NOT NULL DEFAULT 0,
            source_end_line INTEGER NOT NULL DEFAULT 0,
            source_start_byte INTEGER NOT NULL DEFAULT 0,
            source_end_byte INTEGER NOT NULL DEFAULT 0,
            target_start_line INTEGER,
            target_end_line INTEGER,
            target_qualified_name TEXT,
            evidence TEXT,
            receiver_hint TEXT,
            resolution TEXT NOT NULL DEFAULT 'unresolved',
            callee_start_byte INTEGER,
            callee_end_byte INTEGER,
            edge_kind TEXT NOT NULL,
            confidence TEXT NOT NULL
        );
        DROP TABLE edge_oracle;
        CREATE TABLE edge_oracle(
            edge_id INTEGER NOT NULL,
            file_sha TEXT NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            resolved_symbol_id INTEGER,
            scip_symbol TEXT NOT NULL,
            kind TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(edge_id, tool, tool_version),
            FOREIGN KEY(edge_id) REFERENCES edges(id) ON DELETE CASCADE
        ) STRICT;
        INSERT INTO edges(id, to_name, from_name, edge_kind, confidence, resolution, evidence)
        VALUES (7, 'target', 'caller', 'calls_name', 'Syntactic', 'qualified_suffix', 'target()');
        INSERT INTO edge_oracle(edge_id, file_sha, tool, tool_version, resolved_symbol_id, \
         scip_symbol, kind, computed_at)
        VALUES (7, 'sha', 'rust-analyzer', 'v1', NULL, 'sym', 'upgrade', 0);
        ",
    )
    .unwrap();

    schema::apply_edge_string_interning(&conn).unwrap();

    let (to_name, evidence, resolution): (String, String, String) = conn
        .query_row("SELECT to_name, evidence, resolution FROM edges WHERE id = 7", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        (to_name.as_str(), evidence.as_str(), resolution.as_str()),
        ("target", "target()", "qualified_suffix")
    );
    let object_type: String = conn
        .query_row("SELECT type FROM sqlite_master WHERE name = 'edges'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(object_type, "view", "legacy table converted to the view shape");

    // The re-pointed FK still cascades verdicts away with their edge.
    conn.execute("DELETE FROM edges_data WHERE id = 7", []).unwrap();
    let verdicts: i64 =
        conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(verdicts, 0, "edge_oracle FK was re-pointed at edges_data");
}

/// The query_warm regression (#79 follow-up): an OR-branch string equality on a dictionary
/// column cannot be transformed by the planner through the view's value joins — it silently
/// picks a non-selective index (`to_symbol_id IS NULL` scans most of the table) instead of
/// `idx_edges_to_name`. Hot readers therefore compare the view's exposed `to_name_id` against a
/// constant dictionary-lookup subquery. This pins the PLAN: the caller-count predicate shape
/// must drive the to_name int index, and the legacy string form must never silently return.
#[test]
fn or_branch_name_predicates_use_the_to_name_index() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();

    let plan = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.join("\n")
    };

    // The production count_callers / callers() / traversal predicate shape.
    let fixed = plan(
        "SELECT COUNT(*) FROM edges WHERE edge_kind IN ('calls_name','constructs','uses_macro') \
         AND (to_symbol_id = 5 OR (to_symbol_id IS NULL AND to_name_id = (SELECT id FROM \
         name_strings WHERE value = 'x')))",
    );
    assert!(
        fixed.contains("idx_edges_to_name"),
        "the OR's name branch must drive the to_name int index, got plan:\n{fixed}"
    );

    // Simple equality through the view still transforms on its own (no subquery needed).
    let simple = plan("SELECT id FROM edges WHERE to_name = 'x'");
    assert!(
        simple.contains("idx_edges_to_name"),
        "plain to_name equality must use the int index, got plan:\n{simple}"
    );
}

/// C2 live before/after report: the heuristic "before" counts come from the `edges` index, the
/// after-side verdict counts + run-only fields come from the run's `OracleReport`, the precision/
/// recall come from diffing `edge_oracle`, and the moniker tally from `logical_symbol_monikers` —
/// all stamped onto the C0 schema with the caller's profile + provenance.
#[test]
fn resolution_report_assembles_before_after_from_index() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() {}\n");

    // Heuristic "before": four Exact edges resolved to an in-corpus symbol (resolved_before = 4),
    // three NameOnly edges (unresolved_before = 3); total_edges = 7 (all carry a callee range).
    let target = h.add_symbol(f, "target", 3, 9);
    h.add_edge(f, "target", 14, 20, "Exact", Some(target));
    h.add_edge(f, "other", 21, 26, "Exact", Some(target));
    let up = h.add_edge(f, "up", 30, 32, "NameOnly", None);
    let ext = h.add_edge(f, "ext", 33, 36, "NameOnly", None);
    let _plain = h.add_edge(f, "plain", 37, 42, "NameOnly", None);
    let conf_sym = h.add_symbol(f, "conf", 43, 47);
    let conf = h.add_edge(f, "conf", 48, 52, "Exact", Some(conf_sym));
    let contra_sym = h.add_symbol(f, "contra", 53, 59);
    let contra = h.add_edge(f, "contra", 60, 66, "Exact", Some(contra_sym));

    // Oracle verdicts: Upgrade + ResolvedExternal on the NameOnly edges; Confirm + Contradict on
    // two Exact edges → precision = 1 / (1 + 1) = 0.5.
    let file_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    let write = |edge: i64, kind: OracleResolutionKind, resolved: Option<i64>| {
        h.write_verdict(edge, &file_sha, resolved, "scip x `t`().", kind);
    };
    write(up, OracleResolutionKind::Upgrade, Some(target));
    write(ext, OracleResolutionKind::ResolvedExternal, None);
    write(conf, OracleResolutionKind::Confirm, Some(conf_sym));
    write(contra, OracleResolutionKind::Contradict, Some(contra_sym));

    // Two logical symbols enriched with a moniker for this tool.
    for id in [1_i64, 2] {
        h.conn
            .execute(
                "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, \
                 moniker, computed_at) VALUES (?1, ?2, ?3, ?4, 0)",
                params![id, TOOL.as_db_str(), VERSION, format!("m{id}")],
            )
            .unwrap();
    }

    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert("rust".to_string(), vec!["src".to_string()]);
    let profile = super::CorpusProfile {
        corpus_id: "rust-test".to_string(),
        tier: "small".to_string(),
        repo: "r".to_string(),
        rev: "1".to_string(),
        tool: TOOL.as_db_str().to_string(),
        prepare: Vec::new(),
        bindings,
        health: super::CorpusHealth {
            expected_min_heuristic_edges: 1,
            expected_min_oracle_examined: 1,
            expected_max_skipped_drifted: 0,
            expected_min_symbols_with_moniker: 1,
            expected_min_resolved_external: None,
            timeout_minutes: 8,
        },
    };
    // `tool_version` MUST match the VERSION the verdicts were written under — it's the metric scope
    // key as well as the report envelope's provenance.
    let provenance = super::RunProvenance {
        tool_version: VERSION.to_string(),
        rag_rat_commit: "commit".to_string(),
        worktree_id: WORKTREE.to_string(),
        production_sha: "prod".to_string(),
    };
    let run = super::OracleReport {
        upgraded: 1,
        resolved_external: 1,
        confirmed: 1,
        contradicted: 1,
        covered_calls: 8,
        oracle_only_calls: 2,
        skipped_drifted: 2,
        monikers_written: 2,
        ..Default::default()
    };

    let report =
        super::resolution_report(&h.conn, &profile, &provenance, TOOL, COMMIT, WORKTREE, &run)
            .unwrap();

    // Before/after resolution.
    assert_eq!(report.resolution.total_edges, 7);
    assert_eq!(report.resolution.resolved_before, 4);
    assert_eq!(report.resolution.unresolved_before, 3);
    assert_eq!(report.resolution.resolved_after, 4 + 1 + 1);
    // Verdict transitions (from the run) + moniker tally (from the index) + run-only drift.
    assert_eq!(report.upgraded, 1);
    assert_eq!(report.resolved_external, 1);
    assert_eq!(report.confirmed, 1);
    assert_eq!(report.contradicted, 1);
    assert_eq!(report.symbols_with_moniker, 2);
    assert_eq!(report.skipped_drifted, 2);
    // Diffed metrics: precision 1/(1+1), recall covered/(covered+oracle_only) = 8/10.
    assert!((report.metrics.precision - 0.5).abs() < 1e-9, "precision: {report:?}");
    assert!((report.metrics.recall - 0.8).abs() < 1e-9, "recall: {report:?}");
    // Envelope.
    assert_eq!(report.corpus_profile_hash, profile.hash());
    assert_eq!(report.tool_version, VERSION);
    assert_eq!(report.tool, TOOL.as_db_str());
}

/// `scip::stabilize_moniker_version` pins a scip-typescript local moniker's version to `_` so a
/// `package.json` version bump doesn't churn it (the basis for moniker-anchored memory relocation),
/// while leaving every other tool's symbols — and unparsable / package-less / local ones —
/// untouched.
#[test]
fn stabilize_moniker_version_pins_typescript_package_version() {
    use super::scip::stabilize_moniker_version as norm;

    // The version (3rd package field) is rewritten to `_`; name + descriptors are preserved.
    let v1 = "scip-typescript npm tsmon 9.9.9 `src/a.ts`/greet().";
    let v2 = "scip-typescript npm tsmon 9.9.10 `src/a.ts`/greet().";
    let normed = norm(OracleTool::ScipTypescript, v1);
    assert_eq!(normed, "scip-typescript npm tsmon _ `src/a.ts`/greet().");
    // Two different package versions normalize to the SAME moniker — the relocation invariant.
    assert_eq!(norm(OracleTool::ScipTypescript, v1), norm(OracleTool::ScipTypescript, v2));
    // Already `_` is borrowed unchanged (idempotent).
    let already = "scip-typescript npm tsmon _ `src/a.ts`/greet().";
    assert!(matches!(norm(OracleTool::ScipTypescript, already), std::borrow::Cow::Borrowed(_)));

    // Other tools pass through verbatim — scip-python already pins via `--project-version _`.
    assert_eq!(norm(OracleTool::ScipPython, v1), v1);
    assert_eq!(norm(OracleTool::RustAnalyzer, v1), v1);
    // A local symbol (no package) is left alone.
    let local = "local 42";
    assert_eq!(norm(OracleTool::ScipTypescript, local), local);
}

/// The per-tool default position encoding for SCIP documents that leave the field unset:
/// scip-typescript and scip-java emit UTF-16 columns (confirmed empirically), the rest stay
/// Unspecified.
#[test]
fn default_position_encoding_is_utf16_for_typescript_and_java() {
    use ::scip::types::PositionEncoding::{
        UTF16CodeUnitOffsetFromLineStart as U16, UnspecifiedPositionEncoding as UNSPEC,
    };
    assert_eq!(OracleTool::ScipTypescript.default_position_encoding(), U16);
    assert_eq!(OracleTool::ScipJava.default_position_encoding(), U16);
    assert_eq!(OracleTool::RustAnalyzer.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::ScipClang.default_position_encoding(), UNSPEC);
    assert_eq!(OracleTool::ScipPython.default_position_encoding(), UNSPEC);
}
