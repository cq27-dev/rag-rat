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
/// Content is retracted under EVERY pin, including one this binary folds — and THIS predicate is
/// one of the three things that does the retracting, not a consequence of the authority gates.
/// `stream_owner_account_for_cleanup` answers `None` here and the content refold declassifies;
/// `resolve_stream_authority` skips pinned authors; the projector loads none. So narrowing this to
/// the unsupported case is not a documentation change — it is the decision to evaluate content
/// under an executable pin, and it must be taken deliberately, with the whole set moved together.
///
/// It is unproven that the content evaluator is correct under `ControlV2`: every `content_cuts` in
/// the v2 fixtures is empty, so a cut that condemns content has never been folded through a pin.
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
        // BOTH pinned states refuse, and the executable one is not an oversight — but not for the
        // reason once recorded here. The projection DOES carry the register effects of the
        // account's v2 operations; `pinned_history` composes them and the shared fold path writes
        // them, which is what [`require_foldable_account_control`] rests on. What an executable pin
        // still cannot do is OPERATE: authoring signs v1 bytes a pinned fold can never make
        // effective, adoption and enrollment cannot transfer the pin, and a second checkpoint
        // proposal on a permanent pin is meaningless.
        AccountControlPolicy::ControlV2(pin) | AccountControlPolicy::UnsupportedVersion(pin) =>
            Err(UnsupportedAccountControlVersion { pin }.into()),
    }
}

/// The gate for READING folded authority: a pin this binary can FOLD answers from a real
/// projection, so only a version it cannot execute refuses.
///
/// Deliberately a second predicate rather than a widening of
/// [`require_supported_account_control`], which stays as the gate for everything that AUTHORS or
/// ADOPTS. Both spellings are one line at a call site, so a reader cannot tell them apart by
/// shape — the difference is which question the site is asking. Migrating a site is therefore an
/// explicit diff line, and a site left on the stricter gate under-grants, which is fail-closed.
///
/// Sound only because `rewrite_authority_projection` runs on the SHARED fold path: under
/// `ControlV2` the roster, incarnation, ownership and grant tables are rebuilt from the
/// checkpoint's frozen history plus the v2 register effects, so these reads return real rows
/// rather than the empties a retracted account would show.
///
/// It does NOT make an account operable. Content stays retracted under every pin, and authoring,
/// adoption and enrollment keep refusing — see [`require_supported_account_control`].
pub fn require_foldable_account_control(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<()> {
    match account_control_policy(conn, account)? {
        AccountControlPolicy::LegacyV1 | AccountControlPolicy::ControlV2(_) => Ok(()),
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
    use crate::account::{annex, control_v2, envelope, fold, ops, storage, test_support};

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
        // Reads of folded authority answer from the rebuilt projection: this pin names a version
        // this binary executes, so refusing them would be refusing data the fold just derived. The
        // refusal these two used to assert belongs to a pin that cannot be executed, which
        // `the_retraction_path_still_empties_every_projection_it_owns` drives directly.
        // `usable_snapshots` answers from candidates plus the projection; the fixture purged its
        // repo rows, so an empty list is the right answer and a refusal is not.
        assert!(super::super::storage::usable_snapshots(&snapshot, account).unwrap().is_empty());
        // Nested `Result`: the outer is the gate, the inner is whether a binding was actually
        // signed. Asserting only the outer would pass on a `NodeAuthError::NotRosterDevice` —
        // exactly the answer a pin that had emptied the roster would give.
        assert!(
            !crate::sign_local_node_binding(&snapshot, account, &[0; 32], 0)
                .unwrap()
                .unwrap()
                .is_empty(),
            "a foldable pin still mints a transport credential from the rebuilt roster",
        );
        // Operating is still refused. The support gate is the authoring/adoption question and is
        // deliberately untouched by the read migration.
        let error = require_supported_account_control(&reopened, account).unwrap_err();
        assert!(error.downcast_ref::<UnsupportedAccountControlVersion>().is_some());
        require_supported_account_control(&reopened, AccountId::from_bytes([9; 32])).unwrap();
        require_foldable_account_control(&reopened, account).unwrap();
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
        // Every row at or beyond V130, not just V130's own. `require_pre_pin_schema` asks whether
        // the ladder reached the pin schema AT ALL, so a later migration's row answers yes on its
        // own — and a ledger carrying one of those while missing V130's is a state the ordered
        // ladder cannot produce, so deleting a single id stops describing a pre-pin store.
        conn.execute(
            "DELETE FROM schema_version WHERE CAST(substr(id, 1, 3) AS INTEGER) >= 130",
            [],
        )
        .unwrap();
        assert_eq!(
            account_control_policy(&conn, AccountId::from_bytes([1; 32])).unwrap(),
            AccountControlPolicy::LegacyV1
        );
    }

    /// Author one control-log `DeviceAdd` at seq 1 on `device`'s own chain, continuing from
    /// `genesis` and citing the founder incarnation `genesis` names. `version` is the whole point:
    /// a v1 sibling and a v2 continuation differ only in the header's `op_version` and the payload
    /// framing that version selects, so every test here builds both from one helper.
    /// Search for an entry whose hash sorts BELOW `threshold`, varying only `build`'s
    /// distinguisher.
    ///
    /// Every fixture hash here descends from a founder key `local_device` mints from OS entropy, so
    /// `threshold` is fresh on every run and a FIXED window of candidates holds no smaller entry
    /// about one run in 182 — measured at 2/300 and 3/300 for the two thresholds this replaces.
    /// `Dev::new` takes a `u8`, so widening the seed window cannot get below ~1/257; the
    /// distinguisher varies a semantically inert field instead, so the search is unbounded and the
    /// odds stop mattering.
    fn smaller_than(
        threshold: super::id::AccountEntryHash,
        build: impl Fn(u32) -> envelope::SignedAccountEntry,
    ) -> envelope::SignedAccountEntry {
        (1u32..)
            .take(100_000)
            .map(build)
            .find(|entry| entry.entry_hash < threshold)
            .expect("100000 candidates without a smaller hash is a fixture fault, not the odds")
    }

    /// `distinguisher` varies the signed payload without changing what any test asserts — the label
    /// is inert here, and 0 emits the exact bytes this fixture produced before it existed, so a
    /// caller that does not search keeps its hash. Nothing derives a key from it and it is not a
    /// nonce: a parameter named for cryptographic material reads to a scanner as cryptographic
    /// material, whatever it holds.
    ///
    /// It is also NOT the enrolled identity: that is `seed`, and it IS load-bearing
    /// (`account_with_one_enrolment` enrols seed 7, which is the very seat
    /// `a_forked_v2_revocations_registers_revoke_nothing` asserts stays open).
    fn device_add_entry(
        account: AccountId,
        device: &crate::identity::LocalDevice,
        genesis: super::id::AccountEntryHash,
        checkpoint_digest: [u8; 32],
        version: u32,
        seed: u8,
        distinguisher: u32,
    ) -> envelope::SignedAccountEntry {
        let enrolled = test_support::Dev::new(seed);
        let op = ops::AccountOp::DeviceAdd {
            device_fingerprint: enrolled.fp,
            ed25519_pubkey: enrolled.ed,
            x25519_pubkey: enrolled.x,
            role: ops::DeviceRole::Member,
            label: (distinguisher != 0).then(|| format!("n{distinguisher}")),
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

    /// Whether `fingerprint` still holds an OPEN roster seat. Named rather than counted: a
    /// revocation assertion that counts open seats also passes when the WRONG seat closed.
    fn seat_open(conn: &Connection, fingerprint: crate::op::DeviceFingerprint) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM account_roster_history WHERE device_fingerprint = ?1 AND \
             closed_at IS NULL)",
            [fingerprint.to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap()
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
        let add = device_add_entry(account, &device, genesis, [0; 32], 1, 7, 0);
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
        let sibling = smaller_than(accepted, |distinguisher| {
            device_add_entry(account, &device, genesis, [0; 32], 1, 20, distinguisher)
        });
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
        let contender = smaller_than(accepted, |distinguisher| {
            device_add_entry(account, &device, genesis, digest, 2, 20, distinguisher)
        });
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
    /// The top-up short-circuits when nothing is outstanding, so a fixture without a reservation
    /// never reaches it at all. With one outstanding it is reached and then skipped, by the pin
    /// guard inside the top-up itself: there is nothing to reserve capacity for on an account no
    /// enrollment can redeem against. That skip is what keeps installing a pin from depending on
    /// enrollment state.
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
                .map(|seed| device_add_entry(account, &device, genesis, digest, 2, *seed, 0))
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

        // The rows existing is not the same as a reader reaching them: every API over this
        // projection is gated, so a rebuilt-but-unreadable projection would be indistinguishable
        // from a retracted one at the seam that matters.
        assert_eq!(
            storage::list_effective_roster_fingerprints(&conn, account).unwrap().len(),
            2,
            "a foldable pin serves the roster it just rebuilt",
        );

        // Folding is still not operating. Reads answer from the projection; authoring, adoption
        // and enrollment keep refusing, and content stays retracted under every pin.
        assert!(require_supported_account_control(&conn, account).is_err());
        assert!(require_foldable_account_control(&conn, account).is_ok());

        // The reads the fold path takes answer from that rebuilt projection rather than refusing.
        // The strict gate above still says no, so these pin the foldable gate's admission and not
        // some general loosening: an account whose projection is rebuilt must be readable through
        // the same reads an unpinned one uses, or the rebuild buys nothing.
        assert!(storage::owned_streams_for_account(&conn, account).unwrap().is_empty());
        assert!(!storage::account_is_contested(&conn, account).unwrap());
        assert_eq!(storage::account_effective_count(&conn, account).unwrap(), 2);
    }

    /// A v2 revocation's content cut reaches the projection a content refold reads.
    ///
    /// The executor decides a revocation on its REGISTERS alone and never inspects `content_cuts`,
    /// so the cut rides through as part of the `AccountOp` and is derived into `content_boundaries`
    /// by the fact derivation the pinned composition shares with v1. Nothing else in the v2 path
    /// carries it, which makes that shared derivation the only thing standing between a v2
    /// revocation and a content boundary.
    ///
    /// The negative control is exact: with no boundary row a CLOSED roster fact answers `Closed`,
    /// and this revocation closes the subject's seat — so a `Cut` here cannot be a default leaking
    /// through.
    #[test]
    fn a_v2_revocation_bounds_content_on_the_stream_its_cut_names() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, device, genesis, enrolled) = account_with_one_enrolment(&conn);
        let digest = install_pin(&conn, account).checkpoint_digest;
        let subject = test_support::Dev::new(7).fp;
        let (stream, _) = test_support::stream_own(account);
        let cut_hash = super::id::AccountEntryHash::from_bytes([0x5c; 32]);

        let view = control_v2::views::ViewManifest { checkpoint: digest, entries: Vec::new() };
        let remove = device_remove_entry(
            account,
            &device,
            enrolled,
            2,
            genesis,
            digest,
            view.digest().unwrap(),
            subject,
            0,
            vec![ops::ContentCut { stream_id: stream, seq: 9, hash: cut_hash }],
        );
        storage::account_ingest(&conn, &remove.signed_bytes, 3).unwrap();
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            annex::author::author_view_manifest_in_tx(&tx, &device, account, &view, 4).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(accepted_flag(&conn, remove.entry_hash), 1, "the revocation applied");
        assert!(!seat_open(&conn, subject), "and closed the subject's seat");

        let roster_ref: super::id::RosterRef = enrolled.into();
        let answer =
            storage::roster_content_authority(&conn, account, roster_ref, subject, stream).unwrap();
        let fold::AuthorityQuery::Effective(authority) = answer else {
            // No interpolation: the query's value is taint-traced from the authority gate, and
            // formatting it into the panic reads as writing authority state to a log.
            panic!("roster content authority did not resolve effective under the pin");
        };
        assert_eq!(
            authority.boundary,
            fold::AuthorityBoundary::Cut { seq: 9, hash: cut_hash },
            "the v2 cut's content boundary reached the projection",
        );
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

    /// Author a v2 `DeviceRemove` of `subject` on `device`'s own chain, naming `pre_cut_view` as
    /// the detached manifest that bounds its credit.
    #[allow(clippy::too_many_arguments)]
    fn device_remove_entry(
        account: AccountId,
        device: &crate::identity::LocalDevice,
        prev: super::id::AccountEntryHash,
        seq: u64,
        genesis: super::id::AccountEntryHash,
        checkpoint_digest: [u8; 32],
        pre_cut_view: [u8; 32],
        subject: crate::op::DeviceFingerprint,
        distinguisher: u32,
        content_cuts: Vec<ops::ContentCut>,
    ) -> envelope::SignedAccountEntry {
        let op = ops::AccountOp::DeviceRemove {
            device_fingerprint: subject,
            control_cut: crate::account::cut::Cut::Empty,
            secrets_cut: crate::account::cut::Cut::Empty,
            content_cuts,
            // Inert, and 0 keeps the bytes this fixture emitted before the parameter existed. It is
            // not a nonce and nothing derives a key from it. The SUBJECT stays the caller's: which
            // device a revocation names is what these tests assert about, so it can never be the
            // thing a hash search varies.
            reason: if distinguisher == 0 {
                "revoked".into()
            } else {
                format!("revoked {distinguisher}")
            },
        };
        // A revocation MUST name a pre-cut view — `ControlOp::encode` refuses otherwise.
        let payload = control_v2::ops::ControlOp {
            checkpoint: checkpoint_digest,
            pre_cut_view: Some(pre_cut_view),
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

    /// A v2 revocation's evidence is a DETACHED manifest it names by digest, and the manifest is an
    /// ordinary annex entry. Both halves of that are load-bearing, so both are asserted here: while
    /// the manifest is not held the cut parks on `ParkCause::Manifest` and applies NOTHING, and
    /// storing that one entry — with nothing else about the account changing — is what lets the
    /// refold hand the executor its evidence and project the register.
    ///
    /// The manifest carries no owner incarnation, which is also deliberate: `held_view_manifests`
    /// must not gate a manifest on its carrier's live authority the way `usable_snapshots` gates a
    /// snapshot, and a gate copied from that sibling would refuse this one outright.
    ///
    /// `a_forked_v2_revocations_registers_revoke_nothing` is the negative complement: the same cut,
    /// forked at its slot, closes no seat at all.
    #[test]
    fn a_v2_revocation_parks_for_want_of_its_manifest_and_applies_once_it_is_stored() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, device, genesis, enrolled) = account_with_one_enrolment(&conn);
        let digest = install_pin(&conn, account).checkpoint_digest;
        let subject = test_support::Dev::new(7).fp;
        // The checkpoint froze the whole roster, so no v2 candidate precedes this cut and the view
        // it nominates names none. An empty view is still a view: the cut commits to its digest.
        let view = control_v2::views::ViewManifest { checkpoint: digest, entries: Vec::new() };
        let remove = device_remove_entry(
            account,
            &device,
            enrolled,
            2,
            genesis,
            digest,
            view.digest().unwrap(),
            subject,
            0,
            vec![],
        );
        storage::account_ingest(&conn, &remove.signed_bytes, 3).unwrap();

        let status = |conn: &Connection| -> String {
            conn.query_row(
                "SELECT status FROM account_entry_status WHERE entry_hash = ?1",
                [remove.entry_hash.as_slice()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(status(&conn), "retained_unfolded", "the executor applied nothing for it");
        assert_eq!(accepted_flag(&conn, remove.entry_hash), 0, "and it is not accepted");
        assert!(seat_open(&conn, subject), "the subject is still an open roster member");

        // Storing the manifest is the ONLY thing that changes.
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            annex::author::author_view_manifest_in_tx(&tx, &device, account, &view, 4).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(status(&conn), "accepted", "the cut now verifies from held rows alone");
        assert_eq!(accepted_flag(&conn, remove.entry_hash), 1);
        assert!(!seat_open(&conn, subject), "and the revocation closed the SUBJECT's roster seat");
        assert!(
            seat_open(&conn, device.fingerprint()),
            "the seat it named, not merely one of them — the founder's is untouched",
        );
    }

    /// A FORKED revocation's registers revoke nothing. Two revocations at one chain slot each cite
    /// the manifest they need, so the executor applies BOTH, and the coherence walk then keeps the
    /// min-hash sibling and forks the other. The forked one named the enrolled member, and that
    /// member keeps its seat — which is why [`super::storage`]'s pinned projection composes
    /// `pinned_history` INSIDE its elimination loop rather than once above it: a history composed
    /// over the pre-fork set carries the loser's register too, and closes a seat no accepted entry
    /// ever cut.
    ///
    /// `a_v2_revocation_parks_for_want_of_its_manifest_and_applies_once_it_is_stored` is the
    /// positive complement: there the revocation is accepted, and it does close its subject's seat.
    #[test]
    fn a_forked_v2_revocations_registers_revoke_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let (account, device, genesis, enrolled) = account_with_one_enrolment(&conn);
        let digest = install_pin(&conn, account).checkpoint_digest;
        let view = control_v2::views::ViewManifest { checkpoint: digest, entries: Vec::new() };
        let manifest = view.digest().unwrap();
        // Both cuts name this one manifest, so neither parks for want of its evidence and the
        // fork — not a missing manifest — is the only thing separating them.
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            annex::author::author_view_manifest_in_tx(&tx, &device, account, &view, 4).unwrap();
            tx.commit().unwrap();
        }

        let revoke = |subject, distinguisher| {
            device_remove_entry(
                account,
                &device,
                enrolled,
                2,
                genesis,
                digest,
                manifest,
                subject,
                distinguisher,
                vec![],
            )
        };
        let member = test_support::Dev::new(7).fp;
        let loser = revoke(member, 0);
        // The same slot, a DIFFERENT subject, and the smaller hash — the side the min-hash
        // tiebreak keeps, which leaves the member's revocation forked. The subject is fixed and the
        // distinguisher varies: revoking a different device is what makes this a fork rather than a
        // duplicate, so it is not something the search may move.
        let other = test_support::Dev::new(20).fp;
        let winner = smaller_than(loser.entry_hash, |distinguisher| revoke(other, distinguisher));
        storage::account_ingest(&conn, &loser.signed_bytes, 5).unwrap();
        storage::account_ingest(&conn, &winner.signed_bytes, 6).unwrap();

        assert_eq!(
            accepted_flag(&conn, winner.entry_hash),
            1,
            "the min-hash sibling applied and holds the slot",
        );
        assert_eq!(accepted_flag(&conn, loser.entry_hash), 0, "the member's revocation forked");
        assert!(
            seat_open(&conn, member),
            "a forked revocation's registers revoke nothing: its subject keeps its roster seat",
        );
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
