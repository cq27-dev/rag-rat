//! The op-log's persisted local device identity (phase B, #513).
//!
//! Exactly ONE ed25519 keypair per store, minted from OS entropy on first use and persisted so it
//! is stable for the life of the index — every entry this install authors (live or backfilled)
//! signs under the same fingerprint instead of a fresh per-process key. Store-global, NOT
//! repo-scoped: a device is a machine identity, orthogonal to the per-repo owner streams it signs.
//!
//! [`local_device`] is the single accessor: it returns the persisted identity, minting-and-storing
//! it on the first call. The single-row `oplog_device_identity` table (`CHECK (id = 0)`) plus an
//! `ON CONFLICT DO NOTHING` insert and a mandatory re-read make a concurrent first-open converge on
//! one identity — the process that loses the insert race silently adopts the winner's key and
//! discards its own. A load re-derives the key from the stored seed and asserts the stored
//! `public_key` / `fingerprint` still agree, so a corrupted row is refused rather than signed
//! under.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use zeroize::Zeroizing;

use super::device::{DevicePublic, DeviceSecret};
use super::op::DeviceFingerprint;

/// The store's local signing identity: the secret (for signing), its verifying key, and the opaque
/// op-model fingerprint. Owns the secret, so it is neither `Clone` nor `Debug`-printable.
pub(crate) struct LocalDevice {
    secret: DeviceSecret,
    public: DevicePublic,
    fingerprint: DeviceFingerprint,
}

impl LocalDevice {
    /// The signing capability — the authoring seam signs entry bodies with this. `pub(super)`: the
    /// secret (and `DeviceSecret` itself) stay oplog-internal; the store's authoring path is the
    /// only caller. Cross-module callers author under a whole `&LocalDevice`, never the raw secret.
    pub(super) fn secret(&self) -> &DeviceSecret {
        &self.secret
    }

    /// The verifying key (for a local self-verify). `pub(super)` to match `DevicePublic`'s
    /// oplog-internal visibility.
    pub(super) fn public(&self) -> DevicePublic {
        self.public
    }

    /// The opaque device fingerprint the op model + signed entry body carry. `pub(crate)` — the
    /// authoring seam in `query::memory` passes it to `chain_tail`; `DeviceFingerprint` is likewise
    /// crate-visible.
    pub(crate) fn fingerprint(&self) -> DeviceFingerprint {
        self.fingerprint
    }
}

/// Return this store's persisted device identity, minting + persisting one on the first call.
/// Idempotent: every later call returns the SAME identity (a stable fingerprint). `now_ms` stamps a
/// freshly-minted row (injected, matching the op-log store convention); it is ignored once an
/// identity already exists.
pub(crate) fn local_device(conn: &Connection, now_ms: i64) -> anyhow::Result<LocalDevice> {
    if let Some(device) = read_identity(conn)? {
        return Ok(device);
    }
    // First use: mint from OS entropy and persist. Under a concurrent first open another process
    // may win the insert; `ON CONFLICT DO NOTHING` makes our write a no-op and the mandatory
    // re-read below returns whichever identity actually landed — so we adopt the incumbent and drop
    // our own freshly-minted seed rather than ever holding a key the store didn't record.
    let fresh = DeviceSecret::generate()?;
    persist_identity(conn, &fresh, now_ms)?;
    read_identity(conn)?
        .context("device identity missing immediately after persist (single-row insert lost?)")
}

/// Insert the identity as the sole row (`id = 0`); a no-op if one already exists.
fn persist_identity(conn: &Connection, secret: &DeviceSecret, now_ms: i64) -> anyhow::Result<()> {
    let public = secret.public();
    conn.execute(
        "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
         VALUES (0, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO NOTHING",
        params![
            secret.seed().as_slice(),
            public.to_bytes().as_slice(),
            public.fingerprint().to_bytes().as_slice(),
            now_ms
        ],
    )?;
    Ok(())
}

/// Read the sole identity row, re-deriving the key from its seed and asserting the stored derived
/// columns still agree. `None` when no identity has been minted yet.
fn read_identity(conn: &Connection) -> anyhow::Result<Option<LocalDevice>> {
    let row = conn
        .query_row(
            "SELECT seed, public_key, fingerprint FROM oplog_device_identity WHERE id = 0",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((seed, stored_public, stored_fingerprint)) = row else {
        return Ok(None);
    };

    let seed: [u8; 32] =
        seed.as_slice().try_into().context("stored device seed is not exactly 32 bytes")?;
    let seed = Zeroizing::new(seed);
    let secret = DeviceSecret::from_seed(&seed);
    let public = secret.public();
    // The seed is the source of truth; the derived columns are a legibility copy. If they disagree
    // the row was corrupted or hand-edited — refuse it rather than sign under a mismatched
    // identity.
    anyhow::ensure!(
        public.to_bytes().as_slice() == stored_public.as_slice(),
        "stored device public_key does not match the key derived from its seed"
    );
    let fingerprint = public.fingerprint();
    anyhow::ensure!(
        fingerprint.to_bytes().as_slice() == stored_fingerprint.as_slice(),
        "stored device fingerprint does not match sha256(public_key)"
    );
    Ok(Some(LocalDevice { secret, public, fingerprint }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;

    /// A migrated in-memory DB carrying the V054 identity table.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::apply(&conn).expect("apply schema");
        conn
    }

    /// A fixed receipt time — the identity tests never assert on it.
    fn now() -> i64 {
        1_700_000_000_000
    }

    #[test]
    fn first_call_mints_and_persists_exactly_one_identity() {
        let conn = conn();
        let device = local_device(&conn, now()).expect("mint identity");
        // Exactly one row, and it is the identity we returned.
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM oplog_device_identity", [], |r| r.get(0))
            .expect("count rows");
        assert_eq!(rows, 1, "first call persists exactly one identity row");
        // The returned fingerprint is sha256(pubkey) of the returned key.
        assert_eq!(device.fingerprint().to_bytes(), device.public().fingerprint().to_bytes());
    }

    #[test]
    fn second_call_returns_the_same_stable_identity() {
        let conn = conn();
        let first = local_device(&conn, now()).expect("mint identity");
        // A later call (a different `now_ms`, as a reopen would have) returns the SAME identity and
        // adds no second row — the fingerprint is stable for the life of the store.
        let second = local_device(&conn, now() + 5_000).expect("re-read identity");
        assert_eq!(first.fingerprint().to_bytes(), second.fingerprint().to_bytes());
        assert_eq!(first.public().to_bytes(), second.public().to_bytes());
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM oplog_device_identity", [], |r| r.get(0))
            .expect("count rows");
        assert_eq!(rows, 1, "re-reading must not mint a second identity");
    }

    #[test]
    fn stored_derived_columns_match_the_seed() {
        let conn = conn();
        local_device(&conn, now()).expect("mint identity");
        // Independently re-derive the key from the stored seed and confirm the persisted
        // public_key + fingerprint agree — the legibility copy is not allowed to drift from truth.
        let (seed, public_key, fingerprint): (Vec<u8>, Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT seed, public_key, fingerprint FROM oplog_device_identity WHERE id = 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("read row");
        let seed: [u8; 32] = seed.as_slice().try_into().expect("32-byte seed");
        let derived = DeviceSecret::from_seed(&seed).public();
        assert_eq!(derived.to_bytes().as_slice(), public_key.as_slice());
        assert_eq!(derived.fingerprint().to_bytes().as_slice(), fingerprint.as_slice());
    }

    #[test]
    fn a_row_with_an_existing_identity_is_adopted_not_replaced() {
        // Model the race outcome: a row already exists when the mint path's insert runs. The
        // `ON CONFLICT DO NOTHING` + re-read must return the INCUMBENT, never overwrite it.
        let conn = conn();
        let incumbent = local_device(&conn, now()).expect("incumbent identity");
        // A second `persist_identity` with a different key must be a no-op (the id=0 conflict).
        let intruder = DeviceSecret::generate().expect("csprng");
        assert_ne!(incumbent.public().to_bytes(), intruder.public().to_bytes());
        persist_identity(&conn, &intruder, now() + 1).expect("conflicting insert is a no-op");
        let after = local_device(&conn, now() + 2).expect("re-read");
        assert_eq!(
            incumbent.public().to_bytes(),
            after.public().to_bytes(),
            "the incumbent identity survives a conflicting insert"
        );
    }

    #[test]
    fn a_corrupted_public_key_column_is_refused() {
        let conn = conn();
        local_device(&conn, now()).expect("mint identity");
        // Corrupt the derived public_key so it no longer matches the seed; a load must refuse it
        // rather than sign under a mismatched identity.
        conn.execute("UPDATE oplog_device_identity SET public_key = ?1 WHERE id = 0", params![
            [0u8; 32].as_slice()
        ])
        .expect("corrupt row");
        // `LocalDevice` is deliberately not `Debug` (it holds secret material), so drop the Ok
        // value before `expect_err`.
        let err =
            local_device(&conn, now()).map(|_| ()).expect_err("corrupted identity must be refused");
        assert!(err.to_string().contains("public_key"), "error names the mismatched column");
    }
}
