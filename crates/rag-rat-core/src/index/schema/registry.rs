use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::repo_identity::{LOCAL_ONLY_ID_PREFIX, RepoIdentity, RepoIdentityClass};

/// The `temp.connection_context` key under which the scope view stashes the active repo id (beside
/// `commit_sha` / `worktree_id`). [`active_repo_id`] reads it; `install_scope_view` writes it.
pub(crate) const CONNECTION_CONTEXT_REPO_KEY: &str = "repo_id";

/// The `temp.connection_context` key under which the scope view stashes the active file GENERATION
/// (A6) — the value the `files` view filters on. `write_scope_view` writes it (the LIVE generation
/// for a reader/incremental open, the WRITE generation for the connection driving a full rebuild);
/// [`active_generation`] reads it back for the view-less heal paths.
pub(crate) const CONNECTION_CONTEXT_GENERATION_KEY: &str = "files_generation";

/// The `repo_meta` key holding a repo's LIVE `files.generation` — the pointer a full rebuild flips
/// once its freshly-staged generation is complete (A6). Absent ⇒ 0 (a fresh index, and every row a
/// pre-V043 upgrade carries), so an un-restaged index needs no `repo_meta` write to be visible.
pub(crate) const LIVE_FILES_GENERATION_META_KEY: &str = "live_files_generation";

/// The direct-scoped tables whose [`LEGACY_REPO_ID`] placeholder rows [`register_repo`] re-points
/// at the real id when it adopts a legacy DB. V040 (phase A3) added the core tables; V041 (phase
/// A4) added the seven GitHub papertrail tables plus the derived `github_fts` mirror (own-content
/// FTS5, so its `repo_id UNINDEXED` value updates in place — re-pointing here keeps papertrail
/// scoped to the real id without waiting for the next sync's `rebuild_fts`). `git_file_changes` is
/// intentionally ABSENT: its `(repo_id, commit_hash)` FK to `git_commits(repo_id, hash)` is `ON
/// UPDATE CASCADE`, so rewriting `git_commits.repo_id` re-points its rows automatically — an
/// explicit UPDATE would instead trip the FK (the child would reference the real id before the
/// parent moved). `repo_meta` is handled separately (its FK is to `repos`, and it moved in V039).
/// The A1 adoption contract requires every direct-scoped table backfill in the same call as the
/// `repos`-row rewrite.
const DIRECT_SCOPED_ADOPTION_TABLES: &[&str] = &[
    // V040 (phase A3) core tables.
    "files",
    "packages",
    "logical_symbols",
    "docs",
    "parser_failures",
    "git_commits",
    // V041 (phase A4) GitHub papertrail tables + the derived FTS mirror.
    "github_refs",
    "github_issues",
    "github_comments",
    "github_pull_requests",
    "github_reviews",
    "github_review_comments",
    "github_ref_sync",
    "github_fts",
];

/// The periphery tables that gain a `repo_id` column in the phase-A5 periphery-scoping migration
/// (V042, `apply_repo_id_periphery_scoping`) and therefore also need their [`LEGACY_REPO_ID`]
/// placeholder rows re-pointed at the real id when [`register_repo`] adopts a legacy DB — the A5
/// continuation of the A1/A3 adoption contract. `repo_memory_fts` is the standalone FTS mirror of
/// `repo_memories` (its `repo_id` snapshots the parent's at rebuild time), re-pointed alongside.
///
/// COLUMN GUARD: several of these tables predate V042 — they exist at earlier schema versions
/// WITHOUT a `repo_id` column, which V042 adds or rebuilds in. `register_repo` runs on every open,
/// including against partial-schema bootstrap fixtures that stop before V042, so each re-point is
/// guarded by `column_exists`: a no-op when the column is absent (a table present without its
/// `repo_id` column), the real backfill once V042 has run (every normal open applies the full
/// ladder). The `DIRECT_SCOPED_ADOPTION_TABLES` loop above guards on table-presence instead; a
/// periphery table can exist without the column, so it needs the stronger column-level guard.
const A5_PERIPHERY_DIRECT_SCOPED_TABLES: &[&str] = &[
    "clone_graph_generations",
    "clone_token_df",
    "clone_refinements",
    "oracle_runs",
    "edge_oracle",
    "logical_symbol_monikers",
    "reconcile_attempts",
    "dream_findings",
    "repo_memories",
    "repo_memory_bindings",
    "repo_memory_fts",
];

/// The placeholder `repo_id` a freshly-migrated single-repo DB carries until it is adopted (see the
/// V038 DDL) — and the backfill value every direct-scoped table gets when later migrations add
/// `repo_id` columns (phase A3). A consolidated DB holding more than one repo NEVER carries this id
/// (enforced by [`register_repo`]).
pub const LEGACY_REPO_ID: &str = "__unassigned__";

/// The `repo_meta` key a [`LocalOnly`](crate::repo_identity::RepoIdentityClass::LocalOnly)
/// registration records its SORTED shallow-boundary commit hashes under (newline-joined), so a
/// later LocalOnly→Portable upgrade can PROVE the incoming deepened clone is the same repository —
/// its HEAD must reach these boundary commits. Absent ⇒ no proof recorded ⇒ the upgrade is refused.
const SHALLOW_BOUNDARY_META_KEY: &str = "shallow_boundary";

/// How long a LocalOnly→Portable upgrade waits for a writer still holding the OUTGOING `local:`
/// discriminator's write lock (A6, batch-4 P2) before refusing. Generous — an in-flight
/// index/maintenance pass finishes its lock holds well within it; a timeout surfaces an explicit
/// refusal, never a silent re-point under a live writer.
const UPGRADE_OUTGOING_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Register `identity` in this database's `repos`/`repo_roots` registry, returning the stored
/// `repo_id`.
///
/// Idempotent and safe to call on every open:
/// - **Fresh legacy DB** (only the [`LEGACY_REPO_ID`] placeholder row present) → *adoption*: the
///   placeholder row is rewritten to `identity.repo_id` in place (A1 rewrites only `repos`; phase
///   A3 extends adoption to every direct-scoped table once they gain `repo_id`). The working-tree
///   `root` is recorded in `repo_roots`.
/// - **Already this repo** → a no-op except that a not-yet-seen `root` is appended to `repo_roots`
///   (worktrees of one repo share a `repo_id`, so several roots on one machine are expected).
/// - **A `local:` incumbent + an incoming `Portable` id** → *upgrade in place*: the DB was first
///   indexed under a machine-local shallow-clone id, and the caller has since deepened it (`git
///   fetch --unshallow`, our own remedy) or pinned a stable id, so the incoming identity is now
///   portable. Every scoped row, `repo_meta`, `repo_roots`, and logical-symbol id is re-pointed
///   from the local id to the portable one (the same adoption machinery), rather than stranding the
///   existing index behind a refusal.
/// - **Any other different real repo_id already owns the DB** → refuses (returns an error) rather
///   than corrupting a single-repo DB: two genuinely-different portable repos, or a `LocalOnly`
///   incoming against a real incumbent (a deepened clone must never DOWNGRADE a portable id back to
///   machine-local). Multi-repo registration is deliberately out of scope until the default-path
///   flip (phase A7); until then one DB == one repo.
///
/// `now_ms` is the injected clock (see the repo's time-injection convention), stamped as the
/// registration time on adoption / first insert.
pub fn register_repo(
    conn: &Connection,
    identity: &RepoIdentity,
    root: &Path,
    now_ms: i64,
) -> rusqlite::Result<String> {
    // Defense in depth: the resolver already refuses a pinned placeholder and never derives it, but
    // a caller can hand-build a `RepoIdentity`. Registering the placeholder (or an empty/whitespace
    // id) would degenerate adoption — `real_repo_ids` filters the marker, so the adoption UPDATE
    // would rewrite the placeholder PK to itself, roots would pool under the marker, and this would
    // return success while the DB stayed unadopted (a later real registration then trips the child
    // FK or orphans roots under the marker).
    let trimmed_id = identity.repo_id.trim();
    if trimmed_id.is_empty() || trimmed_id == LEGACY_REPO_ID {
        return Err(registry_refusal(format!(
            "refusing to register the reserved or empty repo_id {:?}: `{LEGACY_REPO_ID}` is the \
             pre-adoption placeholder and an empty id cannot scope rows",
            identity.repo_id
        )));
    }

    let root_str = root.to_string_lossy();
    let real_ids = real_repo_ids(conn)?;

    // Already registered under this id → idempotent; just make sure the root is recorded (and, for
    // a LocalOnly re-registration of the same clone, that its shallow boundary is on record so
    // a later upgrade can prove against it — self-healing for an index first registered before
    // this gate).
    if real_ids.iter().any(|id| id == &identity.repo_id) {
        record_repo_root(conn, &identity.repo_id, &root_str, now_ms)?;
        persist_shallow_boundary(conn, identity)?;
        return Ok(identity.repo_id.clone());
    }

    // A different real repo already owns this DB. The single-repo invariant refuses a second repo —
    // with ONE exception: a shallow-clone UPGRADE. The DB was first indexed under a machine-local
    // `local:` id (a cut shallow clone), and the caller has since deepened it (`git fetch
    // --unshallow`, our own remedy) or pinned a stable id, so the incoming identity is now
    // `Portable`. Re-point the DB from the local id onto the portable one in place (below) instead
    // of stranding the existing index behind a refusal. Every OTHER mismatch still refuses:
    // Portable↔different-Portable (two genuinely different repos), and a `LocalOnly` incoming
    // against any existing real repo (a deepened clone must never DOWNGRADE a portable id back to
    // machine-local). Guarded on exactly one real repo — a consolidated DB (post-A7) is never
    // upgraded through this path.
    let upgrade_from = match real_ids.as_slice() {
        [] => None,
        [incumbent]
            if incumbent.starts_with(LOCAL_ONLY_ID_PREFIX)
                && identity.class == RepoIdentityClass::Portable =>
        {
            // The `local:` prefix + Portable incoming is NECESSARY but not SUFFICIENT: a DB first
            // indexed from a cut shallow clone, later opened from an UNRELATED full repo at the
            // same database path (or with a mistaken pin), would otherwise silently
            // re-point every scoped row, repo_meta, root, and logical id onto that
            // unrelated id — data loss across two repos. Require PROOF the incoming
            // clone is a DEEPENED version of THIS incumbent: the incumbent's recorded
            // shallow-boundary commits must be reachable from the incoming clone's HEAD
            // (a `git fetch --unshallow` keeps those commits in history; a different
            // repo reaches none of them). No recorded boundary (a pre-gate registration) counts as
            // NO PROOF. Verified BEFORE the adoption transaction so a refusal touches nothing.
            let boundary = read_shallow_boundary(conn, incumbent)?;
            let proven = !boundary.is_empty()
                && crate::repo_identity::boundary_reachable_from_head(root, &boundary)
                    .unwrap_or(false);
            if !proven {
                return Err(registry_refusal(unproven_upgrade_error(incumbent, &identity.repo_id)));
            }
            Some(incumbent.clone())
        },
        _ =>
            return Err(registry_refusal(format!(
                "cannot register repo {}: this index is already registered to a different repo {}",
                identity.repo_id, real_ids[0]
            ))),
    };

    // UPGRADE HOLDS BOTH REPO LOCKS (A6, batch-4/5 P2). Writers key their per-repo advisory flock
    // by the DERIVED repo id, and the derivation flips `local:` → portable the moment the clone
    // is deepened — so around an upgrade the lock IDENTITY is unstable: a writer that started
    // PRE-unshallow holds the OUTGOING `local:` lock, a post-unshallow writer holds the INCOMING
    // portable one, and the re-point below must serialize with BOTH. Acquire the two ids' locks
    // in the CANONICAL LEXICOGRAPHIC ORDER (`locks::canonical_lock_order`; the locks module doc
    // owns the ordering rule, which SUPERSEDES the old role-based "incoming-then-outgoing"
    // argument — that argument broke as soon as a second multi-lock path appeared with the roles
    // reversed, the batch-5 fence-gap writer). Each acquisition is reentrant-instant when this
    // thread already holds it (a CLI entry lock for either id) and BOUNDED otherwise — bounded
    // because entry locks are taken identity-blind, so a pre-held later-sorting lock can force an
    // out-of-order edge, and bounded out-of-order edges are what keep that topology deadlock-free
    // (a timeout surfaces a retryable refusal instead of a hang; see the locks module doc).
    // ROOT-PATH-KEYED LOCKS REJECTED as the alternative: two clones of the SAME repo on one
    // machine must still serialize on repo_id, which path-keying would break. A pathless
    // (in-memory) connection skips the locks — no cross-process writer can exist for it.
    let _upgrade_locks = match (&upgrade_from, conn.path().filter(|p| !p.is_empty())) {
        (Some(local_id), Some(db_path)) => {
            let db_path = Path::new(db_path);
            let (first, second) =
                crate::locks::canonical_lock_order(local_id.as_str(), &identity.repo_id);
            let mut guards = Vec::with_capacity(2);
            for repo in [first, second] {
                guards.push(
                    crate::locks::WriteLock::acquire_timeout(
                        db_path,
                        repo,
                        UPGRADE_OUTGOING_LOCK_TIMEOUT,
                    )
                    .map_err(|err| {
                        registry_refusal(format!(
                            "cannot upgrade {local_id}: failed acquiring the write lock for \
                             {repo}: {err}"
                        ))
                    })?
                    .ok_or_else(|| {
                        registry_refusal(format!(
                            "cannot upgrade {local_id}: timed out waiting for an in-flight writer \
                             holding the write lock for {repo}"
                        ))
                    })?,
                );
            }
            Some(guards)
        },
        _ => None,
    };

    // No real repo yet (or a `local:`-id upgrade): adopt in ONE transaction. Insert the real
    // `repos` row FIRST, re-point the source id's children, then drop the source row. Since
    // V039 (phase A2) `repo_meta` carries rows under the placeholder, and its FK to `repos` is
    // enforced (`foreign_keys = ON`), the source PK cannot be rewritten in place while children
    // reference it; the insert-first / delete-last order keeps every FK satisfied at each
    // statement boundary. The enclosing `unchecked_transaction` makes the whole adoption
    // ATOMIC: a crash or mid-sequence error rolls it ALL back, so the DB never lands in the
    // torn state where BOTH the real row and the source survive — a state the "already
    // registered" fast path above would never repair and `single_repo_id`'s one-row expectation
    // would break on. `unchecked_transaction` is the right tool on a shared `&Connection`
    // (rusqlite): it needs no `&mut`, commits on `.commit()`, and rolls back on drop. The fresh
    // path (no placeholder, no upgrade) runs in the same transaction for uniformity.
    let tx = conn.unchecked_transaction()?;
    let placeholder_present = tx
        .query_row("SELECT 1 FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID], |_| Ok(()))
        .optional()?
        .is_some();
    tx.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?2, ?3)",
        params![identity.repo_id, identity.display_name, now_ms],
    )?;
    // The id whose rows this registration re-points ONTO the new id: the machine-local incumbent on
    // a shallow-clone upgrade, else the `__unassigned__` placeholder on a fresh adoption. Both
    // cases re-point the same scoped tables + realign the same logical ids and then drop the
    // old `repos` row; only the upgrade path actually has `repo_roots` rows to move (the
    // placeholder never does, so that UPDATE is a harmless no-op there).
    let repoint_from =
        upgrade_from.as_deref().or_else(|| placeholder_present.then_some(LEGACY_REPO_ID));
    if let Some(source_id) = repoint_from {
        // Move every source-id `repo_meta` row onto the new id EXCEPT the shallow-boundary record:
        // it describes the OLD `local:` clone's cut history and is meaningless under the portable
        // id (which has a full history and never upgrades). Leaving it under `source_id`
        // lets the `DELETE FROM repos WHERE repo_id = source_id` below cascade it away, so
        // the portable id never inherits a stale boundary. Harmless no-op on the
        // placeholder path (no such row).
        tx.execute("UPDATE repo_meta SET repo_id = ?1 WHERE repo_id = ?2 AND key != ?3", params![
            identity.repo_id,
            source_id,
            SHALLOW_BOUNDARY_META_KEY,
        ])?;
        // Re-point every direct-scoped table's source rows onto the real id (A3/A4 extend the
        // A1/A2 adoption contract from `repos`/`repo_meta` to the V040 core tables and the V041
        // GitHub papertrail tables). Runs INSIDE the same adoption transaction, keeping the
        // insert-first ordering: on a fresh open the tables are empty and these are no-ops; on a
        // forward-migrated DB that indexed under the placeholder (or a shallow-clone upgrade) they
        // carry the rows onto the real id atomically with the `repos` rewrite. `git_commits` is
        // updated here; `git_file_changes` follows via its `ON UPDATE CASCADE` FK.
        for table in DIRECT_SCOPED_ADOPTION_TABLES {
            // Guard on table presence: a real consolidated DB is fully migrated (every
            // direct-scoped table exists), but the schema-bootstrap tests exercise
            // adoption against ISOLATION fixtures that seed only the subset a given
            // migration touches (V040 core tables OR V041 github tables). Skipping an
            // absent table keeps adoption correct on the full schema while staying
            // robust to those partial fixtures — a table that does not exist has no
            // source rows to re-point.
            if !adoption_table_present(&tx, table)? {
                continue;
            }
            tx.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), params![
                identity.repo_id,
                source_id
            ])?;
        }
        // A5 periphery tables (clones/oracle/reconcile/memories). Their `repo_id` column lands in
        // V042; several of these tables predate it, so a partial-schema bootstrap fixture that
        // stops before V042 can have the table without the column. Guard each re-point with
        // `column_exists` (no-op when the column is absent, real backfill once V042 has run — every
        // normal open applies the full ladder) so this adoption never trips "no such column". Uses
        // `source_id` (not the placeholder literal) so a shallow-clone upgrade re-points periphery
        // rows off the `local:` incumbent too, exactly like the core/github loop above.
        for table in A5_PERIPHERY_DIRECT_SCOPED_TABLES {
            if super::column_exists(&tx, table, "repo_id")? {
                tx.execute(
                    &format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"),
                    params![identity.repo_id, source_id],
                )?;
            }
        }
        // `dream_findings.id` folds `repo_id` (the `logical_symbols.stable_id` precedent), so the
        // periphery re-point above changed the id every finding SHOULD have — re-derive them (and
        // the in-table `superseded_by` references) under the adopted id: the dream twin of the
        // `realign_logical_symbol_ids` call below. Guarded like the loop above (a partial-schema
        // bootstrap fixture can lack the table or the V042 column); idempotent.
        if super::column_exists(&tx, "dream_findings", "repo_id")? {
            crate::dream::rederive_finding_ids(&tx)?;
        }
        // Move the source id's recorded roots onto the new id BEFORE the source `repos` row is
        // deleted (its FK is `ON DELETE CASCADE`, so the roots would otherwise be dropped). A
        // shallow-clone upgrade carries the local id's roots; the placeholder never has any.
        tx.execute("UPDATE repo_roots SET repo_id = ?1 WHERE repo_id = ?2", params![
            identity.repo_id,
            source_id
        ])?;
        // `logical_symbols.id` is content-derived and now folds `repo_id` (A3), so re-pointing its
        // `repo_id` above changed the id every row SHOULD have — the next `rebuild_logical_symbols`
        // will re-derive `hash(real_repo_id ‖ key)` and dangle every pre-re-point memory/oracle
        // handle still pointing at the source-derived id. Realign the ids (and every reference) in
        // place NOW, before that rebuild. `logical_symbol_members` has an `ON DELETE CASCADE` FK to
        // `logical_symbols(id)`, and adoption runs with `foreign_keys = ON`, so defer FK checks to
        // COMMIT for this transaction (auto-resets on commit/rollback) — otherwise the parent-id
        // UPDATE trips the child FK mid-statement. Idempotent with the V040 migration's own
        // realign.
        tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        crate::index::graph_index::realign_logical_symbol_ids(&tx)?;
        tx.execute("DELETE FROM repos WHERE repo_id = ?1", [source_id])?;
    }
    record_repo_root(&tx, &identity.repo_id, &root_str, now_ms)?;
    // Record a fresh LocalOnly adoption's shallow boundary INSIDE the transaction (atomic with the
    // `repos` row it references), so a future deepened clone can prove the upgrade against it. A
    // no-op for a Portable identity (empty boundary) and for the upgrade path itself (Portable
    // incoming).
    persist_shallow_boundary(&tx, identity)?;
    tx.commit()?;
    // State what happened on a shallow-clone upgrade (a `local:` id re-pointed to a portable one)
    // so the transition is legible in the log — it re-writes every scoped row and every logical
    // id.
    if let Some(local_id) = upgrade_from {
        tracing::warn!(
            old_repo_id = %local_id,
            new_repo_id = %identity.repo_id,
            "shallow-clone identity upgraded: the index was registered under a machine-local id \
             (`local:`) and is now re-pointed to a portable id. All scoped rows, repo_meta, \
             repo_roots, and logical-symbol ids were migrated in place."
        );
    }
    Ok(identity.repo_id.clone())
}

/// Whether `table` exists in the schema (a plain or FTS5-virtual table both register in
/// `sqlite_master` as `type = 'table'`) — the adoption loop's guard so a partial isolation
/// fixture's absent direct-scoped table is skipped rather than tripping a `no such table` failure.
fn adoption_table_present(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A `SQLITE_CONSTRAINT` failure carrying `msg` — the registry's refusal shape. These are
/// single-repo-invariant violations (a reserved/empty id, or a different repo already owning the
/// DB), not real SQL constraint trips; the shape keeps them on `rusqlite::Result` without a bespoke
/// error type for the phase-A registry.
fn registry_refusal(msg: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(msg),
    )
}

/// Persist a [`LocalOnly`](RepoIdentityClass::LocalOnly) identity's sorted shallow-boundary hashes
/// under its repo id, so a later LocalOnly→Portable upgrade can verify the incoming clone deepens
/// THIS clone. Idempotent (upsert). A no-op for a Portable identity (empty boundary — nothing to
/// prove against). Takes a bare `&Connection` so it composes with both the free connection (the
/// idempotent re-registration path) and the adoption transaction.
fn persist_shallow_boundary(conn: &Connection, identity: &RepoIdentity) -> rusqlite::Result<()> {
    if identity.class != RepoIdentityClass::LocalOnly || identity.shallow_boundary.is_empty() {
        return Ok(());
    }
    crate::index::set_repo_meta(
        conn,
        &identity.repo_id,
        SHALLOW_BOUNDARY_META_KEY,
        &identity.shallow_boundary.join("\n"),
    )
}

/// A LocalOnly incumbent's recorded shallow-boundary commit hashes (newline-joined in `repo_meta`),
/// as a vec. Empty when none was recorded — a pre-gate registration, which the upgrade path treats
/// as no proof available.
fn read_shallow_boundary(conn: &Connection, repo_id: &str) -> rusqlite::Result<Vec<String>> {
    let raw = crate::index::repo_meta(conn, repo_id, SHALLOW_BOUNDARY_META_KEY)?;
    Ok(raw
        .map(|value| value.lines().map(str::to_string).filter(|h| !h.is_empty()).collect())
        .unwrap_or_default())
}

/// The refusal message for a LocalOnly→Portable upgrade that could not be PROVEN: the incoming
/// portable clone does not reach the machine-local incumbent's recorded shallow boundary (or none
/// was recorded), so re-pointing the index onto it risks migrating one repo's data onto an
/// unrelated id. Names the pin escape hatch to force it when the two genuinely ARE the same repo.
fn unproven_upgrade_error(incumbent: &str, incoming: &str) -> String {
    format!(
        "cannot upgrade the machine-local repo id {incumbent} to {incoming}: could not prove the \
         incoming repository is a deepened clone of this index (its history does not reach the \
         recorded shallow boundary, or no boundary was recorded). Refusing rather than re-point \
         this index onto a possibly-different repo. If they ARE the same repository, pin `[index] \
         repo_id = \"{incoming}\"` in rag-rat.toml to force it."
    )
}

/// The active repo id for `conn` — the value every repo-scoped read/write scopes against.
///
/// Prefers the SCOPE CONTEXT installed by `install_scope_view`
/// (`temp.connection_context['repo_id']`): a consolidated multi-repo DB is only ever reached
/// through a config-bearing open that registers the repo and installs the context first, so the
/// context branch is authoritative there and never resolves the wrong repo. Falls back to
/// [`sole_repo_id`] when NO context row is installed — the identity-less paths (bare `open`,
/// `create_or_migrate`, a raw test connection, the model-manifest heal before a context is set): a
/// phase-A DB opened without a config holds exactly one repo, so the fallback is unambiguous there.
/// This replaced the old `single_repo_id` stand-in at every free-conn call site (A3).
///
/// An INSTALLED-BUT-EMPTY context (`""`) is AUTHORITATIVE and returned as-is — it is NOT a missing
/// context. The raw read callers (CLI/MCP grep hooks, `query::orientation`) deliberately install
/// `""` when `resolve_scope_repo_id` cannot prove the config repo, meaning "match nothing" rather
/// than "pick a repo". The scoped `files` view then matches no rows; a direct-scoped reader
/// (git history, parser failures, `repo_meta`) scoping on this `""` likewise reads nothing — a repo
/// with the empty id does not exist. Falling through to `sole_repo_id` on an empty context would
/// let those readers serve a SIBLING repo while the file view is empty (round-5 finding). So only
/// the ABSENCE of the context row falls back.
pub(crate) fn active_repo_id(conn: &Connection) -> rusqlite::Result<String> {
    if let Some(repo_id) = context_repo_id(conn) {
        return Ok(repo_id);
    }
    sole_repo_id(conn)
}

/// The active `repo_id` to scope a PERIPHERY table (clones / oracle / reconcile / memories) by, or
/// `None` when that subsystem is not repo-scoped on this connection.
///
/// Those `repo_id` columns are added by the phase-A5 periphery-scoping migration (V042,
/// `apply_repo_id_periphery_scoping`). A normal open applies the full ladder, so the column is
/// present and this returns `Some(active_repo_id)` — the scoped path. The `None` branch is the
/// defensive path for a connection that never ran the ladder (a raw connection, or the
/// pre-migration schema in a forward-migration bootstrap fixture): a `repo_id` predicate would
/// reference a column that does not exist, so the caller instead runs its original unscoped SQL —
/// the pre-A5 repo-global behavior. Gating on the probe lets scoped and raw-connection callers
/// share one code path without a separate unscoped variant.
///
/// `probe_table` is the subsystem's representative table (`repo_memories`,
/// `clone_graph_generations`, `oracle_runs`, …). V042 adds `repo_id` to every table in a subsystem
/// in the SAME migration, so one probe per subsystem is authoritative for the set.
pub(crate) fn periphery_repo_scope(
    conn: &Connection,
    probe_table: &str,
) -> rusqlite::Result<Option<String>> {
    if super::column_exists(conn, probe_table, "repo_id")? {
        Ok(Some(active_repo_id(conn)?))
    } else {
        Ok(None)
    }
}

/// A ` AND <qualifier>.repo_id = '<escaped>'` SQL fragment for a periphery read, or `""` when
/// `scope` is `None` (the periphery is not yet repo-scoped — see [`periphery_repo_scope`]). The id
/// is embedded as an escaped string literal, NOT a bind parameter: these reads carry positional
/// params and dynamic `IN (…)` placeholder lists where re-slotting a bound id is error-prone, and
/// the id is a registry-validated content-derived value (never user free-text at query time) — the
/// same posture `oracle::store::sql_quoted_list` takes for the commit/worktree `IN`-lists. Doubling
/// any single quote keeps it a safe literal regardless.
pub(crate) fn periphery_repo_scope_clause(scope: &Option<String>, qualifier: &str) -> String {
    match scope {
        Some(repo_id) => format!(" AND {qualifier}.repo_id = '{}'", repo_id.replace('\'', "''")),
        None => String::new(),
    }
}

/// Read the active repo id from the per-connection scope context, tolerating the common absence of
/// the `temp.connection_context` table (a raw connection with no view installed) — `.ok()` swallows
/// the "no such table" error into `None`, exactly like `edges::resolve`'s `scope_context_value`.
fn context_repo_id(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM temp.connection_context WHERE key = ?1",
        [CONNECTION_CONTEXT_REPO_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// The LIVE `files.generation` for `repo_id` (A6), read from `repo_meta`. Absent or unparseable ⇒
/// `0`, which is the generation `DEFAULT 0` stamps every pre-V043 row and every never-restaged
/// index carries — so a fresh / upgraded index is visible under generation 0 with no `repo_meta`
/// write. The full rebuild advances this pointer atomically once its staged generation is complete.
pub(crate) fn live_files_generation(conn: &Connection, repo_id: &str) -> rusqlite::Result<i64> {
    let raw = crate::index::repo_meta(conn, repo_id, LIVE_FILES_GENERATION_META_KEY)?;
    Ok(raw.and_then(|value| value.trim().parse::<i64>().ok()).unwrap_or(0))
}

/// The `files.generation` a connection operates on (A6) — the value every generation-scoped read
/// filters against and every view-less heal path pins its `main.files` candidate pool to.
///
/// Prefers the SCOPE CONTEXT (`temp.connection_context['files_generation']`) installed by
/// `write_scope_view`: a reader/incremental open carries the LIVE generation there, and the
/// connection driving a full rebuild carries its WRITE generation (N+1) so its own edge-resolution
/// / logical-symbol reads see the generation it is building, not the one still live. Falls back to
/// the active repo's LIVE generation from `repo_meta` when NO context row is installed — the bare
/// `open` heal paths (`ensure_graph_index_current` → `resolve_edges` → `all_symbols`) run without a
/// scope view, exactly as [`active_repo_id`] falls back to [`sole_repo_id`] there.
pub(crate) fn active_generation(conn: &Connection) -> rusqlite::Result<i64> {
    if let Some(generation) = context_generation(conn) {
        return Ok(generation);
    }
    let repo_id = active_repo_id(conn)?;
    live_files_generation(conn, &repo_id)
}

/// Read the active generation from the per-connection scope context, tolerating the absence of the
/// `temp.connection_context` table (a raw connection with no view installed) — the
/// [`context_repo_id`] pattern, parsing the stored TEXT value to an integer.
fn context_generation(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT value FROM temp.connection_context WHERE key = ?1",
        [CONNECTION_CONTEXT_GENERATION_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|value| value.trim().parse::<i64>().ok())
}

/// The sole repo owning a single-repo database — the identity-less fallback for [`active_repo_id`]
/// and the config-less open paths (bare `open`, `create_or_migrate`, read-only opens). Prefers an
/// adopted real repo, falling back to the [`LEGACY_REPO_ID`] placeholder on a not-yet-adopted DB
/// (`ORDER BY (repo_id = placeholder)` sorts real ids first). Phase A keeps ONE repo per DB on
/// these paths ([`register_repo`] refuses a second real repo until A7), so the result is
/// unambiguous; multi-repo access always flows through the scope context, never here. Demoted from
/// the former universal `single_repo_id` resolver — no hard one-row assertion, so a stray call on a
/// consolidated DB degrades to a deterministic pick rather than a panic.
pub(crate) fn sole_repo_id(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row(
        "SELECT repo_id FROM repos ORDER BY (repo_id = ?1), repo_id LIMIT 1",
        [LEGACY_REPO_ID],
        |row| row.get(0),
    )
}

/// Whether this DB holds MORE THAN ONE real (adopted) repo — the interim guard for global-scope
/// destructive sweeps over tables that do not yet carry `repo_id` (`oracle_runs`,
/// `clone_graph_generations`; both gain the column in V042). When true, a per-repo caller must SKIP
/// the global cleanup rather than wipe a sibling repo's rows. Today `register_repo` keeps this at
/// one real repo until the A7 default-path flip, so the guard is dormant but present for the V042
/// seam.
pub(crate) fn multiple_real_repos(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM repos WHERE repo_id != ?1",
        [LEGACY_REPO_ID],
        |row| row.get(0),
    )?;
    Ok(count > 1)
}

/// Resolve the `repo_id` a config maps to on a connection WITHOUT registering anything — the
/// READ-path counterpart to [`register_repo`]. Used by the read-only open
/// (`try_open_config_read_only`) and the raw-connection scope-view installers (the Claude Code /
/// MCP hooks, `query::orientation`), which must scope to the CONFIG's repo, not the config-blind
/// sole repo: in a consolidated DB (post-A7) the [`sole_repo_id`] pick could bind a SIBLING repo.
///
/// Returns `None` when the repo cannot be proven to be registered — a fresh/unregistered DB, a
/// `Rejected` config, or a consolidated DB whose config root is not recorded. The read-only open
/// then bails to the read-write open (which registers/heals); a raw scope-view caller binds an
/// empty scope rather than a sibling's rows.
///
/// Resolution routes, in order:
///  1. by IDENTITY — [`resolve_repo_identity`](crate::repo_identity::resolve_repo_identity) derives
///     the id (honoring an `[index] repo_id` override); if it is a REGISTERED real repo, use it. A
///     derivable-but-UNREGISTERED id (a new/changed pin, or a now-portable shallow clone) returns
///     `None` — a changed identity must adopt/surface on the read-write path, never silently keep
///     serving the old scope.
///  2. by ROOT (ABSENT configs only) — when there is NO derivable identity (non-git / unborn HEAD),
///     the recorded `repo_roots` mapping for this root path (registration always records the root,
///     so this binds the registered repo for a non-git root, e.g. a directly-seeded test repo).
///  3. the SOLE repo (ABSENT configs only) — on a single-repo DB the sole repo is unambiguous,
///     preserving the pre-A3 read-path behavior for un-adopted temp/test DBs; on a consolidated DB
///     it is not, so `None`.
///
/// A `Rejected` identity (a reserved/`local:` pin, a root-less/corrupt history) short-circuits to
/// `None`: it must NOT silently resolve — the read-write open surfaces the actionable error
/// instead.
pub(crate) fn resolve_config_repo_id(
    conn: &Connection,
    root: &Path,
    repo_id_override: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    match crate::repo_identity::resolve_repo_identity(root, repo_id_override) {
        // Route 1: a derivable id that is registered scopes correctly even in a consolidated DB.
        Ok(identity) if repo_id_is_registered(conn, &identity.repo_id)? =>
            Ok(Some(identity.repo_id)),
        // A RESOLVED but UNREGISTERED identity — a new/changed `[index] repo_id` pin, or a shallow
        // clone that now derives a portable id — must NOT silently bind the OLD scope. Return None
        // so the read-only open declines to the read-write open, which registers/upgrades
        // the identity and surfaces any mismatch. The recorded-root / sole fallbacks are
        // ONLY for an ABSENT config (no derivable id at all), never for a config that
        // resolved to a different, real id: falling through to route 2 here would keep
        // serving the previously-registered repo under the new identity (round-5 finding).
        Ok(_) => Ok(None),
        Err(err) if err.is_absent() => resolve_by_root_or_sole(conn, root),
        // A Rejected config must surface through the read-write open, never resolve silently here.
        Err(_) => Ok(None),
    }
}

/// Whether `repo_id` is a REGISTERED real repo (present in `repos`, and not the placeholder
/// marker).
fn repo_id_is_registered(conn: &Connection, repo_id: &str) -> rusqlite::Result<bool> {
    if repo_id == LEGACY_REPO_ID {
        return Ok(false);
    }
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repos WHERE repo_id = ?1)", [repo_id], |row| {
        row.get(0)
    })
}

/// Routes 2 + 3 of [`resolve_config_repo_id`]: the recorded `repo_roots` mapping for `root`, else
/// the sole repo of a single-repo DB (unambiguous), else `None` on a consolidated DB (cannot prove
/// which repo this root maps to — bind nothing rather than a sibling).
fn resolve_by_root_or_sole(conn: &Connection, root: &Path) -> rusqlite::Result<Option<String>> {
    let root = root.to_string_lossy();
    // A recorded root maps unambiguously to its repo (a physical path belongs to exactly one repo).
    if let Some(repo_id) = conn
        .query_row(
            "SELECT repo_id FROM repo_roots WHERE root = ?1 LIMIT 1",
            [root.as_ref()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(repo_id));
    }
    if multiple_real_repos(conn)? {
        return Ok(None);
    }
    Ok(Some(sole_repo_id(conn)?))
}

/// Every registered `repo_id` that is a real (adopted) id — i.e. excluding the [`LEGACY_REPO_ID`]
/// placeholder. A single-repo DB returns at most one.
fn real_repo_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT repo_id FROM repos WHERE repo_id != ?1 ORDER BY repo_id")?;
    let ids = stmt.query_map([LEGACY_REPO_ID], |row| row.get::<_, String>(0))?;
    ids.collect()
}

/// Append a working-tree root for `repo_id` (idempotent per `(repo_id, root)` — worktrees of one
/// repo pool under the same id, so several roots are expected).
fn record_repo_root(
    conn: &Connection,
    repo_id: &str,
    root: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO repo_roots(repo_id, root, registered_at_ms) VALUES (?1, ?2, ?3)",
        params![repo_id, root, now_ms],
    )?;
    Ok(())
}
