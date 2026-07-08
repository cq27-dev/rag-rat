//! Refine-input loading helpers (#215 Plan 4a): the persisted-row hydration the two-phase refine
//! driver reads BEFORE re-parsing.
//!
//! [`load_refine_rows`] hydrates each member's scoped path + byte range + language + baseline
//! `struct_hash` (the inputs `IndexDatabase::load_refine_members` re-parses), and
//! [`load_source_discriminators`] fetches the per-member `{file_sha256}:{start}-{end}` string that
//! discriminates two structurally-identical-but-source-different classes in the content-addressed
//! refinement cache key. Both are all-or-nothing: a single un-hydratable member returns `Ok(None)`
//! so the caller leaves the whole class un-refined rather than key over a partial multiset.

use rusqlite::Connection;

use super::HYDRATION_CHUNK;
use crate::index::clones::NORM_VERSION;

/// One member's persisted hydration row before the re-parse: scoped path + byte range + language +
/// the baseline `struct_hash` (the canonical sort/cache key). Mirrors `build_class`'s member
/// hydration but additionally pulls `start_byte`/`end_byte` (for the AST descent) and `struct_hash`
/// (the faithfulness pin).
pub(crate) struct RefineRow {
    pub(crate) symbol_id: i64,
    pub(crate) path: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) language: crate::language::Language,
    pub(crate) struct_hash: String,
}

/// Hydrate the scoped path + byte range + language + persisted baseline `struct_hash` for each
/// `member_ids` symbol, in chunks of [`HYDRATION_CHUNK`] (the same SQLite host-param discipline as
/// `build_class`). Filters the baseline normalizer version so a stale fingerprint row never feeds
/// the refine input. Returns `Ok(None)` if ANY requested member is absent (a fingerprint row
/// vanished mid-read, or the symbol fell out of scope): a partial refine would be unfaithful.
pub(crate) fn load_refine_rows(
    conn: &Connection,
    member_ids: &[i64],
) -> anyhow::Result<Option<Vec<RefineRow>>> {
    let mut rows: Vec<RefineRow> = Vec::with_capacity(member_ids.len());
    for chunk in member_ids.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let version_placeholder = format!("?{}", chunk.len() + 1);
        let sql = format!(
            "SELECT symbols.id, files.path, symbols.start_byte, symbols.end_byte, \
             symbols.language, sf.struct_hash
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = {version_placeholder}
             WHERE symbols.id IN ({})
             ORDER BY symbols.id",
            id_placeholders.join(", ")
        );
        let params: Vec<i64> = chunk.iter().copied().chain(std::iter::once(NORM_VERSION)).collect();
        let mut stmt = conn.prepare(&sql)?;
        let chunk_rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let start_byte: i64 = row.get(2)?;
            let end_byte: i64 = row.get(3)?;
            let lang_str: String = row.get(4)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                start_byte,
                end_byte,
                lang_str,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in chunk_rows {
            let (symbol_id, path, start_byte, end_byte, lang_str, struct_hash) = row?;
            // A negative byte offset can't occur (schema NOT NULL, written from usize), but guard
            // the cast so a corrupt row degrades to the un-refined fallback rather than panicking.
            let (Ok(start_byte), Ok(end_byte)) =
                (usize::try_from(start_byte), usize::try_from(end_byte))
            else {
                return Ok(None);
            };
            // An unparseable language string means the row's language is no longer one this build
            // understands — bail to the un-refined fallback.
            let Ok(language) = lang_str.parse::<crate::language::Language>() else {
                return Ok(None);
            };
            rows.push(RefineRow { symbol_id, path, start_byte, end_byte, language, struct_hash });
        }
    }

    // EVERY requested member must hydrate. A missing one (vanished fingerprint row, out-of-scope
    // symbol) makes the class incomplete — refine the whole class or none of it.
    if rows.len() != member_ids.len() {
        return Ok(None);
    }
    rows.sort_unstable_by_key(|r| r.symbol_id);
    Ok(Some(rows))
}

/// Whether ANY member's file has CURRENT SCIP-oracle callee coverage (#275, Plan 3): an
/// `edge_oracle` call-HEAD row whose `file_sha` matches the file's indexed `sha256` and that
/// passes the shared currency gate (`callee_moniker_current_clause`: call-HEAD edge kinds, the
/// LATEST completed run per tool in the active `(commit_sha, worktree_id)` checkout, a still-live
/// resolved definition) — the SAME discipline the moniker fetch applies, so mode and attachment
/// can't diverge. This is the refine-MODE probe — `true` selects [`RefineMode::Scip`], which keys
/// (and caches) the refinement in the scip-mode namespace and lets the loader attach callee
/// monikers. CHEAP (one EXISTS per hydration chunk over `idx_edge_oracle_anchor`), and
/// deterministic for a given (index, oracle) state, so the warm cache probe stays a probe.
///
/// ANY (not ALL) member coverage selects scip mode: a class spanning a covered and an uncovered
/// file still benefits when the compared callee spans all carry monikers, and the collapse itself
/// stays span-exact (a member without a moniker vetoes its own comparison, never the whole
/// class). A class with NO covered file computes byte-identically to baseline, so it keeps the
/// baseline key rather than duplicating rows into the scip namespace.
///
/// [`RefineMode::Scip`]: crate::index::clones::refine::cache::RefineMode
pub(crate) fn oracle_callee_coverage_exists(
    conn: &Connection,
    member_ids: &[i64],
    commit_sha: &str,
    worktree_id: &str,
) -> anyhow::Result<bool> {
    let repo_clause = crate::index::schema::periphery_repo_scope_clause(
        &crate::index::schema::periphery_repo_scope(conn, "edge_oracle")?,
        "edge_oracle",
    );
    for chunk in member_ids.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let commit_slot = format!("?{}", chunk.len() + 1);
        let worktree_slot = format!("?{}", chunk.len() + 2);
        let current_clause = crate::index::oracle::callee_moniker_current_clause(
            conn,
            &commit_slot,
            &worktree_slot,
        )?;
        let sql = format!(
            "SELECT EXISTS(
                 SELECT 1
                 FROM symbols
                 JOIN files ON files.id = symbols.file_id
                 JOIN edge_oracle
                   ON edge_oracle.source_path = files.path
                   AND edge_oracle.file_sha = files.sha256{repo_clause}{current_clause}
                 WHERE symbols.id IN ({})
             )",
            id_placeholders.join(", ")
        );
        let params: Vec<Box<dyn rusqlite::ToSql>> = chunk
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
            .chain([
                Box::new(commit_sha.to_string()) as Box<dyn rusqlite::ToSql>,
                Box::new(worktree_id.to_string()) as Box<dyn rusqlite::ToSql>,
            ])
            .collect();
        let covered: bool =
            conn.query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;
        if covered {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Fetch a per-member SOURCE DISCRIMINATOR — `"{file_sha256}:{start_byte}-{end_byte}"` — for the
/// refinement cache key (#215 Plan 4b, cache-poisoning fix). `file_sha256` is the indexed file
/// content hash; the body byte span pins the member's source range. Together they uniquely
/// determine the member's raw source bytes, so two structurally-identical-but-source-different
/// classes (same `struct_hash` multiset, different real literals) get DISTINCT keys → no
/// cross-class poisoning of the source-specific 4b payload (template / per-member values /
/// signature). Two BYTE-IDENTICAL-source classes still share the discriminator multiset → the same
/// key → true content-addressing of real duplicates is preserved.
///
/// CHEAP — a pure SELECT joining `symbols → files` (no tree-sitter re-parse), so a warm cache probe
/// stays a probe: `refine_class_in_place` calls this BEFORE `refine_lookup`, and the lookup still
/// short-circuits the expensive `load_refine_members` re-parse on a hit. Returns `Ok(None)` when
/// ANY member fails to hydrate (vanished row, out-of-scope symbol) — the caller leaves the class
/// un-refined rather than key over a partial (and therefore structure-only-aliasing) multiset.
pub(crate) fn load_source_discriminators(
    conn: &Connection,
    member_ids: &[i64],
) -> anyhow::Result<Option<Vec<String>>> {
    if member_ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mut discriminators: Vec<String> = Vec::with_capacity(member_ids.len());
    for chunk in member_ids.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT files.sha256, symbols.start_byte, symbols.end_byte
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             WHERE symbols.id IN ({})",
            id_placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let chunk_rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        })?;
        for row in chunk_rows {
            let (sha256, start_byte, end_byte) = row?;
            discriminators.push(format!("{sha256}:{start_byte}-{end_byte}"));
        }
    }
    // EVERY requested member must hydrate, exactly as `load_refine_rows` requires — a partial
    // multiset would alias a different class. Mismatch ⇒ leave un-refined.
    if discriminators.len() != member_ids.len() {
        return Ok(None);
    }
    Ok(Some(discriminators))
}
