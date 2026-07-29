//! Batched, active-scope compositions for `/api/file/symbols` and `/api/file/graph`.

use std::borrow::Cow;
use std::collections::HashMap;

use rag_rat_base::canonical;
use rag_rat_query::load_bearing::{self, LoadBearingBucket, OracleContext};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use unicase::UniCase;

use crate::index::IndexDatabase;

#[derive(Debug, Serialize)]
pub struct LensFileSymbols {
    pub symbols: Vec<LensSymbol>,
}

#[derive(Debug, Serialize)]
pub struct LensSymbol {
    pub name: String,
    pub qname: Option<String>,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_test: bool,
    pub signature: Option<String>,
    pub fan_in: u64,
    pub fan_out: u64,
}

#[derive(Debug, Serialize)]
pub struct LensFileGraph {
    pub symbols: Vec<LensFileSymbolGraph>,
}

#[derive(Debug, Serialize)]
pub struct LensFileSymbolGraph {
    pub name: String,
    pub qname: Option<String>,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_test: bool,
    pub callers: LensGraphCallerCounts,
    pub fan_in_score: f64,
    pub fan_in_bucket: LoadBearingBucket,
    pub dispatch: Vec<LensDispatchDetail>,
}

#[derive(Debug, Default, Serialize)]
pub struct LensGraphCallerCounts {
    pub exact: u64,
    pub syntactic: u64,
    pub name_only: u64,
    pub ambiguous: u64,
    pub tests: u64,
    pub dispatch: u64,
}

#[derive(Debug, Serialize)]
pub struct LensDispatchDetail {
    pub variant: Option<String>,
    pub direction: &'static str,
    pub other_name: Option<String>,
    pub other_path: Option<String>,
    pub other_line: Option<i64>,
}

#[derive(Debug)]
struct FileSymbolRow {
    id: i64,
    name: String,
    qname: Option<String>,
    kind: String,
    start_line: i64,
    end_line: i64,
    is_test: bool,
    signature: Option<String>,
    fan_in: u64,
    fan_out: u64,
    callers: LensGraphCallerCounts,
}

#[derive(Debug)]
struct DispatchRow {
    from_symbol_id: Option<i64>,
    to_symbol_id: Option<i64>,
    evidence: Option<String>,
    source_start_line: i64,
    target_start_line: Option<i64>,
    from_name: Option<String>,
    from_path: Option<String>,
    to_name: Option<String>,
    to_path: Option<String>,
}

impl IndexDatabase {
    /// Return the active-scope spelling for a safe editor path. The fallback scan is enabled only
    /// when the serving filesystem advertised case-insensitive path semantics.
    pub fn lens_canonical_file_path(
        &self,
        path: &str,
        case_insensitive: bool,
    ) -> anyhow::Result<Option<String>> {
        let conn = self.storage.connection();
        let exact = conn
            .query_row("SELECT path FROM files WHERE path = ?1 LIMIT 1", [path], |row| row.get(0))
            .optional()?;
        if exact.is_some() || !case_insensitive {
            return Ok(exact);
        }
        // A case-insensitive volume matches names by Unicode case folding, so the comparison has to
        // fold too. SQLite's built-in `NOCASE` folds ASCII only (`Ä.rs` never reaches the indexed
        // `ä.rs`), and lowercasing is not folding either — Greek `ς.rs` and `σ.rs` stay distinct
        // under `to_lowercase` although the filesystem treats them as one name. A miss here empties
        // every downstream exact-path lane for a file the editor has open. Folding in Rust costs no
        // more than a SQL predicate: the unique index over `path` collates binary, so any
        // case-insensitive comparison scans the table anyway. Path order keeps the answer
        // deterministic when several spellings of one name are indexed.
        let wanted = folded_path(path);
        let mut statement = conn.prepare("SELECT path FROM files ORDER BY path")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let candidate: String = row.get(0)?;
            if folded_path(&candidate) == wanted {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    pub fn lens_file_symbols(&self, path: &str) -> anyhow::Result<LensFileSymbols> {
        let symbols = list_file_symbols_with_graph_counts(self.storage.connection(), path)?
            .into_iter()
            .map(|row| LensSymbol {
                name: row.name,
                qname: row.qname,
                kind: row.kind,
                start_line: row.start_line,
                end_line: row.end_line,
                is_test: row.is_test,
                signature: row.signature,
                fan_in: row.fan_in,
                fan_out: row.fan_out,
            })
            .collect();
        Ok(LensFileSymbols { symbols })
    }

    pub fn lens_file_graph(&self, path: &str) -> anyhow::Result<LensFileGraph> {
        let rows = list_file_symbols_with_graph_counts(self.storage.connection(), path)?;
        if rows.is_empty() {
            return Ok(LensFileGraph { symbols: Vec::new() });
        }
        let symbol_ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let effects = self.load_bearing_oracle_effects()?;
        let importance = load_bearing::scoped_weighted_fan_in_many(
            self.storage.connection(),
            &symbol_ids,
            &OracleContext { effects: effects.as_ref() },
        )?;
        let dispatch = list_file_dispatch_details(self.storage.connection(), path)?;
        let mut dispatch_by_symbol: HashMap<i64, Vec<LensDispatchDetail>> = HashMap::new();
        for detail in dispatch {
            if let Some(symbol_id) = detail.from_symbol_id
                && symbol_ids.contains(&symbol_id)
            {
                dispatch_by_symbol.entry(symbol_id).or_default().push(LensDispatchDetail {
                    variant: detail.evidence.clone(),
                    direction: "constructs",
                    other_name: detail.to_name.clone(),
                    other_path: detail.to_path.clone(),
                    other_line: detail.target_start_line,
                });
            }
            if let Some(symbol_id) = detail.to_symbol_id
                && symbol_ids.contains(&symbol_id)
            {
                dispatch_by_symbol.entry(symbol_id).or_default().push(LensDispatchDetail {
                    variant: detail.evidence,
                    direction: "handled",
                    other_name: detail.from_name,
                    other_path: detail.from_path,
                    other_line: Some(detail.source_start_line),
                });
            }
        }
        let symbols = rows
            .into_iter()
            .map(|row| {
                let load = importance.get(&row.id);
                LensFileSymbolGraph {
                    name: row.name,
                    qname: row.qname,
                    kind: row.kind,
                    start_line: row.start_line,
                    end_line: row.end_line,
                    is_test: row.is_test,
                    callers: row.callers,
                    fan_in_score: load.map_or(0.0, |value| value.score),
                    fan_in_bucket: load.map_or(LoadBearingBucket::Low, |value| value.bucket),
                    dispatch: dispatch_by_symbol.remove(&row.id).unwrap_or_default(),
                }
            })
            .collect();
        Ok(LensFileGraph { symbols })
    }
}

/// Prepare a path for the comparison a case-insensitive volume performs: NFC first, because a
/// decomposed spelling (`a` + U+0308) names the same file as the composed `ä` there, then Unicode
/// case folding through `UniCase`. ASCII paths — nearly every candidate — are already NFC and take
/// `UniCase`'s allocation-free ASCII comparison.
fn folded_path(path: &str) -> UniCase<Cow<'_, str>> {
    let normalized =
        if path.is_ascii() { Cow::Borrowed(path) } else { Cow::Owned(canonical::nfc(path)) };
    UniCase::new(normalized)
}

/// Fetch every symbol and all graph counts for one active-scope file in a single statement. The
/// materialized `requested` CTE bounds both edge aggregates to this file and keeps result ordering
/// deterministic for editor overlays.
fn list_file_symbols_with_graph_counts(
    conn: &Connection,
    path: &str,
) -> anyhow::Result<Vec<FileSymbolRow>> {
    let mut stmt = conn.prepare(
        "WITH requested AS MATERIALIZED (
             SELECT s.id, s.name, ns.value AS qname, s.kind, s.start_line, s.end_line,
                    s.is_test, s.signature
             FROM symbols s
             JOIN files ON files.id = s.file_id
             LEFT JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE files.path = ?1
         ),
         incoming AS (
             SELECT e.to_symbol_id AS symbol_id,
                    COUNT(*) AS fan_in,
                    SUM(CASE WHEN e.confidence = 'Exact' THEN 1 ELSE 0 END) AS exact,
                    SUM(CASE WHEN e.confidence = 'Syntactic' THEN 1 ELSE 0 END) AS syntactic,
                    SUM(CASE WHEN e.confidence = 'NameOnly' THEN 1 ELSE 0 END) AS name_only,
                    SUM(CASE WHEN e.confidence = 'Ambiguous' THEN 1 ELSE 0 END) AS ambiguous,
                    SUM(CASE WHEN source_symbol.is_test = 1 THEN 1 ELSE 0 END) AS tests,
                    SUM(CASE WHEN e.edge_kind = 'dispatches' THEN 1 ELSE 0 END) AS dispatch
             FROM edges e
             JOIN requested ON requested.id = e.to_symbol_id
             JOIN files source_file ON source_file.id = e.source_file_id
             LEFT JOIN symbols source_symbol ON source_symbol.id = e.from_symbol_id
             GROUP BY e.to_symbol_id
         ),
         outgoing AS (
             SELECT e.from_symbol_id AS symbol_id, COUNT(*) AS fan_out
             FROM edges e
             JOIN requested ON requested.id = e.from_symbol_id
             JOIN symbols target_symbol ON target_symbol.id = e.to_symbol_id
             JOIN files target_file ON target_file.id = target_symbol.file_id
             GROUP BY e.from_symbol_id
         )
         SELECT requested.id, requested.name, requested.qname, requested.kind,
                requested.start_line, requested.end_line, requested.is_test, requested.signature,
                COALESCE(incoming.fan_in, 0), COALESCE(outgoing.fan_out, 0),
                COALESCE(incoming.exact, 0), COALESCE(incoming.syntactic, 0),
                COALESCE(incoming.name_only, 0), COALESCE(incoming.ambiguous, 0),
                COALESCE(incoming.tests, 0), COALESCE(incoming.dispatch, 0)
         FROM requested
         LEFT JOIN incoming ON incoming.symbol_id = requested.id
         LEFT JOIN outgoing ON outgoing.symbol_id = requested.id
         ORDER BY requested.start_line, requested.id",
    )?;
    let rows = stmt.query_map([path], |row| {
        Ok(FileSymbolRow {
            id: row.get(0)?,
            name: row.get(1)?,
            qname: row.get(2)?,
            kind: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            is_test: row.get(6)?,
            signature: row.get(7)?,
            fan_in: u64::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
            fan_out: u64::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
            callers: LensGraphCallerCounts {
                exact: u64::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
                syntactic: u64::try_from(row.get::<_, i64>(11)?).unwrap_or(0),
                name_only: u64::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
                ambiguous: u64::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
                tests: u64::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
                dispatch: u64::try_from(row.get::<_, i64>(15)?).unwrap_or(0),
            },
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Load all synthesized dispatch edges touching symbols in one active-scope file. Both endpoint
/// paths are joined through the scoped `files` view, so details cannot cross checkout scope.
fn list_file_dispatch_details(conn: &Connection, path: &str) -> anyhow::Result<Vec<DispatchRow>> {
    let mut stmt = conn.prepare(
        "WITH requested AS MATERIALIZED (
             SELECT s.id FROM symbols s JOIN files ON files.id = s.file_id WHERE files.path = ?1
         )
         SELECT e.from_symbol_id, e.to_symbol_id, e.evidence, e.source_start_line,
                e.target_start_line, source_symbol.name, source_file.path,
                target_symbol.name, target_file.path
         FROM edges e
         JOIN files source_file ON source_file.id = e.source_file_id
         JOIN symbols source_symbol
           ON source_symbol.id = e.from_symbol_id AND source_symbol.file_id = source_file.id
         JOIN symbols target_symbol ON target_symbol.id = e.to_symbol_id
         JOIN files target_file ON target_file.id = target_symbol.file_id
         WHERE e.edge_kind = 'dispatches'
           AND (e.from_symbol_id IN (SELECT id FROM requested)
                OR e.to_symbol_id IN (SELECT id FROM requested))
         ORDER BY e.source_start_line, e.id",
    )?;
    let rows = stmt.query_map([path], |row| {
        Ok(DispatchRow {
            from_symbol_id: row.get(0)?,
            to_symbol_id: row.get(1)?,
            evidence: row.get(2)?,
            source_start_line: row.get(3)?,
            target_start_line: row.get(4)?,
            from_name: row.get(5)?,
            from_path: row.get(6)?,
            to_name: row.get(7)?,
            to_path: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
