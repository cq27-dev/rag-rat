//! External trust is independent of the mutable authority projection. Control v2 execution is
//! deliberately unsupported until the complete frozen-legacy and bounded-credit path is ready.
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::checkpoint::{self, CheckpointBundle, TrustedCheckpointPin, VerifiedCheckpoint};
use super::id::{self, AccountId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountControlPolicy {
    LegacyV1,
    UnsupportedVersion(TrustedCheckpointPin),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedAccountControlVersion {
    pub pin: TrustedCheckpointPin,
}
impl std::fmt::Display for UnsupportedAccountControlVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UnsupportedVersion: account control version {} requires a newer implementation",
            self.pin.required_control_version
        )
    }
}
impl std::error::Error for UnsupportedAccountControlVersion {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinInstallOutcome {
    Installed,
    AlreadyPinned,
}

/// Snapshot-local policy read. Every installed checkpoint currently requires unsupported v2.
pub fn account_control_policy(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<AccountControlPolicy> {
    let row = conn
        .query_row(
            "SELECT checkpoint_digest, required_version FROM account_control_pins WHERE \
             account_id=?1",
            [account.to_bytes().as_slice()],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?)),
        )
        .optional();
    let row = match row {
        Ok(row) => row,
        Err(error) if missing_table(&error, "account_control_pins") => {
            require_pre_pin_schema(conn)?;
            return Ok(AccountControlPolicy::LegacyV1);
        },
        Err(error) => return Err(error.into()),
    };
    match row {
        None => Ok(AccountControlPolicy::LegacyV1),
        Some((digest, version)) =>
            Ok(AccountControlPolicy::UnsupportedVersion(TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: id::fixed(&digest)?,
                required_control_version: version,
            })),
    }
}

/// Call inside the SAME snapshot as the authorized read or the SAME write transaction as mutation.
pub fn require_supported_account_control(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<()> {
    match account_control_policy(conn, account)? {
        AccountControlPolicy::LegacyV1 => Ok(()),
        AccountControlPolicy::UnsupportedVersion(pin) =>
            Err(UnsupportedAccountControlVersion { pin }.into()),
    }
}

/// Persist independent trust and complete proof atomically. Caller owns the IMMEDIATE transaction.
/// No peer advertisement, sync receipt, or ordinary account ingestion calls this API.
pub fn pin_checkpoint_in_tx(
    tx: &Transaction<'_>,
    expected: TrustedCheckpointPin,
    proof: &VerifiedCheckpoint,
) -> anyhow::Result<PinInstallOutcome> {
    anyhow::ensure!(expected == proof.pin(), "checkpoint proof differs from expected pin");
    if let AccountControlPolicy::UnsupportedVersion(existing) =
        account_control_policy(tx, expected.account_id)?
    {
        anyhow::ensure!(existing == expected, "conflicting permanent account control pin");
        return Ok(PinInstallOutcome::AlreadyPinned);
    }
    let bundle = proof.bundle();
    tx.execute("INSERT INTO account_control_pins VALUES (?1,?2,?3,?4)", params![
        expected.account_id.to_bytes().as_slice(),
        expected.checkpoint_digest.as_slice(),
        expected.required_control_version,
        bundle.certificate
    ])?;
    for (ordinal, bytes) in bundle.evidence.iter().enumerate() {
        tx.execute("INSERT INTO account_control_pin_evidence VALUES (?1,?2,?3)", params![
            expected.account_id.to_bytes().as_slice(),
            i64::try_from(ordinal)?,
            bytes
        ])?;
    }
    // Preserve stream ownership routing after suppressing the derived ownership rows. Derive
    // from the authenticated proof too, so importing a pin before history is equally protected.
    let accepted = proof.accepted_legacy_entries().collect::<std::collections::HashSet<_>>();
    tx.execute(
        "INSERT OR IGNORE INTO account_control_pin_streams SELECT account_id, stream_id FROM \
         account_stream_ownership WHERE account_id=?1",
        [expected.account_id.to_bytes().as_slice()],
    )?;
    for bytes in &bundle.evidence {
        let entry = super::envelope::decode_account_signed(bytes)?;
        if entry.header.log_id == super::fold::CONTROL_LOG
            && accepted.contains(&entry.entry_hash)
            && let super::ops::DecodedAccountOp::Known(super::ops::AccountOp::StreamOwn {
                stream_id,
                ..
            }) = super::ops::decode(entry.header.entry_type, &entry.payload)?
        {
            tx.execute(
                "INSERT OR IGNORE INTO account_control_pin_streams VALUES (?1,?2)",
                params![expected.account_id.to_bytes().as_slice(), stream_id.to_bytes().as_slice()],
            )?;
        }
    }
    super::storage::clear_unsupported_authority_in_tx(tx, expected.account_id)?;
    Ok(PinInstallOutcome::Installed)
}

/// Export from a single read snapshot; reverify durable proof before handing it to recovery.
pub fn export_account_checkpoint(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<Option<(TrustedCheckpointPin, CheckpointBundle)>> {
    let _snapshot = read_snapshot(conn)?;
    let AccountControlPolicy::UnsupportedVersion(pin) = account_control_policy(conn, account)?
    else {
        return Ok(None);
    };
    let certificate = conn.query_row(
        "SELECT certificate FROM account_control_pins WHERE account_id=?1",
        [account.to_bytes().as_slice()],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM account_control_pin_evidence WHERE account_id=?1 ORDER BY \
         ordinal",
    )?;
    let evidence = stmt
        .query_map([account.to_bytes().as_slice()], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<Vec<u8>>>>()?;
    let bundle = CheckpointBundle { certificate, evidence };
    checkpoint::verify_checkpoint(pin, &bundle)?;
    Ok(Some((pin, bundle)))
}

/// A content header identifies its owner through the stream. Keep that route after projection
/// suppression so a grantee cannot bypass an owner's unsupported pin.
pub(super) fn require_supported_stream_control(
    conn: &Connection,
    stream: crate::stream::StreamId,
) -> anyhow::Result<()> {
    let mut stmt = match conn.prepare(
        "SELECT account_id FROM account_control_pin_streams WHERE stream_id=?1 UNION SELECT \
         account_id FROM account_stream_ownership WHERE stream_id=?1",
    ) {
        Ok(stmt) => stmt,
        Err(error) if missing_table(&error, "account_control_pin_streams") => {
            require_pre_pin_schema(conn)?;
            return Ok(());
        },
        Err(error) => return Err(error.into()),
    };
    let accounts = stmt
        .query_map([stream.to_bytes().as_slice()], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for account in accounts {
        require_supported_account_control(conn, AccountId::from_bytes(id::fixed(&account)?))?;
    }
    Ok(())
}

fn missing_table(error: &rusqlite::Error, table: &str) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(_, Some(message)) if message == &format!("no such table: {table}"))
}

fn require_pre_pin_schema(conn: &Connection) -> anyhow::Result<()> {
    let current: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_version WHERE CAST(substr(id,1,3) AS INTEGER)>=130)",
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(!current, "account control pin table missing from V130 database");
    Ok(())
}

pub(crate) fn read_snapshot(conn: &Connection) -> rusqlite::Result<Option<Transaction<'_>>> {
    if conn.is_autocommit() {
        Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred).map(Some)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(conn: &Connection, account: AccountId) -> VerifiedCheckpoint {
        let device = crate::local_device(conn, 0).unwrap();
        let tx =
            Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &device).unwrap();
        tx.commit().unwrap();
        checkpoint::verify_checkpoint(
            TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: bundle.certificate_digest(),
                required_control_version: 2,
            },
            &bundle,
        )
        .unwrap()
    }

    #[test]
    fn pin_is_atomic_permanent_idempotent_and_survives_reopen_and_refold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin.sqlite");
        let conn = Connection::open(&path).unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 0).unwrap();
        let first = proof(&conn, account);
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            crate::ensure_owned_stream_v2_in_tx(&tx, "pin-retention-test", 1).unwrap();
            tx.commit().unwrap();
        }
        let proof = proof(&conn, account);
        let expected = proof.pin();
        assert_ne!(first.pin(), expected);
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            pin_checkpoint_in_tx(&tx, expected, &proof).unwrap();
            assert!(require_supported_account_control(&tx, account).is_err());
        }
        assert_eq!(account_control_policy(&conn, account).unwrap(), AccountControlPolicy::LegacyV1);
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        assert_eq!(
            pin_checkpoint_in_tx(&tx, expected, &proof).unwrap(),
            PinInstallOutcome::Installed
        );
        assert_eq!(
            pin_checkpoint_in_tx(&tx, expected, &proof).unwrap(),
            PinInstallOutcome::AlreadyPinned
        );
        assert!(pin_checkpoint_in_tx(&tx, first.pin(), &first).is_err());
        assert_eq!(
            account_control_policy(&tx, account).unwrap(),
            AccountControlPolicy::UnsupportedVersion(expected)
        );
        let other = TrustedCheckpointPin { checkpoint_digest: [3; 32], ..expected };
        assert!(pin_checkpoint_in_tx(&tx, other, &proof).is_err());
        tx.commit().unwrap();
        assert!(conn.execute("DELETE FROM account_control_pins", []).is_err());
        assert!(conn.execute("DELETE FROM account_control_pin_evidence", []).is_err());
        rag_rat_db::schema::purge_repo_rows(&conn, "pin-retention-test").unwrap();
        super::super::storage::refold_account(&conn, account).unwrap();
        let roster: i64 = conn
            .query_row("SELECT count(*) FROM account_roster_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(roster, 0);
        drop(conn);
        let reopened = Connection::open(path).unwrap();
        let (pin, bundle) = export_account_checkpoint(&reopened, account).unwrap().unwrap();
        assert_eq!(pin, expected);
        assert_eq!(&bundle, proof.bundle());
        let snapshot =
            Transaction::new_unchecked(&reopened, rusqlite::TransactionBehavior::Deferred).unwrap();
        assert_eq!(export_account_checkpoint(&snapshot, account).unwrap().unwrap().0, expected);
        let error = super::super::storage::usable_snapshots(&snapshot, account).unwrap_err();
        assert!(error.downcast_ref::<UnsupportedAccountControlVersion>().is_some());
        let error = crate::sign_local_node_binding(&snapshot, account, &[0; 32], 0).unwrap_err();
        assert!(error.downcast_ref::<UnsupportedAccountControlVersion>().is_some());
        let error = require_supported_account_control(&reopened, account).unwrap_err();
        assert!(error.downcast_ref::<UnsupportedAccountControlVersion>().is_some());
        require_supported_account_control(&reopened, AccountId::from_bytes([9; 32])).unwrap();
    }

    #[test]
    fn current_schema_missing_pin_table_is_corruption_not_legacy() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn.execute_batch("DROP TABLE account_control_pins").unwrap();
        assert!(account_control_policy(&conn, AccountId::from_bytes([1; 32])).is_err());
        conn.execute("DELETE FROM schema_version WHERE id='130_account_control_pins'", []).unwrap();
        assert_eq!(
            account_control_policy(&conn, AccountId::from_bytes([1; 32])).unwrap(),
            AccountControlPolicy::LegacyV1
        );
    }
}
