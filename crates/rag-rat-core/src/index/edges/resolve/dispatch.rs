//! Synthesized dispatch edges (#200).
//!
//! Message/actor dispatch hides a real call-graph hop: a function CONSTRUCTS an enum variant
//! (`MlReq::Upsert { .. }`), sends it over a channel, and a `match` arm elsewhere handles it
//! (`MlReq::Upsert { x } => self.upsert(x)`). tree-sitter sees `sender → constructs MlReq` and
//! `handler → calls upsert`, but never `sender → upsert`, so `find_callers(upsert)` misses the
//! sender. Extraction records two FACT edge kinds — `dispatch_construct` (fn → `Enum::Variant`
//! key, in `to_name`) and `dispatch_handle` (`Enum::Variant` key in `evidence`, handler in
//! `to_name`, resolved to its symbol). This pass joins them on the variant key and emits a real
//! `dispatches` edge from each constructor to each handler.
//!
//! Runs AFTER resolution, over the active-checkout `files` scope view, for BOTH drivers
//! (incremental `resolve_all_edges` and full-rebuild `resolve_and_insert_edges`). Idempotent: it
//! drops the prior scope's `dispatches` rows first, so the incremental driver re-runs cleanly.

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::index::edges::{EdgeConfidence, EdgeKind, EdgeStringInterner};

/// Cap on constructors × handlers materialized for one variant key. A catch-all message enum used
/// in many sites would otherwise blow up quadratically into low-value edges; past this we skip the
/// whole variant (counted in the returned `skipped_variants`). 64 comfortably covers real actor
/// enums (a handful of senders, one handler arm each).
const MAX_DISPATCH_PAIRS_PER_VARIANT: usize = 64;

#[derive(Clone, Copy)]
struct Constructor {
    symbol_id: i64,
    source_file_id: i64,
    start_line: i64,
    end_line: i64,
}

struct Handler {
    symbol_id: i64,
    qualified_name: String,
    start_line: i64,
    end_line: i64,
}

/// Outcome of a synthesis pass — how many `dispatches` edges were emitted and how many variants
/// were skipped for exceeding the fan-out cap. Returned so the indexer can `log` the skips (silent
/// truncation would read as "no dispatch beyond this", which isn't true).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DispatchSynthesis {
    pub inserted: usize,
    pub skipped_variants: usize,
}

pub(crate) fn synthesize_dispatch_edges(conn: &Connection) -> anyhow::Result<DispatchSynthesis> {
    let mut interner = EdgeStringInterner::default();
    let dispatches_kind_id = interner.get(conn, EdgeKind::Dispatches.as_str())?;

    // Idempotent re-run: drop any prior synthesized dispatches in the ACTIVE scope before
    // rebuilding them. `files` is the per-connection scope view (#89), so other checkouts are
    // untouched.
    conn.execute(
        "DELETE FROM edges_data
         WHERE edge_kind_id = ?1 AND source_file_id IN (SELECT id FROM files)",
        params![dispatches_kind_id],
    )?;

    let constructors = collect_constructors(conn)?;
    let handlers = collect_handlers(conn)?;
    if constructors.is_empty() || handlers.is_empty() {
        return Ok(DispatchSynthesis::default());
    }
    // Scope the join by the enum DEFINITION (#200 review): the 2-segment key (`Msg::Start`) is
    // module-stripped, so two active enums both named `Msg` would otherwise merge — emitting
    // dispatches from one enum's senders to the other's handlers. Skip a variant ONLY when its enum
    // head names MULTIPLE in-scope Rust enums (genuinely ambiguous → cross-enum risk). A head with
    // zero enum definitions is admitted: it's an alias/import (`use proto::Message as Msg`) or
    // external type, and since both the sender and the handler write the SAME head they still refer
    // to the same enum — gating it out would just lose the dispatch (a recall miss, not safety).
    let ambiguous_enums = ambiguous_enum_names(conn)?;

    let confidence_id = interner.get(conn, EdgeConfidence::Syntactic.as_str())?;
    let resolution_id = interner.get(conn, "dispatch")?;
    let mut result = DispatchSynthesis::default();

    for (variant, ctors) in &constructors {
        let Some(hands) = handlers.get(variant) else {
            continue;
        };
        let enum_name = variant.split("::").next().unwrap_or("");
        if ambiguous_enums.contains(enum_name) {
            result.skipped_variants += 1;
            continue;
        }
        if ctors.len().saturating_mul(hands.len()) > MAX_DISPATCH_PAIRS_PER_VARIANT {
            result.skipped_variants += 1;
            continue;
        }
        // Intern each handler's qualified_name once per variant (reused across constructors).
        let mut to_name_cache: HashMap<i64, i64> = HashMap::new();
        for ctor in ctors.values() {
            for handler in hands.values() {
                if ctor.symbol_id == handler.symbol_id {
                    // A handler that constructs the same variant it handles is not a dispatch INTO
                    // itself; skip the self-edge.
                    continue;
                }
                let to_name_id = match to_name_cache.get(&handler.symbol_id) {
                    Some(id) => *id,
                    None => {
                        let id = interner.get(conn, &handler.qualified_name)?;
                        to_name_cache.insert(handler.symbol_id, id);
                        id
                    },
                };
                conn.prepare_cached(
                    "INSERT INTO edges_data(
                        source_file_id, from_symbol_id, to_name_id, evidence,
                        source_start_line, source_end_line,
                        edge_kind_id, confidence_id,
                        to_symbol_id, target_start_line, target_end_line, resolution_id
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )?
                .execute(params![
                    ctor.source_file_id,
                    ctor.symbol_id,
                    to_name_id,
                    variant, // evidence: the Enum::Variant the dispatch routes through
                    ctor.start_line,
                    ctor.end_line,
                    dispatches_kind_id,
                    confidence_id,
                    handler.symbol_id,
                    handler.start_line,
                    handler.end_line,
                    resolution_id,
                ])?;
                result.inserted += 1;
            }
        }
    }
    Ok(result)
}

/// `variant_key -> { constructor_symbol_id -> Constructor }`, deduped by symbol id (a fn that
/// constructs the variant twice contributes one constructor). Active-scope only via the `files`
/// view.
fn collect_constructors(
    conn: &Connection,
) -> anyhow::Result<HashMap<String, HashMap<i64, Constructor>>> {
    let mut stmt = conn.prepare(
        "SELECT tn.value, d.from_symbol_id, d.source_file_id, d.source_start_line, \
         d.source_end_line
         FROM edges_data d
         JOIN files ON files.id = d.source_file_id
         JOIN edge_strings ek ON ek.id = d.edge_kind_id
         JOIN edge_strings tn ON tn.id = d.to_name_id
         WHERE ek.value = 'dispatch_construct' AND d.from_symbol_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, Constructor {
            symbol_id: row.get(1)?,
            source_file_id: row.get(2)?,
            start_line: row.get(3)?,
            end_line: row.get(4)?,
        }))
    })?;
    let mut out: HashMap<String, HashMap<i64, Constructor>> = HashMap::new();
    for row in rows {
        let (variant, ctor) = row?;
        out.entry(variant).or_default().entry(ctor.symbol_id).or_insert(ctor);
    }
    Ok(out)
}

/// `variant_key -> { handler_symbol_id -> Handler }`, deduped by handler symbol id. The handle
/// fact's `to_name` was resolved to a symbol by the normal resolver, so we only keep arms whose
/// handler bound to an in-corpus symbol (`to_symbol_id IS NOT NULL`) — an unresolved handler is
/// never guessed.
fn collect_handlers(conn: &Connection) -> anyhow::Result<HashMap<String, HashMap<i64, Handler>>> {
    let mut stmt = conn.prepare(
        "SELECT d.evidence, d.to_symbol_id, sym.qualified_name, sym.start_line, sym.end_line
         FROM edges_data d
         JOIN files ON files.id = d.source_file_id
         JOIN edge_strings ek ON ek.id = d.edge_kind_id
         JOIN symbols sym ON sym.id = d.to_symbol_id
         WHERE ek.value = 'dispatch_handle' AND d.to_symbol_id IS NOT NULL AND d.evidence IS NOT \
         NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, Handler {
            symbol_id: row.get(1)?,
            qualified_name: row.get(2)?,
            start_line: row.get(3)?,
            end_line: row.get(4)?,
        }))
    })?;
    let mut out: HashMap<String, HashMap<i64, Handler>> = HashMap::new();
    for row in rows {
        let (variant, handler) = row?;
        out.entry(variant).or_default().entry(handler.symbol_id).or_insert(handler);
    }
    Ok(out)
}

/// Names of RUST `enum` symbols defined MORE THAN ONCE in the active scope (genuinely ambiguous).
/// The dispatch join skips these so two same-named enums in different modules never merge (#200
/// review). Scoped to `language = 'rust'` because the facts are emitted from Rust syntax and the
/// parser also stores C/C++ `enum_specifier` definitions as `kind = 'enum'` — a C++ `enum Msg` must
/// not make a Rust `Msg` actor enum look ambiguous. Active-scope only via the `files` view.
fn ambiguous_enum_names(conn: &Connection) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT sym.name
         FROM symbols sym
         JOIN files ON files.id = sym.file_id
         WHERE sym.kind = 'enum' AND sym.language = 'rust'
         GROUP BY sym.name
         HAVING COUNT(*) > 1",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}
