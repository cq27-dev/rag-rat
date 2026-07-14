use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::language::Language;

/// SQL fragment appended to a name/symbol_path search to drop generated-binding rows (ubrn FFI
/// output, codegen) — navigation noise that buries the source symbol a search is after (#202).
/// Filters on `files.generated`, the same flag search/orientation/tree/clusters default on (set by
/// `crate::index::file_is_generated`: explicit `kind = generated` OR the codegen path heuristic, so
/// it also catches generated code living under a *source* target). Only the *search* arms (name,
/// symbol_path) apply it; an explicit `symbol_id` / `logical_symbol_id` selection is a deliberate
/// pick and is never filtered.
const GENERATED_EXCLUSION_CLAUSE: &str = " AND files.generated = 0";

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
        rename = "id",
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
    #[serde(rename = "lang")]
    pub language: String,
    pub name: String,
    pub qualified_name: String,
    #[serde(rename = "ref")]
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
    #[serde(rename = "id", serialize_with = "crate::serde_big_id::sym_handle::serialize")]
    pub logical_symbol_id: i64,
    #[serde(rename = "lang")]
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

impl SymbolSelector {
    /// The logical-symbol handle to resolve by — `logical_symbol_id` if set, else a `sym_<hex>`
    /// handle that arrived in the `ref`/`symbol_path` slot (#201). A `symbol_lookup` candidate is
    /// emitted with its handle as `id`, so the obvious drill-down — feeding that token back as
    /// `ref` — used to resolve `symbol_path = "sym_…"` against qualified names, match nothing, and
    /// return null. Routing it here (a qualified name never looks like a `sym_` handle, so there's
    /// no ambiguity) makes the handle work in EITHER slot across every symbol-shaped tool.
    pub fn effective_logical_symbol_id(&self) -> Option<i64> {
        self.logical_symbol_id
            .or_else(|| self.symbol_path.as_deref().and_then(crate::serde_big_id::parse_sym_handle))
    }

    /// Whether the `ref`/`symbol_path` slot holds a value SHAPED like a `sym_<hex>` handle — it
    /// parses, OR it starts with `sym_` but is malformed (typo, bad hex, truncated copy). A real
    /// handle is `sym_` + hex with no separators, so we additionally require NO `::` and NO `/`:
    /// that keeps a genuine qualified name that merely *starts* with `sym_` (e.g. a path-qualified
    /// `sym_helpers.rs::build`, or a `sym_dir/…` path) classified as a name, so it stays eligible
    /// for the #152 zero-hit heal. A bad-but-bare handle (`sym_zzzz`) stays id-based and fails
    /// cheaply instead of tripping a heal/reindex that can never recover a handle (#201 review).
    pub fn ref_is_handle_shaped(&self) -> bool {
        self.symbol_path.as_deref().is_some_and(|value| {
            value.starts_with("sym_") && !value.contains("::") && !value.contains('/')
        })
    }
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
    // A bare name search excludes generated bindings by default (#202) — this path has no opt-in.
    let mut hits = lookup_name(conn, name, language, limit, false)?;
    enrich_symbol_hits(conn, &mut hits)?;
    Ok(hits)
}

pub fn lookup_candidates(
    conn: &Connection,
    selector: &SymbolSelector,
    include_generated: bool,
) -> anyhow::Result<SymbolLookup> {
    let candidates = candidates_for_selector(conn, selector, include_generated)?;
    // A handle — `id`, or a `sym_<hex>` in the ref/symbol_path slot (#201) — resolves to one
    // logical symbol's members; binding to any member is binding to the same thing, so a
    // cfg-split/overload group must NOT report disambiguation (the client has no more specific
    // token to give). Mirrors `select_one`'s handle short-circuit.
    let disambiguation_required = selector.effective_logical_symbol_id().is_none()
        && needs_disambiguation(&candidates, selector.allow_ambiguous);
    Ok(SymbolLookup { disambiguation_required, candidates, stale_files: Vec::new() })
}

pub fn select_one(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<Result<Option<SymbolHit>, SymbolDisambiguation>> {
    // Drill-down paths (impact, graph, memory) navigate source; generated bindings are never the
    // target, so exclude them — and an explicit id selector bypasses the filter anyway (#202).
    let mut candidates = candidates_for_selector(conn, selector, false)?;
    if candidates.is_empty() {
        return Ok(Ok(None));
    }
    if selector.effective_logical_symbol_id().is_some() {
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
             qn.value,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
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
    include_generated: bool,
) -> anyhow::Result<Vec<SymbolHit>> {
    // `logical_symbol_id`, OR a `sym_<hex>` handle that arrived in the `symbol_path`/`ref` slot
    // (#201) — resolve both as the logical symbol so a drilled-down candidate handle works in
    // either slot.
    if let Some(logical_symbol_id) = selector.effective_logical_symbol_id() {
        return lookup_logical_members(conn, logical_symbol_id, selector.limit);
    }
    if let Some(symbol_id) = selector.symbol_id {
        return Ok(lookup_by_id(conn, symbol_id)?.into_iter().collect());
    }
    if let Some(symbol_path) = selector.symbol_path.as_deref() {
        let mut hits = lookup_symbol_path(
            conn,
            symbol_path,
            selector.language,
            selector.limit,
            include_generated,
        )?;
        enrich_symbol_hits(conn, &mut hits)?;
        return Ok(hits);
    }
    let Some(symbol) = selector.symbol.as_deref() else {
        anyhow::bail!("one of symbol_id, symbol_path, or symbol is required");
    };
    let mut hits = lookup_name(conn, symbol, selector.language, selector.limit, include_generated)?;
    enrich_symbol_hits(conn, &mut hits)?;
    Ok(hits)
}

/// Tie-break order for same-named `symbol_lookup` candidates, by symbol kind: the definition a
/// human most likely meant, first. Nominal types (0) before named/abstract types (1) before
/// callables (2) before members and values (3) before namespaces (4); a container that merely
/// EXTENDS a type someone else declared (Rust `impl`, Swift `extension`) sorts near-last (8), and
/// an unranked kind last (9).
///
/// Covers every kind ANY backend can emit — `symbol_kind_rank_covers_every_indexed_kind` proves it
/// against `languages::all_symbol_kinds()`. That completeness is the point: this used to be a
/// literal SQL `CASE` that knew only the Rust/TS/Kotlin kinds, so a Swift `protocol` or `actor`, a
/// C++ `namespace` or `union`, and a Rust `module` or `macro` all fell into the unknown bucket and
/// ranked BELOW `impl` — a `protocol Foo` losing to an unrelated `const Foo`.
const SYMBOL_KIND_RANK: &[(&str, i64)] = &[
    // Nominal aggregate types.
    ("struct", 0),
    ("class", 0),
    ("object", 0),
    ("actor", 0),
    ("union", 0),
    // Named and abstract types.
    ("enum", 1),
    ("trait", 1),
    ("interface", 1),
    ("protocol", 1),
    ("type", 1),
    // Callables.
    ("function", 2),
    ("method", 2),
    ("constructor", 2),
    ("macro", 2),
    ("operator", 2),
    // Members and values.
    ("const", 3),
    ("property", 3),
    ("static", 3),
    ("enum_case", 3),
    ("precedence_group", 3),
    // Namespaces.
    ("module", 4),
    ("namespace", 4),
    // Extenders of a type declared elsewhere.
    ("impl", 8),
    ("extension", 8),
];

/// [`SYMBOL_KIND_RANK`] as the SQL `CASE` expression the lookup's `ORDER BY` uses. Generated rather
/// than hand-written so the table stays the single source of truth.
fn symbol_kind_rank_sql() -> String {
    let arms = SYMBOL_KIND_RANK
        .iter()
        .map(|(kind, rank)| format!("WHEN '{kind}' THEN {rank}"))
        .collect::<Vec<_>>()
        .join("\n            ");
    format!("CASE symbols.kind\n            {arms}\n            ELSE 9\n          END")
}

fn lookup_name(
    conn: &Connection,
    name: &str,
    language: Option<Language>,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SymbolHit>> {
    // Fuzzy qualified-name match is interned (#224): match against the shared `name_strings` pool
    // (`qualified_name_id IN (SELECT id FROM name_strings WHERE value LIKE ?2)`) then scope back to
    // `symbols` via the join. The `name = ?1` exact arm is unaffected and stays on
    // `idx_symbols_name` — keep it first so a bare-name hit is indexed. The projection reads
    // the value back through the `qn` join so the output `qualified_name` field is unchanged.
    let mut sql = "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, \
                   qn.value,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE (symbols.name = ?1
               OR symbols.qualified_name_id IN (SELECT id FROM name_strings WHERE value LIKE ?2))
    "
    .to_string();
    if !include_generated {
        sql.push_str(GENERATED_EXCLUSION_CLAUSE);
    }
    if language.is_some() {
        sql.push_str(" AND symbols.language = ?3");
    }
    sql.push_str(&format!(
        "
        ORDER BY
          CASE WHEN symbols.name = ?1 THEN 0 ELSE 1 END,
          {},
          files.path,
          symbols.start_byte
        LIMIT ?
        ",
        symbol_kind_rank_sql(),
    ));

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
        SELECT logical_symbols.id, logical_symbols.language, logical_symbols.path,
               logical_symbols.logical_name, qn.value, logical_symbols.kind,
               logical_symbols.variant_count, logical_symbols.group_reason
        FROM logical_symbols
        LEFT JOIN name_strings qn ON qn.id = logical_symbols.qualified_name_id
        WHERE logical_symbols.id = ?1
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
               logical_symbols.logical_name, qn.value, logical_symbols.kind,
               logical_symbols.variant_count, logical_symbols.group_reason
        FROM logical_symbol_members
        JOIN logical_symbols ON logical_symbols.id = logical_symbol_members.logical_symbol_id
        LEFT JOIN name_strings qn ON qn.id = logical_symbols.qualified_name_id
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
               qn.value, symbols.kind, symbols.start_byte, symbols.end_byte,
               symbols.signature, symbols.docs
        FROM logical_symbol_members
        JOIN symbols ON symbols.id = logical_symbol_members.symbol_id
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
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
    // Stamp the logical handle onto every member: callers selecting by `logical_symbol_id` (impact,
    // graph, memory) build their graph options from `SymbolHit.logical_symbol_id`, and a `None`
    // here silently narrows the exact caller/callee set to the first concrete row instead of
    // the whole logical group (#149 review). We already know the id, so set it without a
    // per-member query.
    if let Some(logical) = lookup_logical_by_id(conn, logical_symbol_id)? {
        for hit in &mut hits {
            hit.logical_symbol_id = Some(logical.logical_symbol_id);
            hit.logical_variant_count = Some(logical.variant_count);
            hit.logical_group_reason = Some(logical.group_reason.clone());
        }
    }
    Ok(hits)
}

fn lookup_symbol_path(
    conn: &Connection,
    symbol_path: &str,
    language: Option<Language>,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SymbolHit>> {
    // Exact qualified-name match is interned (#224): resolve the name to its pool id once, then hit
    // `idx_symbols_qualified_name_id`. The projection reads the value back through the `qn` join.
    let mut sql = "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, \
                   qn.value,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
    "
    .to_string();
    if !include_generated {
        sql.push_str(GENERATED_EXCLUSION_CLAUSE);
    }
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The class tripwire (#635): EVERY symbol kind any language backend can emit must carry an
    /// explicit rank. Driven off `languages::all_symbol_kinds()` — the backends' own declaration —
    /// so registering a language with a new kind (a Swift `protocol`, a C++ `namespace`) reddens
    /// HERE, at the point the ranking would silently dump it into the unknown bucket, instead of
    /// shipping a lookup that sorts it below `impl`. Ranking is a downstream consumer of the
    /// language registry; this is what keeps the two from drifting.
    #[test]
    fn symbol_kind_rank_covers_every_indexed_kind() {
        let ranked: HashSet<&str> = SYMBOL_KIND_RANK.iter().map(|(kind, _)| *kind).collect();
        let unranked: Vec<&str> = crate::index::languages::all_symbol_kinds()
            .into_iter()
            .filter(|kind| !ranked.contains(kind))
            .collect();
        assert!(
            unranked.is_empty(),
            "symbol kinds emitted by a language backend but never ranked in SYMBOL_KIND_RANK \
             (they would sort into the unknown bucket, below `impl`): {unranked:?}"
        );
    }

    /// The rank table is a tie-break, so it is only meaningful if the kinds actually order against
    /// each other: a nominal type outranks a callable outranks a member, and both outrank the
    /// extender/unknown buckets.
    #[test]
    fn symbol_kind_rank_orders_types_before_callables_before_extenders() {
        let rank = |kind: &str| {
            SYMBOL_KIND_RANK.iter().find(|(name, _)| *name == kind).map_or(9, |(_, rank)| *rank)
        };
        assert!(rank("actor") < rank("function"), "a type outranks a callable");
        assert!(rank("protocol") < rank("function"), "a protocol outranks a callable");
        assert!(rank("function") < rank("property"), "a callable outranks a member");
        assert!(rank("enum_case") < rank("extension"), "a member outranks a bare extension");
        assert!(rank("extension") < rank("nonexistent_kind"), "a known kind outranks an unknown");
        // The generated SQL keeps the table's arms and the unknown fallback.
        let sql = symbol_kind_rank_sql();
        assert!(sql.contains("WHEN 'protocol' THEN 1"), "{sql}");
        assert!(sql.contains("ELSE 9"), "{sql}");
    }

    fn selector(logical_symbol_id: Option<i64>, symbol_path: Option<&str>) -> SymbolSelector {
        SymbolSelector {
            logical_symbol_id,
            symbol_id: None,
            symbol_path: symbol_path.map(str::to_string),
            symbol: None,
            language: None,
            allow_ambiguous: false,
            limit: 10,
        }
    }

    #[test]
    fn effective_logical_symbol_id_accepts_a_handle_in_either_slot() {
        let handle = 0x688b_7144_3793_b726_u64 as i64;
        let token = crate::serde_big_id::format_sym_handle(handle);
        // Explicit `id` wins.
        assert_eq!(selector(Some(handle), None).effective_logical_symbol_id(), Some(handle));
        // #201: a sym_<hex> handle fed into the `ref`/symbol_path slot resolves as the handle.
        assert_eq!(selector(None, Some(&token)).effective_logical_symbol_id(), Some(handle));
        // A real qualified name is NOT a handle → falls through to the symbol_path lookup.
        assert_eq!(
            selector(None, Some("crates/x/src/a.rs::foo")).effective_logical_symbol_id(),
            None
        );
        assert_eq!(selector(None, None).effective_logical_symbol_id(), None);
    }
}
