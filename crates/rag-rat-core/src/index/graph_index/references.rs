//! Durable references to logical-symbol ids: the on-open realign, the remap / vacate / null
//! rewrites a key-drift heal applies, and synced distill-anchor re-resolution.

use super::logical_key::LogicalSymbolKey;
use crate::index::*;

/// Recompute every `logical_symbols` row's content-derived id from its OWN `repo_id` and re-point
/// that id everywhere it is referenced, so ids stay consistent now that
/// [`LogicalSymbolKey::stable_id`] folds `repo_id` in (A3). Returns the number of rows remapped.
///
/// WHY: on an UPGRADED DB (existing pre-fold `logical_symbols`), or when a `repo_id` changes at
/// adoption (placeholder → real), the next [`IndexDatabase::rebuild_logical_symbols`] would
/// re-derive every logical symbol under a NEW id, dangling every `repo_memory_bindings`,
/// `repo_memory_call_paths`, `logical_symbol_monikers`, and `logical_symbol_members` row that still
/// points at the OLD id (pre-V040 memories + oracle data). This migrates those references IN PLACE
/// before the first rebuild, so a bound memory resolves to the same symbol under the new id. Called
/// from the V040 migration (after the `repo_id` backfill) and from [`register_repo`] adoption
/// (after the placeholder → real re-point), both idempotent: a row already at `hash(repo_id ‖ key)`
/// is skipped, so the two calls compose without double-remapping.
///
/// The hash inputs are row-resident EXCEPT `signature`, which `logical_symbols` does not store —
/// recover it from any member's `symbols.signature` (every member of a group shares it, since the
/// signature is part of the key). A logical symbol with no live member is an orphan the next
/// rebuild would drop anyway (its binding is already effectively dead), so it is left untouched.
///
/// FK NOTE: `logical_symbol_members` carries an `ON DELETE CASCADE` FK to `logical_symbols(id)`, so
/// the caller MUST run with FK enforcement OFF (the V040 migration) or DEFERRED
/// (`PRAGMA defer_foreign_keys = ON`, the adoption transaction) — else the parent-id UPDATE trips
/// the child FK. The remap runs inside the caller's transaction (torn-safe) and uses a NEGATIVE
/// temp-id pass so a new id that equals another remapped row's OLD id can never collide on the PK
/// mid-migration.
pub(crate) fn realign_logical_symbol_ids(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    struct Row {
        old_id: i64,
        repo_id: String,
        language: String,
        path: String,
        name: String,
        qualified_name: Option<String>,
        scope_path: Option<String>,
        kind: String,
        signature: Option<String>,
    }
    let has_scope_path = conn.prepare("SELECT scope_path FROM symbols LIMIT 0").is_ok();
    // Take the members' scope only when they AGREE on it. A pre-V040 group can hold the very
    // collision this version fixes — same-named, same-signature methods under different impl
    // owners — and picking one member arbitrarily would move the whole group's durable references
    // onto that one owner's new id. If the chosen owner then happens to be the surviving exact
    // key, the drift heal sees an unchanged reference and silently hands shared memories or
    // monikers to an arbitrary owner. A disagreeing group yields NULL and is skipped here, so the
    // key-drift relocation path handles the split as the ambiguity it is. The unanimity test
    // itself (how the collected member scopes are compared) is documented at the comparison below.
    let scope_path_subquery = if has_scope_path {
        "(SELECT GROUP_CONCAT(COALESCE(s.scope_path, ''), char(31))
            FROM logical_symbol_members m JOIN symbols s ON s.id = m.symbol_id
           WHERE m.logical_symbol_id = ls.id)"
    } else {
        "''"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT ls.id, ls.repo_id, ls.language, ls.path, ls.logical_name,
                (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                ls.kind,
                (SELECT s.signature FROM logical_symbol_members m
                   JOIN symbols s ON s.id = m.symbol_id
                  WHERE m.logical_symbol_id = ls.id LIMIT 1),
                {scope_path_subquery}
         FROM logical_symbols ls",
    ))?;
    let rows = stmt
        .query_map([], |r| {
            let language: String = r.get(2)?;
            Ok(Row {
                old_id: r.get(0)?,
                repo_id: r.get(1)?,
                path: r.get(3)?,
                name: r.get(4)?,
                qualified_name: r.get(5)?,
                kind: r.get(6)?,
                signature: r.get(7)?,
                scope_path: r.get::<_, Option<String>>(8)?.and_then(|joined| {
                    // One IDENTICAL scope across every member, or nothing. A group whose members
                    // disagree is the pre-upgrade owner collision — version 1 keyed without the
                    // scope, so `Foo<T>::run` and `Foo<U>::run` could share a group — and
                    // realigning it would hand the whole group's references to one arbitrary owner.
                    //
                    // Deliberately compared RAW. Folding first would let the check pass for members
                    // that are genuinely different entities: the only string-level fold available
                    // here drops generic arguments, so it equates `Foo<u8>::run` with
                    // `Foo<u16>::run`, which coexist. Recovering binder-only equivalence would mean
                    // re-canonicalizing a stored string without its tree, and the renderer is
                    // deliberately AST-driven. So an equivalent-under-renaming group is skipped
                    // rather than adopted — a decline, which this design tolerates, instead of a
                    // mis-adoption, which it does not.
                    let mut canonical = joined.split('\u{1f}').map(str::to_string);
                    let first = canonical.next()?;
                    canonical.all(|path| path == first).then_some(first)
                }),
                language,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut remap: Vec<(i64, i64)> = Vec::new();
    for row in rows {
        // A NULL interned `qualified_name` means the row predates the #224 backfill (rebuild would
        // itself fail on it) or is otherwise unrecoverable — skip rather than hash a wrong key.
        let Some(qualified_name) = row.qualified_name else {
            continue;
        };
        // NULL here means the group's members disagree on their owner scope (or it has none):
        // leave it for the drift relocation rather than prealigning it to one member's identity.
        let Some(scope_path) = row.scope_path else {
            continue;
        };
        let key = LogicalSymbolKey {
            language: row.language,
            path: row.path,
            name: row.name,
            qualified_name,
            scope_path,
            kind: row.kind,
            signature: row.signature,
        };
        let new_id = key.stable_id(&row.repo_id);
        if new_id != row.old_id {
            remap.push((row.old_id, new_id));
        }
    }

    remap_logical_symbol_ids(conn, &remap)?;
    Ok(remap.len())
}

/// Apply an old → new id remap across the PK row and every referencing table, two-phase through
/// negative temp ids: `stable_id` values are all >= 0 (`>> 1`), so a `-(i+1)` temp can never
/// collide with a real id or with another temp. Phase 1 vacates every OLD id in the remap set;
/// phase 2 lands the finals — so a new id equal to another remapped row's old id never trips the
/// PK mid-pass. (A new id equal to an already-aligned row's id is a 63-bit hash collision — the
/// same astronomically-unlikely case a plain rebuild already surfaces loudly.) Shared by
/// [`realign_logical_symbol_ids`] (`repo_id` changes: the PK row moves with its references) and
/// the key-drift heal (#493: the re-derived rows already hold the new ids, so the
/// `logical_symbols`/members updates no-op and the reference tables are the payload).
fn remap_logical_symbol_ids(
    conn: &rusqlite::Connection,
    remap: &[(i64, i64)],
) -> rusqlite::Result<()> {
    for (i, (old_id, _)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_id(conn, *old_id, temp_id)?;
    }
    for (i, (_, new_id)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_id(conn, temp_id, *new_id)?;
    }
    let call_path_remap: Vec<(i64, Option<i64>)> =
        remap.iter().map(|(old_id, new_id)| (*old_id, Some(*new_id))).collect();
    rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids(conn, conn, &call_path_remap)?;
    Ok(())
}

/// Move a single logical-symbol id from `from` to `to` across the PK row and every column that
/// references it — the members join table, per-tool monikers, memory bindings, call-path endpoints,
/// and call-path edge identities. See [`realign_logical_symbol_ids`] for the FK-off/deferred
/// requirement.
fn rewrite_logical_symbol_id(
    conn: &rusqlite::Connection,
    from: i64,
    to: i64,
) -> rusqlite::Result<()> {
    conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![to, from])?;
    conn.execute(
        "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
        params![to, from],
    )?;
    rewrite_logical_symbol_references(conn, from, to, bindings_lens_capable(conn)?)
}

/// Move ONLY the durable references to a logical-symbol id — monikers, memory bindings,
/// call-path endpoints — leaving the `logical_symbols` row and its members untouched. The drift
/// heal (#493) must use this shape: after a key change, a snapshot row's OLD id can be OCCUPIED
/// by a different symbol's re-derived row (a key swap, not a hash collision), and the full
/// [`rewrite_logical_symbol_id`] would MOVE that innocent row along with the drifted reference.
fn rewrite_logical_symbol_references(
    conn: &rusqlite::Connection,
    from: i64,
    to: i64,
    bindings_lens_capable: bool,
) -> rusqlite::Result<()> {
    let lens_repos = binding_lens_repos_for_logical_symbol(conn, from, bindings_lens_capable)?;
    // `logical_symbol_monikers` has no FK, so a DANGLING row (its logical row died in some
    // earlier wholesale rebuild; the next oracle run sweeps it) can already occupy `to` for the
    // same tool — and the plain UPDATE below would abort the whole rebuild on the PK. The moving
    // row is the one bound to the symbol that now lives at `to`, so the stale occupant loses:
    // displace exactly the colliding rows first. The correlate deliberately omits `repo_id`: a
    // logical id is repo-UNIQUE by construction (`stable_id` folds `repo_id`), so same-id +
    // same-tool is the full collision key — and the V040 migration runs this against the
    // pre-V042 moniker shape, which has no `repo_id` column yet.
    conn.execute(
        "DELETE FROM logical_symbol_monikers
          WHERE logical_symbol_id = ?1
            AND EXISTS (SELECT 1 FROM logical_symbol_monikers src
                         WHERE src.logical_symbol_id = ?2
                           AND src.tool = logical_symbol_monikers.tool)",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE logical_symbol_monikers SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE repo_memory_bindings SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET start_logical_symbol_id = ?1
          WHERE start_logical_symbol_id = ?2",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET end_logical_symbol_id = ?1
          WHERE end_logical_symbol_id = ?2",
        params![to, from],
    )?;
    // The two tables below are guarded on existence because this runs DURING migrations:
    // `realign_logical_symbol_ids` is a `MigrationHooks` entry, so an index forward-migrating from
    // an old version reaches here at a schema state that predates them.
    if table_present(conn, "repo_node_edges")? {
        conn.execute(
            "UPDATE repo_node_edges SET target_logical_symbol_id = ?1
              WHERE target_logical_symbol_id = ?2",
            params![to, from],
        )?;
    }
    // Distill anchors store the logical id as the OPAQUE `sym_<hex>` TEXT handle, not an INTEGER,
    // which is exactly why they were missed: every other reference column is an i64, so a remap
    // that updated "the id columns" silently skipped this one (#810). A stale token then points at
    // whatever now occupies that id — surfacing the previous occupant's decision record on an
    // unrelated symbol, the failure mode that matters most here.
    // Capture the anchor repos BEFORE the remap rewrites the handle they match on.
    let anchor_repos = distill_anchor_lens_repos(conn, from)?;
    if table_present(conn, "papertrail_distill_anchors")? {
        conn.execute(
            "UPDATE papertrail_distill_anchors SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![
                rag_rat_base::serde_big_id::format_sym_handle(to),
                rag_rat_base::serde_big_id::format_sym_handle(from)
            ],
        )?;
    }
    bump_binding_lens_revisions(conn, &lens_repos)?;
    // The anchor's `logical_symbol_id`/`resolved` are checkout-local and never author a sync entry;
    // this is a local Lens-freshness bump (the papertrail-lane drive-by changed), replacing the
    // trigger V112 dropped. `bump_papertrail_lens_lanes` advances both enrichment + papertrail and
    // is registration-gated.
    for repo_id in &anchor_repos {
        crate::distill::bump_papertrail_lens_lanes(conn, repo_id)?;
    }
    Ok(())
}

/// Does `table` exist in this connection? Kept rusqlite-native (rather than the `rag_rat_db`
/// helper, which returns `anyhow`) so it composes with the `rusqlite::Result` remap seam.
pub(super) fn table_present(conn: &rusqlite::Connection, table: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn column_present(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for result in columns {
        if result? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the memory-bindings lens-bump tables exist in the shape the bump queries need. These
/// are per-CONNECTION schema facts; the drift-heal/remap passes resolve them ONCE and hand the
/// answer down — a per-call `PRAGMA table_info` inside a thousands-of-symbols regroup is the
/// #1112 instruction regression.
pub(super) fn bindings_lens_capable(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    Ok(column_present(conn, "repo_memory_bindings", "repo_id")?
        && table_present(conn, "repos")?
        && table_present(conn, "repo_meta")?)
}

fn binding_lens_repos_for_logical_symbol(
    conn: &rusqlite::Connection,
    logical_symbol_id: i64,
    capable: bool,
) -> rusqlite::Result<Vec<String>> {
    if !capable {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT b.repo_id
         FROM repo_memory_bindings AS b
         JOIN repos AS r ON r.repo_id = b.repo_id
         WHERE b.logical_symbol_id = ?1",
    )?;
    stmt.query_map([logical_symbol_id], |row| row.get(0))?.collect()
}

fn bump_binding_lens_revisions(
    conn: &rusqlite::Connection,
    repo_ids: &[String],
) -> rusqlite::Result<()> {
    for repo_id in repo_ids {
        rag_rat_db::meta::bump_lens_revisions(conn, repo_id, &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
        ])?;
    }
    Ok(())
}

/// Registered repos that carry a distill anchor for `from`'s `sym_<hex>` handle. Captured BEFORE a
/// relocation rewrites/nulls that handle, so the caller can advance the papertrail Lens lane for
/// the affected repos — `papertrail_distill_anchors` sync (#1139) dropped the row triggers that
/// used to do this. The JOIN to `repos` keeps only registered repos (an ungated
/// `bump_lens_revisions` throws on the `repo_meta`→`repos` FK); the table-presence guard returns
/// empty at migration-time schema states where the anchors/`repos`/`repo_meta` tables do not yet
/// exist.
fn distill_anchor_lens_repos(
    conn: &rusqlite::Connection,
    from: i64,
) -> rusqlite::Result<Vec<String>> {
    if !(table_present(conn, "papertrail_distill_anchors")?
        && table_present(conn, "repos")?
        && table_present(conn, "repo_meta")?)
    {
        return Ok(Vec::new());
    }
    let handle = rag_rat_base::serde_big_id::format_sym_handle(from);
    let mut stmt = conn.prepare(
        "SELECT DISTINCT a.repo_id
         FROM papertrail_distill_anchors AS a
         JOIN repos AS r ON r.repo_id = a.repo_id
         WHERE a.logical_symbol_id = ?1",
    )?;
    stmt.query_map([handle], |row| row.get(0))?.collect()
}

/// NULL every CALL-PATH reference to a drifted id the heal could not realign (#493 review),
/// including a persisted edge callee identity after rebuilding its derived hashes when possible.
/// Call-path references are the exception to the self-healing ladder: `validate_call_path_binding`
/// re-checks only the stored EDGE fingerprints and NEVER consults or repairs the endpoint ids, so
/// a stale (occupied-by-another OR vanished) endpoint would be a permanent bogus `sym_8000…`
/// hydration surfaces and no validator ever fixes — no matter whether the id is sentineled or
/// left on the dead value. NULL is the supported "no recorded endpoint" state every reader
/// already guards for. Both the `repo_memory_call_paths` endpoint columns AND the
/// `repo_memory_bindings` row for `binding_kind = 'call_path'` (whose `logical_symbol_id` is the
/// start-or-end endpoint, equally ignored by the validator) are cleared. Called for EVERY
/// no-winner id — occupied and vanished alike — unlike the sentinel/delete cleanup below which is
/// occupied-only.
pub(super) fn null_call_path_references(
    conn: &rusqlite::Connection,
    from: i64,
    bindings_lens_capable: bool,
) -> rusqlite::Result<()> {
    let lens_repos = binding_lens_repos_for_logical_symbol(conn, from, bindings_lens_capable)?;
    rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids(conn, conn, &[(from, None)])?;
    conn.execute(
        "UPDATE repo_memory_bindings SET logical_symbol_id = NULL
          WHERE logical_symbol_id = ?1 AND binding_kind = 'call_path'",
        params![from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET start_logical_symbol_id = NULL
          WHERE start_logical_symbol_id = ?1",
        params![from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET end_logical_symbol_id = NULL
          WHERE end_logical_symbol_id = ?1",
        params![from],
    )?;
    // Distill anchors and node-edge targets are cleared for EVERY no-winner id, not just occupied
    // ones, because both carry a STATUS alongside the reference. Left alone on a vanished id they
    // keep asserting `resolved = 1` / `anchor_status = 'current'` for a target that resolves to
    // nothing — a claim their readers act on. Memory bindings differ and stay occupied-only: a
    // vanished binding is unresolvable, which is exactly what lets the validate-time relocation
    // ladder find it a new home later.
    if table_present(conn, "repo_node_edges")? {
        conn.execute(
            "UPDATE repo_node_edges SET target_logical_symbol_id = NULL, anchor_status = 'gone'
              WHERE target_logical_symbol_id = ?1",
            params![from],
        )?;
    }
    // Capture the anchor repos BEFORE the vacate nulls the handle they match on.
    let anchor_repos = distill_anchor_lens_repos(conn, from)?;
    if table_present(conn, "papertrail_distill_anchors")? {
        conn.execute(
            "UPDATE papertrail_distill_anchors SET logical_symbol_id = NULL, resolved = 0
              WHERE logical_symbol_id = ?1",
            params![rag_rat_base::serde_big_id::format_sym_handle(from)],
        )?;
    }
    bump_binding_lens_revisions(conn, &lens_repos)?;
    // Local-column relocation → a local Lens-freshness bump for the anchor's repos (see the sibling
    // remap site); never authors a sync entry.
    for repo_id in &anchor_repos {
        crate::distill::bump_papertrail_lens_lanes(conn, repo_id)?;
    }
    Ok(())
}

/// Re-derive the device-local resolution (`logical_symbol_id` / `resolved`) of SELECTED SYMBOL
/// distill anchors for `repo_id` from their portable `(name, file_path)` against the repo's current
/// index at `generation`. A SYMBOL anchor's `logical_symbol_id`/`resolved` are `local_columns` — a
/// peer never receives them — so a replicated anchor lands unresolved and surfaces through NEITHER
/// read path (`records_for_symbol` matches the handle; `records_for_path`'s symbol branch wants
/// `resolved = 1`). This is what lets a synced SYMBOL anchor surface as drive-by on the peer, and
/// it mirrors the owner's mining-time `(name, file_path) → logical_symbol_id` resolution
/// (`distill::candidates::symbols_in_file`).
///
/// It overwrites a handle ONLY when it is NULL or no longer NAMES the anchor's symbol. "Valid" is
/// NAME-bound, not file-bound: a live handle whose symbol has the anchor's `name` (in any file) is
/// kept, so a symbol that merely moved files keeps its resolution (`records_for_symbol` joins on
/// the handle, and the whole #810 relocation posture carries handles across drift), and a genuine
/// same-name overload anchor keeps its own precise handle rather than being collapsed onto the
/// lowest-id sibling. A NULL is filled; a stale handle (a regeneration changed the `name` at a
/// fixed `candidate_ordinal`, leaving the old handle) is re-derived to the lowest-`symbols.id`
/// match.
///
/// The symbol subquery is pinned to `repo_id` + `generation` because this runs on the view-less
/// bare-open path where `files`/`symbols` are the unscoped base tables holding every repo and every
/// (until-gc) generation — the `all_symbols` #89/A3/A6 rule. Resolution is deliberately
/// worktree-INVARIANT (repo + generation, not the active commit/worktree overlay), like
/// `all_symbols`: a logical symbol is the repo-level identity that GROUPS a file's overlays, so
/// overlays of one file share a handle, and the read paths (`records_for_path`'s live-file join) do
/// the per-checkout scoping at query time. The differs-only WHERE (`derived IS NOT stored`) means
/// `conn.changes()` counts only rows whose resolution truly changed, so the registration-gated
/// papertrail Lens-lane bump fires exactly when drive-by output changed.
pub(crate) fn resolve_synced_symbol_anchors(
    conn: &rusqlite::Connection,
    repo_id: &str,
    generation: i64,
) -> rusqlite::Result<()> {
    if !table_present(conn, "papertrail_distill_anchors")? {
        return Ok(());
    }
    // The lowest-`symbols.id` handle for the anchor's `(name, file_path)` in this repo+generation,
    // or NULL. Reused verbatim for the SET, the `resolved` flag, and the differs guard. The
    // tables are `main.`-qualified (base, not the per-connection scope VIEW): the view is
    // already repo-scoped but does NOT project `repo_id`/`generation`, so the explicit pin here
    // must read the base tables — the `all_symbols` #89/A3/A6 rule, and what makes this correct
    // on BOTH the view-installed (incremental/open) and view-less (bare-open) paths.
    const DERIVED: &str = "SELECT 'sym_' || format('%x', m.logical_symbol_id)
         FROM main.symbols s
         JOIN main.files f ON f.id = s.file_id
         JOIN main.logical_symbol_members m ON m.symbol_id = s.id
         WHERE f.repo_id = ?1 AND f.generation = ?2 AND f.kind != 'deleted'
           AND f.path = a.file_path AND s.name = a.name
         ORDER BY s.id LIMIT 1";
    // Name-bound validity: does the STORED handle still name a symbol called `a.name` (any file)?
    const STORED_HANDLE_STILL_NAMES_IT: &str = "SELECT 1
         FROM main.symbols s
         JOIN main.files f ON f.id = s.file_id
         JOIN main.logical_symbol_members m ON m.symbol_id = s.id
         WHERE f.repo_id = ?1 AND f.generation = ?2 AND f.kind != 'deleted' AND s.name = a.name
           AND 'sym_' || format('%x', m.logical_symbol_id) = a.logical_symbol_id";
    let sql = format!(
        "UPDATE papertrail_distill_anchors AS a
         SET logical_symbol_id = ({DERIVED}),
             resolved = CASE WHEN ({DERIVED}) IS NOT NULL THEN 1 ELSE 0 END
         WHERE a.repo_id = ?1 AND a.anchor_kind = 'symbol' AND a.selected = 1
           AND ({DERIVED}) IS NOT a.logical_symbol_id
           AND (a.logical_symbol_id IS NULL OR NOT EXISTS ({STORED_HANDLE_STILL_NAMES_IT}))"
    );
    let changed = conn.execute(&sql, params![repo_id, generation])?;
    if changed > 0 {
        crate::distill::bump_papertrail_lens_lanes(conn, repo_id)?;
    }
    Ok(())
}

/// Move a drifted reference OFF an OCCUPIED id (a LIVE row now belonging to a DIFFERENT symbol)
/// the heal could not realign (#493) — the vanished-id case needs no such move (a dead id already
/// resolves to nothing). Call-path references are handled separately by
/// [`null_call_path_references`]; this covers the two kinds that DO self-heal on an unresolvable
/// id, so they must be pushed off the live-wrong id:
/// - `repo_memory_bindings.logical_symbol_id` (every kind EXCEPT `call_path`) parks on
///   [`VACATED_LOGICAL_SYMBOL_ID`]: an unresolvable id is exactly what makes the validate-time
///   relocation ladder run, so the binding self-heals with a visible papertrail — where leaving it
///   on the occupied LIVE id would validate as healthy and strand it silently.
/// - `logical_symbol_monikers` rows are DELETED: their PK is `(repo_id, logical_symbol_id, tool)`,
///   so two vacated ids carrying the same tool's moniker would collide on a shared sentinel and
///   abort the rebuild — and a moniker pointing at a dead symbol is worthless anyway
///   (oracle-derived, re-derived by the next `oracle run`).
pub(super) fn vacate_logical_symbol_references(
    conn: &rusqlite::Connection,
    from: i64,
    bindings_lens_capable: bool,
) -> rusqlite::Result<()> {
    let lens_repos = binding_lens_repos_for_logical_symbol(conn, from, bindings_lens_capable)?;
    conn.execute("DELETE FROM logical_symbol_monikers WHERE logical_symbol_id = ?1", params![
        from
    ])?;
    conn.execute(
        "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
          WHERE logical_symbol_id = ?2 AND binding_kind != 'call_path'",
        params![VACATED_LOGICAL_SYMBOL_ID, from],
    )?;
    bump_binding_lens_revisions(conn, &lens_repos)?;
    Ok(())
}

/// The reference-only two-phase remap the drift heal applies (#493): same negative-temp-id
/// discipline as [`remap_logical_symbol_ids`] — with occupied-id swaps in the set, a pair's
/// SOURCE can equal another pair's TARGET, so the direct rewrite would collide mid-pass — but
/// touching only the reference tables (see [`rewrite_logical_symbol_references`]).
pub(super) fn remap_logical_symbol_references(
    conn: &rusqlite::Connection,
    remap: &[(i64, i64)],
) -> rusqlite::Result<()> {
    let capable = bindings_lens_capable(conn)?;
    for (i, (old_id, _)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_references(conn, *old_id, temp_id, capable)?;
    }
    for (i, (_, new_id)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_references(conn, temp_id, *new_id, capable)?;
    }
    let call_path_remap: Vec<(i64, Option<i64>)> =
        remap.iter().map(|(old_id, new_id)| (*old_id, Some(*new_id))).collect();
    rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids(conn, conn, &call_path_remap)?;
    Ok(())
}

/// Where the drift heal parks a reference it had to move OFF an occupied id without an evidence
/// winner (#493): resolves to nothing by construction (`stable_id` is >= 0 and this is far below
/// the small negative `-(i+1)` temp range the two-phase remaps use, so no phase of any pass —
/// this heal's or a later one's — can ever capture a vacated reference). A vacated binding walks
/// the validate-time relocation ladder on the next pass, exactly like a vanished id.
const VACATED_LOGICAL_SYMBOL_ID: i64 = i64::MIN;

#[cfg(test)]
mod relocation_lens_bump_tests {
    use rag_rat_base::serde_big_id::format_sym_handle;
    use rusqlite::{Connection, params};

    use super::{null_call_path_references, rewrite_logical_symbol_references};

    fn papertrail_rev(conn: &Connection, repo_id: &str) -> i64 {
        rag_rat_db::meta::repo_meta(conn, repo_id, rag_rat_db::meta::LENS_PAPERTRAIL_REVISION_META)
            .unwrap()
            .map(|value| value.parse().unwrap())
            .unwrap_or(0)
    }

    fn scratch_with_anchor(handle_id: i64) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, candidate_ordinal, anchor_kind,
                  logical_symbol_id, file_path, name, resolved, selected, repo_id)
             VALUES ('github', 'o/r', 'issue', '5', 0, 'symbol', ?1, 'src/x.rs', 'Foo', 1, 1, 'r')",
            params![format_sym_handle(handle_id)],
        )
        .unwrap();
        conn
    }

    /// A logical-id remap carries the anchor's opaque handle to the new id AND advances the
    /// papertrail Lens lane for the anchor's repo — the row triggers that used to do this were
    /// dropped when the table became syncable, so the relocation code now owns the bump.
    #[test]
    fn a_remap_bumps_the_papertrail_lane_for_the_anchor_repo() {
        let conn = scratch_with_anchor(100);
        let before = papertrail_rev(&conn, "r");
        rewrite_logical_symbol_references(&conn, 100, 200, true).unwrap();
        let token: String = conn
            .query_row(
                "SELECT logical_symbol_id FROM papertrail_distill_anchors WHERE item_key = '5'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(token, format_sym_handle(200), "the handle follows the symbol");
        assert!(papertrail_rev(&conn, "r") > before, "the papertrail lane advances");
    }

    /// A no-winner vacate clears the anchor's local resolution and likewise advances the lane.
    #[test]
    fn a_vacate_bumps_the_papertrail_lane_for_the_anchor_repo() {
        let conn = scratch_with_anchor(100);
        let before = papertrail_rev(&conn, "r");
        null_call_path_references(&conn, 100, true).unwrap();
        let (token, resolved): (Option<String>, i64) = conn
            .query_row(
                "SELECT logical_symbol_id, resolved FROM papertrail_distill_anchors
                 WHERE item_key = '5'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((token, resolved), (None, 0), "the local resolution is cleared");
        assert!(papertrail_rev(&conn, "r") > before, "the papertrail lane advances");
    }
}

#[cfg(test)]
mod synced_anchor_resolution_tests {
    use rag_rat_base::serde_big_id::format_sym_handle;
    use rusqlite::{Connection, params};

    use super::resolve_synced_symbol_anchors;

    fn papertrail_rev(conn: &Connection, repo_id: &str) -> i64 {
        rag_rat_db::meta::repo_meta(conn, repo_id, rag_rat_db::meta::LENS_PAPERTRAIL_REVISION_META)
            .unwrap()
            .map(|value| value.parse().unwrap())
            .unwrap_or(0)
    }

    fn scratch() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        // Hand-seed the symbol index directly (FK off), the raw-`main`-table fixture Fable's review
        // requires: a scope view would mask a consolidated-repo / superseded-generation bug.
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        for repo in ["r", "s"] {
            conn.execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
                [repo],
            )
            .unwrap();
        }
        conn
    }

    fn add_file(conn: &Connection, id: i64, path: &str, repo: &str, generation: i64) {
        conn.execute(
            "INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               repo_id, generation)
             VALUES (?1, ?2, 'rust', 'source', 'sha', 0, 0, ?3, ?4)",
            params![id, path, repo, generation],
        )
        .unwrap();
    }

    fn add_symbol(conn: &Connection, sym_id: i64, file_id: i64, name: &str, logical: i64) {
        conn.execute(
            "INSERT INTO symbols(id, file_id, language, name, kind, start_byte, end_byte)
             VALUES (?1, ?2, 'rust', ?3, 'function', 0, 0)",
            params![sym_id, file_id, name],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, \
             end_line)
             VALUES (?1, ?2, 0, 0)",
            params![logical, sym_id],
        )
        .unwrap();
    }

    /// A selected symbol anchor; `handle` is its stored `logical_symbol_id` (None =
    /// synced/unresolved).
    fn add_anchor(conn: &Connection, item_key: &str, repo: &str, name: &str, handle: Option<i64>) {
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, candidate_ordinal, anchor_kind,
                  logical_symbol_id, file_path, name, resolved, selected, repo_id)
             VALUES ('github', 'o/r', 'issue', ?1, 0, 'symbol', ?2, 'src/x.rs', ?3,
                     ?4, 1, ?5)",
            params![item_key, handle.map(format_sym_handle), name, handle.is_some() as i64, repo],
        )
        .unwrap();
    }

    fn anchor(conn: &Connection, item_key: &str) -> (Option<String>, i64) {
        conn.query_row(
            "SELECT logical_symbol_id, resolved FROM papertrail_distill_anchors
             WHERE item_key = ?1 AND repo_id = 'r'",
            [item_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn resolves_a_null_synced_anchor_and_bumps_the_lane() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        add_anchor(&conn, "5", "r", "Foo", None);
        let before = papertrail_rev(&conn, "r");
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(anchor(&conn, "5"), (Some(format_sym_handle(100)), 1));
        assert!(papertrail_rev(&conn, "r") > before, "a fresh resolution bumps the lane");
    }

    #[test]
    fn a_second_run_changes_nothing_and_does_not_bump() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        add_anchor(&conn, "5", "r", "Foo", None);
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        let settled = papertrail_rev(&conn, "r");
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(anchor(&conn, "5"), (Some(format_sym_handle(100)), 1));
        assert_eq!(papertrail_rev(&conn, "r"), settled, "an idempotent re-run must not bump");
    }

    #[test]
    fn a_valid_overload_handle_is_preserved() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        // Two same-name symbols, distinct handles — the owner mined each its own precise anchor.
        add_symbol(&conn, 30, 1, "Dup", 300);
        add_symbol(&conn, 31, 1, "Dup", 301);
        add_anchor(&conn, "5", "r", "Dup", Some(300));
        add_anchor(&conn, "6", "r", "Dup", Some(301));
        let before = papertrail_rev(&conn, "r");
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        // The second anchor keeps 301 rather than collapsing onto the lowest-id 300.
        assert_eq!(anchor(&conn, "5"), (Some(format_sym_handle(300)), 1));
        assert_eq!(anchor(&conn, "6"), (Some(format_sym_handle(301)), 1));
        assert_eq!(papertrail_rev(&conn, "r"), before, "preserving valid handles does not bump");
    }

    #[test]
    fn a_stale_handle_whose_name_moved_on_is_re_derived() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        add_symbol(&conn, 20, 1, "Bar", 200);
        // A regeneration made candidate_ordinal 0 name "Bar", but the synced handle still names
        // Foo.
        add_anchor(&conn, "5", "r", "Bar", Some(100));
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(anchor(&conn, "5"), (Some(format_sym_handle(200)), 1), "re-derived to Bar");
    }

    #[test]
    fn a_live_handle_whose_symbol_moved_files_is_kept() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_file(&conn, 2, "src/y.rs", "r", 0);
        // The symbol now lives in y.rs, but the anchor's file_path still says x.rs. Validity is
        // name-bound, so the live handle is kept (records_for_symbol joins on the handle).
        add_symbol(&conn, 40, 2, "Moved", 400);
        add_anchor(&conn, "5", "r", "Moved", Some(400));
        let before = papertrail_rev(&conn, "r");
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(anchor(&conn, "5"), (Some(format_sym_handle(400)), 1), "the live handle stays");
        assert_eq!(papertrail_rev(&conn, "r"), before);
    }

    #[test]
    fn a_null_anchor_with_no_local_match_stays_unresolved_and_does_not_bump() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        add_anchor(&conn, "5", "r", "Ghost", None);
        let before = papertrail_rev(&conn, "r");
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(anchor(&conn, "5"), (None, 0), "no local symbol → still unresolved");
        assert_eq!(papertrail_rev(&conn, "r"), before, "a no-op must not bump");
    }

    #[test]
    fn file_and_unselected_anchors_are_left_alone() {
        let conn = scratch();
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        // A FILE anchor (surfaces via the synced file_path) and an UNSELECTED symbol candidate.
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, candidate_ordinal, anchor_kind,
                  logical_symbol_id, file_path, name, resolved, selected, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 0, 'file', NULL, 'src/x.rs', 'src/x.rs', 1, 1, \
             'r'),
                    ('github', 'o/r', 'issue', '8', 1, 'symbol', NULL, 'src/x.rs', 'Foo', 0, 0, \
             'r')",
            [],
        )
        .unwrap();
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(anchor(&conn, "7"), (None, 1), "the file anchor is untouched");
        assert_eq!(anchor(&conn, "8"), (None, 0), "the unselected candidate is untouched");
    }

    #[test]
    fn a_shared_path_resolves_only_against_the_active_repo() {
        let conn = scratch();
        // Both repos have a `src/x.rs` with a `Foo`; the pass must pick repo r's symbol.
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        add_file(&conn, 2, "src/x.rs", "s", 0);
        add_symbol(&conn, 20, 2, "Foo", 999);
        add_anchor(&conn, "5", "r", "Foo", None);
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(
            anchor(&conn, "5"),
            (Some(format_sym_handle(100)), 1),
            "resolves against repo r, never the sibling repo's same-path symbol"
        );
    }

    #[test]
    fn overlapping_worktree_overlays_resolve_to_the_shared_logical_id() {
        let conn = scratch();
        // Two worktree overlays of the SAME file (same repo + generation, distinct worktree_id) —
        // as a linked checkout produces. Both hold a `Foo` folded into ONE logical symbol (logical
        // grouping is cross-overlay), so resolution is worktree-invariant: it yields the shared
        // handle whichever overlay's symbol row wins the lowest-id tiebreak.
        for (file_id, sym_id, worktree) in [(1, 10, "wt-a"), (2, 11, "wt-b")] {
            conn.execute(
                "INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, \
                 indexed_at_ms,
                                   repo_id, generation, worktree_id)
                 VALUES (?1, 'src/x.rs', 'rust', 'source', 'sha', 0, 0, 'r', 0, ?2)",
                params![file_id, worktree],
            )
            .unwrap();
            add_symbol(&conn, sym_id, file_id, "Foo", 100);
        }
        add_anchor(&conn, "5", "r", "Foo", None);
        resolve_synced_symbol_anchors(&conn, "r", 0).unwrap();
        assert_eq!(
            anchor(&conn, "5"),
            (Some(format_sym_handle(100)), 1),
            "overlays of one file share a logical id, so resolution is worktree-invariant"
        );
    }

    #[test]
    fn a_superseded_generation_symbol_is_excluded() {
        let conn = scratch();
        // A dead generation-0 `Foo` and a live generation-1 `Foo`, both at src/x.rs in repo r.
        add_file(&conn, 1, "src/x.rs", "r", 0);
        add_symbol(&conn, 10, 1, "Foo", 100);
        add_file(&conn, 2, "src/x.rs", "r", 1);
        add_symbol(&conn, 20, 2, "Foo", 500);
        add_anchor(&conn, "5", "r", "Foo", None);
        resolve_synced_symbol_anchors(&conn, "r", 1).unwrap();
        assert_eq!(
            anchor(&conn, "5"),
            (Some(format_sym_handle(500)), 1),
            "resolves against the live generation, never a superseded row"
        );
    }
}
