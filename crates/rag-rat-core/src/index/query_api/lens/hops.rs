//! `/api/symbol/{callers,callees}` compositions over native graph traversal.

use std::collections::HashMap;

use rusqlite::{Connection, params_from_iter};
use serde::Serialize;

use crate::index::IndexDatabase;

#[derive(Debug, Serialize)]
pub struct LensCallers {
    pub callers: Vec<LensSymbolHop>,
}

#[derive(Debug, Serialize)]
pub struct LensCallees {
    pub callees: Vec<LensSymbolHop>,
}

/// Compatibility projection of the #216 endpoint hop. Reindex-churning edge, file, and symbol
/// row ids are deliberately omitted; `qname`, `path`, and the callsite line are the stable
/// editor references.
#[derive(Debug, Serialize)]
pub struct LensSymbolHop {
    pub edge_kind: String,
    pub confidence: String,
    pub resolution: String,
    pub source_start_line: i64,
    pub name: String,
    pub qname: Option<String>,
    pub kind: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug)]
struct HopSymbolInfo {
    name: String,
    qname: Option<String>,
    kind: String,
}

impl IndexDatabase {
    pub fn lens_symbol_callers(
        &self,
        qualified_name: &str,
        limit: u32,
    ) -> anyhow::Result<LensCallers> {
        let hops = self.find_callers_with_options(
            qualified_name,
            limit,
            &rag_rat_query::graph::GraphTraversalOptions::default(),
        )?;
        Ok(LensCallers { callers: adapt_hops(self.storage.connection(), hops, true)? })
    }

    pub fn lens_symbol_callees(
        &self,
        qualified_name: &str,
        limit: u32,
    ) -> anyhow::Result<LensCallees> {
        let hops = self.trace_callees_with_options(
            qualified_name,
            limit,
            &rag_rat_query::graph::GraphTraversalOptions::default(),
        )?;
        Ok(LensCallees { callees: adapt_hops(self.storage.connection(), hops, false)? })
    }
}

pub(super) fn adapt_hops(
    conn: &Connection,
    hops: Vec<rag_rat_query::graph::GraphHop>,
    reverse: bool,
) -> anyhow::Result<Vec<LensSymbolHop>> {
    let edge_ids = hops.iter().map(|hop| hop.edge_id).collect::<Vec<_>>();
    let mut info = symbol_info_for_edges(conn, &edge_ids, reverse)?;
    if !reverse {
        let compiler_targets = hops
            .iter()
            .filter(|hop| hop.confidence == "compiler")
            .filter_map(|hop| hop.to_symbol.clone())
            .collect::<Vec<_>>();
        let resolved = symbol_info_for_qualified_names(conn, &compiler_targets)?;
        for hop in &hops {
            if hop.confidence == "compiler"
                && let Some(symbol) = hop.to_symbol.as_ref().and_then(|qname| resolved.get(qname))
            {
                info.insert(hop.edge_id, symbol.clone());
            }
        }
    }
    Ok(hops
        .into_iter()
        .filter_map(|hop| {
            // A hop without a callsite has no stable editor anchor — fabricating line 1 of an
            // empty path would paint a misleading lens.
            let callsite = hop.callsite?;
            let (name, qname, kind) = match info.get(&hop.edge_id) {
                Some(symbol) =>
                    (symbol.name.clone(), symbol.qname.clone(), Some(symbol.kind.clone())),
                // File-level callers deliberately have no `from_symbol_id`; traversal still
                // supplies their indexed path as the source name and their real callsite.
                None if reverse => (hop.from_symbol?, None, None),
                None => return None,
            };
            Some(LensSymbolHop {
                edge_kind: hop.edge_kind,
                confidence: hop.confidence,
                resolution: hop.resolution,
                source_start_line: callsite.line,
                name,
                qname,
                kind,
                path: callsite.path,
            })
        })
        .collect())
}

/// Resolve every hop's opposite endpoint in one scoped query. Keying by edge id preserves symbols
/// without qualified names and prevents unresolved fallback names from masquerading as qnames.
fn symbol_info_for_edges(
    conn: &Connection,
    edge_ids: &[i64],
    reverse: bool,
) -> anyhow::Result<HashMap<i64, HopSymbolInfo>> {
    if edge_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let marks = edge_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let other_symbol_id = if reverse { "e.from_symbol_id" } else { "e.to_symbol_id" };
    let sql = format!(
        "SELECT e.id, s.name, ns.value, s.kind
         FROM edges e
         JOIN files source_file ON source_file.id = e.source_file_id
         JOIN symbols s ON s.id = {other_symbol_id}
         JOIN files symbol_file ON symbol_file.id = s.file_id
         LEFT JOIN name_strings ns ON ns.id = s.qualified_name_id
         WHERE e.id IN ({marks})
         ORDER BY e.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(edge_ids), |row| {
        Ok((row.get::<_, i64>(0)?, HopSymbolInfo {
            name: row.get(1)?,
            qname: row.get(2)?,
            kind: row.get(3)?,
        }))
    })?;
    let mut info = HashMap::new();
    for row in rows {
        let (edge_id, value) = row?;
        info.entry(edge_id).or_insert(value);
    }
    Ok(info)
}

fn symbol_info_for_qualified_names(
    conn: &Connection,
    qualified_names: &[String],
) -> anyhow::Result<HashMap<String, HopSymbolInfo>> {
    if qualified_names.is_empty() {
        return Ok(HashMap::new());
    }
    let marks = qualified_names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT ns.value, s.name, s.kind
         FROM symbols s
         JOIN files ON files.id = s.file_id
         JOIN name_strings ns ON ns.id = s.qualified_name_id
         WHERE ns.value IN ({marks})
         ORDER BY s.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(qualified_names), |row| {
        let qname = row.get::<_, String>(0)?;
        Ok((qname.clone(), HopSymbolInfo {
            name: row.get(1)?,
            qname: Some(qname),
            kind: row.get(2)?,
        }))
    })?;
    let mut info = HashMap::new();
    for row in rows {
        let (qualified_name, value) = row?;
        info.entry(qualified_name).or_insert(value);
    }
    Ok(info)
}
