//! `/api/symbol/{callers,callees}` compositions over native graph traversal.

use std::collections::HashMap;

use rag_rat_query::graph::{self, GraphResolutionMode, GraphTraversalOptions};
use rusqlite::{Connection, OptionalExtension as _, params_from_iter};
use serde::Serialize;

use crate::index::IndexDatabase;

/// How a hop request names the symbol it wants the neighbours of.
///
/// The opaque `sym_<hex>` logical-symbol handle is the stable identity every symbol-shaped surface
/// hands out, and it is the only selector that distinguishes two overloads sharing one qualified
/// name — the grouping key folds the signature in, so `Alpha::run` and `Beta::run` are different
/// logical symbols even though both qualify as `src/lib.rs::run`.
#[derive(Clone, Debug)]
pub enum LensHopSelector {
    /// Preferred: the `sym_<hex>` handle, already decoded to its logical-symbol id.
    Handle(i64),
    /// Compatibility fallback for a client that sends no handle. A name expands to EVERY symbol
    /// the traversal seeds from it — every symbol carrying it as a qualified name, or, while the
    /// short name is unambiguous, the one carrying it as a short name — so a file with overloads
    /// reports the union of their neighbours.
    QualifiedName(String),
}

/// Which selector answered a hop request, echoed so a caller can tell an id-resolved answer from
/// the qualified-name fallback without inspecting its own request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LensHopResolvedBy {
    /// Resolved by the `sym_<hex>` handle: the hops belong to exactly one logical symbol.
    Id,
    /// Resolved by qualified name: `matched_symbols > 1` means the answer is a union.
    Ref,
}

#[derive(Debug, Serialize)]
pub struct LensCallers {
    pub callers: Vec<LensSymbolHop>,
    pub resolved_by: LensHopResolvedBy,
    /// Active-scope symbols the selector expanded to. Read it against `resolved_by`, because the
    /// two lanes count different things: on the handle lane it is the logical symbol's member
    /// rows (its cfg variants — one symbol for almost every symbol, and always ONE logical
    /// symbol, so it never signals ambiguity there), and on the fallback lane it is every symbol
    /// the traversal seeds from the name — by qualified name, or by short name while that short
    /// name is unambiguous — so `> 1` there says the hops are a union. Zero therefore means the
    /// name expanded to no symbol at all, never that the answer is unknown.
    pub matched_symbols: u64,
}

#[derive(Debug, Serialize)]
pub struct LensCallees {
    pub callees: Vec<LensSymbolHop>,
    pub resolved_by: LensHopResolvedBy,
    /// Same meaning as [`LensCallers::matched_symbols`].
    pub matched_symbols: u64,
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

/// The traversal a [`LensHopSelector`] resolved to, plus what the answer should report about it.
#[derive(Debug)]
struct HopTraversal {
    /// Name seed for the traversal. Inert on the handle lane, where the seeded predicate is the
    /// logical-symbol membership set alone; kept truthful so a query log still names the symbol.
    seed: String,
    options: GraphTraversalOptions,
    resolved_by: LensHopResolvedBy,
    matched_symbols: u64,
}

impl IndexDatabase {
    /// `None` when a `sym_<hex>` handle names no symbol in the active checkout — a handle held
    /// across a rename or from another checkout. Reported as such rather than silently answering
    /// with an empty neighbour set or falling through to the ambiguous qualified-name lane.
    pub fn lens_symbol_callers(
        &self,
        selector: &LensHopSelector,
        limit: u32,
    ) -> anyhow::Result<Option<LensCallers>> {
        let Some(traversal) = self.resolve_lens_hop_selector(selector)? else {
            return Ok(None);
        };
        let hops = self.find_callers_with_options(&traversal.seed, limit, &traversal.options)?;
        Ok(Some(LensCallers {
            callers: adapt_hops(self.storage.connection(), hops, true)?,
            resolved_by: traversal.resolved_by,
            matched_symbols: traversal.matched_symbols,
        }))
    }

    /// `None` carries the same meaning as in [`Self::lens_symbol_callers`].
    pub fn lens_symbol_callees(
        &self,
        selector: &LensHopSelector,
        limit: u32,
    ) -> anyhow::Result<Option<LensCallees>> {
        let Some(traversal) = self.resolve_lens_hop_selector(selector)? else {
            return Ok(None);
        };
        let hops = self.trace_callees_with_options(&traversal.seed, limit, &traversal.options)?;
        Ok(Some(LensCallees {
            callees: adapt_hops(self.storage.connection(), hops, false)?,
            resolved_by: traversal.resolved_by,
            matched_symbols: traversal.matched_symbols,
        }))
    }

    /// Turn the request's selector into the traversal that answers it.
    ///
    /// The handle lane traverses in `Exact` mode seeded on the logical symbol, which reduces both
    /// predicates to "an edge endpoint is a member of THIS logical symbol" — the qualified-name OR
    /// arms that the other modes keep are exactly what re-collapses overloads, and dropping them
    /// also makes the hop list the drill-down of the caller/fan-out counts `/api/file/symbols` and
    /// `/api/file/graph` already show, which are counted per symbol id over resolved edges.
    ///
    /// The qualified-name lane keeps the historical `Syntactic` default so an older client that
    /// sends no handle sees byte-identical hops; `matched_symbols` is how it learns that the name
    /// it sent covers more than one symbol.
    fn resolve_lens_hop_selector(
        &self,
        selector: &LensHopSelector,
    ) -> anyhow::Result<Option<HopTraversal>> {
        match selector {
            LensHopSelector::Handle(logical_symbol_id) => {
                let Some((seed, matched_symbols)) =
                    logical_symbol_in_scope(self.storage.connection(), *logical_symbol_id)?
                else {
                    return Ok(None);
                };
                Ok(Some(HopTraversal {
                    seed,
                    options: GraphTraversalOptions {
                        resolution_mode: GraphResolutionMode::Exact,
                        logical_symbol_id: Some(*logical_symbol_id),
                        ..GraphTraversalOptions::default()
                    },
                    resolved_by: LensHopResolvedBy::Id,
                    matched_symbols,
                }))
            },
            LensHopSelector::QualifiedName(reference) => Ok(Some(HopTraversal {
                seed: reference.clone(),
                options: GraphTraversalOptions::default(),
                resolved_by: LensHopResolvedBy::Ref,
                // Counted by the traversal's own seed rule rather than by qualified name alone:
                // the fallback also answers a bare short name, and reporting zero next to that
                // answer's hops would read as "the selector matched nothing".
                matched_symbols: graph::syntactic_seed_symbol_count(
                    self.storage.connection(),
                    reference,
                )?,
            })),
        }
    }
}

/// The logical symbol's qualified name and how many members it has IN THE ACTIVE SCOPE. Scoping
/// through the `files` view is what makes a handle minted in a sibling checkout read as absent
/// instead of resolving against rows this checkout cannot see; `None` means no member survives.
fn logical_symbol_in_scope(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<(String, u64)>> {
    let row = conn
        .query_row(
            // Short name as the second-choice seed: `qualified_name_id` is nullable, and a seed of
            // `''` leaves the traversal's query log naming nothing at all.
            "SELECT COALESCE(MIN(qn.value), MIN(symbols.name), ''), COUNT(*)
             FROM logical_symbol_members member
             JOIN symbols ON symbols.id = member.symbol_id
             JOIN files ON files.id = symbols.file_id
             LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
             WHERE member.logical_symbol_id = ?1",
            [logical_symbol_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    Ok(row
        .filter(|(_, members)| *members > 0)
        .map(|(qualified_name, members)| (qualified_name, u64::try_from(members).unwrap_or(0))))
}

pub(super) fn adapt_hops(
    conn: &Connection,
    hops: Vec<graph::GraphHop>,
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
