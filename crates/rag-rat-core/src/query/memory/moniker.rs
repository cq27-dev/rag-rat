//! SCIP moniker anchors for repo memories (#70, SCIP phase 3).
//!
//! `oracle run` records each in-corpus SCIP definition's symbol string ("moniker") against the
//! defining logical symbol in `logical_symbol_monikers` (see `index/oracle/run.rs`). This module
//! is the memory-side consumer:
//!
//! - **Auto-binding:** a memory created (or rebound) on a symbol whose logical symbol has a known
//!   moniker gets an additional `scip_moniker` binding row automatically — binding_id is the
//!   moniker, with tool + tool_version provenance on the row.
//! - **Validation:** a `scip_moniker` binding re-resolves its moniker against the current oracle
//!   data and refreshes its location fields.
//! - **Relocation fallback:** when a symbol/logical_symbol binding's other anchors are exhausted
//!   (qualified name gone, no content-hash match), the sibling moniker binding is re-resolved; a
//!   unique live match relocates the binding with `relocation_reason = "moniker-match"`. A match
//!   under a different current `tool_version` is lower confidence and additionally requires
//!   `symbol_kind` corroboration.

use super::*;

/// Why a moniker relocation succeeded — persisted on `repo_memory_bindings.relocation_reason` so
/// `doctor`/MCP output can distinguish a semantic-identity relocate from the default
/// qualified-name/content paths.
pub(crate) const MONIKER_MATCH_REASON: &str = "moniker-match";

/// Why a `scip_moniker` binding's own anchor string was rewritten: its live logical symbol got a
/// NEW moniker from the latest run (rust-analyzer monikers embed the Cargo package version, so a
/// routine version bump changes every string without changing any symbol identity). The rebind is
/// keyed off our own content-derived logical id, not fuzzy matching.
pub(crate) const MONIKER_REFRESH_REASON: &str = "moniker-refresh";

/// The binding kind for a moniker anchor row.
pub(crate) const SCIP_MONIKER_BINDING_KIND: &str = "scip_moniker";

/// One `logical_symbol_monikers` row, as read for auto-binding.
#[derive(Debug, Clone)]
pub(crate) struct MonikerRow {
    pub(crate) moniker: String,
    pub(crate) tool: String,
    pub(crate) tool_version: String,
}

/// The current moniker for a logical symbol, if the oracle has recorded one. At most one row per
/// tool; with a single tool per language today the first row wins deterministically (ORDER BY
/// tool so a future multi-tool index stays stable).
pub(crate) fn moniker_for_logical_symbol(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<MonikerRow>> {
    conn.query_row(
        "
        SELECT moniker, tool, tool_version
        FROM logical_symbol_monikers
        WHERE logical_symbol_id = ?1
        ORDER BY tool
        LIMIT 1
        ",
        [logical_symbol_id],
        |row| Ok(MonikerRow { moniker: row.get(0)?, tool: row.get(1)?, tool_version: row.get(2)? }),
    )
    .optional()
    .map_err(Into::into)
}

/// The current moniker row for a logical symbol under a SPECIFIC tool — the read the validation
/// re-derivation path uses (the binding records which tool supplied its anchor, so the refresh
/// must not cross tools).
pub(crate) fn moniker_for_logical_symbol_tool(
    conn: &Connection,
    logical_symbol_id: i64,
    tool: &str,
) -> anyhow::Result<Option<MonikerRow>> {
    conn.query_row(
        "
        SELECT moniker, tool, tool_version
        FROM logical_symbol_monikers
        WHERE logical_symbol_id = ?1 AND tool = ?2
        ",
        params![logical_symbol_id, tool],
        |row| Ok(MonikerRow { moniker: row.get(0)?, tool: row.get(1)?, tool_version: row.get(2)? }),
    )
    .optional()
    .map_err(Into::into)
}

/// What re-resolving a moniker against the current oracle data concluded.
#[derive(Debug)]
pub(crate) enum MonikerResolution {
    /// The tool has no moniker rows at all — the oracle never ran (or its data was cleared), so
    /// nothing can be said either way.
    NoData,
    /// The tool has current data and no row carries this moniker: the latest oracle run did not
    /// see this symbol.
    Gone,
    /// A row carries the moniker but its content-derived logical id no longer exists — the symbol
    /// changed (or moved) after the last `oracle run`; the data is outdated, not authoritative.
    Dangling,
    /// More than one live logical symbol carries this moniker — ambiguous, never relocate.
    Ambiguous,
    /// Exactly one live logical symbol carries this moniker.
    Unique { logical_symbol_id: i64, tool_version: String },
}

/// Re-resolve a moniker for `tool` against the current `logical_symbol_monikers` data, joining
/// live `logical_symbols` (a dangling row — its content-derived id died in a rebuild — must not
/// resolve).
pub(crate) fn resolve_moniker(
    conn: &Connection,
    moniker: &str,
    tool: &str,
) -> anyhow::Result<MonikerResolution> {
    let mut stmt = conn.prepare(
        "
        SELECT logical_symbol_monikers.logical_symbol_id, logical_symbol_monikers.tool_version,
               logical_symbols.id IS NOT NULL AS live
        FROM logical_symbol_monikers
        LEFT JOIN logical_symbols ON logical_symbols.id = logical_symbol_monikers.logical_symbol_id
        WHERE logical_symbol_monikers.moniker = ?1 AND logical_symbol_monikers.tool = ?2
        ",
    )?;
    let rows = stmt
        .query_map(params![moniker, tool], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, bool>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut live = rows.iter().filter(|(_, _, is_live)| *is_live);
    let first = live.next();
    let second = live.next();
    match (rows.is_empty(), first, second) {
        (true, ..) => {
            let tool_has_rows: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM logical_symbol_monikers WHERE tool = ?1)",
                [tool],
                |row| row.get(0),
            )?;
            Ok(if tool_has_rows { MonikerResolution::Gone } else { MonikerResolution::NoData })
        },
        (false, None, _) => Ok(MonikerResolution::Dangling),
        (false, Some(_), Some(_)) => Ok(MonikerResolution::Ambiguous),
        (false, Some((logical_symbol_id, tool_version, _)), None) =>
            Ok(MonikerResolution::Unique {
                logical_symbol_id: *logical_symbol_id,
                tool_version: tool_version.clone(),
            }),
    }
}

/// Build the relocation target for a live logical symbol: its representative member symbol
/// (chunk-first, member-table fallback) with the same fields `relocate_symbol_by_name` recovers,
/// so a moniker relocate rebinds identically to a name/content relocate.
pub(crate) fn relocate_match_for_logical_symbol(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<RelocateMatch>> {
    let chunk = chunk_for_logical_symbol(conn, logical_symbol_id)?;
    let member_symbol_id = match chunk.as_ref().and_then(|c| c.symbol_id) {
        Some(id) => id,
        None => {
            // No chunk-attached member (e.g. a chunking gap): fall back to the first member row.
            let Some(id) = conn
                .query_row(
                    "SELECT symbol_id FROM logical_symbol_members
                     WHERE logical_symbol_id = ?1 ORDER BY start_line LIMIT 1",
                    [logical_symbol_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            id
        },
    };
    let Some(qualified_name) = conn
        .query_row(
            "SELECT qn.value FROM symbols
             LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
             WHERE symbols.id = ?1",
            [member_symbol_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let Some(path) = conn
        .query_row(
            "SELECT files.path FROM symbols JOIN files ON files.id = symbols.file_id
             WHERE symbols.id = ?1",
            [member_symbol_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let (symbol_kind, signature_hash) = symbol_signal(conn, member_symbol_id)?;
    Ok(Some(RelocateMatch {
        binding_id: qualified_name,
        symbol_id: member_symbol_id,
        logical_symbol_id: Some(logical_symbol_id),
        path,
        chunk_id: chunk.as_ref().map(|c| c.chunk_id),
        start_line: chunk.as_ref().map(|c| c.start_line),
        end_line: chunk.as_ref().map(|c| c.end_line),
        symbol_kind,
        signature_hash,
    }))
}

/// The moniker recorded for a memory at bind time: the `scip_moniker` sibling binding's
/// `(moniker, tool, tool_version)`, if the memory has one.
pub(crate) fn moniker_binding_for_memory(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Option<MonikerRow>> {
    conn.query_row(
        "
        SELECT binding_id, moniker_tool, moniker_tool_version
        FROM repo_memory_bindings
        WHERE memory_id = ?1 AND binding_kind = ?2
        LIMIT 1
        ",
        params![memory_id, SCIP_MONIKER_BINDING_KIND],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )
    .optional()
    .map(|row| {
        row.and_then(|(moniker, tool, tool_version)| {
            Some(MonikerRow { moniker, tool: tool?, tool_version: tool_version? })
        })
    })
    .map_err(Into::into)
}

/// The moniker relocation fallback for a `symbol` / `logical_symbol` binding whose other anchors
/// are exhausted (#70): re-resolve the memory's recorded moniker against current oracle data and
/// return the unique live target, or `None`.
///
/// Confidence gate: a match whose current `tool_version` differs from the one recorded at bind
/// time is lower confidence (the tool's moniker format may have changed between versions), so it
/// additionally requires `symbol_kind` corroboration against the binding's stored kind — a
/// same-version match relocates on moniker identity alone.
pub(crate) fn relocate_binding_by_moniker(
    conn: &Connection,
    binding: &RepoMemoryBinding,
) -> anyhow::Result<Option<RelocateMatch>> {
    let Some(recorded) = moniker_binding_for_memory(conn, &binding.memory_id)? else {
        return Ok(None);
    };
    let MonikerResolution::Unique { logical_symbol_id, tool_version } =
        resolve_moniker(conn, &recorded.moniker, &recorded.tool)?
    else {
        return Ok(None);
    };
    let Some(matched) = relocate_match_for_logical_symbol(conn, logical_symbol_id)? else {
        return Ok(None);
    };
    let cross_version = tool_version != recorded.tool_version;
    if cross_version {
        let corroborated = match (binding.symbol_kind.as_deref(), matched.symbol_kind.as_deref()) {
            (Some(stored), Some(found)) => stored == found,
            _ => false,
        };
        if !corroborated {
            return Ok(None);
        }
    }
    Ok(Some(matched))
}

/// Insert the automatic `scip_moniker` binding for a freshly bound memory, when the primary
/// binding is a symbol/logical_symbol whose logical symbol has a known moniker. No-op otherwise.
/// `INSERT OR IGNORE` keeps a re-create of the same memory id from tripping the binding PK.
pub(crate) fn insert_auto_moniker_binding(
    conn: &Connection,
    memory_id: &str,
    primary: &ResolvedBinding,
    now: i64,
) -> anyhow::Result<()> {
    if !matches!(primary.binding_kind.as_str(), "symbol" | "logical_symbol") {
        return Ok(());
    }
    let Some(logical_symbol_id) = primary.logical_symbol_id else {
        return Ok(());
    };
    let Some(row) = moniker_for_logical_symbol(conn, logical_symbol_id)? else {
        return Ok(());
    };
    conn.execute(
        "
        INSERT OR IGNORE INTO repo_memory_bindings(
            memory_id, binding_kind, binding_id, path, start_line, end_line, logical_symbol_id,
            symbol_id, chunk_id, edge_id, commit_hash, github_owner, github_repo, github_number,
            symbol_kind, signature_hash, moniker_tool, moniker_tool_version, anchor_status,
            created_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, NULL, NULL, NULL, ?10, ?11, ?12, \
         ?13, 'current', ?14)
        ",
        params![
            memory_id,
            SCIP_MONIKER_BINDING_KIND,
            row.moniker,
            primary.path,
            primary.start_line,
            primary.end_line,
            logical_symbol_id,
            primary.symbol_id,
            primary.chunk_id,
            primary.symbol_kind,
            primary.signature_hash,
            row.tool,
            row.tool_version,
            now
        ],
    )?;
    Ok(())
}

/// Validate a `scip_moniker` binding: re-resolve the moniker against current oracle data and
/// refresh the binding's location fields from the live logical symbol. Statuses:
///
/// - `unverified` — no oracle data for the tool at all (nothing can be said), or a malformed row
///   missing its tool provenance.
/// - `gone` — the tool has current data and the moniker is not in it (and the stored logical symbol
///   is dead too).
/// - `stale` — the data is outdated (the moniker's row dangles after the symbol changed) or
///   ambiguous (two live logical symbols share it); awaiting the next `oracle run`.
/// - `current` / `relocated` — anchored to a live logical symbol; `relocated` when the resolved
///   logical symbol — or the moniker string itself — changed (the binding is rewritten).
///
/// Resolution order is deliberate. (1) The binding's stored `logical_symbol_id` is a
/// CONTENT-DERIVED stable id — when it is still live and carries a current moniker row, that is
/// the strongest evidence and also heals MONIKER-STRING DRIFT: rust-analyzer monikers embed the
/// Cargo package version, so a routine version bump rewrites every string while no symbol
/// changes; matching by string alone would mark every anchor `gone` forever. The rebind takes the
/// row's fresh `tool_version` as new provenance (it is a re-bind under current data, not a
/// cross-version string trust). (2) Only when the stored id is dead (the file-move case) does the
/// recorded STRING resolve against current rows — and there the bind-time
/// `moniker_tool_version` is deliberately NOT refreshed, so the cross-version corroboration gate
/// in [`relocate_binding_by_moniker`] always compares against bind-time provenance (Codex P1: a
/// refreshed "last verified" version would silently downgrade a real cross-version match to
/// same-version).
pub(crate) fn validate_moniker_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let Some(tool) = binding.moniker_tool.clone() else {
        return Ok("unverified".to_string());
    };
    // (1) The stored stable logical id is live and has a current moniker row → anchor there.
    if let Some(stored_id) = binding.logical_symbol_id
        && crate::query::symbol::lookup_logical_by_id(conn, stored_id)?.is_some()
        && let Some(row) = moniker_for_logical_symbol_tool(conn, stored_id, &tool)?
    {
        let Some(matched) = relocate_match_for_logical_symbol(conn, stored_id)? else {
            return Ok("stale".to_string());
        };
        let drifted = binding.binding_id != row.moniker;
        apply_relocate_match(binding, &matched);
        if drifted {
            binding.binding_id = row.moniker;
            binding.moniker_tool_version = Some(row.tool_version);
            binding.relocation_reason = Some(MONIKER_REFRESH_REASON.to_string());
            return Ok("relocated".to_string());
        }
        return Ok("current".to_string());
    }
    // (2) The stored id is dead (file move) or has no current row: resolve the recorded string.
    match resolve_moniker(conn, &binding.binding_id, &tool)? {
        MonikerResolution::NoData => Ok("unverified".to_string()),
        MonikerResolution::Gone => Ok("gone".to_string()),
        MonikerResolution::Dangling | MonikerResolution::Ambiguous => Ok("stale".to_string()),
        MonikerResolution::Unique { logical_symbol_id, tool_version: _ } => {
            let moved = binding.logical_symbol_id != Some(logical_symbol_id);
            let Some(matched) = relocate_match_for_logical_symbol(conn, logical_symbol_id)? else {
                return Ok("stale".to_string());
            };
            apply_relocate_match(binding, &matched);
            if moved {
                binding.relocation_reason = Some(MONIKER_MATCH_REASON.to_string());
                Ok("relocated".to_string())
            } else {
                Ok("current".to_string())
            }
        },
    }
}

/// Rewrite a binding's location fields from a relocate match (shared by the moniker validate arms;
/// the symbol/logical fallback in validate.rs additionally rewrites `binding_id`).
fn apply_relocate_match(binding: &mut RepoMemoryBinding, matched: &RelocateMatch) {
    binding.logical_symbol_id = matched.logical_symbol_id;
    binding.symbol_id = Some(matched.symbol_id);
    binding.path = Some(matched.path.clone());
    binding.chunk_id = matched.chunk_id;
    binding.start_line = matched.start_line;
    binding.end_line = matched.end_line;
    binding.symbol_kind = matched.symbol_kind.clone();
    binding.signature_hash = matched.signature_hash.clone();
}
