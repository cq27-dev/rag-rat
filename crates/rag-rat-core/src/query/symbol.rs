use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::language::Language;

#[derive(Debug, Serialize)]
pub struct SymbolHit {
    // The raw rowid is the internal handle + FK target, but it's reassigned on every reindex
    // (#149), so it never crosses the wire — `logical_symbol_id` is the stable, opaque handle a
    // consumer caches/passes back. Kept on the struct for in-process use; never serialized.
    #[serde(skip_serializing)]
    pub symbol_id: i64,
    // The stable symbol handle: a content-derived id emitted as an opaque `sym_<hex>` token so a
    // JSON client can't round it (>2^53) or mistake it for a number to compute on (#130/#149).
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::serde_big_id::sym_handle_opt::serialize"
    )]
    pub logical_symbol_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_variant_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_group_reason: Option<String>,
    pub file_id: i64,
    pub path: String,
    pub file_kind: String,
    pub language: String,
    pub name: String,
    pub qualified_name: String,
    pub symbol_path: String,
    pub kind: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub signature: Option<String>,
    pub docs: Option<String>,
    /// LOCAL structural-load signal (scoped weighted fan-in) for this symbol — the THIRD
    /// importance scale, NOT PageRank. Attached by the `symbol_lookup` enrichment pass. `None`
    /// when the symbol has no in-edges in the active scope or wasn't enriched. See
    /// `crate::query::load_bearing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<crate::query::load_bearing::ImportanceEnrichment>,
}

#[derive(Debug, Default, Serialize)]
pub struct SymbolLookup {
    pub candidates: Vec<SymbolHit>,
    pub disambiguation_required: bool,
    /// Files among the candidates whose on-disk content differs from the index (dirty relative to
    /// HEAD/overlay) and that the lazy heal did not bring current — so a candidate's line numbers
    /// may be stale (#147/#148). Empty in the common case (heal succeeded or nothing was dirty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalSymbolHit {
    #[serde(serialize_with = "crate::serde_big_id::sym_handle::serialize")]
    pub logical_symbol_id: i64,
    pub language: String,
    pub path: String,
    pub logical_name: String,
    pub qualified_name: String,
    pub kind: String,
    pub variant_count: u64,
    pub group_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalSymbolMember {
    // Internal rowid only — a member is identified on the wire by its cfg/signature/lines, never
    // by the reindex-churned id (#149).
    #[serde(skip_serializing)]
    pub symbol_id: i64,
    pub cfg_expr: Option<String>,
    pub signature_hash: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Debug, Clone)]
pub struct SymbolSelector {
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub symbol: Option<String>,
    pub language: Option<Language>,
    pub allow_ambiguous: bool,
    pub limit: u32,
}

#[derive(Debug, Serialize)]
pub struct SymbolDisambiguation {
    pub candidates: Vec<SymbolHit>,
    pub disambiguation_required: bool,
}

pub fn lookup(
    conn: &Connection,
    name: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut hits = lookup_name(conn, name, language, limit)?;
    enrich_symbol_hits(conn, &mut hits)?;
    Ok(hits)
}

pub fn lookup_candidates(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<SymbolLookup> {
    let candidates = candidates_for_selector(conn, selector)?;
    Ok(SymbolLookup {
        disambiguation_required: needs_disambiguation(&candidates, selector.allow_ambiguous),
        candidates,
        stale_files: Vec::new(),
    })
}

pub fn select_one(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<Result<Option<SymbolHit>, SymbolDisambiguation>> {
    let mut candidates = candidates_for_selector(conn, selector)?;
    if candidates.is_empty() {
        return Ok(Ok(None));
    }
    if selector.logical_symbol_id.is_some() {
        return Ok(Ok(Some(candidates.remove(0))));
    }
    if needs_disambiguation(&candidates, selector.allow_ambiguous) {
        return Ok(Err(SymbolDisambiguation { candidates, disambiguation_required: true }));
    }
    Ok(Ok(Some(candidates.remove(0))))
}

/// Like [`select_one`], but treats a candidate set that all belongs to a single logical symbol
/// (cfg-split twins, or an overload group) as unambiguous. Binding to any member is binding to
/// the same logical thing, so we collapse to the group's first member instead of forcing a
/// disambiguation prompt. Used by `memory rebind`: the caller wants ONE bind target, and the
/// memory-doctor suggestion for a cfg-split helper must resolve cleanly rather than dead-ending.
pub fn select_one_for_bind(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<Result<Option<SymbolHit>, SymbolDisambiguation>> {
    match select_one(conn, selector)? {
        Ok(hit) => Ok(Ok(hit)),
        Err(disambiguation) => Ok(collapse_logical_group(disambiguation)),
    }
}

/// Collapse a disambiguation whose every candidate shares one `logical_symbol_id` to that
/// group's first member; otherwise return the disambiguation unchanged. A `None` logical id on
/// any candidate means it isn't part of a single group, so it stays ambiguous.
fn collapse_logical_group(
    disambiguation: SymbolDisambiguation,
) -> Result<Option<SymbolHit>, SymbolDisambiguation> {
    let shared = disambiguation.candidates.first().and_then(|c| c.logical_symbol_id);
    if let Some(shared) = shared
        && disambiguation.candidates.iter().all(|c| c.logical_symbol_id == Some(shared))
    {
        return Ok(disambiguation.candidates.into_iter().next());
    }
    Err(disambiguation)
}

pub fn lookup_by_id(conn: &Connection, symbol_id: i64) -> anyhow::Result<Option<SymbolHit>> {
    let mut hit = conn
        .query_row(
            "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, \
             symbols.qualified_name,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.id = ?1
        ",
            [symbol_id],
            symbol_hit_row,
        )
        .optional()?;
    if let Some(hit) = hit.as_mut() {
        enrich_symbol_hit(conn, hit)?;
    }
    Ok(hit)
}

fn candidates_for_selector(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<Vec<SymbolHit>> {
    if let Some(logical_symbol_id) = selector.logical_symbol_id {
        return lookup_logical_members(conn, logical_symbol_id, selector.limit);
    }
    if let Some(symbol_id) = selector.symbol_id {
        return Ok(lookup_by_id(conn, symbol_id)?.into_iter().collect());
    }
    if let Some(symbol_path) = selector.symbol_path.as_deref() {
        let mut hits = lookup_symbol_path(conn, symbol_path, selector.language, selector.limit)?;
        enrich_symbol_hits(conn, &mut hits)?;
        return Ok(hits);
    }
    let Some(symbol) = selector.symbol.as_deref() else {
        anyhow::bail!("one of symbol_id, symbol_path, or symbol is required");
    };
    let mut hits = lookup_name(conn, symbol, selector.language, selector.limit)?;
    enrich_symbol_hits(conn, &mut hits)?;
    Ok(hits)
}

fn lookup_name(
    conn: &Connection,
    name: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut sql = "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, \
                   symbols.qualified_name,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE (symbols.name = ?1 OR symbols.qualified_name LIKE ?2)
    "
    .to_string();
    if language.is_some() {
        sql.push_str(" AND symbols.language = ?3");
    }
    sql.push_str(
        "
        ORDER BY
          CASE WHEN symbols.name = ?1 THEN 0 ELSE 1 END,
          CASE symbols.kind
            WHEN 'struct' THEN 0
            WHEN 'class' THEN 0
            WHEN 'object' THEN 0
            WHEN 'enum' THEN 1
            WHEN 'trait' THEN 1
            WHEN 'interface' THEN 1
            WHEN 'type' THEN 1
            WHEN 'function' THEN 2
            WHEN 'method' THEN 2
            WHEN 'const' THEN 3
            WHEN 'property' THEN 3
            WHEN 'static' THEN 3
            WHEN 'impl' THEN 8
            ELSE 9
          END,
          files.path,
          symbols.start_byte
        LIMIT ?
        ",
    );

    let fuzzy = format!("%{name}%");
    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(language) = language {
        stmt.query_map(params![name, fuzzy, language.as_str(), limit], symbol_hit_row)?
    } else {
        stmt.query_map(params![name, fuzzy, limit], symbol_hit_row)?
    };

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    enrich_symbol_hits(conn, &mut hits)?;
    Ok(hits)
}

pub fn lookup_logical_by_id(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<LogicalSymbolHit>> {
    conn.query_row(
        "
        SELECT id, language, path, logical_name, qualified_name, kind, variant_count, group_reason
        FROM logical_symbols
        WHERE id = ?1
        ",
        [logical_symbol_id],
        logical_symbol_hit_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn logical_for_symbol_id(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<Option<LogicalSymbolHit>> {
    conn.query_row(
        "
        SELECT logical_symbols.id, logical_symbols.language, logical_symbols.path,
               logical_symbols.logical_name, logical_symbols.qualified_name, logical_symbols.kind,
               logical_symbols.variant_count, logical_symbols.group_reason
        FROM logical_symbol_members
        JOIN logical_symbols ON logical_symbols.id = logical_symbol_members.logical_symbol_id
        WHERE logical_symbol_members.symbol_id = ?1
        ",
        [symbol_id],
        logical_symbol_hit_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn logical_members(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Vec<LogicalSymbolMember>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbol_id, cfg_expr, signature_hash, start_line, end_line
        FROM logical_symbol_members
        WHERE logical_symbol_id = ?1
        ORDER BY start_line, symbol_id
        ",
    )?;
    let rows = stmt.query_map([logical_symbol_id], |row| {
        Ok(LogicalSymbolMember {
            symbol_id: row.get(0)?,
            cfg_expr: row.get(1)?,
            signature_hash: row.get(2)?,
            start_line: row.get(3)?,
            end_line: row.get(4)?,
        })
    })?;
    let mut members = Vec::new();
    for row in rows {
        members.push(row?);
    }
    Ok(members)
}

fn lookup_logical_members(
    conn: &Connection,
    logical_symbol_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name,
               symbols.qualified_name, symbols.kind, symbols.start_byte, symbols.end_byte,
               symbols.signature, symbols.docs
        FROM logical_symbol_members
        JOIN symbols ON symbols.id = logical_symbol_members.symbol_id
        JOIN files ON files.id = symbols.file_id
        WHERE logical_symbol_members.logical_symbol_id = ?1
        ORDER BY symbols.start_byte, symbols.id
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![logical_symbol_id, limit], symbol_hit_row)?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

fn lookup_symbol_path(
    conn: &Connection,
    symbol_path: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut sql = "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, \
                   symbols.qualified_name,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.qualified_name = ?1
    "
    .to_string();
    if language.is_some() {
        sql.push_str(" AND symbols.language = ?2");
    }
    sql.push_str(" ORDER BY files.path, symbols.start_byte LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(language) = language {
        stmt.query_map(params![symbol_path, language.as_str(), limit], symbol_hit_row)?
    } else {
        stmt.query_map(params![symbol_path, limit], symbol_hit_row)?
    };

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

fn needs_disambiguation(candidates: &[SymbolHit], allow_ambiguous: bool) -> bool {
    !allow_ambiguous && candidates.len() > 1
}

fn symbol_hit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolHit> {
    let qualified_name = row.get(6)?;
    Ok(SymbolHit {
        symbol_id: row.get(0)?,
        logical_symbol_id: None,
        logical_variant_count: None,
        logical_group_reason: None,
        file_id: row.get(1)?,
        path: row.get(2)?,
        file_kind: row.get(3)?,
        language: row.get(4)?,
        name: row.get(5)?,
        symbol_path: qualified_name,
        qualified_name: row.get(6)?,
        kind: row.get(7)?,
        start_byte: row.get(8)?,
        end_byte: row.get(9)?,
        signature: row.get(10)?,
        docs: row.get(11)?,
        importance: None,
    })
}

fn logical_symbol_hit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogicalSymbolHit> {
    let variant_count = u64::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
    Ok(LogicalSymbolHit {
        logical_symbol_id: row.get(0)?,
        language: row.get(1)?,
        path: row.get(2)?,
        logical_name: row.get(3)?,
        qualified_name: row.get(4)?,
        kind: row.get(5)?,
        variant_count,
        group_reason: row.get(7)?,
    })
}

fn enrich_symbol_hits(conn: &Connection, hits: &mut [SymbolHit]) -> anyhow::Result<()> {
    for hit in hits {
        enrich_symbol_hit(conn, hit)?;
    }
    Ok(())
}

fn enrich_symbol_hit(conn: &Connection, hit: &mut SymbolHit) -> anyhow::Result<()> {
    if let Some(logical) = logical_for_symbol_id(conn, hit.symbol_id)? {
        hit.logical_symbol_id = Some(logical.logical_symbol_id);
        hit.logical_variant_count = Some(logical.variant_count);
        hit.logical_group_reason = Some(logical.group_reason);
    }
    Ok(())
}
