//! External trust is independent of the mutable authority projection.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::checkpoint::{self, CheckpointBundle, TrustedCheckpointPin, VerifiedCheckpoint};
use super::id::{self, AccountId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountControlPolicy {
    LegacyV1,
    /// Pinned at a control version this binary EXECUTES: the account folds from its checkpoint
    /// instead of being retracted. Operational authority is still refused — see
    /// [`require_supported_account_control`].
    ControlV2(TrustedCheckpointPin),
    /// Pinned at a control version this binary cannot execute. Acceptance and every derived
    /// projection are retracted; the signed candidates stay for a binary that can execute it.
    UnsupportedVersion(TrustedCheckpointPin),
}

impl AccountControlPolicy {
    /// The pin this policy names, whatever version it requires.
    fn pin(self) -> Option<TrustedCheckpointPin> {
        match self {
            Self::LegacyV1 => None,
            Self::ControlV2(pin) | Self::UnsupportedVersion(pin) => Some(pin),
        }
    }
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

/// Snapshot-local policy read, dispatching on the version the pin itself names.
///
/// The dispatch is per-ACCOUNT, never a global constant: an UNPINNED account keeps folding v1
/// whatever versions its rows carry, quarantining a v2 entry it meets, and that is what keeps an
/// un-upgraded peer converging on the same accepted set as everyone else.
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
        Some((digest, version)) => {
            let pin = TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: id::fixed(&digest)?,
                required_control_version: version,
            };
            Ok(policy_for(pin))
        },
    }
}

/// The version dispatch itself. Split out because V130's `CHECK(required_version=2)` means no
/// stored row can reach the unsupported branch today — this is the only place the forward-compat
/// half of the dispatch can be exercised.
fn policy_for(pin: TrustedCheckpointPin) -> AccountControlPolicy {
    if pin.required_control_version == super::control_v2::ops::CONTROL_VERSION {
        AccountControlPolicy::ControlV2(pin)
    } else {
        AccountControlPolicy::UnsupportedVersion(pin)
    }
}

/// Whether `account` sits under ANY permanent control pin. Nothing operational can be done with
/// such an account whichever version it names — [`require_supported_account_control`] refuses for
/// both — so this is the "there is nothing to reach here" predicate.
///
/// It is NOT a claim that the account's derived state is gone: a pin this binary executes folds
/// from its checkpoint and KEEPS its authority projection. What every pin still retracts is
/// content, which [`stream_control_pinned`] answers for.
pub fn account_is_pinned(conn: &Connection, account: AccountId) -> anyhow::Result<bool> {
    Ok(account_control_policy(conn, account)?.pin().is_some())
}

/// Whether `stream` is routed to ANY pinned account, by ownership or by the pin's routing table.
/// The routing table is derived from the authenticated checkpoint proof, so this is the tightest
/// "the pin retracted this stream" answer a cleanup consumer can ask.
///
/// Content is retracted under EVERY pin, including one this binary folds: the content acceptance
/// path resolves stream authority through gated reads (`account_is_contested` and the
/// ownership/access-mode lookups), and those keep refusing until an executable pin can also be
/// operated under. Narrowing this to the unsupported case would un-retract a stream whose content
/// still cannot be evaluated.
pub fn stream_control_pinned(
    conn: &Connection,
    stream: crate::stream::StreamId,
) -> anyhow::Result<bool> {
    for account in pin_routed_accounts(conn, stream)? {
        if account_is_pinned(conn, account)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Call inside the SAME snapshot as the authorized read or the SAME write transaction as mutation.
pub fn require_supported_account_control(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<()> {
    match account_control_policy(conn, account)? {
        AccountControlPolicy::LegacyV1 => Ok(()),
        // BOTH pinned states refuse, and the executable one is not an oversight: a pin this binary
        // can FOLD is not one it can operate under, because the authority projection a support gate
        // reads does not yet carry the register effects of the account's v2 operations.
        AccountControlPolicy::ControlV2(pin) | AccountControlPolicy::UnsupportedVersion(pin) =>
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
    if let Some(existing) = account_control_policy(tx, expected.account_id)?.pin() {
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
    // Re-derive everything the pin changes, inside the install transaction. A pin this binary
    // executes rebuilds its projection from the checkpoint; one it cannot execute retracts it. Both
    // verdicts come from the dispatch a later fold takes, so install and refold can never disagree.
    super::storage::refold_after_pin_install_in_tx(tx, expected.account_id)?;
    Ok(PinInstallOutcome::Installed)
}

/// Export from a single read snapshot; reverify durable proof before handing it to recovery.
pub fn export_account_checkpoint(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<Option<(TrustedCheckpointPin, CheckpointBundle)>> {
    let _snapshot = read_snapshot(conn)?;
    let Some(pin) = account_control_policy(conn, account)?.pin() else {
        return Ok(None);
    };
    let bundle = stored_checkpoint_bundle(conn, account)?;
    checkpoint::verify_checkpoint(pin, &bundle)?;
    Ok(Some((pin, bundle)))
}

/// The durable certificate and evidence rows behind `account`'s pin, exactly as installed.
/// Deliberately unverified: both callers verify, and the caching one keys on the digest the
/// certificate hashes to.
fn stored_checkpoint_bundle(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<CheckpointBundle> {
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
    Ok(CheckpointBundle { certificate, evidence })
}

/// Verified checkpoints keyed by the account and the exact digest its pin names.
type CheckpointCache = HashMap<(AccountId, [u8; 32]), Rc<VerifiedCheckpoint>>;

thread_local! {
    /// Reconstructing a checkpoint re-verifies every evidence signature, and a pinned account
    /// reconstructs it on EVERY fold. Both inputs are immutable — the V130 triggers refuse UPDATE
    /// and DELETE on the pin row and its evidence — and the pinned digest commits to the exact
    /// evidence set, so a hit under the same key can only be the proof this call would rebuild.
    ///
    /// Thread-local, not process-global: process-global test state passes nextest and then fails
    /// the single-process coverage run. It never evicts, which is bounded by the number of pinned
    /// accounts a thread touches — a pin is permanent and operator-installed, so that set does not
    /// grow with traffic.
    static VERIFIED_CHECKPOINTS: RefCell<CheckpointCache> = RefCell::new(CheckpointCache::new());
}

/// The verified checkpoint behind `account`'s executable pin.
///
/// Errors when the account carries no pin this binary executes, and when the durable proof does not
/// verify. The second is deliberately not a retraction: those rows are immutable, so a proof that
/// stopped verifying is local corruption, and folding an account on a pin that does not verify is
/// the one thing the pin exists to prevent.
pub(super) fn verified_checkpoint(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<Rc<VerifiedCheckpoint>> {
    let AccountControlPolicy::ControlV2(pin) = account_control_policy(conn, account)? else {
        anyhow::bail!("account carries no control pin this binary executes");
    };
    let key = (account, pin.checkpoint_digest);
    if let Some(cached) = VERIFIED_CHECKPOINTS.with(|cache| cache.borrow().get(&key).cloned()) {
        return Ok(cached);
    }
    let bundle = stored_checkpoint_bundle(conn, account)?;
    let proof = Rc::new(checkpoint::verify_checkpoint(pin, &bundle)?);
    VERIFIED_CHECKPOINTS.with(|cache| cache.borrow_mut().insert(key, Rc::clone(&proof)));
    Ok(proof)
}

/// A content header identifies its owner through the stream. Keep that route after projection
/// suppression so a grantee cannot bypass an owner's unsupported pin.
pub(super) fn require_supported_stream_control(
    conn: &Connection,
    stream: crate::stream::StreamId,
) -> anyhow::Result<()> {
    for account in pin_routed_accounts(conn, stream)? {
        require_supported_account_control(conn, account)?;
    }
    Ok(())
}

/// The accounts a stream is routed to: its folded owner, plus any pin whose routing table still
/// names the stream after the ownership row was suppressed. Shared so the retraction predicate and
/// the support gate can never disagree about WHICH accounts a stream answers to — only about what
/// each one's policy means.
fn pin_routed_accounts(
    conn: &Connection,
    stream: crate::stream::StreamId,
) -> anyhow::Result<Vec<AccountId>> {
    let mut stmt = match conn.prepare(
        "SELECT account_id FROM account_control_pin_streams WHERE stream_id=?1 UNION SELECT \
         account_id FROM account_stream_ownership WHERE stream_id=?1",
    ) {
        Ok(stmt) => stmt,
        Err(error) if missing_table(&error, "account_control_pin_streams") => {
            require_pre_pin_schema(conn)?;
            return Ok(Vec::new());
        },
        Err(error) => return Err(error.into()),
    };
    let accounts = stmt
        .query_map([stream.to_bytes().as_slice()], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    accounts.iter().map(|account| Ok(AccountId::from_bytes(id::fixed(account)?))).collect()
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
    use crate::account::{control_v2, envelope, fold, ops, storage, test_support};

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
            AccountControlPolicy::ControlV2(expected),
            "the schema admits only version 2, so every installed pin is one this binary executes",
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
        assert_eq!(
            roster, 1,
            "a pin this binary executes REBUILDS the roster from its checkpoint rather than \
             emptying it — the founder's own enrolment is the fixture's whole roster",
        );
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

    /// A local account owning `repo` with one projected note on its stream.
    fn account_with_a_projected_note(
        conn: &Connection,
        repo: &str,
    ) -> (AccountId, crate::stream::StreamId) {
        use crate::op::{MemoryOp, NodeContent, NodeId};
        let account = crate::local_account(conn, 0).unwrap();
        let tx =
            Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        let stream = crate::ensure_owned_stream_v2_in_tx(&tx, repo, 1).unwrap();
        tx.commit().unwrap();
        let note = MemoryOp::NodeCreate {
            node_id: NodeId::from("n1"),
            content: NodeContent {
                kind: "Invariant".into(),
                title: "n1".into(),
                body: "body".into(),
                confidence: "high".into(),
                source: "agent".into(),
                tags: Vec::new(),
                payload: None,
            },
        };
        crate::author_content_batch(conn, stream, &[note], crate::SealPolicy::Plaintext, 1)
            .unwrap();
        assert_eq!(
            crate::content_projection::list_projected_content_nodes(conn, stream).unwrap().len(),
            1
        );
        (account, stream)
    }

    /// The pin re-projects the streams it retracts in its own transaction. A later fold of the
    /// pinned account finds nothing left to flip and leaves the (empty) projection alone.
    #[test]
    fn pin_empties_the_projection_once_and_a_later_fold_leaves_it_alone() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, stream) = account_with_a_projected_note(&conn, "pin-projection-test");
        let proof = proof(&conn, account);
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        pin_checkpoint_in_tx(&tx, proof.pin(), &proof).unwrap();
        assert!(
            crate::content_projection::list_projected_content_nodes(&tx, stream)
                .unwrap()
                .is_empty(),
            "retracted inside the install transaction"
        );
        tx.commit().unwrap();
        let epoch = crate::content_projection::content_projection_epoch(&conn, stream).unwrap();
        super::super::storage::refold_account(&conn, account).unwrap();
        assert_eq!(
            crate::content_projection::content_projection_epoch(&conn, stream).unwrap(),
            epoch,
            "nothing re-projects a stream the pin already emptied"
        );
    }

    /// A projection owned by a NEWER binary must not be rewritten by this one — but the pin is
    /// the fail-closed mechanism and lands anyway: acceptance is retracted, the projection is left
    /// as it is, and a pending mark hands the rewrite to the binary that owns it.
    #[test]
    fn pin_install_lands_under_a_newer_projector_and_hands_it_the_reprojection() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, stream) = account_with_a_projected_note(&conn, "pin-stamp-test");
        let proof = proof(&conn, account);
        conn.execute(
            "INSERT OR REPLACE INTO oplog_meta(key, value) VALUES ('content_projector_version', \
             ?1)",
            [(crate::content_projection::CONTENT_PROJECTOR_VERSION + 1).to_string()],
        )
        .unwrap();

        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        assert_eq!(
            pin_checkpoint_in_tx(&tx, proof.pin(), &proof).unwrap(),
            PinInstallOutcome::Installed
        );
        tx.commit().unwrap();
        assert!(account_is_pinned(&conn, account).unwrap());
        let accepted: i64 = conn
            .query_row("SELECT count(*) FROM content_entries WHERE accepted = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accepted, 0, "acceptance is retracted regardless");
        assert_eq!(
            crate::content_projection::list_projected_content_nodes(&conn, stream).unwrap().len(),
            1,
            "the newer binary's projection is not rewritten by this one"
        );
        assert!(
            crate::account::content_stream_has_pending_refold(&conn, stream).unwrap(),
            "and it is owed the rewrite"
        );
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

    /// Author one control-log `DeviceAdd` at seq 1 on `device`'s own chain, continuing from
    /// `genesis` and citing the founder incarnation `genesis` names. `version` is the whole point:
    /// a v1 sibling and a v2 continuation differ only in the header's `op_version` and the payload
    /// framing that version selects, so every test here builds both from one helper.
    fn device_add_entry(
        account: AccountId,
        device: &crate::identity::LocalDevice,
        genesis: super::id::AccountEntryHash,
        checkpoint_digest: [u8; 32],
        version: u32,
        seed: u8,
    ) -> envelope::SignedAccountEntry {
        let enrolled = test_support::Dev::new(seed);
        let op = ops::AccountOp::DeviceAdd {
            device_fingerprint: enrolled.fp,
            ed25519_pubkey: enrolled.ed,
            x25519_pubkey: enrolled.x,
            role: ops::DeviceRole::Member,
            label: None,
        };
        let payload = if version == control_v2::ops::CONTROL_VERSION {
            control_v2::ops::ControlOp {
                checkpoint: checkpoint_digest,
                pre_cut_view: None,
                op: op.clone(),
            }
            .encode()
            .unwrap()
        } else {
            ops::encode(&op).unwrap()
        };
        envelope::sign_account_entry(
            device.secret(),
            &envelope::AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: device.fingerprint(),
                seq: 1,
                prev_hash: Some(genesis),
                parent_ref: Some(genesis),
                entry_type: ops::entry_type_of(&op),
                op_version: version,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(genesis.into()),
            },
            &payload,
        )
        .unwrap()
    }

    fn accepted_flag(conn: &Connection, hash: super::id::AccountEntryHash) -> i64 {
        conn.query_row(
            "SELECT accepted FROM account_entries WHERE entry_hash = ?1",
            [hash.as_slice()],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn row_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    /// A local account whose founder has enrolled one member at seq 1. Returns the account, the
    /// founder device, the genesis hash (which is also the founder's owner incarnation) and the
    /// accepted seq-1 entry.
    fn account_with_one_enrolment(
        conn: &Connection,
    ) -> (
        AccountId,
        crate::identity::LocalDevice,
        super::id::AccountEntryHash,
        super::id::AccountEntryHash,
    ) {
        let account = crate::local_account(conn, 1).unwrap();
        let device = crate::local_device(conn, 1).unwrap();
        let genesis = crate::read_local_account_genesis(conn).unwrap().unwrap();
        let add = device_add_entry(account, &device, genesis, [0; 32], 1, 7);
        storage::account_ingest(conn, &add.signed_bytes, 1).unwrap();
        assert_eq!(
            accepted_flag(conn, add.entry_hash),
            1,
            "the enrolment is accepted before the pin"
        );
        (account, device, genesis, add.entry_hash)
    }

    fn install_pin(conn: &Connection, account: AccountId) -> TrustedCheckpointPin {
        let proof = proof(conn, account);
        let tx =
            Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        pin_checkpoint_in_tx(&tx, proof.pin(), &proof).unwrap();
        tx.commit().unwrap();
        proof.pin()
    }

    /// THE constraint the pinned fold rests on. A v1 control entry that arrives AFTER the pin is
    /// not in the checkpoint's evidence, so it never becomes effective and never reaches the
    /// min-hash tiebreak. Account ingest is NOT pin-gated, so without this an ordinary v1 sibling
    /// with a smaller entry hash would demote the checkpoint's own winner and collapse that
    /// device's accepted chain — reviving a frozen branch loser out of ordinary v1 traffic.
    #[test]
    fn a_post_pin_v1_sibling_with_a_smaller_hash_cannot_displace_the_checkpoints_winner() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, device, genesis, accepted) = account_with_one_enrolment(&conn);
        install_pin(&conn, account);

        // A sibling at the SAME slot whose hash sorts BELOW the accepted entry's — the side that
        // wins `select_coherent_branches`' min-hash tiebreak whenever both are effective.
        let sibling = (20u8..=200)
            .map(|seed| device_add_entry(account, &device, genesis, [0; 32], 1, seed))
            .find(|entry| entry.entry_hash < accepted)
            .expect("a smaller-hash sibling exists");
        storage::account_ingest(&conn, &sibling.signed_bytes, 2).unwrap();

        assert_eq!(accepted_flag(&conn, accepted), 1, "the checkpoint's winner keeps its slot");
        assert_eq!(
            accepted_flag(&conn, sibling.entry_hash),
            0,
            "a post-pin v1 sibling never competes for a slot the checkpoint decided",
        );
    }

    /// A v2 continuation may EXTEND a device's accepted chain but never CONTEST a slot the
    /// checkpoint already decided.
    ///
    /// Such an entry really is authorized: its `prev_hash` is checkpoint-accepted so the ancestry
    /// walk lands, and the founder signing it cites its own live incarnation, so the executor
    /// applies it. The min-hash tiebreak is symmetric, so without the frozen-slot guard an applied
    /// v2 entry with the smaller hash displaces the checkpoint's own winner and collapses the
    /// accepted chain above it — selecting a different historical branch, which is the frozen
    /// branch-loser revival the pin exists to prevent. Being authorized is not permission to
    /// rewrite what the pin froze.
    #[test]
    fn an_applied_v2_entry_cannot_contest_a_slot_the_checkpoint_already_decided() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, device, genesis, accepted) = account_with_one_enrolment(&conn);
        let digest = install_pin(&conn, account).checkpoint_digest;

        // Same slot as the accepted enrolment, and the smaller hash — the side that wins the
        // min-hash tiebreak the moment both are effective.
        let contender = (20u8..=200)
            .map(|seed| device_add_entry(account, &device, genesis, digest, 2, seed))
            .find(|entry| entry.entry_hash < accepted)
            .expect("a smaller-hash v2 contender exists");
        storage::account_ingest(&conn, &contender.signed_bytes, 2).unwrap();

        assert_eq!(accepted_flag(&conn, accepted), 1, "the checkpoint's winner keeps its slot");
        assert_eq!(
            accepted_flag(&conn, contender.entry_hash),
            0,
            "an APPLIED v2 entry still never displaces a slot the checkpoint decided",
        );
    }

    /// Installing a pin must not fail merely because an enrollment invite is outstanding.
    ///
    /// The fold's reservation top-up resolves the account's streams through the GATED
    /// `owned_streams_for_account`, which refuses under either pin state — and it short-circuits
    /// when nothing is outstanding, so a fixture without a reservation never reached it. There is
    /// nothing to reserve capacity for on an account no enrollment can redeem against, so the whole
    /// top-up is skipped under a pin; running it would fail the fold, and the pin install with it,
    /// blaming a version mismatch that is not the cause.
    #[test]
    fn a_pin_installs_with_an_enrollment_reservation_outstanding() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, _device, _genesis, enrolled) = account_with_one_enrolment(&conn);
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            crate::upsert_account_candidate_reservation_in_tx(
                &tx,
                account,
                [9; 32],
                4,
                4096,
                2,
                rag_rat_base::time::now_ms() + 3_600_000,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        install_pin(&conn, account);
        assert_eq!(
            accepted_flag(&conn, enrolled),
            1,
            "the pin installed and folded with the reservation still outstanding",
        );
        storage::refold_account(&conn, account).unwrap();
        assert_eq!(accepted_flag(&conn, enrolled), 1, "and every later fold is equally unaffected");
    }

    /// Two v2 continuations at ONE chain slot resolve to exactly one accepted entry, and to the
    /// same one whichever order they arrive in. Selection never PROMOTES: an applied v2 entry only
    /// becomes EFFECTIVE, and the ordinary coherence walk then picks the sibling by min hash.
    /// Both orders replay against a BYTE-IDENTICAL database, copied once the pin has landed: a
    /// local account mints fresh keys on every call, so two separately built stores would compare
    /// the winners of two different accounts and prove nothing about ordering.
    #[test]
    fn two_v2_siblings_at_one_slot_resolve_to_one_accepted_under_either_order() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.sqlite");
        let siblings = {
            let conn = Connection::open(&origin).unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            let account = crate::local_account(&conn, 1).unwrap();
            let device = crate::local_device(&conn, 1).unwrap();
            let genesis = crate::read_local_account_genesis(&conn).unwrap().unwrap();
            let digest = install_pin(&conn, account).checkpoint_digest;
            let siblings: Vec<_> = [31u8, 32]
                .iter()
                .map(|seed| device_add_entry(account, &device, genesis, digest, 2, *seed))
                .collect();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            siblings
        };
        let winner_of = |name: &str, order: [usize; 2]| {
            let path = dir.path().join(name);
            std::fs::copy(&origin, &path).unwrap();
            let conn = Connection::open(&path).unwrap();
            for index in order {
                storage::account_ingest(&conn, &siblings[index].signed_bytes, 2).unwrap();
            }
            let accepted: Vec<_> = siblings
                .iter()
                .filter(|entry| accepted_flag(&conn, entry.entry_hash) == 1)
                .map(|entry| entry.entry_hash)
                .collect();
            assert_eq!(accepted.len(), 1, "exactly one v2 sibling holds the slot");
            accepted[0]
        };
        let forward = winner_of("forward.sqlite", [0, 1]);
        assert_eq!(
            forward,
            winner_of("reverse.sqlite", [1, 0]),
            "the surviving sibling does not depend on arrival order",
        );
        assert_eq!(
            forward,
            siblings.iter().map(|entry| entry.entry_hash).min().unwrap(),
            "and the survivor is the min-hash sibling the coherence walk picks",
        );
    }

    /// A pinned account's authority projection is REBUILT from its checkpoint, not emptied. The
    /// retraction made such an account survivable; a pin this binary executes has to do better.
    #[test]
    fn an_executable_pin_rebuilds_the_authority_projection_instead_of_emptying_it() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, _device, _genesis, enrolled) = account_with_one_enrolment(&conn);
        install_pin(&conn, account);

        assert!(account_is_pinned(&conn, account).unwrap());
        assert_eq!(row_count(&conn, "account_roster_history"), 2, "founder plus the enrolment");
        assert_eq!(row_count(&conn, "account_owner_incarnations"), 1, "the founder's incarnation");
        assert_eq!(row_count(&conn, "account_auth_state"), 1, "classified, not absent");
        assert_eq!(
            row_count(&conn, "account_entries WHERE accepted = 1"),
            2,
            "the checkpoint's accepted control chain keeps its acceptance",
        );
        assert_eq!(accepted_flag(&conn, enrolled), 1);
        // Folding is not operating: the support gate is deliberately untouched by this dispatch.
        assert!(require_supported_account_control(&conn, account).is_err());
    }

    /// The retraction path is reached only by a pin naming a version this binary cannot execute,
    /// and V130's `CHECK(required_version=2)` means no stored row can name one yet. Drive it
    /// directly so the forward-compat branch still works when a later schema admits one.
    #[test]
    fn the_retraction_path_still_empties_every_projection_it_owns() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, _device, _genesis, enrolled) = account_with_one_enrolment(&conn);
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        storage::clear_unsupported_authority_in_tx(&tx, account).unwrap();
        tx.commit().unwrap();
        for table in [
            "account_roster_content_boundaries",
            "account_roster_history",
            "account_owner_incarnations",
            "account_stream_ownership",
            "account_stream_grants",
            "account_stream_grant_cuts",
            "account_auth_state",
            "account_repo_incarnation_current",
        ] {
            assert_eq!(row_count(&conn, table), 0, "{table} is retracted");
        }
        assert_eq!(accepted_flag(&conn, enrolled), 0, "acceptance is retracted too");
    }

    /// Author a v2 `DeviceRemove` of `subject` on `device`'s own chain.
    fn device_remove_entry(
        account: AccountId,
        device: &crate::identity::LocalDevice,
        prev: super::id::AccountEntryHash,
        seq: u64,
        genesis: super::id::AccountEntryHash,
        checkpoint_digest: [u8; 32],
        subject: crate::op::DeviceFingerprint,
    ) -> envelope::SignedAccountEntry {
        let op = ops::AccountOp::DeviceRemove {
            device_fingerprint: subject,
            control_cut: crate::account::cut::Cut::Empty,
            secrets_cut: crate::account::cut::Cut::Empty,
            content_cuts: vec![],
            reason: "revoked".into(),
        };
        // A revocation MUST name a pre-cut view — `ControlOp::encode` refuses otherwise — and this
        // digest is exactly the manifest no refold can supply.
        let payload = control_v2::ops::ControlOp {
            checkpoint: checkpoint_digest,
            pre_cut_view: Some([9; 32]),
            op: op.clone(),
        }
        .encode()
        .unwrap();
        envelope::sign_account_entry(
            device.secret(),
            &envelope::AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: device.fingerprint(),
                seq,
                prev_hash: Some(prev),
                parent_ref: Some(prev),
                entry_type: ops::entry_type_of(&op),
                op_version: control_v2::ops::CONTROL_VERSION,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(genesis.into()),
            },
            &payload,
        )
        .unwrap()
    }

    /// A v2 revocation cannot yet take effect through a refold, and the reason is structural rather
    /// than a policy choice: `ControlOp::encode` requires every revocation to name a DETACHED
    /// pre-cut manifest by digest, and `execute_held` supplies no manifests, so `plan_replay`
    /// cannot resolve the view and the operation parks on `ParkCause::Manifest`. Nothing
    /// persists a manifest or hands one to the executor.
    ///
    /// So the removal is neither accepted nor projected, and the subject stays on the roster. This
    /// is fail-closed — a revocation that cannot be verified applies nothing — but it also means
    /// the register effects `v2::pinned_history` composes are unreachable from here until
    /// detached manifests become durable. When that lands, this test fails and is the place to
    /// say what the refold now does instead.
    #[test]
    fn a_v2_revocation_parks_for_want_of_the_manifest_no_refold_can_supply() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, device, genesis, enrolled) = account_with_one_enrolment(&conn);
        let digest = install_pin(&conn, account).checkpoint_digest;
        let subject = test_support::Dev::new(7).fp;
        let remove = device_remove_entry(account, &device, enrolled, 2, genesis, digest, subject);
        storage::account_ingest(&conn, &remove.signed_bytes, 3).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM account_entry_status WHERE entry_hash = ?1",
                [remove.entry_hash.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "retained_unfolded", "the executor applied nothing for it");
        assert_eq!(accepted_flag(&conn, remove.entry_hash), 0, "and it is not accepted");
        let open_roster: i64 = conn
            .query_row(
                "SELECT count(*) FROM account_roster_history WHERE closed_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open_roster, 2, "the subject is still an open roster member");
    }

    /// The same operation WITHOUT a pre-cut view cannot even be authored: the presence rule is a
    /// property of the signed payload, so there is no way to sidestep the manifest by omitting it.
    #[test]
    fn a_revocation_cannot_be_authored_without_naming_a_pre_cut_view() {
        let op = ops::AccountOp::DeviceRemove {
            device_fingerprint: test_support::Dev::new(7).fp,
            control_cut: crate::account::cut::Cut::Empty,
            secrets_cut: crate::account::cut::Cut::Empty,
            content_cuts: vec![],
            reason: "revoked".into(),
        };
        let encoded =
            control_v2::ops::ControlOp { checkpoint: [1; 32], pre_cut_view: None, op }.encode();
        assert!(
            encoded.is_err_and(|error| error.to_string().contains("pre-cut view presence")),
            "a revocation must name the manifest that bounds its credit",
        );
    }

    /// The forward-compat half of the version dispatch, unreachable through a stored row while the
    /// V130 CHECK admits only version 2.
    #[test]
    fn the_version_dispatch_executes_two_and_refuses_anything_else() {
        let pin = |version| TrustedCheckpointPin {
            account_id: AccountId::from_bytes([1; 32]),
            checkpoint_digest: [2; 32],
            required_control_version: version,
        };
        assert_eq!(policy_for(pin(2)), AccountControlPolicy::ControlV2(pin(2)));
        for version in [0, 1, 3, 99] {
            assert_eq!(
                policy_for(pin(version)),
                AccountControlPolicy::UnsupportedVersion(pin(version)),
                "version {version} is not one this binary can execute",
            );
        }
    }
}
