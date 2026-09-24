//! The op-log's persisted local device identity (phase B, #513).
//!
//! Exactly ONE ed25519 keypair per store, minted from OS entropy on first use and persisted so it
//! is stable for the life of the index — every entry this install authors (live or backfilled)
//! signs under the same fingerprint instead of a fresh per-process key. The one exception is
//! re-enrollment: a store whose device was removed from its account stages a fresh identity
//! ([`stage_reenrollment_identity`]) that the enrollment adoption swaps in, keeping only the old
//! X25519 key (`oplog_retired_identities`) to read history sealed to it. Store-global, NOT
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

use super::device::{DevicePublic, DeviceSecret, DeviceX25519Public, DeviceX25519Secret};
use super::op::DeviceFingerprint;

/// The store's local device identity: the ed25519 signing key (secret + verifying key + opaque
/// fingerprint) AND the X25519 encryption key (sync phase C, §5). Owns both secrets, so it is
/// neither `Clone` nor `Debug`-printable.
pub struct LocalDevice {
    secret: DeviceSecret,
    public: DevicePublic,
    fingerprint: DeviceFingerprint,
    // The X25519 encryption keypair: account bootstrap publishes the public half, content-key
    // sealing wraps to it and unwraps with the secret (`account::secrets`), and discovery
    // decrypts with the secret alone.
    x25519_secret: DeviceX25519Secret,
    x25519_public: DeviceX25519Public,
}

impl LocalDevice {
    /// The persisted ed25519 public key used to identify this store during device enrollment.
    /// Exposes only public material; the signing secret remains oplog-internal.
    pub fn ed25519_public_key(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// The persisted X25519 public key to which this store's content-key wraps are sealed during
    /// device enrollment. Exposes only public material; the decryption secret remains
    /// oplog-internal.
    pub fn x25519_public_key(&self) -> [u8; 32] {
        self.x25519_public.to_bytes()
    }

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

    /// The opaque device fingerprint the op model + signed entry body carry. `pub` — the
    /// authoring seam in `query::memory` passes it to `chain_tail`; `DeviceFingerprint` is likewise
    /// crate-visible.
    pub fn fingerprint(&self) -> DeviceFingerprint {
        self.fingerprint
    }

    /// The device's X25519 public encryption key — what account bootstrap publishes and content-key
    /// wraps are sealed to.
    pub(super) fn x25519_public(&self) -> DeviceX25519Public {
        self.x25519_public
    }

    /// The device's X25519 secret — the sealed-box unwrap / ECDH side of content-key sealing.
    pub(super) fn x25519_secret(&self) -> &DeviceX25519Secret {
        &self.x25519_secret
    }

    /// The unwrap side of this identity, for content-key recovery.
    pub(super) fn recipient(&self) -> Recipient<'_> {
        Recipient {
            fingerprint: self.fingerprint,
            x25519_secret: &self.x25519_secret,
            x25519_public: self.x25519_public,
        }
    }

    /// Consume the identity for its X25519 secret alone, dropping the signing key with the rest.
    /// For a caller that decrypts but never signs and outlives the load — holding the whole
    /// identity there would keep the ed25519 secret alive for no reason.
    pub(super) fn into_x25519_secret(self) -> DeviceX25519Secret {
        self.x25519_secret
    }
}

/// A key content-key wraps are addressed to: a fingerprint and the X25519 keypair that opens wraps
/// naming it. It carries no signing key, so it cannot reach an authoring seam.
pub(super) struct Recipient<'a> {
    pub(super) fingerprint: DeviceFingerprint,
    pub(super) x25519_secret: &'a DeviceX25519Secret,
    pub(super) x25519_public: DeviceX25519Public,
}

/// A device identity this store replaced when it re-enrolled after removal (#1417): only the
/// fingerprint and X25519 key, kept to open content keys wrapped to it before the re-enrollment.
pub(super) struct RetiredIdentity {
    fingerprint: DeviceFingerprint,
    x25519_secret: DeviceX25519Secret,
    x25519_public: DeviceX25519Public,
}

impl RetiredIdentity {
    pub(super) fn recipient(&self) -> Recipient<'_> {
        Recipient {
            fingerprint: self.fingerprint,
            x25519_secret: &self.x25519_secret,
            x25519_public: self.x25519_public,
        }
    }
}

/// Every identity this store retired, oldest first. Read-only: decryption paths call it.
pub(super) fn retired_identities(conn: &Connection) -> anyhow::Result<Vec<RetiredIdentity>> {
    let mut stmt = conn.prepare(
        "SELECT fingerprint, x25519_secret, x25519_public FROM oplog_retired_identities
          ORDER BY retired_at_ms, fingerprint",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(fingerprint, secret, public)| {
            let fingerprint: [u8; 32] = fingerprint
                .as_slice()
                .try_into()
                .context("retired fingerprint is not exactly 32 bytes")?;
            let (x25519_secret, x25519_public) = read_x25519(Some(secret), Some(public))?
                .context("a retired identity always carries its X25519 key")?;
            Ok(RetiredIdentity {
                fingerprint: DeviceFingerprint::from_bytes(fingerprint),
                x25519_secret,
                x25519_public,
            })
        })
        .collect()
}

/// Stage the identity this store will re-enroll under, reusing one already staged (#1417). Both
/// keys are fresh: a forked copy of this store elsewhere holds the old X25519 secret and must not
/// open what is wrapped to the new identity. Nothing changes the live identity until
/// [`adopt_pending_identity_in_tx`] runs inside a successful adoption. Returns the staged public
/// keys, `(ed25519, x25519)`.
pub fn stage_reenrollment_identity(
    conn: &Connection,
    now_ms: i64,
) -> anyhow::Result<([u8; 32], [u8; 32])> {
    let ed = DeviceSecret::generate()?;
    let x = DeviceX25519Secret::generate()?;
    conn.execute(
        "INSERT INTO oplog_pending_identity(id, seed, x25519_secret, created_at_ms)
         VALUES (0, ?1, ?2, ?3)
         ON CONFLICT(id) DO NOTHING",
        params![ed.seed().as_slice(), x.secret_bytes().as_slice(), now_ms],
    )?;
    pending_identity_keys(conn)?.context("staged identity missing immediately after insert")
}

/// The public keys of the staged re-enrollment identity, `(ed25519, x25519)`, if one is staged.
pub fn pending_identity_keys(conn: &Connection) -> anyhow::Result<Option<([u8; 32], [u8; 32])>> {
    Ok(read_pending(conn)?.map(|(ed, x)| (ed.public().to_bytes(), x.public().to_bytes())))
}

fn read_pending(conn: &Connection) -> anyhow::Result<Option<(DeviceSecret, DeviceX25519Secret)>> {
    let row = conn
        .query_row(
            "SELECT seed, x25519_secret FROM oplog_pending_identity WHERE id = 0",
            [],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    row.map(|(seed, x)| {
        let seed: [u8; 32] =
            seed.as_slice().try_into().context("staged seed is not exactly 32 bytes")?;
        let x: [u8; 32] =
            x.as_slice().try_into().context("staged x25519 secret is not exactly 32 bytes")?;
        let (seed, x) = (Zeroizing::new(seed), Zeroizing::new(x));
        Ok((DeviceSecret::from_seed(&seed), DeviceX25519Secret::from_seed(&x)))
    })
    .transpose()
}

/// Inside the adoption that enrolls `enrolled`: if it is the staged identity, retire the live one
/// and swap the staged one in (#1417). The old fingerprint and X25519 key move to
/// `oplog_retired_identities`; the old signing seed is dropped, so nothing signs under it again.
/// Returns the retired fingerprint, or `None` when `enrolled` is not the staged identity (an
/// ordinary first enrollment).
pub(crate) fn adopt_pending_identity_in_tx(
    tx: &rusqlite::Transaction<'_>,
    enrolled: DeviceFingerprint,
    now_ms: i64,
) -> anyhow::Result<Option<DeviceFingerprint>> {
    let Some((ed, x)) = read_pending(tx)? else {
        return Ok(None);
    };
    let public = ed.public();
    if public.fingerprint() != enrolled {
        return Ok(None);
    }
    let old = read_identity(tx)?
        .and_then(StoredIdentity::into_local)
        .context("this store has no complete device identity to retire")?;
    tx.execute(
        "INSERT INTO oplog_retired_identities(
             fingerprint, x25519_secret, x25519_public, retired_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            old.fingerprint.to_bytes().as_slice(),
            old.x25519_secret.secret_bytes().as_slice(),
            old.x25519_public.to_bytes().as_slice(),
            now_ms,
        ],
    )?;
    let changed = tx.execute(
        "UPDATE oplog_device_identity
            SET seed = ?1, public_key = ?2, fingerprint = ?3, created_at_ms = ?4,
                x25519_secret = ?5, x25519_public = ?6
          WHERE id = 0 AND fingerprint = ?7",
        params![
            ed.seed().as_slice(),
            public.to_bytes().as_slice(),
            enrolled.to_bytes().as_slice(),
            now_ms,
            x.secret_bytes().as_slice(),
            x.public().to_bytes().as_slice(),
            old.fingerprint.to_bytes().as_slice(),
        ],
    )?;
    anyhow::ensure!(changed == 1, "the device identity changed while it was being retired");
    tx.execute("DELETE FROM oplog_pending_identity WHERE id = 0", [])?;
    Ok(Some(old.fingerprint))
}

/// The fingerprints whose account entries a re-enrolling store does not claim to hold: every
/// identity it retired, plus the live one while a replacement is staged. Its own entries the
/// inviting owner never received (a forked branch, work authored after its removal) would
/// otherwise refuse every redemption as held-state it cannot reconcile, and they are condemned by
/// the removal's cut in any case.
pub(crate) fn unclaimed_own_fingerprints(
    conn: &Connection,
) -> anyhow::Result<Vec<DeviceFingerprint>> {
    let mut out: Vec<DeviceFingerprint> =
        retired_identities(conn)?.into_iter().map(|retired| retired.fingerprint).collect();
    if pending_identity_keys(conn)?.is_some()
        && let Some(live) = local_device_fingerprint(conn)?
    {
        out.push(live);
    }
    Ok(out)
}

/// Read this store's local device fingerprint WITHOUT minting one — `None` when no identity has
/// been persisted yet. Unlike [`local_device`] this has NO mint side effect, so it is safe in a
/// read/gate path that must never create an identity as a side effect (e.g. the §18b
/// candidate-capacity budget, which excludes the local device's own signed rows from the
/// remote-flood ceiling but must not mint a device just to run a capacity check). Reads only the
/// persisted `fingerprint` column, which is a forge-proof key: a content row can carry this
/// fingerprint only if it was signed with the local device key (`verify_content_signed` binds the
/// signature to `header.device_fingerprint`), unlike the attacker-settable `author_account_id`.
pub fn local_device_fingerprint(conn: &Connection) -> anyhow::Result<Option<DeviceFingerprint>> {
    let stored: Option<Vec<u8>> = conn
        .query_row("SELECT fingerprint FROM oplog_device_identity WHERE id = 0", [], |row| {
            row.get(0)
        })
        .optional()?;
    stored
        .map(|bytes| {
            let bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .context("stored device fingerprint is not exactly 32 bytes")?;
            Ok(DeviceFingerprint::from_bytes(bytes))
        })
        .transpose()
}

/// Load this store's complete persisted device identity WITHOUT minting a row or backfilling a
/// pre-V058 row. Projection is a read-side operation: merely opening accepted sealed content must
/// never create cryptographic identity state. `None` therefore means either no identity has been
/// persisted or the persisted legacy identity has no X25519 key yet. Corrupt stored key material
/// remains a loud error.
pub fn load_local_device(conn: &Connection) -> anyhow::Result<Option<LocalDevice>> {
    Ok(read_identity(conn)?.and_then(StoredIdentity::into_local))
}

/// The identity row as stored: the ed25519 device (always present) plus the X25519 encryption key
/// IF it has been minted/backfilled yet (a pre-V058 row carries neither X25519 column).
struct StoredIdentity {
    secret: DeviceSecret,
    public: DevicePublic,
    fingerprint: DeviceFingerprint,
    x25519: Option<(DeviceX25519Secret, DeviceX25519Public)>,
}

/// Return this store's persisted device identity, minting + persisting one on the first call and
/// backfilling the X25519 key for a pre-V058 ed25519-only row. Idempotent: every later call returns
/// the SAME identity (a stable fingerprint AND a stable encryption key). `now_ms` stamps a
/// freshly-minted row (injected, matching the op-log store convention); it is ignored once an
/// identity already exists.
pub fn local_device(conn: &Connection, now_ms: i64) -> anyhow::Result<LocalDevice> {
    let stored = match read_identity(conn)? {
        Some(stored) => stored,
        None => {
            // First use: mint BOTH keys from OS entropy and persist. Under a concurrent first open
            // another process may win the insert; `ON CONFLICT DO NOTHING` makes our write a no-op
            // and the mandatory re-read returns whichever identity actually landed — so we adopt
            // the incumbent and drop our own freshly-minted seeds rather than ever
            // holding keys the store didn't record.
            let ed = DeviceSecret::generate()?;
            let x = DeviceX25519Secret::generate()?;
            persist_identity(conn, &ed, &x, now_ms)?;
            read_identity(conn)?.context(
                "device identity missing immediately after persist (single-row insert lost?)",
            )?
        },
    };
    match stored.x25519 {
        Some((x25519_secret, x25519_public)) => Ok(LocalDevice {
            secret: stored.secret,
            public: stored.public,
            fingerprint: stored.fingerprint,
            x25519_secret,
            x25519_public,
        }),
        // A row minted before V058 carries no X25519 key yet — backfill it under a CAS so
        // concurrent opens can't split into two encryption identities. `stored`'s ed25519 identity
        // is the same row the backfill re-reads, so nothing from it is needed past this point.
        None => backfill_x25519(conn),
    }
}

/// Backfill the X25519 key for a pre-V058 ed25519-only identity. Mints a FRESH, independent X25519
/// key, then swaps it in with a guarded UPDATE — the compare-and-swap is the `WHERE x25519_secret
/// IS NULL` clause: SQLite serializes writers, so a racing opener's UPDATE (running only after ours
/// commits) matches no rows and its own re-read adopts our key. We never branch on the affected-row
/// count; the mandatory re-read below is the source of truth (winner reads its own write, loser
/// adopts the incumbent), exactly as the ed25519 mint path's `ON CONFLICT DO NOTHING` + re-read
/// does.
fn backfill_x25519(conn: &Connection) -> anyhow::Result<LocalDevice> {
    let fresh = DeviceX25519Secret::generate()?;
    conn.execute(
        "UPDATE oplog_device_identity
            SET x25519_secret = ?1, x25519_public = ?2
          WHERE id = 0 AND x25519_secret IS NULL",
        params![fresh.secret_bytes().as_slice(), fresh.public().to_bytes().as_slice()],
    )?;
    read_identity(conn)?
        .context("device identity vanished during X25519 backfill")?
        .into_local()
        .context("X25519 columns still NULL immediately after the backfill CAS")
}

impl StoredIdentity {
    /// Consume into a complete [`LocalDevice`], or `None` if the X25519 key is not present yet.
    fn into_local(self) -> Option<LocalDevice> {
        let (x25519_secret, x25519_public) = self.x25519?;
        Some(LocalDevice {
            secret: self.secret,
            public: self.public,
            fingerprint: self.fingerprint,
            x25519_secret,
            x25519_public,
        })
    }
}

/// Insert the identity as the sole row (`id = 0`) with BOTH keys; a no-op if one already exists.
fn persist_identity(
    conn: &Connection,
    secret: &DeviceSecret,
    x25519: &DeviceX25519Secret,
    now_ms: i64,
) -> anyhow::Result<()> {
    let public = secret.public();
    conn.execute(
        "INSERT INTO oplog_device_identity(
             id, seed, public_key, fingerprint, created_at_ms, x25519_secret, x25519_public)
         VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO NOTHING",
        params![
            secret.seed().as_slice(),
            public.to_bytes().as_slice(),
            public.fingerprint().to_bytes().as_slice(),
            now_ms,
            x25519.secret_bytes().as_slice(),
            x25519.public().to_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

/// Read the sole identity row, re-deriving each key from its stored secret and asserting the stored
/// derived columns still agree. `None` when no identity has been minted yet; the returned
/// [`StoredIdentity`] carries `x25519: None` for a pre-V058 row awaiting backfill.
fn read_identity(conn: &Connection) -> anyhow::Result<Option<StoredIdentity>> {
    let row = conn
        .query_row(
            "SELECT seed, public_key, fingerprint, x25519_secret, x25519_public
               FROM oplog_device_identity WHERE id = 0",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((seed, stored_public, stored_fingerprint, x_secret_col, x_public_col)) = row else {
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
    let x25519 = read_x25519(x_secret_col, x_public_col)?;
    Ok(Some(StoredIdentity { secret, public, fingerprint, x25519 }))
}

/// Re-derive + validate the X25519 key from its stored columns. Both present ⇒ rebuild from the
/// secret and assert the stored public agrees (same legibility-copy discipline as ed25519). Both
/// absent ⇒ `None` (a pre-V058 row awaiting backfill). Exactly one present ⇒ a corrupted row,
/// refused.
fn read_x25519(
    secret_col: Option<Vec<u8>>,
    public_col: Option<Vec<u8>>,
) -> anyhow::Result<Option<(DeviceX25519Secret, DeviceX25519Public)>> {
    match (secret_col, public_col) {
        (Some(secret_bytes), Some(stored_public)) => {
            let secret_bytes: [u8; 32] = secret_bytes
                .as_slice()
                .try_into()
                .context("stored device x25519_secret is not exactly 32 bytes")?;
            let secret_bytes = Zeroizing::new(secret_bytes);
            let x25519_secret = DeviceX25519Secret::from_seed(&secret_bytes);
            let x25519_public = x25519_secret.public();
            anyhow::ensure!(
                x25519_public.to_bytes().as_slice() == stored_public.as_slice(),
                "stored device x25519_public does not match the key derived from its x25519_secret"
            );
            Ok(Some((x25519_secret, x25519_public)))
        },
        (None, None) => Ok(None),
        _ => anyhow::bail!(
            "oplog_device_identity has exactly one of x25519_secret / x25519_public set — \
             corrupted"
        ),
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;

    use super::*;

    /// A fully-migrated in-memory DB carrying the identity table with its V058 X25519 columns.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::apply(&conn, &crate::test_hooks()).expect("apply schema");
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
    fn read_only_load_neither_mints_nor_backfills_an_identity() {
        let conn = conn();
        assert!(load_local_device(&conn).unwrap().is_none());
        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM oplog_device_identity", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0, "a projection read must not mint an identity");

        let ed = DeviceSecret::from_seed(&[0x31; 32]);
        let public = ed.public();
        conn.execute(
            "INSERT INTO oplog_device_identity(
                 id, seed, public_key, fingerprint, created_at_ms, x25519_secret, x25519_public)
             VALUES(0, ?1, ?2, ?3, ?4, NULL, NULL)",
            params![
                ed.seed().as_slice(),
                public.to_bytes().as_slice(),
                public.fingerprint().to_bytes().as_slice(),
                now(),
            ],
        )
        .unwrap();
        assert!(load_local_device(&conn).unwrap().is_none());
        let x25519: (Option<Vec<u8>>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT x25519_secret, x25519_public FROM oplog_device_identity WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(x25519, (None, None), "a projection read must not backfill X25519");
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
        let intruder_x = DeviceX25519Secret::generate().expect("csprng");
        assert_ne!(incumbent.public().to_bytes(), intruder.public().to_bytes());
        persist_identity(&conn, &intruder, &intruder_x, now() + 1)
            .expect("conflicting insert is a no-op");
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

    #[test]
    fn mint_persists_both_keys() {
        let conn = conn();
        local_device(&conn, now()).expect("mint identity");
        // A freshly minted identity carries the ed25519 seed AND the X25519 encryption key.
        let (x_secret, x_public): (Option<Vec<u8>>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT x25519_secret, x25519_public FROM oplog_device_identity WHERE id = 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read row");
        assert!(x_secret.is_some(), "x25519_secret is persisted at mint");
        assert!(x_public.is_some(), "x25519_public is persisted at mint");
    }

    #[test]
    fn x25519_is_backfilled_for_a_pre_existing_ed25519_only_identity() {
        let conn = conn();
        let original = local_device(&conn, now()).expect("mint identity");
        // Simulate a pre-V058 row: strip the X25519 columns back to NULL.
        conn.execute(
            "UPDATE oplog_device_identity SET x25519_secret = NULL, x25519_public = NULL WHERE id \
             = 0",
            [],
        )
        .expect("null out x25519");
        // The next open backfills the X25519 key while preserving the ed25519 identity.
        let backfilled = local_device(&conn, now() + 1).expect("backfill x25519");
        assert_eq!(
            original.fingerprint().to_bytes(),
            backfilled.fingerprint().to_bytes(),
            "the ed25519 identity is preserved across the backfill",
        );
        // The columns are now non-null and internally consistent (public derives from secret).
        let (x_secret, x_public): (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT x25519_secret, x25519_public FROM oplog_device_identity WHERE id = 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read backfilled row");
        let seed: [u8; 32] = x_secret.as_slice().try_into().expect("32-byte x25519 secret");
        assert_eq!(
            DeviceX25519Secret::from_seed(&seed).public().to_bytes().as_slice(),
            x_public.as_slice(),
            "the backfilled x25519 columns are consistent",
        );
        // Stable across the next reopen — no re-backfill, no drift.
        let reopened = local_device(&conn, now() + 2).expect("reopen");
        assert_eq!(
            backfilled.x25519_public().to_bytes(),
            reopened.x25519_public().to_bytes(),
            "the backfilled key is stable across reopens",
        );
    }

    #[test]
    fn concurrent_x25519_backfill_adopts_the_incumbent() {
        let conn = conn();
        local_device(&conn, now()).expect("mint identity");
        // Model the race: strip to a pre-V058 row, then a COMPETING writer wins the backfill with a
        // known key before our open runs its guarded CAS.
        conn.execute(
            "UPDATE oplog_device_identity SET x25519_secret = NULL, x25519_public = NULL WHERE id \
             = 0",
            [],
        )
        .expect("null out x25519");
        let incumbent_x = DeviceX25519Secret::from_seed(&[0x42; 32]);
        conn.execute(
            "UPDATE oplog_device_identity SET x25519_secret = ?1, x25519_public = ?2 WHERE id = 0",
            params![
                incumbent_x.secret_bytes().as_slice(),
                incumbent_x.public().to_bytes().as_slice()
            ],
        )
        .expect("incumbent backfill");
        // The guarded CAS itself writes NOTHING once the key is set — the loser's UPDATE is a
        // no-op.
        let rows = conn
            .execute(
                "UPDATE oplog_device_identity SET x25519_secret = ?1, x25519_public = ?2
                   WHERE id = 0 AND x25519_secret IS NULL",
                params![[9u8; 32].as_slice(), [9u8; 32].as_slice()],
            )
            .expect("guarded update");
        assert_eq!(rows, 0, "the CAS guard writes nothing when the key is already set");
        // And our open adopts the incumbent's key, never overwriting it.
        let adopted = local_device(&conn, now() + 1).expect("adopt incumbent");
        assert_eq!(
            adopted.x25519_public().to_bytes(),
            incumbent_x.public().to_bytes(),
            "the incumbent x25519 key survives a concurrent open",
        );
    }

    #[test]
    fn a_corrupted_x25519_public_column_is_refused() {
        let conn = conn();
        local_device(&conn, now()).expect("mint identity");
        // Corrupt x25519_public so it no longer derives from the stored secret; a load must refuse
        // it.
        conn.execute("UPDATE oplog_device_identity SET x25519_public = ?1 WHERE id = 0", params![
            [0u8; 32].as_slice()
        ])
        .expect("corrupt row");
        let err =
            local_device(&conn, now()).map(|_| ()).expect_err("corrupted x25519 must be refused");
        assert!(err.to_string().contains("x25519_public"), "error names the mismatched column");
    }
}
