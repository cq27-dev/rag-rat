//! The local account's roster as an operator sees it, for the device commands that recover from a
//! forked chain (#1417): which devices are enrolled, their roles and labels, which hold owner
//! authority, and which one this store is.

use rusqlite::{Connection, params};

use super::bootstrap::{self, LocalAccountRef};
use super::ops::{self, AccountOp, DecodedAccountOp, DeviceRole};
use super::{AccountId, envelope, id};
use crate::op::DeviceFingerprint;

/// One roster-effective device of the local account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterDevice {
    pub fingerprint: DeviceFingerprint,
    pub role: DeviceRole,
    /// The label its enrollment carried, if any.
    pub label: Option<String>,
    /// Whether it holds an open owner incarnation (the founder, an owner enrollment, a promotion).
    pub owner: bool,
    /// Whether it is this store's own device.
    pub this_device: bool,
}

/// The local account's roster-effective devices, founder first then in enrollment order. Empty
/// when the store has not minted an account yet.
pub fn local_account_roster(conn: &Connection) -> anyhow::Result<Vec<RosterDevice>> {
    let Some(LocalAccountRef { account_id, .. }) = bootstrap::local_account_ref(conn)? else {
        return Ok(Vec::new());
    };
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_foldable_account_control(conn, account_id)?;
    let this_device = crate::identity::local_device_fingerprint(conn)?;
    let mut stmt = conn.prepare(
        "SELECT r.device_fingerprint, r.role, e.signed_bytes,
                EXISTS(SELECT 1 FROM account_owner_incarnations o
                        WHERE o.account_id = r.account_id
                          AND o.device_fingerprint = r.device_fingerprint
                          AND o.closed_at IS NULL)
           FROM account_roster_history r
           LEFT JOIN account_entries e ON e.entry_hash = r.roster_ref
          WHERE r.account_id = ?1 AND r.closed_at IS NULL
          ORDER BY r.effective_at, r.device_fingerprint",
    )?;
    let rows = stmt
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut roster: Vec<RosterDevice> = Vec::with_capacity(rows.len());
    for (fingerprint, role, enrollment, owner) in rows {
        let fingerprint = DeviceFingerprint::from_bytes(id::fixed(&fingerprint)?);
        // One fingerprint can hold several open rows (concurrent winners); list it once.
        if roster.iter().any(|device| device.fingerprint == fingerprint) {
            continue;
        }
        roster.push(RosterDevice {
            fingerprint,
            role: DeviceRole::from_db_str(&role)?,
            label: enrollment.as_deref().and_then(enrollment_label),
            owner,
            this_device: this_device == Some(fingerprint),
        });
    }
    Ok(roster)
}

/// Whether `device` was removed from `account_id`: it held a roster seat and holds none now. A
/// removed fingerprint can never be enrolled again (the fold rejects a tombstoned re-add), so
/// enrollment refuses it up front and the joiner re-enrolls under a fresh identity (#1417).
pub fn device_was_removed(
    conn: &Connection,
    account_id: AccountId,
    device: DeviceFingerprint,
) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM account_roster_history
                        WHERE account_id = ?1 AND device_fingerprint = ?2
                          AND closed_at IS NOT NULL)
            AND NOT EXISTS(SELECT 1 FROM account_roster_history
                            WHERE account_id = ?1 AND device_fingerprint = ?2
                              AND closed_at IS NULL)",
        params![account_id.to_bytes().as_slice(), device.to_bytes().as_slice()],
        |row| row.get(0),
    )?)
}

/// The label the enrolling entry — a `DeviceAdd`, or the founder's genesis — carried. `None` for
/// an unlabelled enrollment or bytes that do not decode (the listing is informational).
fn enrollment_label(signed_bytes: &[u8]) -> Option<String> {
    let signed = envelope::decode_account_signed(signed_bytes).ok()?;
    match ops::decode(signed.header.entry_type, &signed.payload).ok()? {
        DecodedAccountOp::Known(
            AccountOp::DeviceAdd { label, .. } | AccountOp::AccountGenesis { label, .. },
        ) => label,
        _ => None,
    }
}

/// Resolve a device an operator typed: the full 64 hex digits, or a prefix of at least 8 that
/// names exactly one device on the local roster.
pub fn resolve_roster_device(conn: &Connection, input: &str) -> anyhow::Result<RosterDevice> {
    let wanted = input.trim().to_ascii_lowercase();
    anyhow::ensure!(
        wanted.len() >= 8 && wanted.chars().all(|c| c.is_ascii_hexdigit()),
        "name a device by its fingerprint: at least the first 8 hex digits (`rag-rat sync \
         devices` lists them)"
    );
    let mut matches = local_account_roster(conn)?
        .into_iter()
        .filter(|device| {
            rag_rat_base::hash::hex_lower(&device.fingerprint.to_bytes()).starts_with(&wanted)
        })
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => anyhow::bail!(
            "no enrolled device's fingerprint starts with `{wanted}` (`rag-rat sync devices` \
             lists them)"
        ),
        n => anyhow::bail!("`{wanted}` matches {n} devices; give more of the fingerprint"),
    }
}
