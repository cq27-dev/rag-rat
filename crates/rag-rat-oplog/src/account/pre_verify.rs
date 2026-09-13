//! The unauthenticated pre-verify park queues shared by the account (`account_pre_verify`) and
//! content (`content_pre_verify`) ingest paths.
//!
//! Both park an envelope whose signer is not yet known, keyed by `signed_hash` and owned by the
//! account it claims. The retention rule is one rule for both: oldest-first per-owner and global
//! budgets, ties broken by `signed_hash`, so replicas holding the same rows and timestamps retain
//! the same set.

use rusqlite::{Connection, params};

use super::AccountId;

/// One pre-verify park table: rows keyed by `signed_hash`, owned by `owner_column`.
pub(in crate::account) struct PreVerifyQueue {
    pub(in crate::account) table: &'static str,
    pub(in crate::account) owner_column: &'static str,
}

/// A retention budget and the capacity scope reported when it evicts.
pub(in crate::account) struct QueueBudget<S> {
    pub(in crate::account) max: usize,
    pub(in crate::account) scope: S,
}

/// What enforcing both budgets did to a just-inserted row.
pub(in crate::account) enum BudgetOutcome<S> {
    /// The row is still parked; `evicted` names each budget that dropped older rows.
    Parked { evicted: Vec<S> },
    /// Enforcing this budget evicted the inserted row itself.
    AtCapacity(S),
}

impl PreVerifyQueue {
    pub(in crate::account) fn contains(
        &self,
        conn: &Connection,
        signed_hash: &[u8; 32],
    ) -> rusqlite::Result<bool> {
        conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE signed_hash = ?1)", self.table),
            params![signed_hash.as_slice()],
            |row| row.get(0),
        )
    }

    pub(in crate::account) fn delete(
        &self,
        conn: &Connection,
        signed_hash: &[u8],
    ) -> rusqlite::Result<()> {
        conn.execute(&format!("DELETE FROM {} WHERE signed_hash = ?1", self.table), [signed_hash])?;
        Ok(())
    }

    /// Keep the queue within the per-owner then the global budget, evicting oldest-first. Ties use
    /// `signed_hash`, so replicas with the same rows and timestamps retain the same set.
    pub(in crate::account) fn enforce_budget<S: Copy>(
        &self,
        conn: &Connection,
        owner: AccountId,
        inserted_signed_hash: &[u8; 32],
        per_owner: QueueBudget<S>,
        global: QueueBudget<S>,
    ) -> rusqlite::Result<BudgetOutcome<S>> {
        let mut evicted = Vec::new();
        for (owner, QueueBudget { max, scope }) in [(Some(owner), per_owner), (None, global)] {
            if self.evict_oldest(conn, owner, max)? > 0 {
                evicted.push(scope);
            }
            if !self.contains(conn, inserted_signed_hash)? {
                return Ok(BudgetOutcome::AtCapacity(scope));
            }
        }
        Ok(BudgetOutcome::Parked { evicted })
    }

    /// Delete the oldest rows over `limit` — scoped to `owner`, or queue-wide for `None` — and
    /// return how many went.
    fn evict_oldest(
        &self,
        conn: &Connection,
        owner: Option<AccountId>,
        limit: usize,
    ) -> rusqlite::Result<usize> {
        let owner = owner.map(AccountId::to_bytes);
        let (table, owner_column) = (self.table, self.owner_column);
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE (?1 IS NULL OR {owner_column} = ?1)"),
            params![owner.as_ref().map(<[u8; 32]>::as_slice)],
            |row| row.get(0),
        )?;
        let limit = limit as i64;
        if count <= limit {
            return Ok(0);
        }
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE signed_hash IN (
                     SELECT signed_hash FROM {table}
                     WHERE (?1 IS NULL OR {owner_column} = ?1)
                     ORDER BY received_at_ms, signed_hash
                     LIMIT ?2
                 )"
            ),
            params![owner.as_ref().map(<[u8; 32]>::as_slice), count - limit],
        )
    }
}
