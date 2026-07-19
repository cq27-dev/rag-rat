use std::path::Path;

use rag_rat_base::config::Config;
use rag_rat_base::repo_identity::{
    LEGACY_REPO_ID, LOCAL_ONLY_ID_PREFIX, RepoIdentity, RepoIdentityClass,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::hooks::MigrationHooks;

/// The `temp.connection_context` key under which the scope view stashes the active repo id (beside
/// `commit_sha` / `worktree_id`). [`active_repo_id`] reads it; `install_scope_view` writes it.
pub const CONNECTION_CONTEXT_REPO_KEY: &str = "repo_id";

/// The `temp.connection_context` key under which the scope view stashes the active file GENERATION
/// (A6) — the value the `files` view filters on. `write_scope_view` writes it (the LIVE generation
/// for a reader/incremental open, the WRITE generation for the connection driving a full rebuild);
/// [`active_generation`] reads it back for the view-less heal paths.
pub const CONNECTION_CONTEXT_GENERATION_KEY: &str = "files_generation";

/// The `repo_meta` key holding a repo's LIVE `files.generation` — the pointer a full rebuild flips
/// once its freshly-staged generation is complete (A6). Absent ⇒ 0 (a fresh index, and every row a
/// pre-V043 upgrade carries), so an un-restaged index needs no `repo_meta` write to be visible.
pub const LIVE_FILES_GENERATION_META_KEY: &str = "live_files_generation";

/// The direct-scoped tables whose [`LEGACY_REPO_ID`] placeholder rows [`register_repo`] re-points
/// at the real id when it adopts a legacy DB. V040 (phase A3) added the core tables; V041 (phase
/// A4) added the github papertrail tables, which V060 normalized into the provider-neutral
/// papertrail_* set listed here (the migration copies `repo_id` verbatim, so placeholder rows
/// stay placeholder and adoption re-points them exactly as before — `papertrail_fts` is
/// own-content FTS5, so its `repo_id UNINDEXED` value updates in place). `git_file_changes` is
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
    // The provider-neutral papertrail tables (V060, successors of the seven V041 github_* tables)
    // + the derived FTS mirror.
    "papertrail_refs",
    "papertrail_items",
    "papertrail_comments",
    "papertrail_sync_cursor",
    "papertrail_item_tags",
    "papertrail_fts",
    // V073 (#702): the provider-attested closing-edge substrate — direct `repo_id` in the
    // natural key from birth, so a LocalOnly→Portable adoption re-points its rows with the
    // rest of the papertrail family.
    "papertrail_closing_edges",
    // V076 (#703): the distilled-record store — record row + junction children + work queue +
    // run stats, all with direct `repo_id` in their keys from birth (no FK children between them),
    // so a LocalOnly→Portable adoption re-points every one with the papertrail family. Omitting
    // any would strand its rows under the retired id, invisible through the active scope.
    "papertrail_distill",
    "papertrail_distill_evidence",
    "papertrail_distill_anchors",
    "papertrail_distill_alternatives",
    "papertrail_distill_record_commits",
    "papertrail_distill_edges",
    "papertrail_distill_queue",
    "papertrail_distill_runs",
    // V056 (#566) derived change-coupling table: standalone, direct `repo_id`, no FK children, so
    // a LocalOnly→Portable adoption re-points its rows here (moving them together with the
    // `git_coupling_stamp` repo_meta so the derived table stays consistent + fresh), and the
    // late-merge path DELETEs the retiring id's rows. Without this the stamp would move but the
    // rows strand — `ensure_coupling_fresh` would then see a matching stamp and serve an empty
    // section.
    "git_change_couplings",
];

/// The periphery tables that gain a `repo_id` column in the phase-A5 periphery-scoping migration
/// (V042, `apply_repo_id_periphery_scoping`) — plus the V045 dream-v2 verification siblings
/// (`memory_reality` / `memory_summaries` / `memory_model_failures`), repo_id-scoped from birth —
/// and therefore also need their [`LEGACY_REPO_ID`] placeholder rows re-pointed at the real id when
/// [`register_repo`] adopts a legacy DB — the A5 continuation of the A1/A3 adoption contract.
/// `repo_memory_fts` is the standalone FTS mirror of `repo_memories` (its `repo_id` snapshots the
/// parent's at rebuild time), re-pointed alongside.
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
    // V056 (#114): the per-moniker external dependency contract, repo_id-scoped from birth like
    // its oracle-periphery siblings; a LocalOnly→Portable adoption must re-point its rows too.
    "external_symbols",
    "reconcile_attempts",
    "dream_findings",
    "repo_memories",
    "repo_memory_bindings",
    "repo_memory_fts",
    // Dream-v2 siblings — repo_id-scoped like `dream_findings`; a LocalOnly→Portable adoption
    // must re-point their rows too. Guarded by `column_exists` in the re-point loop, so an older
    // partial-schema fixture (table/column absent) is a no-op.
    "memory_reality",
    "memory_summaries",
    "memory_model_failures",
    // The typed node-edge set (#464, V049): `repo_id` is the OWNER repo (the source node's), so a
    // LocalOnly→Portable adoption re-points it exactly like the other periphery tables. Its
    // `target_repo_id` is a REFERENCE and is deliberately NOT re-pointed here (it may name a
    // sibling).
    "repo_node_edges",
];

/// The `repo_meta` key a [`LocalOnly`](rag_rat_base::repo_identity::RepoIdentityClass::LocalOnly)
/// registration records its SORTED shallow-boundary commit hashes under (newline-joined), so a
/// later LocalOnly→Portable upgrade can PROVE the incoming deepened clone is the same repository —
/// its HEAD must reach these boundary commits. Absent ⇒ no proof recorded ⇒ the upgrade is refused.
const SHALLOW_BOUNDARY_META_KEY: &str = "shallow_boundary";

/// How long a LocalOnly→Portable upgrade waits for a writer still holding the OUTGOING `local:`
/// discriminator's write lock (A6, batch-4 P2) before refusing. Generous — an in-flight
/// index/maintenance pass finishes its lock holds well within it; a timeout surfaces an explicit
/// refusal, never a silent re-point under a live writer.
const UPGRADE_OUTGOING_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long [`register_repo`] waits for the DB-global registry lock (A7) before refusing — a
/// registration is a millisecond-scale write, so a timeout means a wedged sibling process, and an
/// explicit retryable refusal beats queueing forever.
const REGISTRY_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
/// - **A `local:` incumbent + an incoming `Portable` id that proves the upgrade** → *upgrade in
///   place*: the DB was first indexed under a machine-local shallow-clone id, and the caller has
///   since deepened it (`git fetch --unshallow`, our own remedy) or pinned a stable id, so the
///   incoming identity is now portable. Every scoped row, `repo_meta`, `repo_roots`, and
///   logical-symbol id is re-pointed from the local id to the portable one (the same adoption
///   machinery), rather than stranding the existing index behind a refusal. On a consolidated DB
///   the incumbent is found by boundary reachability among ALL registered repos, so only the
///   matching repo moves.
/// - **A genuinely new repo joining the DB** → *fresh registration*: a new `repos` row and a
///   `repo_roots` entry, nothing re-pointed. A7 makes several repos sharing one global database the
///   default (replacing phase A's single-repo "refuse a second real repo" invariant).
/// - **A new id whose working-tree `root` is already recorded under a DIFFERENT real repo** →
///   refuses: a physical path belongs to exactly one repo, so an unregistered id at an
///   already-owned root is a checkout whose identity changed without proving an upgrade (a
///   re-shallowed clone downgrading a portable id, a deepened clone with no recorded boundary, or a
///   rewritten root commit). Refusing avoids forking the repo across two ids; the remedy is to
///   unshallow or pin `[index] repo_id`.
///
/// `now_ms` is the injected clock (see the repo's time-injection convention), stamped as the
/// registration time on adoption / first insert.
///
/// Records this checkout's working-tree `root` in `repo_roots` (the "this checkout was INDEXED
/// here" signal). This is the INDEXING entry point — rebuild / incremental / consolidate / the
/// tests. A read-only `open_config` (doctor / MCP / query) must NOT record the root; it calls
/// [`register_repo_read_only`] instead (see that function for why, #427).
pub fn register_repo(
    conn: &Connection,
    identity: &RepoIdentity,
    root: &Path,
    now_ms: i64,
    hooks: &MigrationHooks,
) -> rusqlite::Result<String> {
    register_repo_inner(conn, identity, root, now_ms, true, hooks)
}

/// The `index_meta` key that TOMBSTONES a repo removed via `rag-rat rm` (#767). GLOBAL scope on
/// purpose: the removal purge deletes every repo-scoped row, so a marker meant to OUTLIVE the
/// removal must live outside that sweep — `index_meta` carries no `repo_id` column and is never
/// swept.
fn removed_repo_key(repo_id: &str) -> String {
    format!("removed_repo:{repo_id}")
}

/// Tombstone `repo_id` as removed-via-`rm` at `removed_at_ms`. Written INSIDE the purge transaction
/// so it commits atomically with the deletion; [`register_repo`] then refuses to re-register the id
/// until [`clear_repo_removed`] (via `rag-rat init`) lifts it — the durable guard against a writer
/// that queued behind the removal lock with a STALE in-memory config silently repopulating the repo
/// after the purge (a lock alone cannot stop a process that already cloned its `Config`).
pub fn mark_repo_removed(
    conn: &Connection,
    repo_id: &str,
    removed_at_ms: i64,
) -> anyhow::Result<()> {
    crate::meta::set_meta(conn, &removed_repo_key(repo_id), &removed_at_ms.to_string())
}

/// Whether `repo_id` is tombstoned as removed-via-`rm`. A direct `index_meta` read (rusqlite-typed
/// so [`register_repo_inner`] can gate on it without an error-type conversion).
pub fn is_repo_removed(conn: &Connection, repo_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM index_meta WHERE key = ?1)",
        [removed_repo_key(repo_id).as_str()],
        |row| row.get(0),
    )
}

/// Lift the removed-via-`rm` tombstone for `repo_id` — `rag-rat init`'s deliberate re-add, the ONE
/// path allowed to bring a removed repo back. Idempotent (clearing an absent tombstone is a no-op).
pub fn clear_repo_removed(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    crate::meta::delete_meta(conn, &removed_repo_key(repo_id))
}

/// Register/adopt `identity` WITHOUT recording the working-tree `root` in `repo_roots` — the
/// read-only open path (`open_config`: doctor / MCP / query, #427). A recorded root is the "this
/// checkout was INDEXED here" signal `is_root_already_indexed` / `same_identity_join_note` key on,
/// so a mere read must not create one: otherwise a fresh same-identity clone that was only ever
/// opened for reading would masquerade as indexed and let a later empty index prune the shared
/// scope. All the identity work (adopt placeholder → real, `local:` → Portable upgrade, refuse a
/// conflicting owner) still runs — a read on a legacy DB still adopts it — only the `repo_roots`
/// INSERT is skipped; the next indexing pass records the root.
pub fn register_repo_read_only(
    conn: &Connection,
    identity: &RepoIdentity,
    root: &Path,
    now_ms: i64,
    hooks: &MigrationHooks,
) -> rusqlite::Result<String> {
    register_repo_inner(conn, identity, root, now_ms, false, hooks)
}

/// The registration impl shared by [`register_repo`] (records the root) and
/// [`register_repo_read_only`] (does not). `record_root` gates ONLY the `repo_roots` INSERT; every
/// identity decision below is identical.
fn register_repo_inner(
    conn: &Connection,
    identity: &RepoIdentity,
    root: &Path,
    now_ms: i64,
    record_root: bool,
    hooks: &MigrationHooks,
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

    // #767: refuse to (re-)register a repo tombstoned by `rag-rat rm`, on BOTH the indexing
    // (`record_root`) AND the read-only adoption paths. A writer that queued behind the removal
    // lock resumes with a stale in-memory config and would otherwise repopulate the just-purged
    // repo — and a read-only open is NOT harmless when it is write-capable: a manual
    // `papertrail sync` registers read-only, then commits `papertrail_*` rows. A
    // genuinely-removed repo has no surviving config to reach here (rm deletes it); `rag-rat
    // init` / `consolidate` clear the tombstone for a deliberate re-add. This is the cheap
    // PRE-FILTER; the AUTHORITATIVE check re-runs inside the adoption transaction below (this
    // read races rm's tombstone commit — the in-transaction one cannot).
    if is_repo_removed(conn, trimmed_id)? {
        return Err(registry_refusal(format!(
            "repo {trimmed_id} was removed with `rag-rat rm` — run `rag-rat init` in the repo to \
             re-add it"
        )));
    }

    let root_str = root.to_string_lossy();

    // REGISTRY LOCK (A7): serialize the WHOLE read-decide-write sequence across processes.
    // Registration reads the registered-repo set, decides (idempotent / upgrade / refuse / fresh),
    // then writes — two repos' concurrent FIRST registrations on the shared global DB would
    // otherwise interleave between the read and the write (`SQLITE_BUSY_SNAPSHOT` on the deferred
    // write upgrade, or a `repos`-PK constraint on the same-id race), and neither failure is
    // retried on the CLI paths. Per-repo write locks cannot serialize this — the racing writers
    // hold DIFFERENT repo ids by construction — so this is a DB-global lock like the schema lock,
    // following the same ordering rule (per-repo entry locks → global lock; the per-repo locks the
    // UPGRADE path takes while holding this are BOUNDED, so no cross-type cycle can hang — see
    // `locks::registry_lock_path`). Bounded and reentrant; a timeout is a retryable refusal. A
    // pathless (in-memory) connection skips it — no cross-process writer can exist for it.
    let _registry_lock = match conn.path().filter(|p| !p.is_empty()) {
        Some(db_path) => Some(
            rag_rat_base::locks::WriteLock::acquire_registry_timeout(
                Path::new(db_path),
                REGISTRY_LOCK_TIMEOUT,
            )
            .map_err(|err| {
                registry_refusal(format!(
                    "cannot register repo {}: failed acquiring the repo-registry lock: {err}",
                    identity.repo_id
                ))
            })?
            .ok_or_else(|| {
                registry_refusal(format!(
                    "cannot register repo {}: timed out waiting for a concurrent registration to \
                     finish; re-run the command",
                    identity.repo_id
                ))
            })?,
        ),
        None => None,
    };
    // Read the registered set INSIDE the registry lock, so the decision below cannot race a
    // concurrent registration's write (a same-id racer that lost the lock re-reads a set already
    // containing the id and collapses into the idempotent path).
    let real_ids = real_repo_ids(conn)?;

    // Already registered under this id → idempotent; just make sure the root is recorded (and, for
    // a LocalOnly re-registration of the same clone, that its shallow boundary is on record so
    // a later upgrade can prove against it — self-healing for an index first registered before
    // this gate).
    if real_ids.iter().any(|id| id == &identity.repo_id) {
        // Even the idempotent path must not silently map one physical root onto TWO repos: a
        // checkout whose identity changed to an ALREADY-REGISTERED id (an `[index] repo_id` pin
        // switched to an existing repo, an in-place re-clone) would otherwise record its root
        // under the second id and make `resolve_config_repo_id`'s recorded-root route
        // non-deterministic (`LIMIT 1` over two owners). ONE exception before refusing: the
        // LATE-upgrade merge — the owner is a `local:` incumbent whose (root ∧ boundary) proof
        // holds for the incoming portable id, i.e. this is the SECOND shallow clone of an
        // upstream deepening after a sibling clone already claimed the portable id. Refusing that
        // shape would strand it permanently (the pin remedy re-enters this same guard); merging
        // retires the local id into the registered one.
        if let Some(owner) = real_root_owner(conn, &root_str, &identity.repo_id)? {
            if late_upgrade_is_proven(conn, &owner, identity, root)? {
                merge_local_incumbent_into_registered(
                    conn,
                    identity,
                    &owner,
                    &root_str,
                    now_ms,
                    record_root,
                )?;
                persist_shallow_boundary(conn, identity)?;
                return Ok(identity.repo_id.clone());
            }
            return Err(registry_refusal(mismatched_root_owner_error(
                &owner,
                &identity.repo_id,
                &root_str,
            )));
        }
        if record_root {
            record_repo_root(conn, &identity.repo_id, &root_str, now_ms)?;
        }
        persist_shallow_boundary(conn, identity)?;
        return Ok(identity.repo_id.clone());
    }

    // The incoming id is NOT yet registered. Three outcomes, decided BEFORE any lock or transaction
    // so a refusal touches nothing:
    //
    //  (1) UPGRADE — the incoming id is Portable and some already-registered `local:` incumbent's
    //      recorded shallow boundary is reachable from the incoming clone's HEAD: the caller
    // deepened      that machine-local shallow clone (`git fetch --unshallow`, our own remedy)
    // or opened a      full clone of the same repo, so re-point the incumbent onto the portable
    // id IN PLACE      (below). Searching ALL registered ids — not a lone incumbent — is what
    // makes this correct      on a CONSOLIDATED multi-repo DB (A7): only the matching repo's
    // boundary is reachable, and      the re-point touches only that repo's rows, so every
    // other registered repo is left alone.
    //
    //  (2) FRESH REGISTRATION — a genuinely NEW repo joining the DB. A7 makes several repos sharing
    //      one global database the default, so a new id at an unclaimed working tree simply gets
    // its      own `repos` row (no re-point). This is the behavior that replaces phase A's
    // single-repo      "refuse a second real repo" invariant.
    //
    //  (3) REFUSAL — the incoming id is new AND its working-tree `root` is already recorded under a
    //      DIFFERENT real repo (see [`real_root_owner`]). A physical path belongs to exactly one
    //      repo, so this is a checkout whose identity changed WITHOUT proving an upgrade: a
    //      re-shallowed clone (a `LocalOnly` incoming must never DOWNGRADE the portable id its root
    //      already owns), a deepened clone whose shallow boundary was never recorded (unprovable
    //      upgrade), or a rewritten root commit. Refuse rather than fork the repo across two ids;
    //      the remedy is to unshallow or pin `[index] repo_id`. This generalizes the phase-A
    //      single-repo "different real repo" / "unproven upgrade" / "no downgrade" refusals to the
    //      multi-repo DB, keyed on the ROOT rather than a lone incumbent.
    let upgrade_from = find_upgradeable_local_incumbent(conn, &real_ids, identity, root)?;
    if upgrade_from.is_none()
        && let Some(owner) = real_root_owner(conn, &root_str, &identity.repo_id)?
    {
        return Err(registry_refusal(mismatched_root_owner_error(
            &owner,
            &identity.repo_id,
            &root_str,
        )));
    }

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
        (Some(local_id), Some(db_path)) =>
            Some(acquire_dual_repo_locks(Path::new(db_path), local_id, &identity.repo_id)?),
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
    // IMMEDIATE, not deferred (A7): the transaction is decided-then-written, and a deferred BEGIN
    // that upgrades to a write mid-transaction can fail `SQLITE_BUSY_SNAPSHOT` (not retried by
    // `busy_timeout`) if any other writer committed since the first read. `BEGIN IMMEDIATE` takes
    // the write lock up front under the normal busy-timeout wait. The registry flock above already
    // serializes registrations against each other; IMMEDIATE covers contention with NON-registry
    // writers (an index pass on a sibling repo).
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // #767 review: re-check the removal tombstone INSIDE the adoption transaction. The pre-filter
    // above reads the tombstone BEFORE the registry lock and any transaction, and `rm` takes
    // neither — so without this re-check a stale (preloaded-config) registration could pass the
    // pre-filter, have rm purge + tombstone in the gap, and then INSERT the `repos` row after rm
    // reported success. This transaction and rm's purge are both IMMEDIATE, so they serialize on
    // the SQLite write lock and the tombstone state read here is stable for the whole adoption:
    // set → rm committed first, refuse; unset → rm's purge waits for this commit and removes the
    // just-registered repo itself.
    if is_repo_removed(&tx, trimmed_id)? {
        return Err(registry_refusal(format!(
            "repo {trimmed_id} was removed with `rag-rat rm` — run `rag-rat init` in the repo to \
             re-add it"
        )));
    }
    let placeholder_present = tx
        .query_row("SELECT 1 FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID], |_| Ok(()))
        .optional()?
        .is_some();
    // `ON CONFLICT DO NOTHING` is defense in depth for the lockless (pathless/in-memory)
    // connections the registry flock cannot serialize: a same-id racer that somehow landed first
    // collapses this into a no-op instead of a PK constraint failure, and the row content it wrote
    // is what an idempotent re-registration would have kept anyway.
    tx.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_id) DO NOTHING",
        params![identity.repo_id, identity.display_name, now_ms],
    )?;
    // The id whose rows this registration re-points ONTO the new id: the machine-local incumbent on
    // a shallow-clone upgrade, else the `__unassigned__` placeholder on a fresh adoption. Both
    // cases re-point the same scoped tables + realign the same logical ids and then drop the
    // old `repos` row; only the upgrade path actually has `repo_roots` rows to move (the
    // placeholder never does, so that UPDATE is a harmless no-op there). The placeholder is adopted
    // ONLY on a genuinely fresh legacy DB (no real repo yet): on a CONSOLIDATED DB that already
    // holds real repos (A7), a new repo registers FRESH and must NOT claim any vestigial
    // placeholder rows as its own — so `repoint_from` is `None` there and the block below is
    // skipped entirely.
    let repoint_from = match upgrade_from.as_deref() {
        Some(local_id) => Some(local_id),
        None if real_ids.is_empty() && placeholder_present => Some(LEGACY_REPO_ID),
        None => None,
    };
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
        // A1/A2 adoption contract from `repos`/`repo_meta` to the V040 core tables and the
        // papertrail tables, provider-neutral since V060). Runs INSIDE the same adoption
        // transaction, keeping the insert-first ordering: on a fresh open the tables are
        // empty and these are no-ops; on a forward-migrated DB that indexed under the
        // placeholder (or a shallow-clone upgrade) they carry the rows onto the real id
        // atomically with the `repos` rewrite. `git_commits` is updated here;
        // `git_file_changes` follows via its `ON UPDATE CASCADE` FK.
        for table in DIRECT_SCOPED_ADOPTION_TABLES {
            // Guard on table presence: a real consolidated DB is fully migrated (every
            // direct-scoped table exists), but the schema-bootstrap tests exercise
            // adoption against ISOLATION fixtures that seed only the subset a given
            // migration touches (V040 core tables OR the papertrail tables). Skipping an
            // absent table keeps adoption correct on the full schema while staying
            // robust to those partial fixtures — a table that does not exist has no
            // source rows to re-point.
            if !adoption_table_present(&tx, table)? {
                continue;
            }
            // `main.`-qualified: adoption can run on a connection that already carries the
            // temp `files` scope view (the incremental pass's bare open installs it BEFORE
            // adopting), and an unqualified `UPDATE files` would hit that view — "cannot
            // modify files because it is a view". The qualifier pins every re-point to the
            // real table regardless of what temp views the connection carries.
            tx.execute(
                &format!("UPDATE main.{table} SET repo_id = ?1 WHERE repo_id = ?2"),
                params![identity.repo_id, source_id],
            )?;
        }
        // A5 periphery tables (clones/oracle/reconcile/memories). Their `repo_id` column lands in
        // V042; several of these tables predate it, so a partial-schema bootstrap fixture that
        // stops before V042 can have the table without the column. Guard each re-point with
        // `column_exists` (no-op when the column is absent, real backfill once V042 has run — every
        // normal open applies the full ladder) so this adoption never trips "no such column". Uses
        // `source_id` (not the placeholder literal) so a shallow-clone upgrade re-points periphery
        // rows off the `local:` incumbent too, exactly like the core/papertrail loop above.
        for table in A5_PERIPHERY_DIRECT_SCOPED_TABLES {
            if super::column_exists(&tx, table, "repo_id")? {
                // `main.`-qualified for the same view-shadowing reason as the core loop above.
                tx.execute(
                    &format!("UPDATE main.{table} SET repo_id = ?1 WHERE repo_id = ?2"),
                    params![identity.repo_id, source_id],
                )?;
            }
        }
        // repo_node_edges (#464): the loop above re-pointed the OWNER `repo_id`; a SAME-repo
        // `target_repo_id` must move with it (a cross-repo target names a sibling and is left
        // alone). Edge reads self-heal `target_repo_id` from the live node, but keep the
        // stored column honest here too. Guarded like the loop (a partial-schema fixture
        // can lack the V049 column).
        if super::column_exists(&tx, "repo_node_edges", "repo_id")? {
            tx.execute(
                "UPDATE main.repo_node_edges SET target_repo_id = ?1 WHERE target_repo_id = ?2",
                params![identity.repo_id, source_id],
            )?;
        }
        // `dream_findings.id` folds `repo_id` (the `logical_symbols.stable_id` precedent), so the
        // periphery re-point above changed the id every finding SHOULD have — re-derive them (and
        // the in-table `superseded_by` references) under the adopted id: the dream twin of the
        // `realign_logical_symbol_ids` call below. Guarded like the loop above (a partial-schema
        // bootstrap fixture can lack the table or the V042 column); idempotent.
        if super::column_exists(&tx, "dream_findings", "repo_id")? {
            (hooks.rederive_dream_finding_ids)(&tx)?;
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
        (hooks.realign_logical_symbol_ids)(&tx)?;
        tx.execute("DELETE FROM repos WHERE repo_id = ?1", [source_id])?;
    }
    if record_root {
        record_repo_root(&tx, &identity.repo_id, &root_str, now_ms)?;
    }
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

/// Acquire BOTH repo ids' per-repo write locks in the CANONICAL LEXICOGRAPHIC ORDER
/// (`locks::canonical_lock_order`), each reentrant-instant when this thread already holds it and
/// BOUNDED otherwise — the A6 rule that keeps identity-transition flows deadlock-free (in-order
/// unbounded edges cannot cycle under a total order; bounded out-of-order edges self-break).
/// Shared by the in-place LocalOnly→Portable upgrade and the LATE-upgrade merge, both of which
/// re-point/move rows across an OUTGOING `local:` id and an incoming/target id and must serialize
/// with writers holding EITHER lock (a pre-unshallow writer holds the `local:` one, a
/// post-unshallow writer the portable one).
fn acquire_dual_repo_locks(
    db_path: &Path,
    outgoing_id: &str,
    target_id: &str,
) -> rusqlite::Result<Vec<rag_rat_base::locks::WriteLock>> {
    let (first, second) = rag_rat_base::locks::canonical_lock_order(outgoing_id, target_id);
    let mut guards = Vec::with_capacity(2);
    for repo in [first, second] {
        guards.push(
            rag_rat_base::locks::WriteLock::acquire_timeout(
                db_path,
                repo,
                UPGRADE_OUTGOING_LOCK_TIMEOUT,
            )
            .map_err(|err| {
                registry_refusal(format!(
                    "cannot upgrade {outgoing_id}: failed acquiring the write lock for {repo}: \
                     {err}"
                ))
            })?
            .ok_or_else(|| {
                registry_refusal(format!(
                    "cannot upgrade {outgoing_id}: timed out waiting for an in-flight writer \
                     holding the write lock for {repo}"
                ))
            })?,
        );
    }
    Ok(guards)
}

/// The direct-scoped tables whose rows the LATE-upgrade merge DELETES under the retiring `local:`
/// id (its DERIVED data): the A5 periphery list MINUS the three memory tables, which are AUTHORED
/// and are MOVED onto the target id instead. Children fall via `ON DELETE CASCADE`
/// (`clone_edges`/`clone_subblock_postings` off `clone_graph_generations`,
/// `logical_symbol_members` off `logical_symbols` — deleted via [`DIRECT_SCOPED_ADOPTION_TABLES`]).
const LATE_MERGE_DERIVED_PERIPHERY_TABLES: &[&str] = &[
    "clone_graph_generations",
    "clone_token_df",
    "clone_refinements",
    "oracle_runs",
    "edge_oracle",
    "logical_symbol_monikers",
    // V056 (#114): DERIVED (re-produced by a fresh oracle run under the target id), so it is
    // DROPPED under the retiring id, not moved — same posture as edge_oracle.
    "external_symbols",
    "reconcile_attempts",
    "dream_findings",
];

/// Whether the conflicting `owner` of the incoming root is a `local:` incumbent that PROVES a
/// LATE upgrade onto the already-registered `identity`: the incoming id is Portable, the owner is
/// machine-local, and the owner's recorded shallow boundary is reachable from the incoming root's
/// HEAD — i.e. this checkout IS that shallow clone, deepened AFTER a sibling clone already
/// upgraded their shared upstream id. Without this, the second clone is PERMANENTLY stranded: the
/// idempotent branch's root-owner guard refuses, and pinning the portable id re-enters the same
/// refusal.
fn late_upgrade_is_proven(
    conn: &Connection,
    owner: &str,
    identity: &RepoIdentity,
    root: &Path,
) -> rusqlite::Result<bool> {
    if !owner.starts_with(LOCAL_ONLY_ID_PREFIX) || identity.class != RepoIdentityClass::Portable {
        return Ok(false);
    }
    let boundary = read_shallow_boundary(conn, owner)?;
    Ok(!boundary.is_empty()
        && rag_rat_base::repo_identity::boundary_reachable_from_head(root, &boundary)
            .unwrap_or(false))
}

/// LATE-upgrade merge: retire the `local:` incumbent `owner` INTO the already-registered
/// `target_id` (the second shallow clone of an upstream whose portable id a sibling clone already
/// claimed). Unlike the in-place upgrade, the target has LIVE data — same upstream, so the same
/// paths/commits — and a wholesale re-point would collide on the widened UNIQUE keys and the
/// generation axis. So the merge takes the consolidate-shaped split:
///
///  * AUTHORED data MOVES: `repo_memories` (+ bindings / FTS mirror) re-point to `target_id` — the
///    memory `id` is the bare PK, so a re-point can never collide. The bindings' LOCAL rowid
///    columns and the call-paths' logical-symbol endpoints are NULLed (they reference the retiring
///    id's derived rows, deleted below); the validate loop re-resolves them from the portable
///    anchor after the next index pass — exactly the consolidate posture. Tags/call-paths follow
///    via `memory_id`.
///  * DERIVED data is DROPPED, not migrated: files (cascading chunks/symbols/edges), git history,
///    papertrail, clones, oracle, reconcile, dream rows under `owner` are deleted — a fresh index
///    of this root re-derives them under `target_id`, and the carried `embedding_cache`
///    (content-addressed, global) makes re-embedding a no-op. Leaving them "for gc" was rejected:
///    gc sweeps are per-ACTIVE-repo, so rows under a retired id would be permanent invisible
///    garbage.
///  * The root and registration retire atomically: `repo_roots` moves to `target_id`, the `owner`
///    repos row is deleted (cascading its `repo_meta`, including the now-meaningless shallow
///    boundary). The target's own model-state meta is left as-is — same machine, same upstream.
///
/// Crash-convergent: everything runs in ONE IMMEDIATE transaction under the registry lock and BOTH
/// ids' per-repo locks (canonical order, bounded — the caller-held entry lock for either id is
/// reentrant-instant). A crash rolls the whole merge back to the pre-merge state, which re-proves
/// and re-runs on the next open.
///
/// REGISTRY-STALL TRADEOFF (deliberate — do not split this transaction in an optimization pass
/// without re-weighing it): the cascade-DELETE of the retiring repo's ENTIRE derived dataset runs
/// inside the registry lock, so a very large late-merging repo stalls every OTHER repo's
/// registering open for the DELETE's duration — worst case a bounded, RETRYABLE refusal after
/// `REGISTRY_LOCK_TIMEOUT` (30s), never a hang, and read paths never take the registry lock.
/// Accepted because a late merge happens at most once per shallow clone's lifetime and the
/// alternative forfeits more: draining the derived rows in a PRIOR committed transaction (under
/// the two per-repo locks only) would keep the registry lock hold at milliseconds, but gives up
/// the single-transaction atomicity claim — a crash between the drain and the merge leaves
/// derived-row-less-but-still-registered `local:` state that the next open's re-prove must
/// tolerate (it does converge: the proof re-holds and the merge re-runs over the already-drained
/// rows). That is the sketched escape hatch if the stall ever bites in practice.
///
/// TABLE COVERAGE: [`DIRECT_SCOPED_ADOPTION_TABLES`] + [`LATE_MERGE_DERIVED_PERIPHERY_TABLES`]
/// were audited complete against every `repo_id`-carrying table at V044; V045 widened the github
/// CHILD tables' keys without adding a new `repo_id` table, and V060's papertrail_* successors
/// replaced the github_* entries in the direct list 1:1, so the disposition is unchanged. A
/// future migration adding a NEW
/// `repo_id`-scoped table must add it to one of these lists (or the authored-move set above).
fn merge_local_incumbent_into_registered(
    conn: &Connection,
    identity: &RepoIdentity,
    owner: &str,
    root_str: &str,
    now_ms: i64,
    record_root: bool,
) -> rusqlite::Result<()> {
    let _merge_locks = match conn.path().filter(|p| !p.is_empty()) {
        Some(db_path) =>
            Some(acquire_dual_repo_locks(Path::new(db_path), owner, &identity.repo_id)?),
        None => None,
    };
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // AUTHORED data moves. Call-path logical endpoints first (they key off the memories that are
    // about to change repo_id), then bindings (repo_id + rowid NULLing in one pass), then the
    // memories and the FTS mirror.
    tx.execute(
        "UPDATE repo_memory_call_paths SET start_logical_symbol_id = NULL, end_logical_symbol_id \
         = NULL
         WHERE memory_id IN (SELECT id FROM repo_memories WHERE repo_id = ?1)",
        [owner],
    )?;
    tx.execute(
        "UPDATE main.repo_memory_bindings SET repo_id = ?1, logical_symbol_id = NULL, symbol_id = \
         NULL, chunk_id = NULL, edge_id = NULL WHERE repo_id = ?2",
        params![identity.repo_id, owner],
    )?;
    tx.execute("UPDATE main.repo_memories SET repo_id = ?1 WHERE repo_id = ?2", params![
        identity.repo_id,
        owner
    ])?;
    if adoption_table_present(&tx, "repo_memory_fts")? {
        tx.execute("UPDATE main.repo_memory_fts SET repo_id = ?1 WHERE repo_id = ?2", params![
            identity.repo_id,
            owner
        ])?;
    }
    // Node edges (#464) are AUTHORED — move the owner's edges onto the target id, else they orphan
    // under a `repo_id` whose `repos` row is about to be deleted (no FK cascades them). Re-point
    // the OWNER `repo_id` and any SAME-repo `target_repo_id` (a cross-repo target names a
    // sibling, left alone). The `edge_key` folds node ids (stable across a repo-id move), not
    // repo ids, so no recompute is needed. Guarded for a partial-schema fixture.
    if super::column_exists(&tx, "repo_node_edges", "repo_id")? {
        tx.execute(
            "UPDATE main.repo_node_edges SET target_repo_id = ?1 WHERE target_repo_id = ?2",
            params![identity.repo_id, owner],
        )?;
        tx.execute("UPDATE main.repo_node_edges SET repo_id = ?1 WHERE repo_id = ?2", params![
            identity.repo_id,
            owner
        ])?;
    }
    // DERIVED data drops (cascades take the transitive children).
    for table in DIRECT_SCOPED_ADOPTION_TABLES {
        if adoption_table_present(&tx, table)? {
            tx.execute(&format!("DELETE FROM main.{table} WHERE repo_id = ?1"), [owner])?;
        }
    }
    for table in LATE_MERGE_DERIVED_PERIPHERY_TABLES {
        if super::column_exists(&tx, table, "repo_id")? {
            tx.execute(&format!("DELETE FROM main.{table} WHERE repo_id = ?1"), [owner])?;
        }
    }
    // Registration retires: roots move BEFORE the repos delete (its FK cascades), then the owner
    // row falls, taking its repo_meta (shallow boundary included) with it.
    tx.execute("UPDATE repo_roots SET repo_id = ?1 WHERE repo_id = ?2", params![
        identity.repo_id,
        owner
    ])?;
    tx.execute("DELETE FROM repos WHERE repo_id = ?1", [owner])?;
    if record_root {
        record_repo_root(&tx, &identity.repo_id, root_str, now_ms)?;
    }
    tx.commit()?;
    tracing::warn!(
        old_repo_id = %owner,
        new_repo_id = %identity.repo_id,
        "late shallow-clone upgrade: this checkout's machine-local id was retired into the \
         already-registered portable id (a sibling clone upgraded it first). Its memories moved \
         over; its derived index rows were dropped and will re-derive on the next index pass."
    );
    Ok(())
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
    crate::meta::set_repo_meta(
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
    let raw = crate::meta::repo_meta(conn, repo_id, SHALLOW_BOUNDARY_META_KEY)?;
    Ok(raw
        .map(|value| value.lines().map(str::to_string).filter(|h| !h.is_empty()).collect())
        .unwrap_or_default())
}

/// The already-registered `local:` incumbent, if any, that the INCOMING identity upgrades in place:
/// a Portable incoming from the SAME recorded working tree whose HEAD reaches that incumbent's
/// recorded shallow boundary — proof the incoming clone is a DEEPENED version of the machine-local
/// one (`git fetch --unshallow` keeps the boundary commits in history; an unrelated repo reaches
/// none of them).
///
/// BOTH conditions are load-bearing (root ∧ boundary): with two shallow clones of the SAME upstream
/// registered under distinct `local:` ids — an ordinary shape on the shared global DB — a deepened
/// checkout's HEAD reaches BOTH boundaries, so a boundary-only scan would re-point whichever
/// incumbent SORTS first, hijacking the sibling clone's index and stranding the actually-deepened
/// one behind the root-owner refusal. Deepening happens in place (`git fetch --unshallow` mutates
/// the working tree that registered shallow), so the deepened clone's root IS the incumbent's
/// recorded root — require that match first, then the boundary proof on top (a root match alone
/// could be a re-cloned unrelated repo at a reused path).
///
/// `None` for a LocalOnly incoming (an upgrade only runs Portable-ward — a deepened clone must
/// never downgrade), for a root-matching incumbent with no recorded boundary (a pre-gate
/// registration ⇒ no proof — falls through to the root-owner refusal, which names the pin remedy),
/// or when no root-matching `local:` incumbent exists (a genuinely new clone registers fresh).
/// Runs inside the registry lock, before any transaction, so a non-match touches nothing.
fn find_upgradeable_local_incumbent(
    conn: &Connection,
    real_ids: &[String],
    identity: &RepoIdentity,
    root: &Path,
) -> rusqlite::Result<Option<String>> {
    if identity.class != RepoIdentityClass::Portable {
        return Ok(None);
    }
    let root_str = root.to_string_lossy();
    for id in real_ids {
        if !id.starts_with(LOCAL_ONLY_ID_PREFIX) {
            continue;
        }
        if !repo_has_recorded_root(conn, id, &root_str)? {
            continue;
        }
        let boundary = read_shallow_boundary(conn, id)?;
        if !boundary.is_empty()
            && rag_rat_base::repo_identity::boundary_reachable_from_head(root, &boundary)
                .unwrap_or(false)
        {
            return Ok(Some(id.clone()));
        }
    }
    Ok(None)
}

/// Whether `repo_id` has `root` recorded in `repo_roots` — the upgrade scan's working-tree match
/// (both sides store the identical `to_string_lossy` rendering of a `Config::load`-canonicalized
/// root, so equality is exact). Also the #427 same-identity-join hint's known-checkout test:
/// telling a repo's own re-index from a NEW physical checkout adopting its scope.
pub fn repo_has_recorded_root(
    conn: &Connection,
    repo_id: &str,
    root: &str,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM repo_roots WHERE repo_id = ?1 AND root = ?2)",
        params![repo_id, root],
        |row| row.get(0),
    )
}

/// The real repo id — other than `exclude_id` and the placeholder — that already records `root` in
/// `repo_roots`, or `None` when no OTHER real repo claims this working-tree path. A physical path
/// belongs to exactly one repo, so a recorded owner that differs from the incoming id signals an
/// identity change that must not silently fork the repo — [`register_repo`] refuses it on BOTH the
/// fresh path (unless the incoming proved an upgrade) and the idempotent path (an identity change
/// ONTO an already-registered id). `exclude_id` is the incoming id itself, so the idempotent
/// re-registration of a root under its own repo never self-trips (on the fresh path the incoming id
/// is unregistered and cannot own roots, so the exclusion is a no-op there).
fn real_root_owner(
    conn: &Connection,
    root: &str,
    exclude_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT repo_id FROM repo_roots WHERE root = ?1 AND repo_id != ?2 AND repo_id != ?3 LIMIT \
         1",
        params![root, LEGACY_REPO_ID, exclude_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

/// The refusal message for an unregistered incoming id whose working-tree `root` is already
/// recorded under a DIFFERENT real repo, without proving a shallow-clone upgrade. Covers all three
/// phase-A refusals now keyed on the root: a re-shallowed clone downgrading a portable id, a
/// deepened clone whose shallow boundary was never recorded (unprovable upgrade), and a rewritten
/// root commit. Names both remedies (unshallow / pin) to force it when the two genuinely ARE the
/// same repo.
fn mismatched_root_owner_error(owner: &str, incoming: &str, root: &str) -> String {
    format!(
        "cannot register repo {incoming} for {root}: that working tree is already registered to a \
         different repo {owner}, and the incoming identity did not prove a shallow-clone upgrade \
         (its history does not reach the recorded shallow boundary, or a machine-local id would \
         downgrade a portable one). Refusing rather than fork the repo across two ids. If they \
         ARE the same repository, unshallow the clone (`git fetch --unshallow`) or pin `[index] \
         repo_id = \"{incoming}\"` in rag-rat.toml."
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
pub fn active_repo_id(conn: &Connection) -> rusqlite::Result<String> {
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
pub fn periphery_repo_scope(
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
pub fn periphery_repo_scope_clause(scope: &Option<String>, qualifier: &str) -> String {
    match scope {
        Some(repo_id) => format!(" AND {qualifier}.repo_id = '{}'", repo_id.replace('\'', "''")),
        None => String::new(),
    }
}

/// The repo id INSTALLED in the per-connection scope context, or `None` when no context row exists
/// — the public probe for "is this connection scoped?". Unlike [`active_repo_id`] it NEVER falls
/// back to [`sole_repo_id`], so a caller can distinguish a genuinely scoped connection from the
/// config-less sole-repo pick (the manifest heal's witness needs exactly that distinction: on a
/// multi-repo DB the sole pick is an arbitrary first-sorting repo, not an honest attribution).
pub fn scope_context_repo_id(conn: &Connection) -> Option<String> {
    context_repo_id(conn)
}

/// Read the active repo id from the per-connection scope context, tolerating the common absence of
/// the `temp.connection_context` table (a raw connection with no view installed) — `.ok()` swallows
/// the "no such table" error into `None`, exactly like `edges::resolve`'s `scope_context_value`.
fn context_repo_id(conn: &Connection) -> Option<String> {
    connection_context_value(conn, CONNECTION_CONTEXT_REPO_KEY)
}

/// Read one key from the per-connection scope context (`temp.connection_context`), tolerating the
/// table's common absence (a raw connection with no scope view installed) as `None` — the shared
/// tolerant-read for every context key (`repo_id`, `worktree_id`, `commit_sha`, …).
pub fn connection_context_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM temp.connection_context WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
}

/// The LIVE `files.generation` for `repo_id` (A6), read from `repo_meta`. Absent or unparseable ⇒
/// `0`, which is the generation `DEFAULT 0` stamps every pre-V043 row and every never-restaged
/// index carries — so a fresh / upgraded index is visible under generation 0 with no `repo_meta`
/// write. The full rebuild advances this pointer atomically once its staged generation is complete.
pub fn live_files_generation(conn: &Connection, repo_id: &str) -> rusqlite::Result<i64> {
    let raw = crate::meta::repo_meta(conn, repo_id, LIVE_FILES_GENERATION_META_KEY)?;
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
pub fn active_generation(conn: &Connection) -> rusqlite::Result<i64> {
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
pub fn sole_repo_id(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row(
        "SELECT repo_id FROM repos ORDER BY (repo_id = ?1), repo_id LIMIT 1",
        [LEGACY_REPO_ID],
        |row| row.get(0),
    )
}

/// Whether this DB holds MORE THAN ONE real (adopted) repo — the interim guard for global-scope
/// destructive sweeps over tables that did not yet carry `repo_id` (superseded by V042's real
/// predicates) — and, since A7 made multi-repo the default, the bare-open fail-fast: a config-less
/// `IndexDatabase::open` refuses a DB where this is true rather than silently scoping to the
/// lexicographically-first repo, and `resolve_config_repo_id` declines its sole-repo fallback.
pub fn multiple_real_repos(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM repos WHERE repo_id != ?1",
        [LEGACY_REPO_ID],
        |row| row.get(0),
    )?;
    Ok(count > 1)
}

/// One REAL (non-placeholder) repo in this database's registry, with its recorded source roots —
/// the read-only shape `doctor`'s machine-global-store report lists. `roots` can be empty (identity
/// registered but no root recorded yet).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegisteredRepo {
    pub repo_id: String,
    pub display_name: String,
    pub roots: Vec<String>,
    pub registered_at_ms: i64,
}

/// List every REAL (non-placeholder) repo in the registry with its recorded roots, ordered by
/// display name then id — read-only, for `doctor`'s global-store overview. Excludes the
/// [`LEGACY_REPO_ID`] adoption placeholder.
pub fn registered_repos(conn: &Connection) -> rusqlite::Result<Vec<RegisteredRepo>> {
    let mut repos = conn
        .prepare(
            "SELECT repo_id, display_name, registered_at_ms FROM repos WHERE repo_id != ?1 ORDER \
             BY display_name, repo_id",
        )?
        .query_map([LEGACY_REPO_ID], |row| {
            Ok(RegisteredRepo {
                repo_id: row.get(0)?,
                display_name: row.get(1)?,
                registered_at_ms: row.get(2)?,
                roots: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut roots_stmt =
        conn.prepare("SELECT root FROM repo_roots WHERE repo_id = ?1 ORDER BY root")?;
    for repo in &mut repos {
        repo.roots = roots_stmt
            .query_map([&repo.repo_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(repos)
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
///  1. by IDENTITY — [`resolve_repo_identity`](rag_rat_base::repo_identity::resolve_repo_identity)
///     derives the id (honoring an `[index] repo_id` override); if it is a REGISTERED real repo,
///     use it. A derivable-but-UNREGISTERED id (a new/changed pin, or a now-portable shallow clone)
///     returns `None` — a changed identity must adopt/surface on the read-write path, never
///     silently keep serving the old scope.
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
pub fn resolve_config_repo_id(
    conn: &Connection,
    root: &Path,
    repo_id_override: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    match rag_rat_base::repo_identity::resolve_repo_identity(root, repo_id_override) {
        // Route 1: a derivable id that is registered scopes correctly even in a consolidated DB —
        // UNLESS this root is recorded under a DIFFERENT real repo. That is the read-only MIRROR
        // of `register_repo`'s root-owner refusal: after an `[index] repo_id` pin is switched to a
        // SIBLING's id, the write path refuses, but without this check the read-only fast path
        // (MCP reads, hooks) would silently serve the sibling's scope. Returning `None` declines
        // the fast path, so the caller falls back to the read-write open where the refusal
        // surfaces with its remedy.
        Ok(identity) if repo_id_is_registered(conn, &identity.repo_id)? => {
            let root_str = root.to_string_lossy();
            if real_root_owner(conn, &root_str, &identity.repo_id)?.is_some() {
                return Ok(None);
            }
            Ok(Some(identity.repo_id))
        },
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
pub fn repo_id_is_registered(conn: &Connection, repo_id: &str) -> rusqlite::Result<bool> {
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

/// The earliest-registered recorded root for `repo_id` (a representative "home" checkout to name
/// in the #427 join hint), or `None` when the repo has no recorded roots. Read-only.
pub fn earliest_recorded_root(
    conn: &Connection,
    repo_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT root FROM repo_roots WHERE repo_id = ?1 ORDER BY registered_at_ms, root LIMIT 1",
        params![repo_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
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

/// The single "this checkout is `repo_id`'s already-INDEXED home" signal, shared by every #427
/// consumer (the empty-index guard AND the same-identity-join warning). Keep both consumers on THIS
/// helper so a fix (or a footgun) can never apply to one and not the other.
///
/// Two indexing-only signals, either of which proves an indexing pass ran at this exact checkout:
/// 1. Its working-tree root is recorded in `repo_roots` for `repo_id`. Recording is now
///    INDEXING-ONLY (`register_repo` records; the read-only `register_repo_read_only` does not), so
///    a recorded root is trustworthy — immune BOTH to a read-only `open_config` merely registering
///    (the earlier fix's concern) AND to a same-identity sibling clone stealing recognition: each
///    checkout records its OWN `repo_roots` row, so B indexing the shared repo leaves A's row
///    intact (whereas the single-valued `source_root` is last-writer-wins — B's index overwrote it
///    to B, wrongly making A look un-indexed, #427 review).
/// 2. `repo_id`'s persisted `source_root` equals this root — the FALLBACK for an identity-less
///    (non-git / unborn) root, which never gets a `repo_roots` entry (`adopt_repo_from_config`
///    sole-picks WITHOUT `register_repo`). Also written only by an indexing pass.
pub fn repo_indexed_at_this_root(
    conn: &rusqlite::Connection,
    repo_id: &str,
    config: &Config,
) -> anyhow::Result<bool> {
    if crate::schema::repo_has_recorded_root(conn, repo_id, &config.root.to_string_lossy())? {
        return Ok(true);
    }
    Ok(crate::meta::repo_meta(conn, repo_id, "source_root")?
        == Some(config.root.display().to_string()))
}

/// [`is_root_already_indexed`] against an ALREADY-OPEN connection — used by the indexing paths that
/// hold a migrated read/write connection and must judge "already indexed" BEFORE they adopt (which
/// would record this root and defeat a post-adoption check). See that function for the
/// `source_root` rationale.
pub fn is_root_already_indexed_conn(
    conn: &rusqlite::Connection,
    config: &Config,
) -> anyhow::Result<bool> {
    // Primary: the repo this config resolves to (a registered identity, or the recorded-root / sole
    // fallback for an identity-less root) was last indexed at THIS root.
    if let Some(repo_id) =
        resolve_config_repo_id(conn, &config.root, config.repo_id_override.as_deref())?
    {
        return repo_indexed_at_this_root(conn, &repo_id, config);
    }
    // Fallback: the config's derived git identity is not registered yet, but a LEGACY index still
    // living under the `__unassigned__` placeholder (a pre-adoption DB that predates open-time
    // adoption) is single-repo and carries its `source_root` under that placeholder. Recognize it
    // via the SOLE repo so a legacy index whose files were all deleted PRUNES on the first upgrade
    // run instead of being refused as first-time-empty (the adoption that re-points the placeholder
    // runs right after this check, in `adopt_repo_from_config`). Guarded to a single-repo DB: a
    // multi-repo store has no sole placeholder, and matching on `source_root` means this can only
    // ever be TRUE for THIS root's own prior index — never a sibling's or the primary's.
    if !multiple_real_repos(conn)?
        && let Ok(sole) = sole_repo_id(conn)
    {
        return repo_indexed_at_this_root(conn, &sole, config);
    }
    Ok(false)
}
