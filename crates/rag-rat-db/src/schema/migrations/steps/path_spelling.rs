use rusqlite::{Connection, params};

use crate::schema::migrations::column_exists;

/// Every table whose `worktree_id` column holds a CHECKOUT PATH — the scope key `worktree_id_of`
/// derives by canonicalizing a checkout directory. `files` is the load-bearing one (its
/// `(commit_sha, worktree_id)` pair is the active-scope view every read goes through); the other
/// three are keyed the same way, and GC prunes all four off the same live worktree set.
///
/// `migration_097_covers_every_worktree_id_column_in_the_schema` pins this list against the live
/// schema, so a table that grows a `worktree_id` cannot silently miss the rekey.
pub const V097_WORKTREE_ID_SCOPED_TABLES: &[&str] =
    &["files", "packages", "oracle_runs", "external_symbols"];

/// The `repo_meta` key prefix whose SUFFIX is a `worktree_id` — the overlay refresh basis, THE
/// same constant `rag-rat-core`'s overlay reads and writes. Rekeying the row's VALUE is not enough
/// here: the worktree identity is in the KEY, so a stale key is a basis record the rekeyed scope
/// can never find.
const V097_WORKTREE_OVERLAY_BASIS_PREFIX: &str = crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX;

/// Every meta key whose VALUE is a checkout path — the `repo_meta` / `index_meta` rows a freshly
/// canonicalized root is compared against TEXTUALLY.
///
/// Deliberately NOT here, having been checked one by one: `git_history_indexed_head` and
/// `git_commit` (commit hashes), `git_history_indexed_shallow` / `_complete` (flags),
/// `local_crate_roots` (Cargo crate NAMES), and everything model/embedding/FTS-related (ids,
/// versions, counters). `files.path` and `packages.manifest_dir` are stored RELATIVE to the root,
/// so a root respelling never reaches them.
///
/// `pub` for the same reason as [`V097_WORKTREE_ID_SCOPED_TABLES`]: a meta key is just a string, so
/// nothing about adding a path-valued one fails to compile.
/// `every_absolute_path_in_the_meta_bag_is_rekeyed_or_reviewed` walks a real index and requires
/// every absolute-path value to be either in this list or in an explicitly reviewed exception set.
pub const V097_PATH_VALUED_META_KEYS: &[&str] =
    &["source_root", crate::meta::GIT_HISTORY_INDEXED_ROOT_META];

/// The `repo_meta` freshness markers V098 deletes to force the next ordinary index pass to
/// re-derive the path-keyed rows an older binary keyed under a collapsed backslash spelling.
/// Deleting the marker is the lever, not deleting the table: each gates a re-derivation the indexer
/// already owns. `BASE_SCOPE_DISCOVERED_META` gone promotes the next pass to a full tree re-walk,
/// which replaces `files` and everything that CASCADES from it (chunks, symbols, edges, embeddings,
/// blame). `GIT_HISTORY_INDEXED_ROOT_META` gone fails `is_history_current`, forcing a full revwalk
/// that re-reads the commit and file-change rows and restamps the change couplings folded off its
/// freshness key. The `worktree_overlay_basis` keys are cleared separately (their suffix is a
/// worktree_id, so they need a prefix match, not an equality).
const V098_CLEARED_FRESHNESS_KEYS: &[&str] =
    &[crate::meta::BASE_SCOPE_DISCOVERED_META, crate::meta::GIT_HISTORY_INDEXED_ROOT_META];

/// V098 (#1032): make the next ordinary `index` pass re-walk the tree and reload git history, so a
/// store written before the Unix backslash-rendering fix re-derives its path-keyed rows off the
/// corrected spelling.
///
/// The pre-fix binary rendered a path by replacing every backslash with a separator — right on
/// Windows, where a backslash IS the separator, but wrong on Unix, where a literal backslash is an
/// ordinary filename byte. So `foo\bar.rs` was persisted as `foo/bar.rs`: one `files.path`, one
/// `path::name` symbol identity, shared with a genuinely nested `foo/bar.rs`. That rendering was
/// LOSSY, so unlike the Windows verbatim rekey (V097) the stored spelling cannot be repaired in
/// place — `foo/bar.rs` in the store cannot be told apart from a rewritten `foo\bar.rs`. Only a
/// re-walk off the corrected renderer recovers the truth, and this forces one by deleting the
/// freshness markers that would otherwise let the pass skip an unchanged file.
///
/// WHAT KEEPS A PRE-FIX BINARY OFF A STORE THIS HAS CONVERTED — and why that is the whole story.
/// The fence is the SCHEMA VERSION, the ladder's, not this migration's: recording V098 puts an id
/// in `schema_version` a pre-V098 binary does not know, so [`super::status`] answers `Newer` and
/// every open refuses. It reaches a resident process wherever it re-opens — a watcher pass, a CLI
/// or MCP read — because those re-open per operation. This is the SAME fence V097 relies on for the
/// sibling bug; that reasoning applies here unchanged, including the one bounded residual it
/// documents: a pass already past its status check when the upgrade commits can still write once
/// under the old rendering, self-healing on the following pass. That residual is accepted rather
/// than closed with cross-repo write-lock enumeration, which a consolidated store cannot order.
///
/// LEDGER-ATOMIC because the marker deletion and the `schema_version` stamp must never be
/// separately visible: a store whose markers are gone but whose V098 row has not landed still
/// answers `Compatible` to a pre-V098 binary, which would re-walk under the OLD renderer and
/// re-collapse the very spellings this exists to correct — for a moment on a healthy upgrade,
/// indefinitely after a crash between two commits. So the body takes the ladder's transaction and
/// opens none of its own.
///
/// DELIBERATELY LEFT to a read-time filter or an ordinary later pass, not swept here:
///  * the oracle verdict tables — `edge_oracle` is joined on `file_sha = files.sha256`, so a stale
///    row only resurfaces against a real sibling of identical content and edge geometry, and it
///    re-derives on the next `oracle run`;
///  * the clone posting family — rotated out by the next clone generation;
///  * durable path-anchored DECISION data — a persisted human/model choice keyed by a path, which a
///    reindex must not destroy. Two tables hold it today: `repo_memory_bindings` (re-anchored by
///    the relocation engine) and `papertrail_distill_anchors` rows with `selected = 1` (a model
///    selection the distill extractor preserves across a rerun of unchanged input, by its own V078
///    invariant — so quarantining or regenerating them here would lose the decision, which is why
///    the reindex leaves them). Any future table of this shape belongs in this bucket, not in the
///    sweep.
///
/// Reaching any of those needs a lossy collision with a real slash-spelled sibling AND a
/// pre-existing backslash-named Unix file — the near-impossible case the correctness fix stops from
/// ever recurring. Without a sibling the stale row simply points at a path no file has, and is
/// inert.
///
/// Runs on every platform: which spellings a store carries is a property of the store, not of the
/// host reading it, and the ladder is forward-only, so a skip would record V098 as applied without
/// doing the work.
pub fn apply_reindex_after_unix_backslash_rendering(conn: &Connection) -> rusqlite::Result<()> {
    for key in V098_CLEARED_FRESHNESS_KEYS {
        conn.execute("DELETE FROM repo_meta WHERE key = ?1", [key])?;
    }
    // The overlay basis lives under one key PER checkout, its worktree_id in the suffix — a prefix
    // match clears them all. The constant holds no `%`/`_`/`\`, so a bare LIKE needs no ESCAPE.
    conn.execute("DELETE FROM repo_meta WHERE key LIKE ?1 || '%'", [
        crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX,
    ])?;
    // The one path-keyed DERIVED table a file re-walk does not cascade: no FK to `files`, keyed by
    // a bare path, re-recorded per file on the next parse. Whole-table because every row
    // re-derives.
    conn.execute("DELETE FROM parser_failures", [])?;
    Ok(())
}

/// V097 (#1048): rewrite every persisted path spelling that the pre-fix `canonicalize` wrote in
/// the Windows `\\?\` VERBATIM form into the plain spelling this binary now produces.
///
/// The upgrade hazard this closes is silent on Windows. These stored strings are compared
/// TEXTUALLY against a freshly-canonicalized path:
///  * `worktree_id` — a canonicalized checkout path, carried by every linked worktree's overlay
///    rows and every dirty row (committed base rows are shared across checkouts under `worktree_id
///    = ''` and are not implicated). Once production answers `C:\…` and the rows still say
///    `\\?\C:\…`, those rows fall out of the active scope AND out of the GC live set, which is
///    built from the same fresh canonicalization: `garbage_collect` reads every stored id as a
///    checkout that no longer exists and DELETES its rows. Registered, live worktrees, pruned as
///    dead on the first maintenance pass after the upgrade.
///  * `repo_roots.root` / `repo_meta[source_root]` — `repo_indexed_at_this_root` is the "this
///    checkout was indexed here" signal behind the empty-index guard. A stale spelling makes an
///    established checkout look first-time, so an index run whose files have just been deleted is
///    refused as an accidental empty repo instead of pruning, and the deleted files' rows stay live
///    until the user finds `--allow-empty`.
///  * `repo_meta[git_history_indexed_root]` — the git-history reload gate's root cursor. A stale
///    spelling fails the `is_history_current` / `prepare_plan` comparison, so the first pass after
///    the upgrade takes the FULL path: the whole commit + file-change set is deleted and re-read
///    off a fresh revwalk, and the repo's blame cache is wiped with it. Self-healing after one
///    pass, but a minutes-long stall and a cold blame cache on a large repo.
///
/// One derived value is knowingly left to re-derive: `repo_meta[git_coupling_stamp]` folds the
/// history cursor snapshot (root spelling included) into its own freshness key, so rekeying the
/// cursor makes it stale. That is the change-coupling table's ordinary invalidation path — a
/// bounded window recompute, with an in-memory fallback that keeps reads correct meanwhile — and
/// the same recompute happens anyway on the next history apply. Rewriting a composite freshness
/// stamp from a migration would buy nothing and couple the ladder to that stamp's format.
///
/// Rewriting goes through `paths::rekeyed_from_verbatim`, the SAME rule production canonicalizes
/// with, not a blind prefix strip: verbatim form is still produced (and still correct) for UNC
/// shares, paths past `MAX_PATH`, and reserved DOS names, and rewriting those would BREAK the match
/// this exists to preserve.
///
/// WHAT KEEPS A PRE-UPGRADE BINARY OFF A STORE THIS HAS CONVERTED. Rekeying is only safe if no
/// binary that predates it can still write to, or garbage-collect, the rekeyed rows — an older
/// build derives the live worktree set from the OLD spelling, so it would read every rekeyed id as
/// a checkout that no longer exists. The fence is the SCHEMA VERSION, and it is the ladder's, not
/// this migration's: recording V097 puts a migration id in `schema_version` that a pre-V097 binary
/// does not know, so [`super::status`] answers `Newer` and every open refuses. It covers a RESIDENT
/// process, not just a fresh command, WHEREVER THAT PROCESS RE-OPENS, which every path that
/// indexes, queries, or garbage-collects does per operation: a watcher pass re-opens through
/// `open_and_migrate` at its start, inside the per-repo write flock and long before its gc stage,
/// so the pass after the upgrade fails at the gate with nothing written; the lighter watch-counter
/// flush tests `status() == Compatible` on its own connection before writing; CLI and MCP reads go
/// through `open_and_migrate` or `try_open_config_read_only`.
///
/// ONE resident writer does hold a connection across that check, and it is the reason this
/// paragraph is not a general rule: `sync serve` / `sync init` open the index once and then run the
/// accept loop on that same connection for the process's whole life, ingesting peer op-log entries
/// without re-checking the schema. A server started before the upgrade keeps writing to a store a
/// newer binary has converted. That is outside THIS migration's hazard for reasons specific to what
/// the loop writes, not because the fence reaches it: no table is registered for table-sync
/// (`SYNCABLE_TABLES` is empty), `repo_roots` is written only by the indexing path's
/// `register_repo`, and the loop runs no gc and derives no live-worktree set — so it can neither
/// prune a rekeyed row nor write a path-spelled column back in the old spelling. A FUTURE
/// data-converting migration over a table the op-log projection or table-sync touches must re-check
/// that for itself rather than inherit this conclusion.
///
/// That fence only holds if the conversion and the stamp are never separately visible, so this
/// migration is flagged `ledger_atomic`: the ladder runs the sweep inside the same IMMEDIATE
/// transaction that writes the `schema_version` row. Committed separately, the rekeyed store would
/// answer `Compatible` to a pre-V097 binary until the stamp landed — for a moment on a healthy
/// upgrade, indefinitely after a crash between the two commits — and every refusal above would wave
/// that binary through onto rows it reads as dead checkouts.
///
/// The one case the version cannot fence is a pass ALREADY past that check when the upgrade
/// commits. A new binary's INDEXING opens cannot cause it (they take the per-repo write flock
/// before migrating, so they wait behind the in-flight pass); only a non-indexing open — a query,
/// an MCP read — migrates under the global schema lock alone, which by design does not serialize
/// against per-repo writers. Deliberately left: the migration must not take per-repo flocks it
/// would have to enumerate and order across every repo in a consolidated store, and the exposure
/// is bounded — the rows at risk are derived overlay/dirty rows, which the next overlay refresh
/// re-derives, and committed base rows (`worktree_id = ''`, a live `commit_sha`) are outside it.
///
/// IT RUNS ON EVERY PLATFORM, not only on Windows. Which spellings a store carries is a property of
/// the STORE, not of the host reading it: one repository directory reachable from both a Windows
/// path and a WSL/container mount is one SQLite file, and whichever binary opens first is the one
/// that runs the ladder. Skipping the sweep off Windows would let that first opener record V097 as
/// applied without converting anything — and the ladder is forward-only, so the Windows binary
/// would never revisit it and would keep the spellings that get its rows collected as a dead
/// checkout, which is the failure this migration exists to prevent. `rekeyed_from_verbatim` decides
/// droppability textually for that reason. The cost of dropping the host skip is small and was
/// measured rather than assumed: the sweep's reads are `SELECT DISTINCT` over four `worktree_id`
/// columns, and `files` — the only large one — answers from `idx_files_worktree_path` as a covering
/// scan; on a ~2 GB twenty-repo store the whole sweep is tens of milliseconds, once, on the open
/// that upgrades it.
pub fn apply_windows_verbatim_path_rekey(conn: &Connection) -> rusqlite::Result<()> {
    rekey_persisted_path_spellings(conn, rag_rat_base::paths::rekeyed_from_verbatim)
}

/// [`apply_windows_verbatim_path_rekey`] with the spelling rule injected.
///
/// The rule is a parameter so the ROW-WALKING half — which columns and which meta keys the pass
/// covers — can be driven over a REAL index built on the host running the test. The production rule
/// only ever rewrites the Windows verbatim shape, which no checkout on a Unix CI runner is spelled
/// in; injecting a rule that maps the spellings such an index actually holds is what lets the Linux
/// leg observe the scope-restore and the GC-survival, rather than observing "nothing happened" and
/// passing just as well against a pass that covers no tables at all. `pub` for that reason alone.
///
/// No transaction of its own: the ladder runs this migration inside one and commits it WITH the
/// `schema_version` row, because a converted store that is not yet stamped still answers
/// `Compatible` to the pre-V097 binary the conversion locks out. Opening a nested transaction here
/// would fail outright, and committing one would reintroduce that window.
pub fn rekey_persisted_path_spellings(
    conn: &Connection,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    for table in V097_WORKTREE_ID_SCOPED_TABLES {
        if column_exists(conn, table, "worktree_id")? {
            rekey_column(conn, table, "worktree_id", rekey)?;
        }
    }
    if column_exists(conn, "repo_roots", "root")? {
        rekey_column(conn, "repo_roots", "root", rekey)?;
    }
    // These keys land in `repo_meta` at V039; a pre-V039 store still carries them in the global
    // `index_meta`, and the ladder replays in order, so by here they have moved. Both are swept
    // anyway — the pass is idempotent and a stale copy either table kept is equally stale. The
    // sweep is KEY-SCOPED: most meta values are not paths, and rewriting one that merely starts
    // with those bytes would corrupt it.
    for (table, column) in [("repo_meta", "value"), ("index_meta", "value")] {
        if column_exists(conn, table, column)? {
            for key in V097_PATH_VALUED_META_KEYS {
                rekey_meta_value_at_key(conn, table, key, rekey)?;
            }
        }
    }
    if column_exists(conn, "repo_meta", "key")? {
        rekey_worktree_overlay_basis_keys(conn, rekey)?;
    }
    Ok(())
}

/// Rewrite the stale spellings in `table.column` in place.
///
/// Reads the DISTINCT values first: a `worktree_id` column has one value per checkout however many
/// million rows carry it, so the rule runs a handful of times and each rewrite is one indexed
/// UPDATE.
///
/// `UPDATE OR IGNORE` because the target spelling can already be present. On the real upgrade it
/// cannot be — the old binary only ever wrote the verbatim form — but a store written by a MIX of
/// binaries can hold both, and there the plain-spelled row is the one production just wrote and
/// must win. Skipping leaves the verbatim row for GC, which is the correct disposition for a
/// superseded duplicate; the alternatives are worse in both directions (`OR REPLACE` would cascade
/// the live row's children away, a bare UPDATE would abort the upgrade on a constraint failure).
fn rekey_column(
    conn: &Connection,
    table: &str,
    column: &str,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    let stored: Vec<String> = {
        let mut stmt = conn
            .prepare(&format!("SELECT DISTINCT {column} FROM main.{table} WHERE {column} != ''"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old in stored {
        let Some(new) = rekey(&old) else { continue };
        conn.execute(
            &format!("UPDATE OR IGNORE main.{table} SET {column} = ?1 WHERE {column} = ?2"),
            params![new, old],
        )?;
    }
    Ok(())
}

/// Rewrite the VALUE of a single meta row whose key is `key` — the `source_root` case, where the
/// path is the value rather than part of the key.
fn rekey_meta_value_at_key(
    conn: &Connection,
    table: &str,
    key: &str,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    let stored: Vec<String> = {
        let mut stmt =
            conn.prepare(&format!("SELECT DISTINCT value FROM main.{table} WHERE key = ?1"))?;
        let rows = stmt.query_map([key], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old in stored {
        let Some(new) = rekey(&old) else { continue };
        conn.execute(
            &format!("UPDATE OR IGNORE main.{table} SET value = ?1 WHERE key = ?2 AND value = ?3"),
            params![new, key, old],
        )?;
    }
    Ok(())
}

/// Rewrite the overlay-basis rows whose KEY embeds a stale `worktree_id`
/// (`worktree_overlay_basis:<worktree_id>`).
///
/// Left behind, the basis record is unreachable under the rekeyed scope, and the overlay refresh
/// reads a missing basis as "never refreshed" — correct but wasteful (a full re-derive per linked
/// checkout). The GC that prunes basis rows outside the live worktree set would then delete it,
/// which is harmless once the row is orphaned but leaves the quiet-window anchor gone. Moving the
/// key keeps the basis attached to the checkout it describes.
fn rekey_worktree_overlay_basis_keys(
    conn: &Connection,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    let stored: Vec<String> = {
        // `\` is not a LIKE metacharacter in SQLite, but `_` in the prefix is — match on the
        // literal prefix with `substr` instead of relying on an ESCAPE clause.
        let mut stmt =
            conn.prepare("SELECT DISTINCT key FROM main.repo_meta WHERE substr(key, 1, ?1) = ?2")?;
        let prefix_len = V097_WORKTREE_OVERLAY_BASIS_PREFIX.len() as i64;
        let rows = stmt
            .query_map(params![prefix_len, V097_WORKTREE_OVERLAY_BASIS_PREFIX], |row| {
                row.get::<_, String>(0)
            })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old_key in stored {
        let Some(worktree_id) = old_key.strip_prefix(V097_WORKTREE_OVERLAY_BASIS_PREFIX) else {
            continue;
        };
        let Some(rekeyed) = rekey(worktree_id) else { continue };
        conn.execute("UPDATE OR IGNORE main.repo_meta SET key = ?1 WHERE key = ?2", params![
            format!("{V097_WORKTREE_OVERLAY_BASIS_PREFIX}{rekeyed}"),
            old_key
        ])?;
    }
    Ok(())
}
