//! Domain hooks the schema layer calls during migrations and repo adoption.
//!
//! Migrations occasionally rebuild DERIVED data whose canonical builder lives in a domain crate
//! above this one (dream finding ids, the papertrail FTS mirror, account-derived projections,
//! logical-symbol graph realignment). Those builders are intentionally NOT copied
//! into the migration ladder — a derived-data rebuild must run the shipped, current builder so
//! the rebuilt rows match what the domain writes at runtime. The domain crates can't be
//! dependencies of this one (they depend on it), so the callers supply the builders through this
//! struct at every migrate/adopt entry point.
//!
//! Plain function pointers, not closures: every hook is a free function with no state, and fn
//! pointers keep the struct `Copy` and trivially shareable across the lock-holding entry points.

use rusqlite::{Connection, Transaction};

/// The domain builders the schema layer may invoke. Constructed once by the engine crate
/// (`migration_hooks()` there) and passed into every entry point that can run migrations or
/// adoption.
#[derive(Clone, Copy)]
pub struct MigrationHooks {
    /// Rebuild dream finding ids after a schema change to their derivation inputs
    /// (dream's `rederive_finding_ids`).
    pub rederive_dream_finding_ids: fn(&Connection) -> rusqlite::Result<()>,
    /// Rebuild account-derived projections from the op-log
    /// (oplog's `backfill_authority_projection`).
    pub backfill_authority_projection: fn(&Transaction<'_>) -> rusqlite::Result<()>,
    /// Rebuild the papertrail FTS mirror (papertrail's `rebuild_fts`).
    pub rebuild_papertrail_fts: fn(&Connection) -> rusqlite::Result<()>,
    /// One-time V113 purge of `/3` candidates violating the lamport clamp, plus their dependent
    /// chain tails and over-ceiling pre-verify rows (oplog's `purge_legacy_lamport_violators`).
    /// Lives behind a hook because the lamport sits inside the signed CBOR envelope, which the
    /// migration ladder's SQL cannot decode.
    pub purge_legacy_lamport_violators: fn(&Connection) -> rusqlite::Result<()>,
    /// Re-align logical-symbol ids after adoption re-points repo-scoped rows
    /// (graph_index's `realign_logical_symbol_ids`). Returns the realigned-row count.
    pub realign_logical_symbol_ids: fn(&Connection) -> rusqlite::Result<usize>,
}

impl MigrationHooks {
    /// Hooks that rebuild nothing. Sound ONLY where no derived domain data can exist — fresh
    /// scratch databases in tests and schema-only tooling. Production opens must pass the engine
    /// crate's real builders: on a lived-in database these no-ops would leave papertrail FTS,
    /// dream finding ids, or the authority projection stale after the migrations that rebuild
    /// them.
    pub fn noop() -> Self {
        MigrationHooks {
            rederive_dream_finding_ids: |_| Ok(()),
            backfill_authority_projection: |_| Ok(()),
            rebuild_papertrail_fts: |_| Ok(()),
            purge_legacy_lamport_violators: |_| Ok(()),
            realign_logical_symbol_ids: |_| Ok(0),
        }
    }
}
