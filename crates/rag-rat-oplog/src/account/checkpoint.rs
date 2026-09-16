//! Standalone signed checkpoint proposals. A valid historical signature is not a trust decision:
//! verification requires the digest obtained independently from the operator or trusted transfer.
//! These APIs do not install a pin or enable control v2.

use std::collections::{HashMap, HashSet};

use minicbor::{Decoder, Encoder};

use super::envelope::{self, SignedAccountEntry, VerifiedAccountEntry};
use super::fold::{self, AccountClassification, AuthorityQuery};
use super::id::{self, AccountEntryHash, AccountId, OwnerId};
use super::ops::DeviceCut;
use super::{annex, candidate, storage};
use crate::cbor::{self, VecEncoderExt};
use crate::device::{DevicePublic, DeviceSecret};
use crate::identity::LocalDevice;
use crate::op::DeviceFingerprint;

const CHECKPOINT_DOMAIN: &str = "rag-rat/control-checkpoint/1";
const SIGNED_DOMAIN: &str = "rag-rat/control-checkpoint-signed/1";
const EVIDENCE_DOMAIN: &str = "rag-rat/control-checkpoint-evidence/1";
/// Protocol bounds, independent of the receiver's configurable ingestion budgets.
pub const CHECKPOINT_EVIDENCE_MAX_ENTRIES: usize = 4096;
pub const CHECKPOINT_EVIDENCE_MAX_BYTES: usize = 16 * 1024 * 1024;
const CERTIFICATE_MAX_BYTES: usize = 1024;

/// External trust input. Never construct this from an unsolicited peer advertisement or receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedCheckpointPin {
    pub account_id: AccountId,
    pub checkpoint_digest: [u8; 32],
    pub required_control_version: u32,
}

/// Exportable proof, including losing forks. The evidence order is immaterial; duplicates reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointBundle {
    pub certificate: Vec<u8>,
    pub evidence: Vec<Vec<u8>>,
}

impl CheckpointBundle {
    /// Digest for operator review/export. Computing it does not establish trust in this bundle.
    pub fn certificate_digest(&self) -> [u8; 32] {
        cbor::sha256(&self.certificate)
    }
}

/// Verified against an externally supplied pin and the exact declared legacy evidence.
/// This is deliberately opaque: neither an arbitrary snapshot nor a caller-built projection can
/// be substituted for the verified legacy branch/authority closure.
pub struct VerifiedCheckpoint {
    pin: TrustedCheckpointPin,
    bundle: CheckpointBundle,
    forked: HashSet<AccountEntryHash>,
    continuation_heads: Vec<DeviceCut>,
    frozen: fold::v2::FrozenLegacy,
}

impl VerifiedCheckpoint {
    pub(super) fn frozen_legacy(&self) -> &fold::v2::FrozenLegacy {
        &self.frozen
    }

    pub fn pin(&self) -> TrustedCheckpointPin {
        self.pin
    }
    pub fn bundle(&self) -> &CheckpointBundle {
        &self.bundle
    }
    pub fn accepted_legacy_entries(&self) -> impl Iterator<Item = AccountEntryHash> + '_ {
        self.frozen.accepted_entries()
    }
    /// Branch/authority-closure losers, distinct from other nonaccepted legacy entries.
    pub fn forked_legacy_entries(&self) -> impl Iterator<Item = AccountEntryHash> + '_ {
        self.forked.iter().copied()
    }
    /// All nonaccepted control candidates, including parked/rejected entries. This is diagnostic
    /// membership, not permission to drop their evidence or their legacy register contributions.
    pub fn nonaccepted_legacy_entries(&self) -> impl Iterator<Item = AccountEntryHash> + '_ {
        self.frozen
            .entries()
            .iter()
            .filter(|entry| {
                entry.header.log_id == fold::CONTROL_LOG
                    && !self.frozen.accepted_at_checkpoint(&entry.entry_hash)
            })
            .map(|entry| entry.entry_hash)
    }
    pub fn continuation_heads(&self) -> &[DeviceCut] {
        &self.continuation_heads
    }
}

#[derive(Debug)]
pub enum CheckpointError {
    PinMismatch,
    MissingEvidence,
    Invalid(anyhow::Error),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PinMismatch =>
                f.write_str("checkpoint does not match the externally trusted pin"),
            Self::MissingEvidence => f.write_str(
                "checkpoint evidence is incomplete or differs from the signed commitment",
            ),
            Self::Invalid(error) => write!(f, "invalid checkpoint: {error}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

impl From<anyhow::Error> for CheckpointError {
    fn from(error: anyhow::Error) -> Self {
        Self::Invalid(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Certificate {
    account: AccountId,
    genesis: AccountEntryHash,
    evidence_digest: [u8; 32],
    projection_hash: [u8; 32],
    signer_key: [u8; 32],
    incarnation: OwnerId,
}

impl Certificate {
    fn body(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes);
        e.put_array(8);
        e.put_str(CHECKPOINT_DOMAIN);
        e.put_bytes(&self.account.to_bytes());
        e.put_u64(2);
        e.put_bytes(self.genesis.as_slice());
        e.put_bytes(&self.evidence_digest);
        e.put_bytes(&self.projection_hash);
        e.put_bytes(&self.signer_key);
        e.put_bytes(self.incarnation.as_slice());
        bytes
    }
}

fn signed_certificate(body: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes);
    e.put_array(3);
    e.put_str(SIGNED_DOMAIN);
    e.put_bytes(body);
    e.put_bytes(signature);
    bytes
}

fn decode_certificate(bytes: &[u8]) -> anyhow::Result<Certificate> {
    anyhow::ensure!(bytes.len() <= CERTIFICATE_MAX_BYTES, "oversized checkpoint certificate");
    cbor::require_canonical_cbor(bytes)?;
    let mut outer = Decoder::new(bytes);
    anyhow::ensure!(
        outer.array()? == Some(3) && outer.str()? == SIGNED_DOMAIN,
        "checkpoint envelope grammar"
    );
    let body = outer.bytes()?;
    let signature = id::fixed::<64>(outer.bytes()?)?;
    anyhow::ensure!(outer.position() == bytes.len(), "trailing checkpoint bytes");
    cbor::require_canonical_cbor(body)?;
    let mut d = Decoder::new(body);
    anyhow::ensure!(
        d.array()? == Some(8) && d.str()? == CHECKPOINT_DOMAIN,
        "checkpoint body grammar"
    );
    let account = AccountId::from_bytes(id::fixed::<32>(d.bytes()?)?);
    anyhow::ensure!(d.u32()? == 2, "unsupported checkpoint control version");
    let cert = Certificate {
        account,
        genesis: id::fixed::<32>(d.bytes()?)?.into(),
        evidence_digest: id::fixed(d.bytes()?)?,
        projection_hash: id::fixed(d.bytes()?)?,
        signer_key: id::fixed(d.bytes()?)?,
        incarnation: id::fixed::<32>(d.bytes()?)?.into(),
    };
    anyhow::ensure!(
        d.position() == body.len() && cert.body() == body,
        "noncanonical checkpoint body"
    );
    DevicePublic::from_bytes(&cert.signer_key)?.verify(body, &signature)?;
    Ok(cert)
}

fn evidence_digest(hashes: impl Iterator<Item = AccountEntryHash>) -> [u8; 32] {
    let mut hashes = hashes.collect::<Vec<_>>();
    hashes.sort_unstable();
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes);
    e.put_array(2);
    e.put_str(EVIDENCE_DOMAIN);
    e.put_array(hashes.len() as u64);
    for hash in hashes {
        e.put_bytes(hash.as_slice());
    }
    cbor::sha256(&bytes)
}

fn decode_evidence(evidence: &[Vec<u8>]) -> anyhow::Result<Vec<SignedAccountEntry>> {
    anyhow::ensure!(
        evidence.len() <= CHECKPOINT_EVIDENCE_MAX_ENTRIES,
        "checkpoint evidence count exceeds protocol limit"
    );
    let bytes = evidence.iter().try_fold(0usize, |total, bytes| total.checked_add(bytes.len()));
    anyhow::ensure!(
        bytes.is_some_and(|bytes| bytes <= CHECKPOINT_EVIDENCE_MAX_BYTES),
        "checkpoint evidence bytes exceed protocol limit"
    );
    let entries = evidence
        .iter()
        .map(|bytes| envelope::decode_account_signed(bytes))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut hashes = HashSet::new();
    anyhow::ensure!(
        entries.iter().all(|entry| hashes.insert(entry.entry_hash)),
        "duplicate checkpoint evidence"
    );
    Ok(entries)
}

fn verify_evidence(
    account: AccountId,
    signed: &[SignedAccountEntry],
) -> anyhow::Result<Vec<VerifiedAccountEntry>> {
    // Validate every envelope before discovering any key, exactly as account ingestion does.
    for entry in signed {
        anyhow::ensure!(entry.header.account_id == account, "foreign checkpoint evidence");
        anyhow::ensure!(entry.header.op_version == 1, "checkpoint evidence is not legacy v1");
        storage::validate_storable_header_payload(&entry.header, &entry.payload)
            .map_err(anyhow::Error::msg)?;
    }
    let mut keys = HashMap::new();
    let mut pending = signed.iter().collect::<Vec<_>>();
    let mut verified = Vec::with_capacity(signed.len());
    while !pending.is_empty() {
        let before = pending.len();
        let mut remaining = Vec::new();
        for entry in pending {
            // An entry may supply its own signer key, but its introduction of another key only
            // becomes available after this entry authenticates. Pooling unverified introductions
            // would admit mutually introducing cycles that a fresh store cannot import.
            let mut self_keys = HashMap::new();
            storage::add_self_pubkey(&mut self_keys, &entry.header, &entry.payload);
            let Some(key) = keys
                .get(&entry.header.device_fingerprint)
                .or_else(|| self_keys.get(&entry.header.device_fingerprint))
            else {
                remaining.push(entry);
                continue;
            };
            let authenticated = storage::authenticate_entry(&entry.signed_bytes, key)
                .map_err(anyhow::Error::msg)?;
            storage::add_self_pubkey(&mut keys, &authenticated.header, &authenticated.payload);
            verified.push(authenticated);
        }
        anyhow::ensure!(
            remaining.len() < before,
            "checkpoint evidence has unresolved signer dependencies"
        );
        pending = remaining;
    }
    Ok(verified)
}

/// Verify a proof against independent trust. A historically authorized signer does not authorize
/// a competing digest: the pin comparison precedes all historical replay.
pub fn verify_checkpoint(
    expected: TrustedCheckpointPin,
    bundle: &CheckpointBundle,
) -> Result<VerifiedCheckpoint, CheckpointError> {
    if bundle.certificate.len() > CERTIFICATE_MAX_BYTES {
        return Err(CheckpointError::Invalid(anyhow::anyhow!("oversized checkpoint certificate")));
    }
    if expected.required_control_version != 2
        || cbor::sha256(&bundle.certificate) != expected.checkpoint_digest
    {
        return Err(CheckpointError::PinMismatch);
    }
    let certificate = decode_certificate(&bundle.certificate)?;
    if certificate.account != expected.account_id {
        return Err(CheckpointError::PinMismatch);
    }
    let signed = decode_evidence(&bundle.evidence)?;
    if evidence_digest(signed.iter().map(|entry| entry.entry_hash)) != certificate.evidence_digest {
        return Err(CheckpointError::MissingEvidence);
    }
    let entries = verify_evidence(certificate.account, &signed)?;
    verify_projection(expected, bundle, certificate, entries).map_err(CheckpointError::Invalid)
}

fn verify_projection(
    expected: TrustedCheckpointPin,
    bundle: &CheckpointBundle,
    certificate: Certificate,
    entries: Vec<VerifiedAccountEntry>,
) -> anyhow::Result<VerifiedCheckpoint> {
    let (projection, trace) = storage::project_checkpoint_with_trace(&entries);
    anyhow::ensure!(
        projection.history.classification() == AccountClassification::Live,
        "checkpoint account is contested"
    );
    anyhow::ensure!(
        projection.history.genesis_hash() == Some(certificate.genesis),
        "checkpoint genesis mismatch"
    );
    anyhow::ensure!(
        annex::projection::folded_state_hash(&projection.history) == certificate.projection_hash,
        "checkpoint legacy projection mismatch"
    );
    let signer = DevicePublic::from_bytes(&certificate.signer_key)?.fingerprint();
    anyhow::ensure!(
        matches!(
            projection.history.owner_incarnation_effective(certificate.incarnation, signer),
            AuthorityQuery::Effective(_)
        ),
        "checkpoint signer is not a live owner in the declared legacy view"
    );
    let mut heads: HashMap<DeviceFingerprint, DeviceCut> = HashMap::new();
    let headers =
        entries.iter().map(|e| (e.entry_hash, e.header.clone())).collect::<HashMap<_, _>>();
    for entry in &entries {
        if entry.header.log_id == fold::CONTROL_LOG
            && projection.accepted.contains(&entry.entry_hash)
        {
            let head = heads.entry(entry.header.device_fingerprint).or_insert(DeviceCut {
                device_fingerprint: entry.header.device_fingerprint,
                seq: entry.header.seq,
                hash: entry.entry_hash,
            });
            if entry.header.seq > head.seq {
                head.seq = entry.header.seq;
                head.hash = entry.entry_hash;
            }
        }
    }
    let mut continuation_heads = heads.into_values().collect::<Vec<_>>();
    continuation_heads.sort_unstable_by_key(|head| head.device_fingerprint.to_bytes());
    let branch =
        candidate::resolve_control_frontier(certificate.account, &continuation_heads, &headers)
            .map_err(|error| {
                anyhow::anyhow!("invalid checkpoint continuation branches: {error:?}")
            })?;
    anyhow::ensure!(branch == projection.accepted, "checkpoint accepted branches are incomplete");
    let frozen = fold::v2::FrozenLegacy::new(
        entries,
        projection.history,
        trace.ok_or_else(|| anyhow::anyhow!("checkpoint has no final legacy trace"))?,
        projection.accepted,
    );
    Ok(VerifiedCheckpoint {
        pin: expected,
        bundle: bundle.clone(),
        forked: projection.forked,
        continuation_heads,
        frozen,
    })
}

/// Build a proposal from one caller-held transactional snapshot. This does not approve or install
/// it. Every peer must independently receive the same digest before any eventual activation.
pub fn prepare_checkpoint_in_tx(
    tx: &rusqlite::Transaction<'_>,
    account: AccountId,
    signer: &LocalDevice,
) -> anyhow::Result<CheckpointBundle> {
    super::control_policy::require_supported_account_control(tx, account)?;
    let evidence = storage::account_entries_for_enrollment(tx, account)?
        .into_iter()
        .map(|entry| entry.signed_bytes)
        .collect::<Vec<_>>();
    prepare_checkpoint(account, &evidence, signer.secret())
}

fn prepare_checkpoint(
    account: AccountId,
    evidence: &[Vec<u8>],
    signer: &DeviceSecret,
) -> anyhow::Result<CheckpointBundle> {
    let entries = verify_evidence(account, &decode_evidence(evidence)?)?;
    let projection = storage::project_verified_checkpoint_evidence(&entries);
    let incarnation = projection
        .history
        .owner_incarnation_facts()
        .find_map(|(id, fact)| {
            (fact.authority.device_fingerprint == signer.public().fingerprint()
                && fact.closed_at.is_none())
            .then_some(*id)
        })
        .ok_or_else(|| anyhow::anyhow!("checkpoint proposal requires an open owner incarnation"))?;
    let cert = Certificate {
        account,
        genesis: projection
            .history
            .genesis_hash()
            .ok_or_else(|| anyhow::anyhow!("checkpoint has no genesis"))?,
        evidence_digest: evidence_digest(entries.iter().map(|entry| entry.entry_hash)),
        projection_hash: annex::projection::folded_state_hash(&projection.history),
        signer_key: signer.public().to_bytes(),
        incarnation,
    };
    let body = cert.body();
    let bundle = CheckpointBundle {
        certificate: signed_certificate(&body, &signer.sign(&body)),
        evidence: evidence.to_vec(),
    };
    // Internal self-check is not an external trust decision and installs nothing.
    verify_checkpoint(
        TrustedCheckpointPin {
            account_id: account,
            checkpoint_digest: cbor::sha256(&bundle.certificate),
            required_control_version: 2,
        },
        &bundle,
    )?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::super::envelope::AccountEntryHeader;
    use super::super::ops::{self, AccountOp};
    use super::super::test_support::Dev;
    use super::*;

    fn fixture() -> (Dev, AccountId, Vec<u8>, AccountEntryHash) {
        let founder = Dev::new(1);
        let payload = ops::encode(&AccountOp::AccountGenesis {
            ed25519_pubkey: founder.ed,
            x25519_pubkey: founder.x,
            nonce16: [0; 16],
            created_at_ms: 0,
            label: None,
        })
        .unwrap();
        let account = id::account_id_from_genesis_payload(&payload);
        let signed = envelope::sign_account_entry(
            &founder.secret,
            &AccountEntryHeader {
                account_id: account,
                log_id: 0,
                device_fingerprint: founder.fp,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: 0,
                op_version: 1,
                crypto_suite: 0,
                auth_len: 0,
                key_id: None,
                authority_ref: None,
            },
            &payload,
        )
        .unwrap();
        (founder, account, signed.signed_bytes, signed.entry_hash)
    }

    fn pin(account: AccountId, bundle: &CheckpointBundle) -> TrustedCheckpointPin {
        TrustedCheckpointPin {
            account_id: account,
            checkpoint_digest: bundle.certificate_digest(),
            required_control_version: 2,
        }
    }

    // A signer can commit to unusable evidence while keeping the genesis-only projection claim.
    // Construct that hostile certificate independently of the safe proposal authoring API.
    fn certify_extra_evidence(
        founder: &Dev,
        account: AccountId,
        genesis: Vec<u8>,
        extra: Vec<Vec<u8>>,
    ) -> CheckpointBundle {
        let mut bundle = prepare_checkpoint(account, &[genesis], &founder.secret).unwrap();
        bundle.evidence.extend(extra);
        let mut cert = decode_certificate(&bundle.certificate).unwrap();
        cert.evidence_digest = evidence_digest(
            bundle
                .evidence
                .iter()
                .map(|bytes| envelope::decode_account_signed(bytes).unwrap().entry_hash),
        );
        let body = cert.body();
        bundle.certificate = signed_certificate(&body, &founder.secret.sign(&body));
        bundle
    }

    #[test]
    fn checkpoint_and_ingestion_reject_the_same_unstorable_evidence() {
        let (founder, account, genesis, hash) = fixture();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        storage::account_ingest(&conn, &genesis, 0).unwrap();
        for (log_id, entry_type, crypto_suite, seq) in [
            (1, 0, 0, 0),         // malformed plaintext secrets
            (3, 0, 0, 0),         // malformed plaintext snapshot
            (3, 0, 1, 0),         // snapshots can never be sealed
            (0, 99, 0, u64::MAX), // valid opaque payload, unstorable sequence
        ] {
            let signed = envelope::sign_account_entry(
                &founder.secret,
                &AccountEntryHeader {
                    account_id: account,
                    log_id,
                    device_fingerprint: founder.fp,
                    seq,
                    prev_hash: (seq != 0).then_some(hash),
                    parent_ref: Some(hash),
                    entry_type,
                    op_version: 1,
                    crypto_suite,
                    auth_len: 1,
                    key_id: (crypto_suite != 0).then_some([0; 32]),
                    authority_ref: Some(hash.into()),
                },
                &[0x80],
            )
            .unwrap();
            assert!(matches!(
                storage::account_ingest(&conn, &signed.signed_bytes, 0).unwrap(),
                storage::IngestOutcome::Rejected(_)
            ));
            let bundle = certify_extra_evidence(&founder, account, genesis.clone(), vec![
                signed.signed_bytes,
            ]);
            assert!(matches!(
                verify_checkpoint(pin(account, &bundle), &bundle),
                Err(CheckpointError::Invalid(_))
            ));
        }
    }

    #[test]
    fn unrooted_mutual_key_introductions_cannot_authenticate_a_checkpoint() {
        let (founder, account, genesis, hash) = fixture();
        let a = Dev::new(4);
        let b = Dev::new(5);
        let mut extra = Vec::new();
        for (signer, subject) in [(&a, &b), (&b, &a)] {
            let payload = ops::encode(&AccountOp::DeviceAdd {
                device_fingerprint: subject.fp,
                ed25519_pubkey: subject.ed,
                x25519_pubkey: subject.x,
                role: ops::DeviceRole::Member,
                label: None,
            })
            .unwrap();
            let signed = envelope::sign_account_entry(
                &signer.secret,
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: signer.fp,
                    seq: 0,
                    prev_hash: None,
                    parent_ref: Some(hash),
                    entry_type: 1,
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(hash.into()),
                },
                &payload,
            )
            .unwrap();
            extra.push(signed.signed_bytes);
        }
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        storage::account_ingest(&conn, &genesis, 0).unwrap();
        for bytes in &extra {
            storage::account_ingest(&conn, bytes, 0).unwrap();
        }
        assert_eq!(storage::account_entries_for_enrollment(&conn, account).unwrap().len(), 1);
        let bundle = certify_extra_evidence(&founder, account, genesis, extra);
        assert!(matches!(
            verify_checkpoint(pin(account, &bundle), &bundle),
            Err(CheckpointError::Invalid(_))
        ));
    }

    #[test]
    fn standalone_checkpoint_preserves_identity_and_requires_external_digest() {
        let (founder, account, genesis, hash) = fixture();
        let bundle =
            prepare_checkpoint(account, std::slice::from_ref(&genesis), &founder.secret).unwrap();
        let trusted = pin(account, &bundle);
        let proof = verify_checkpoint(trusted, &bundle).unwrap();
        assert_eq!(proof.pin(), trusted);
        assert_eq!(proof.bundle().evidence, vec![genesis]);
        assert_eq!(proof.accepted_legacy_entries().collect::<HashSet<_>>(), HashSet::from([hash]));
        assert_eq!(proof.continuation_heads()[0].hash, hash);
        let wrong = TrustedCheckpointPin { checkpoint_digest: [0; 32], ..trusted };
        assert!(matches!(verify_checkpoint(wrong, &bundle), Err(CheckpointError::PinMismatch)));
        let wrong = TrustedCheckpointPin { account_id: AccountId::from_bytes([99; 32]), ..trusted };
        assert!(matches!(verify_checkpoint(wrong, &bundle), Err(CheckpointError::PinMismatch)));
        assert_eq!(fold::SUPPORTED_OP_VERSION, 1);
    }

    #[test]
    fn missing_evidence_is_not_a_verified_empty_projection() {
        let (founder, account, genesis, _) = fixture();
        let bundle = prepare_checkpoint(account, &[genesis], &founder.secret).unwrap();
        let trusted = pin(account, &bundle);
        let missing = CheckpointBundle { evidence: vec![], ..bundle };
        assert!(matches!(
            verify_checkpoint(trusted, &missing),
            Err(CheckpointError::MissingEvidence)
        ));
    }

    #[test]
    fn stranger_cannot_certify_another_accounts_projection() {
        let (_, account, genesis, _) = fixture();
        let stranger = Dev::new(99);
        assert!(prepare_checkpoint(account, &[genesis], &stranger.secret).is_err());
    }

    #[test]
    fn nonaccepted_legacy_entries_are_not_all_branch_losers() {
        let (founder, account, genesis, hash) = fixture();
        let unknown = envelope::sign_account_entry(
            &founder.secret,
            &AccountEntryHeader {
                account_id: account,
                log_id: 0,
                device_fingerprint: founder.fp,
                seq: 1,
                prev_hash: Some(hash),
                parent_ref: Some(hash),
                entry_type: 99,
                op_version: 1,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(hash.into()),
            },
            &[0x80],
        )
        .unwrap();
        let unknown_hash = unknown.entry_hash;
        let bundle =
            prepare_checkpoint(account, &[genesis, unknown.signed_bytes], &founder.secret).unwrap();
        let proof = verify_checkpoint(pin(account, &bundle), &bundle).unwrap();
        assert!(proof.nonaccepted_legacy_entries().any(|hash| hash == unknown_hash));
        assert!(!proof.forked_legacy_entries().any(|hash| hash == unknown_hash));
        assert_eq!(proof.continuation_heads()[0].hash, hash);
    }

    #[test]
    fn tampered_certificate_fails_even_when_attacker_supplies_its_digest() {
        let (founder, account, genesis, _) = fixture();
        let mut bundle = prepare_checkpoint(account, &[genesis], &founder.secret).unwrap();
        *bundle.certificate.last_mut().unwrap() ^= 1;
        assert!(matches!(
            verify_checkpoint(pin(account, &bundle), &bundle),
            Err(CheckpointError::Invalid(_))
        ));
    }

    #[test]
    fn duplicate_and_oversized_evidence_are_invalid_not_truncated() {
        let (founder, account, genesis, _) = fixture();
        assert!(
            prepare_checkpoint(account, &[genesis.clone(), genesis.clone()], &founder.secret)
                .is_err()
        );
        let many = vec![genesis; CHECKPOINT_EVIDENCE_MAX_ENTRIES + 1];
        assert!(prepare_checkpoint(account, &many, &founder.secret).is_err());
        let oversized = vec![vec![0; CHECKPOINT_EVIDENCE_MAX_BYTES + 1]];
        assert!(prepare_checkpoint(account, &oversized, &founder.secret).is_err());
    }

    #[test]
    fn losing_forks_are_committed_and_replay_is_order_independent() {
        let (founder, account, genesis, hash) = fixture();
        let mut evidence = vec![genesis];
        let mut forks = Vec::new();
        for seed in [2, 3] {
            let device = Dev::new(seed);
            let op = AccountOp::DeviceAdd {
                device_fingerprint: device.fp,
                ed25519_pubkey: device.ed,
                x25519_pubkey: device.x,
                role: ops::DeviceRole::Member,
                label: None,
            };
            let signed = envelope::sign_account_entry(
                &founder.secret,
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: founder.fp,
                    seq: 1,
                    prev_hash: Some(hash),
                    parent_ref: Some(hash),
                    entry_type: 1,
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(hash.into()),
                },
                &ops::encode(&op).unwrap(),
            )
            .unwrap();
            forks.push(signed.entry_hash);
            evidence.push(signed.signed_bytes);
        }
        let bundle = prepare_checkpoint(account, &evidence, &founder.secret).unwrap();
        let proof = verify_checkpoint(pin(account, &bundle), &bundle).unwrap();
        forks.sort_unstable();
        assert!(proof.accepted_legacy_entries().any(|hash| hash == forks[0]));
        assert!(proof.forked.contains(&forks[1]));
        evidence.reverse();
        let reordered = prepare_checkpoint(account, &evidence, &founder.secret).unwrap();
        assert_eq!(bundle.certificate, reordered.certificate);
        let mut stripped = bundle.clone();
        stripped
            .evidence
            .retain(|bytes| envelope::decode_account_signed(bytes).unwrap().entry_hash != forks[1]);
        assert!(matches!(
            verify_checkpoint(pin(account, &bundle), &stripped),
            Err(CheckpointError::MissingEvidence)
        ));
    }
}
