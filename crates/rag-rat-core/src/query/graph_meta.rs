use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::query::ReadChunk;
use crate::search::lexical::SearchHit;

const FULL_GRAPH_NOTE: &str = "Call graph is tree-sitter/syntactic, not compiler-resolved.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphMetaMode {
    None,
    Compact,
    Full,
}

impl GraphMetaMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "none" | "false" => Ok(Self::None),
            "compact" | "true" => Ok(Self::Compact),
            "full" => Ok(Self::Full),
            other => anyhow::bail!(
                "unknown graph metadata mode `{other}`; expected none, compact, or full"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<GraphSymbol>,
    pub caller_count: u64,
    pub callee_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_callers: Vec<CallerEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_callees: Vec<CalleeEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<CallerEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<CalleeEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub referenced_types: Vec<TypeEvidence>,
    pub truncated: GraphTruncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphSymbol {
    // Internal rowid — never serialized (reindex-churned, #149); the wire identity is `ref` /
    // qualified_name. (Was leaking onto the wire as `id` before the #149 sweep covered this
    // struct.)
    #[serde(skip_serializing)]
    pub id: i64,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    #[serde(rename = "ref")]
    pub symbol_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallerEvidence {
    #[serde(rename = "ref")]
    pub symbol_path: String,
    pub path: String,
    pub line: i64,
    pub callsite: CallsiteEvidence,
    pub edge_kind: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalleeEvidence {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_symbol_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    pub callsite: CallsiteEvidence,
    pub edge_kind: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallsiteEvidence {
    pub path: String,
    pub line: i64,
    pub span: [i64; 2],
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportEvidence {
    pub target: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeEvidence {
    pub name: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphTruncation {
    pub callers: bool,
    pub callees: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub imports: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub referenced_types: bool,
}

pub fn attach_to_search_hits(
    conn: &Connection,
    hits: &mut [SearchHit],
    mode: GraphMetaMode,
    limit: u32,
) -> anyhow::Result<()> {
    if mode == GraphMetaMode::None {
        return Ok(());
    }
    let limit = limit.max(1);
    for hit in hits {
        hit.graph = evidence_for_chunk(conn, hit.chunk_id, mode, limit)?;
    }
    Ok(())
}

pub fn attach_to_read_chunk(
    conn: &Connection,
    chunk: &mut ReadChunk,
    mode: GraphMetaMode,
    limit: u32,
) -> anyhow::Result<()> {
    if mode == GraphMetaMode::None {
        return Ok(());
    }
    chunk.graph = evidence_for_chunk(conn, chunk.chunk_id, mode, limit.max(1))?;
    Ok(())
}

fn evidence_for_chunk(
    conn: &Connection,
    chunk_id: i64,
    mode: GraphMetaMode,
    limit: u32,
) -> anyhow::Result<Option<GraphEvidence>> {
    let Some(symbol) = primary_symbol(conn, chunk_id)? else {
        return Ok(None);
    };
    let caller_count = count_callers(conn, &symbol)?;
    let callee_count = count_callees(conn, symbol.id)?;
    let mut evidence = GraphEvidence {
        symbol: (mode == GraphMetaMode::Full).then(|| symbol.public.clone()),
        caller_count,
        callee_count,
        top_callers: Vec::new(),
        top_callees: Vec::new(),
        callers: Vec::new(),
        callees: Vec::new(),
        imports: Vec::new(),
        referenced_types: Vec::new(),
        truncated: GraphTruncation::default(),
        notes: Vec::new(),
    };
    let callers = callers(conn, &symbol, limit)?;
    let callees = callees(conn, symbol.id, limit)?;
    evidence.truncated.callers = caller_count > u64::try_from(callers.len()).unwrap_or(u64::MAX);
    evidence.truncated.callees = callee_count > u64::try_from(callees.len()).unwrap_or(u64::MAX);
    if mode == GraphMetaMode::Full {
        evidence.callers = callers;
        evidence.callees = callees;
        evidence.imports = imports(conn, chunk_id, limit)?;
        evidence.referenced_types = referenced_types(conn, symbol.id, limit)?;
        evidence.truncated.imports =
            count_imports(conn, chunk_id)? > u64::try_from(evidence.imports.len()).unwrap_or(0);
        evidence.truncated.referenced_types = count_referenced_types(conn, symbol.id)?
            > u64::try_from(evidence.referenced_types.len()).unwrap_or(0);
        evidence.notes.push(FULL_GRAPH_NOTE.to_string());
    } else {
        evidence.top_callers = callers;
        evidence.top_callees = callees;
    }
    Ok(Some(evidence))
}

#[derive(Debug, Clone)]
struct PrimarySymbol {
    id: i64,
    name: String,
    public: GraphSymbol,
}

fn primary_symbol(conn: &Connection, chunk_id: i64) -> anyhow::Result<Option<PrimarySymbol>> {
    Ok(conn
        .query_row(
            "
            SELECT symbols.id, symbols.name, symbols.qualified_name, symbols.kind, files.path
            FROM chunks
            JOIN symbols ON symbols.file_id = chunks.file_id
             AND symbols.start_byte < chunks.end_byte
             AND symbols.end_byte > chunks.start_byte
            JOIN files ON files.id = symbols.file_id
            WHERE chunks.id = ?1
            ORDER BY
              CASE symbols.kind
                WHEN 'function' THEN 0
                WHEN 'method' THEN 1
                WHEN 'class' THEN 2
                WHEN 'struct' THEN 3
                ELSE 9
              END,
              symbols.start_byte ASC
            LIMIT 1
            ",
            [chunk_id],
            |row| {
                let id = row.get(0)?;
                let name: String = row.get(1)?;
                let qualified_name: String = row.get(2)?;
                let kind = row.get(3)?;
                let path: String = row.get(4)?;
                Ok(PrimarySymbol {
                    id,
                    name: name.clone(),
                    public: GraphSymbol {
                        id,
                        name,
                        qualified_name: qualified_name.clone(),
                        kind,
                        symbol_path: symbol_path(&path, &qualified_name),
                    },
                })
            },
        )
        .optional()?)
}

fn count_callers(conn: &Connection, symbol: &PrimarySymbol) -> anyhow::Result<u64> {
    let count = conn
        .prepare_cached(
            "
        SELECT COUNT(DISTINCT COALESCE(from_symbol_id, -id))
        FROM edges
        WHERE edge_kind IN ('calls_name', 'constructs', 'uses_macro')
          AND (to_symbol_id = ?1 OR (to_symbol_id IS NULL AND to_name_id = (SELECT id FROM \
             edge_strings WHERE value = ?2)))
        ",
        )?
        .query_row(params![symbol.id, symbol.name], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn count_callees(conn: &Connection, symbol_id: i64) -> anyhow::Result<u64> {
    // Mirror the filter in `callees()` so `callee_count` (and thus the `truncated` flag) reflects
    // the callees actually surfaced — not the unresolved name-only std calls we hide.
    let count = conn
        .prepare_cached(
            "
        SELECT COUNT(DISTINCT COALESCE(CAST(to_symbol_id AS TEXT), to_name))
        FROM edges
        WHERE from_symbol_id = ?1
          AND edge_kind IN ('calls_name', 'constructs', 'uses_macro')
          AND (
              edge_kind != 'calls_name'
              OR to_symbol_id IS NOT NULL
              OR (confidence = 'Syntactic' AND target_qualified_name IS NOT NULL)
          )
        ",
        )?
        .query_row([symbol_id], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn count_imports(conn: &Connection, chunk_id: i64) -> anyhow::Result<u64> {
    count_edges_for_chunk(conn, chunk_id, &["imports"])
}

fn count_referenced_types(conn: &Connection, symbol_id: i64) -> anyhow::Result<u64> {
    count_edges_for_symbol(conn, symbol_id, &["references_type", "implements", "extends"])
}

fn count_edges_for_symbol(
    conn: &Connection,
    symbol_id: i64,
    edge_kinds: &[&str],
) -> anyhow::Result<u64> {
    let count = conn.query_row(
        &format!(
            "
        SELECT COUNT(DISTINCT COALESCE(CAST(to_symbol_id AS TEXT), to_name))
        FROM edges
            WHERE from_symbol_id = ?1
              AND edge_kind IN ({})
            ",
            quoted(edge_kinds),
        ),
        [symbol_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn count_edges_for_chunk(
    conn: &Connection,
    chunk_id: i64,
    edge_kinds: &[&str],
) -> anyhow::Result<u64> {
    let count = conn.query_row(
        &format!(
            "
            SELECT COUNT(*)
            FROM edges
            JOIN chunks ON chunks.file_id = edges.source_file_id
            WHERE chunks.id = ?1
              AND edges.from_symbol_id IS NULL
              AND edges.edge_kind IN ({})
            ",
            quoted(edge_kinds),
        ),
        [chunk_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn callers(
    conn: &Connection,
    symbol: &PrimarySymbol,
    limit: u32,
) -> anyhow::Result<Vec<CallerEvidence>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT DISTINCT
               source_files.path,
               COALESCE(source_symbols.qualified_name, edges.from_name, source_files.path),
               COALESCE(NULLIF(edges.source_start_line, 0), source_chunks.start_line, 1),
               COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), \
         source_chunks.start_line, 1),
               edges.edge_kind,
               edges.confidence
        FROM edges
        JOIN files source_files ON source_files.id = edges.source_file_id
        LEFT JOIN symbols source_symbols ON source_symbols.id = edges.from_symbol_id
        LEFT JOIN chunks source_chunks ON source_chunks.file_id = edges.source_file_id
          AND source_symbols.start_byte >= source_chunks.start_byte
          AND source_symbols.start_byte < source_chunks.end_byte
        WHERE edges.edge_kind IN ('calls_name', 'constructs', 'uses_macro')
          AND (edges.to_symbol_id = ?1 OR (edges.to_symbol_id IS NULL AND edges.to_name_id = \
         (SELECT id FROM edge_strings WHERE value = ?2)))
        ORDER BY
          CASE edges.confidence
            WHEN 'Exact' THEN 0
            WHEN 'Syntactic' THEN 1
            WHEN 'NameOnly' THEN 2
            ELSE 3
          END,
          source_files.path,
          source_chunks.start_line
        LIMIT ?3
        ",
    )?;
    let rows = stmt.query_map(params![symbol.id, symbol.name, expanded_limit(limit)], |row| {
        let path: String = row.get(0)?;
        let qualified_name: String = row.get(1)?;
        let source_start_line = row.get(2)?;
        let source_end_line = row.get(3)?;
        Ok(CallerEvidence {
            symbol_path: symbol_path(&path, &qualified_name),
            path: path.clone(),
            line: source_start_line,
            callsite: CallsiteEvidence {
                path,
                line: source_start_line,
                span: [source_start_line, source_end_line],
            },
            edge_kind: row.get(4)?,
            confidence: confidence(row.get::<_, String>(5)?.as_str()).to_string(),
        })
    })?;
    let mut seen = BTreeSet::new();
    let mut callers = collect_rows(rows)?
        .into_iter()
        .filter(|caller| seen.insert((caller.symbol_path.clone(), caller.edge_kind.clone())))
        .collect::<Vec<_>>();
    callers.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(callers)
}

fn callees(conn: &Connection, symbol_id: i64, limit: u32) -> anyhow::Result<Vec<CalleeEvidence>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT DISTINCT
               edges.to_name,
               target_files.path,
               target_symbols.qualified_name,
               COALESCE(edges.target_start_line, target_chunks.start_line),
               source_files.path,
               COALESCE(NULLIF(edges.source_start_line, 0), source_chunks.start_line, 1),
               COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), \
         source_chunks.start_line, 1),
               edges.edge_kind,
               edges.confidence
        FROM edges
        JOIN files source_files ON source_files.id = edges.source_file_id
        LEFT JOIN symbols target_symbols ON target_symbols.id = edges.to_symbol_id
        LEFT JOIN files target_files ON target_files.id = target_symbols.file_id
        LEFT JOIN chunks target_chunks ON target_chunks.file_id = target_symbols.file_id
          AND target_symbols.start_byte >= target_chunks.start_byte
          AND target_symbols.start_byte < target_chunks.end_byte
        LEFT JOIN symbols source_symbols ON source_symbols.id = edges.from_symbol_id
        LEFT JOIN chunks source_chunks ON source_chunks.file_id = edges.source_file_id
          AND source_symbols.start_byte >= source_chunks.start_byte
          AND source_symbols.start_byte < source_chunks.end_byte
        WHERE edges.from_symbol_id = ?1
          AND edges.edge_kind IN ('calls_name', 'constructs', 'uses_macro')
          -- Drop unresolved name-only calls (`.map()`, `.var_os()`, std combinators): they
          -- resolve to nothing in-repo and are pure noise in a chunk's callee summary. Keep
          -- resolved calls and syntactically-resolvable ones, plus constructs/macros.
          AND (
              edges.edge_kind != 'calls_name'
              OR edges.to_symbol_id IS NOT NULL
              OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT NULL)
          )
        ORDER BY
          CASE edges.confidence
            WHEN 'Exact' THEN 0
            WHEN 'Syntactic' THEN 1
            WHEN 'NameOnly' THEN 2
            ELSE 3
          END,
          source_chunks.start_line,
          edges.to_name
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![symbol_id, expanded_limit(limit)], |row| {
        let target: String = row.get(0)?;
        let path: Option<String> = row.get(1)?;
        let qualified_name: Option<String> = row.get(2)?;
        let callsite_path: String = row.get(4)?;
        let callsite_start_line = row.get(5)?;
        let callsite_end_line = row.get(6)?;
        Ok(CalleeEvidence {
            target,
            resolved_symbol_path: path
                .as_ref()
                .zip(qualified_name.as_ref())
                .map(|(path, qualified_name)| symbol_path(path, qualified_name)),
            path,
            line: row.get(3)?,
            callsite: CallsiteEvidence {
                path: callsite_path,
                line: callsite_start_line,
                span: [callsite_start_line, callsite_end_line],
            },
            edge_kind: row.get(7)?,
            confidence: confidence(row.get::<_, String>(8)?.as_str()).to_string(),
        })
    })?;
    let mut seen = BTreeSet::new();
    let mut callees = collect_rows(rows)?
        .into_iter()
        .filter(|callee| {
            seen.insert((
                callee.target.clone(),
                callee.resolved_symbol_path.clone(),
                callee.edge_kind.clone(),
            ))
        })
        .collect::<Vec<_>>();
    callees.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(callees)
}

fn imports(conn: &Connection, chunk_id: i64, limit: u32) -> anyhow::Result<Vec<ImportEvidence>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT edges.to_name, edges.confidence
        FROM edges
        JOIN chunks ON chunks.file_id = edges.source_file_id
        WHERE chunks.id = ?1
          AND edges.from_symbol_id IS NULL
          AND edges.edge_kind = 'imports'
        ORDER BY edges.to_name
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![chunk_id, i64::from(limit)], |row| {
        Ok(ImportEvidence {
            target: row.get(0)?,
            confidence: confidence(row.get::<_, String>(1)?.as_str()).to_string(),
        })
    })?;
    collect_rows(rows)
}

fn referenced_types(
    conn: &Connection,
    symbol_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<TypeEvidence>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT DISTINCT edges.to_name, edges.confidence
        FROM edges
        WHERE edges.from_symbol_id = ?1
          AND edges.edge_kind IN ('references_type', 'implements', 'extends')
        ORDER BY
          CASE edges.confidence
            WHEN 'Exact' THEN 0
            WHEN 'Syntactic' THEN 1
            WHEN 'NameOnly' THEN 2
            ELSE 3
          END,
          edges.to_name
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![symbol_id, i64::from(limit)], |row| {
        Ok(TypeEvidence {
            name: row.get(0)?,
            confidence: confidence(row.get::<_, String>(1)?.as_str()).to_string(),
        })
    })?;
    collect_rows(rows)
}

fn symbol_path(path: &str, qualified_name: &str) -> String {
    if qualified_name == path || qualified_name.starts_with(&format!("{path}::")) {
        return qualified_name.to_string();
    }
    format!("{path}::{qualified_name}")
}

fn confidence(value: &str) -> &'static str {
    crate::query::graph::normalize_confidence(value)
}

fn quoted(values: &[&str]) -> String {
    values.iter().map(|value| format!("'{value}'")).collect::<Vec<_>>().join(", ")
}

fn expanded_limit(limit: u32) -> i64 {
    i64::from(limit.max(1)).saturating_mul(4)
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
