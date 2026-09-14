use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::graph::{CONFIDENCE_ORDER_SQL, RESOLVED_OPERATOR_ONLY};
use crate::{ReadChunk, SearchHit};

const FULL_GRAPH_NOTE: &str = "Call graph is tree-sitter/syntactic, not compiler-resolved.";

/// The edge kinds a chunk's graph summary counts and lists as calls. It is NOT the traversal's
/// `graph::CALL_EDGE_KINDS` (`calls_name`, `constructs`, `dispatches`, `uses_operator`): this set
/// adds `uses_macro` and leaves out the synthesized `dispatches` hop, so a chunk's `caller_count`
/// and `find_callers` count different populations. The counts and the lists below all splice it,
/// which is what keeps each count honest about its list.
const GRAPH_META_CALL_EDGE_KINDS: &str =
    "('calls_name', 'constructs', 'uses_operator', 'uses_macro')";

/// A caller edge of the symbol bound as `?1`, or bound by its short name `?2`: resolved to the
/// symbol, or unresolved with that name. `count_callers` and `callers` both splice it, so
/// `caller_count` (and thus the `truncated` flag) counts the population the list draws from.
const CALLER_OF_SYMBOL_OR_NAME: &str = "(edges.to_symbol_id = ?1 OR (edges.to_symbol_id IS NULL \
                                        AND edges.to_name_id = (SELECT id FROM name_strings WHERE \
                                        value = ?2)))";

/// The callees a chunk's summary surfaces: unresolved name-only calls resolve to nothing in-repo
/// and are pure noise there, so they are dropped, while a call may retain a qualified syntactic
/// target. (Operator declarations must resolve to an indexed symbol — the separate
/// `RESOLVED_OPERATOR_ONLY` guard.) `count_callees` and `callees` both splice it, so
/// `callee_count` (and thus the `truncated` flag) reflects the callees actually surfaced. Valid
/// wherever the edges table is named or aliased `edges`.
const SURFACED_CALLEE_ONLY: &str = "(edges.edge_kind != 'calls_name' OR edges.to_symbol_id IS NOT \
                                    NULL OR (edges.confidence = 'Syntactic' AND \
                                    edges.target_qualified_name IS NOT NULL))";

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
            SELECT symbols.id, symbols.name, qn.value, symbols.kind, files.path
            FROM chunks
            JOIN symbols ON symbols.file_id = chunks.file_id
             AND symbols.start_byte < chunks.end_byte
             AND symbols.end_byte > chunks.start_byte
            JOIN files ON files.id = symbols.file_id
            LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
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
    // GENERATION-SCOPED via the `files` view (batch 6, count-scoping class): the `to_symbol_id =
    // ?1` arm keys on a LIVE rowid (dead-generation edges carry re-minted ids, so they never
    // match — as the clean `count_callees`/`count_edges_for_symbol` siblings rely on), but the
    // unresolved `(to_symbol_id IS NULL AND to_name_id = …)` arm matches callsites purely by
    // NAME and so counts dead-generation edges during a dead-generation window, inflating
    // `caller_count`/`truncated` on GraphEvidence. Joining the scoped view bounds the count to
    // the live generation's callsites (columns qualified because `files.id` would otherwise
    // collide with the `-id` edge term).
    let count = conn
        .prepare_cached(&format!(
            "
        SELECT COUNT(DISTINCT COALESCE(edges.from_symbol_id, -edges.id))
        FROM edges
        JOIN files source_files ON source_files.id = edges.source_file_id
        WHERE edges.edge_kind IN {GRAPH_META_CALL_EDGE_KINDS}
          AND {RESOLVED_OPERATOR_ONLY}
          AND {CALLER_OF_SYMBOL_OR_NAME}
        ",
        ))?
        .query_row(params![symbol.id, symbol.name], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn count_callees(conn: &Connection, symbol_id: i64) -> anyhow::Result<u64> {
    // The kind set and filters `callees()` splices, so `callee_count` (and thus the `truncated`
    // flag) reflects the callees actually surfaced — not the unresolved name-only std calls we
    // hide.
    let count = conn
        .prepare_cached(&format!(
            "
        SELECT COUNT(DISTINCT COALESCE(CAST(to_symbol_id AS TEXT), to_name))
        FROM edges
        WHERE from_symbol_id = ?1
          AND edge_kind IN {GRAPH_META_CALL_EDGE_KINDS}
          AND {SURFACED_CALLEE_ONLY}
          AND {RESOLVED_OPERATOR_ONLY}
        ",
        ))?
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
    let mut stmt = conn.prepare_cached(&format!(
        "
        SELECT DISTINCT
               source_files.path,
               COALESCE(source_qn.value, edges.from_name, source_files.path),
               COALESCE(NULLIF(edges.source_start_line, 0), source_chunks.start_line, 1),
               COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), \
         source_chunks.start_line, 1),
               edges.edge_kind,
               edges.confidence
        FROM edges
        JOIN files source_files ON source_files.id = edges.source_file_id
        LEFT JOIN symbols source_symbols ON source_symbols.id = edges.from_symbol_id
        LEFT JOIN name_strings source_qn ON source_qn.id = source_symbols.qualified_name_id
        LEFT JOIN chunks source_chunks ON source_chunks.file_id = edges.source_file_id
          AND source_symbols.start_byte >= source_chunks.start_byte
          AND source_symbols.start_byte < source_chunks.end_byte
        WHERE edges.edge_kind IN {GRAPH_META_CALL_EDGE_KINDS}
          AND {RESOLVED_OPERATOR_ONLY}
          AND {CALLER_OF_SYMBOL_OR_NAME}
        ORDER BY
          {CONFIDENCE_ORDER_SQL},
          source_files.path,
          source_chunks.start_line
        LIMIT ?3
        ",
    ))?;
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
    let mut stmt = conn.prepare_cached(&format!(
        "
        SELECT DISTINCT
               edges.to_name,
               target_files.path,
               target_qn.value,
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
        LEFT JOIN name_strings target_qn ON target_qn.id = target_symbols.qualified_name_id
        LEFT JOIN files target_files ON target_files.id = target_symbols.file_id
        LEFT JOIN chunks target_chunks ON target_chunks.file_id = target_symbols.file_id
          AND target_symbols.start_byte >= target_chunks.start_byte
          AND target_symbols.start_byte < target_chunks.end_byte
        LEFT JOIN symbols source_symbols ON source_symbols.id = edges.from_symbol_id
        LEFT JOIN chunks source_chunks ON source_chunks.file_id = edges.source_file_id
          AND source_symbols.start_byte >= source_chunks.start_byte
          AND source_symbols.start_byte < source_chunks.end_byte
        WHERE edges.from_symbol_id = ?1
          AND edges.edge_kind IN {GRAPH_META_CALL_EDGE_KINDS}
          AND {SURFACED_CALLEE_ONLY}
          AND {RESOLVED_OPERATOR_ONLY}
        ORDER BY
          {CONFIDENCE_ORDER_SQL},
          source_chunks.start_line,
          edges.to_name
        LIMIT ?2
        ",
    ))?;
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
    let mut stmt = conn.prepare_cached(&format!(
        "
        SELECT DISTINCT edges.to_name, edges.confidence
        FROM edges
        WHERE edges.from_symbol_id = ?1
          AND edges.edge_kind IN ('references_type', 'implements', 'extends')
        ORDER BY
          {CONFIDENCE_ORDER_SQL},
          edges.to_name
        LIMIT ?2
        ",
    ))?;
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
    crate::graph::normalize_confidence(value)
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

#[cfg(test)]
mod tests {
    use rag_rat_base::checkout::CheckoutRef;
    use rag_rat_core::index::install_scope_view;
    use rag_rat_db::schema;

    use super::*;

    const SCOPE: CheckoutRef<'static> = CheckoutRef { commit_sha: "c0ffee", worktree_id: "" };

    fn add_symbol(conn: &Connection, file_id: i64, name: &str) -> i64 {
        let qualified = format!("a.rs::{name}");
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", [&qualified])
            .unwrap();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                 end_byte, signature, docs)
             VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3),
                     'function', 0, 10, NULL, NULL)",
            params![file_id, name, qualified],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// One `edge_kind` edge at a distinct call-site span; `to` NULL is the unresolved case, which
    /// only its `to_name` can match.
    fn add_edge(
        conn: &Connection,
        file_id: i64,
        from: i64,
        to: Option<i64>,
        to_name: &str,
        edge_kind: &str,
        span: i64,
    ) {
        conn.execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence,
                               source_start_byte, source_end_byte,
                               callee_start_byte, callee_end_byte)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?7, ?8)",
            params![
                file_id,
                from,
                to,
                to_name,
                edge_kind,
                if to.is_some() { "Exact" } else { "NameOnly" },
                span,
                span + 5,
            ],
        )
        .unwrap();
    }

    fn primary(id: i64, name: &str) -> PrimarySymbol {
        PrimarySymbol {
            id,
            name: name.to_string(),
            public: GraphSymbol {
                id,
                name: name.to_string(),
                qualified_name: format!("a.rs::{name}"),
                kind: "function".to_string(),
                symbol_path: format!("a.rs::{name}"),
            },
        }
    }

    /// A chunk's `caller_count` / `callee_count` decide its `truncated` flags, so each must count
    /// exactly the population its list draws from. Every call kind contributes an admitted edge,
    /// and the filtered ones cover each exclusion — an unresolved name-only call, an unresolved
    /// (built-in) operator, and a `dispatches` hop outside the summary's kind set.
    #[test]
    fn caller_and_callee_counts_agree_with_their_lists() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &rag_rat_core::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id)
             VALUES ('a.rs', 'rust', 'source', 'sha', 0, 0, 'c0ffee', '')",
            [],
        )
        .unwrap();
        let file = conn.last_insert_rowid();
        let [focus, called, built, operator, dispatched] =
            ["focus", "called", "built", "operator", "dispatched"]
                .map(|name| add_symbol(&conn, file, name));
        add_edge(&conn, file, focus, Some(called), "called", "calls_name", 10);
        add_edge(&conn, file, focus, None, "unbound", "calls_name", 20);
        add_edge(&conn, file, focus, Some(built), "built", "constructs", 30);
        add_edge(&conn, file, focus, Some(operator), "operator", "uses_operator", 40);
        add_edge(&conn, file, focus, None, "+", "uses_operator", 50);
        add_edge(&conn, file, focus, None, "println", "uses_macro", 60);
        add_edge(&conn, file, focus, Some(dispatched), "dispatched", "dispatches", 70);

        let [target, by_call, by_name, by_macro, by_operator, by_dispatch] =
            ["target", "by_call", "by_name", "by_macro", "by_operator", "by_dispatch"]
                .map(|name| add_symbol(&conn, file, name));
        add_edge(&conn, file, by_call, Some(target), "target", "calls_name", 110);
        add_edge(&conn, file, by_name, None, "target", "constructs", 120);
        add_edge(&conn, file, by_macro, None, "target", "uses_macro", 130);
        add_edge(&conn, file, by_operator, None, "target", "uses_operator", 140);
        add_edge(&conn, file, by_dispatch, Some(target), "target", "dispatches", 150);
        install_scope_view(&conn, SCOPE).unwrap();

        let callee_list = callees(&conn, focus, 100).unwrap();
        let mut surfaced =
            callee_list.iter().map(|callee| callee.target.as_str()).collect::<Vec<_>>();
        surfaced.sort_unstable();
        assert_eq!(surfaced, ["built", "called", "operator", "println"]);
        assert_eq!(count_callees(&conn, focus).unwrap(), 4);

        let target = primary(target, "target");
        let caller_list = callers(&conn, &target, 100).unwrap();
        let mut sources =
            caller_list.iter().map(|caller| caller.symbol_path.as_str()).collect::<Vec<_>>();
        sources.sort_unstable();
        assert_eq!(sources, ["a.rs::by_call", "a.rs::by_macro", "a.rs::by_name"]);
        assert_eq!(count_callers(&conn, &target).unwrap(), 3);
    }
}
