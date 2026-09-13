//! Pre-index, read-only hints the one-shot `index` command surfaces BEFORE it registers or
//! indexes anything (issue #427): whether any file would be discovered, and whether the configured
//! root is joining an already-registered repo's scope. Detection only — never mutates.

use std::path::PathBuf;

use rag_rat_base::config::Config;
use rag_rat_base::repo_identity;
use rag_rat_db::storage::IndexConnection;

use super::schema;

/// `true` iff indexing `config` would walk at least one target file. `false` means the index would
/// register the repo with EMPTY content — the exact zero-`[target_bindings]` footgun of #427. A
/// one-shot walk is fine here: the `index` command that calls this is about to walk anyway.
pub fn would_discover_any_file(config: &Config) -> anyhow::Result<bool> {
    // Cheap short-circuit for the issue's headline case (no `[target_bindings]` → no targets):
    // skip the filesystem walk entirely.
    if config.targets.is_empty() {
        return Ok(false);
    }
    Ok(!super::prep::collect_index_files(config)?.is_empty())
}

/// Open the index database read-only IFF it exists and its schema is READABLE (current or
/// migrateable-forward); else `None`. Shared by the read-only pre-index hints so none of them abort
/// `index()` on a fresh, never-written, garbage, or unreadable-schema database — SQLite defers
/// header validation to the first page read, so a non-DB file opens fine and only faults on the
/// `status` read, which is folded to `None` here.
///
/// An `Older` (migrateable-forward) schema is accepted ONLY once the repo-registry tables it reads
/// exist. Accepting `Older` at all is deliberate: the `repos` / `repo_roots` / `repo_meta` rows the
/// `source_root` probe reads are stable across recent migrations, so rejecting a merely-behind DB
/// would misclassify an already-indexed repo on a pre-upgrade database as "never indexed" and
/// wrongly REFUSE a delete-to-empty prune as a first-time-empty registration (#427 review). But the
/// registry itself was ADDED in V038, and these read-only probes run BEFORE the normal write-open
/// migration — so a database OLDER than V038 is `Older` yet has NO `repos` table. Returning a
/// connection there would fault the probe with `no such table: repos` on `rag-rat index`/`--full`
/// instead of letting the write path migrate + index (#427 review). Gate on the table existing:
/// pre-registry `Older` DBs → `None` (treated as not-indexed / no join hint, exactly as before this
/// widening; the write path then migrates them). `Newer` / `Dirty` / `Missing` / garbage → `None`.
fn open_ro_compatible(config: &Config) -> Option<IndexConnection> {
    if !config.database.exists() {
        return None;
    }
    let storage = IndexConnection::open_read_only_blocking(&config.database).ok()?;
    let readable = match schema::status(storage.connection()) {
        Ok(status) => match status.state {
            schema::SchemaState::Compatible => true,
            // Pre-V038 databases lack the registry these probes read; fall through to migration.
            schema::SchemaState::Older =>
                schema::table_exists(storage.connection(), "repos").unwrap_or(false),
            _ => false,
        },
        Err(_) => false,
    };
    readable.then_some(storage)
}

/// The configured root is a NEW physical checkout sharing an already-registered repo's identity —
/// the same-identity clone / not-yet-anchored worktree of #427.
#[derive(Debug, Clone)]
pub struct SameIdentityJoin {
    /// The registered repo whose scope this checkout would join.
    pub repo_id: String,
    /// A representative recorded root of that repo (its earliest-registered checkout).
    pub existing_root: PathBuf,
}

/// `Some` when indexing `config` would fold its root into an ALREADY-REGISTERED repo's single scope
/// because they share a portable identity, and this root is not yet one of that repo's recorded
/// checkouts (#427). `None` — no warning — for a fresh DB, an incompatible schema, an
/// identity-less root, an unregistered identity, or a re-index of a KNOWN checkout. Read-only:
/// opens the DB `SQLITE_OPEN_READ_ONLY` and never writes. Returns `None` generously on any error
/// resolving identity (a false join warning is worse than a missed one).
pub fn same_identity_join_note(config: &Config) -> anyhow::Result<Option<SameIdentityJoin>> {
    // Fresh / never-written / garbage / incompatible DB → there is no existing repo to join.
    let Some(storage) = open_ro_compatible(config) else {
        return Ok(None);
    };
    let conn = storage.connection();
    let identity = match repo_identity::resolve_repo_identity(
        &config.root,
        config.repo_id_override.as_deref(),
    ) {
        Ok(identity) => identity,
        Err(_) => return Ok(None), // absent / rejected identity → no join hint
    };
    // Only a repo that is ALREADY registered can be "joined". A brand-new identity is a fresh repo.
    if !schema::repo_id_is_registered(conn, &identity.repo_id)? {
        return Ok(None);
    }
    // This exact checkout was already INDEXED as this repo → an ordinary re-index, not a join.
    // Via the shared indexing-only helper: recording is indexing-only, so a read-only `open_config`
    // (doctor / MCP) of a fresh same-identity clone does NOT suppress the warning — the first real
    // index from that clone still gets the `[index] repo_id` guidance (#427 review).
    if rag_rat_db::schema::repo_indexed_at_this_root(conn, &identity.repo_id, config)? {
        return Ok(None);
    }
    let existing = schema::earliest_recorded_root(conn, &identity.repo_id)?;
    Ok(existing.map(|existing_root| SameIdentityJoin {
        repo_id: identity.repo_id,
        existing_root: PathBuf::from(existing_root),
    }))
}

/// Whether THIS checkout has actually been INDEXED before — the signal that scopes the zero-file
/// refusal (#427) to FIRST-TIME empty registrations. An already-indexed root whose last file was
/// just deleted or moved must still be allowed to index, so the incremental / discover pass records
/// the now-empty set (applies the deletion plan) instead of stranding the old rows live; only a
/// brand-new checkout landing empty is the footgun worth refusing.
///
/// Judged by an INDEXING-ONLY signal — a `repo_roots` recording (git repos) or the persisted
/// `source_root` (non-git fallback) — see [`repo_indexed_at_this_root`]. Both are written only by a
/// real indexing pass, never by a read-only `open_config` (a `doctor` / MCP / query open now
/// registers identity via `register_repo_read_only`, which records NO root), so a mere read can't
/// make an un-indexed checkout — or a fresh same-identity CLONE — look indexed and let a later
/// empty `--discover` / `--full` prune the shared scope (#427 review). The per-checkout
/// `repo_roots` row (not the single-valued `source_root`) is also what keeps a same-identity
/// SIBLING clone's index from stealing this checkout's recognition. `Ok(false)` on a fresh /
/// never-written / garbage / unreadable database.
pub fn is_root_already_indexed(config: &Config) -> anyhow::Result<bool> {
    let Some(storage) = open_ro_compatible(config) else {
        return Ok(false);
    };
    rag_rat_db::schema::is_root_already_indexed_conn(storage.connection(), config)
}

/// Whether indexing `config` would FIRST-TIME-register an EMPTY repo — this checkout was not
/// indexed before AND no target files would be discovered. The #427 condition the two INDEXING
/// entry points check before they adopt: the one-shot `index` command turns it into an error
/// (unless `--allow-empty`), and the background paths (watcher, git-hook `maintenance`) DEFER on it
/// (skip, waiting for content) so `rag-rat mcp` / a hook on a misconfigured repo never silently
/// registers an empty index. The cheap already-indexed lookup runs first (via `&&` short-circuit),
/// so an established repo never pays the discovery walk. Read-only.
pub fn is_first_time_empty(config: &Config) -> anyhow::Result<bool> {
    Ok(!is_root_already_indexed(config)? && !would_discover_any_file(config)?)
}

/// [`is_first_time_empty`] against an ALREADY-OPEN connection (the incremental path's migrated
/// bare-open connection), checked BEFORE `adopt_repo_from_config` records the root.
pub fn is_first_time_empty_conn(
    conn: &rusqlite::Connection,
    config: &Config,
) -> anyhow::Result<bool> {
    Ok(!rag_rat_db::schema::is_root_already_indexed_conn(conn, config)?
        && !would_discover_any_file(config)?)
}

/// The core registration path (`rebuild_with_progress`) refused to FIRST-TIME-register an EMPTY
/// index (#427). This is the SINGLE enforcement of the empty-index invariant; callers react to it:
/// the one-shot `index` command surfaces it to the operator, while the background paths (watcher,
/// git-hook `maintenance`) discard it and wait for content. `--allow-empty` (→
/// `Config::allow_empty`) opts in, and this is then never produced.
#[derive(Debug, thiserror::Error)]
#[error(
    "0 files discovered under {root} — no `[target_bindings]` configured (or none match).\nAdd a \
     section like:\n\n    [target_bindings]\n    rust = [\"src\"]\n\nto rag-rat.toml, or pass \
     `--allow-empty` to index nothing."
)]
pub struct EmptyIndexRefused {
    pub root: String,
}

#[cfg(test)]
mod tests;
