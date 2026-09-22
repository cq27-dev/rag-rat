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
const BUNDLE_DOMAIN: &str = "rag-rat/control-checkpoint-bundle/1";
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

    /// The account this bundle certifies, read from the certificate itself.
    ///
    /// For DISPLAY before an irreversible install — an operator inspecting a bundle needs to see
    /// which account it is for, and the answer must come from the bundle rather than from whatever
    /// account the inspecting store happens to hold. Reading it here establishes no trust: the
    /// certificate is not checked against a pin, so a caller deciding anything on this value alone
    /// is trusting the file's own claim about itself.
    pub fn certificate_account(&self) -> anyhow::Result<AccountId> {
        Ok(decode_certificate(&self.certificate)?.account)
    }

    /// The transport form: one canonical CBOR item a proposal is written to and an installing store
    /// reads back.
    ///
    /// The envelope is NOT covered by [`Self::certificate_digest`], and must never become so. That
    /// digest is `sha256(certificate)`, and it is the value an operator relays out of band and
    /// types into `install` — so wrapping the certificate for transport cannot be allowed to change
    /// it, or a bundle proposed by one release would not match the digest relayed for it.
    ///
    /// Evidence order is preserved rather than sorted. [`evidence_digest`] sorts internally, so the
    /// commitment is already order-independent; imposing an order here would be a second, weaker
    /// rule about bytes that nothing reads.
    ///
    /// Encoding enforces every payload bound [`Self::decode`] enforces. Decode additionally bounds
    /// the total ENCODED length, which this does not check; the two agree only because that bound
    /// is sized to cover the largest output this can produce, and
    /// `the_maximum_shape_survives_a_round_trip` is what holds them to it.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Self::check_bounds(
            &self.certificate,
            self.evidence.len(),
            self.evidence.iter().map(Vec::len),
        )?;
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes);
        e.put_array(3);
        e.put_str(BUNDLE_DOMAIN);
        e.put_bytes(&self.certificate);
        e.put_array(self.evidence.len() as u64);
        for entry in &self.evidence {
            e.put_bytes(entry);
        }
        Ok(bytes)
    }

    /// Decode a transport bundle. Duplicate evidence is NOT checked here: `decode_evidence` already
    /// rejects it against `entry_hash`, and a second check over raw bytes would be a weaker copy of
    /// a rule that has a home.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        // Bound the input before recursively validating it or allocating its arrays.
        //
        // Sized FROM the constants, never a guessed slack: `check_bounds` limits payload lengths,
        // while the envelope adds a bstr header (up to 5 bytes) per entry plus its own framing. A
        // flat allowance below that lets `encode` emit a bundle this would refuse, which makes the
        // round-trip promise false at the maximum shape — where no ordinary test reaches.
        //
        // The framing term is derived rather than guessed, so that renaming the domain cannot
        // silently outgrow it: array(3) header, the domain string with its own header, then the
        // certificate and evidence-array headers.
        const FRAMING_MAX: usize = 1 + 2 + BUNDLE_DOMAIN.len() + 5 + 5;
        const ENVELOPE_OVERHEAD_MAX: usize =
            5 * CHECKPOINT_EVIDENCE_MAX_ENTRIES + CERTIFICATE_MAX_BYTES + FRAMING_MAX;
        anyhow::ensure!(
            bytes.len() <= CHECKPOINT_EVIDENCE_MAX_BYTES + ENVELOPE_OVERHEAD_MAX,
            "checkpoint bundle exceeds protocol limits"
        );
        cbor::require_canonical_cbor(bytes)?;
        let mut d = Decoder::new(bytes);
        anyhow::ensure!(
            d.array()? == Some(3) && d.str()? == BUNDLE_DOMAIN,
            "checkpoint bundle grammar"
        );
        // Bound the certificate against its OWN limit before copying it. The pre-check above admits
        // ~16 MiB because evidence may be that large, so a bundle is free to spend that entire
        // budget on a certificate instead — and copying it first would let a remote sender cost the
        // decoder thousands of times what the certificate limit permits, before anything refuses.
        let certificate = d.bytes()?;
        anyhow::ensure!(
            certificate.len() <= CERTIFICATE_MAX_BYTES,
            "checkpoint certificate exceeds protocol limit"
        );
        let certificate = certificate.to_vec();
        let count = d.array()?.ok_or_else(|| anyhow::anyhow!("indefinite bundle evidence"))?;
        anyhow::ensure!(
            count <= CHECKPOINT_EVIDENCE_MAX_ENTRIES as u64,
            "checkpoint evidence count exceeds protocol limit"
        );
        let mut evidence = Vec::with_capacity(count as usize);
        for _ in 0..count {
            evidence.push(d.bytes()?.to_vec());
        }
        Self::check_bounds(&certificate, evidence.len(), evidence.iter().map(Vec::len))?;
        let bundle = Self { certificate, evidence };
        // Re-encode and compare, so a stored bundle is byte-identical to what it decodes to and no
        // alternative encoding of the same content is accepted.
        anyhow::ensure!(
            d.position() == bytes.len() && bundle.encode()? == bytes,
            "noncanonical checkpoint bundle"
        );
        Ok(bundle)
    }

    /// The protocol bounds, shared by both directions so they cannot drift apart.
    fn check_bounds(
        certificate: &[u8],
        count: usize,
        mut lengths: impl Iterator<Item = usize>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            certificate.len() <= CERTIFICATE_MAX_BYTES,
            "checkpoint certificate exceeds protocol limit"
        );
        anyhow::ensure!(
            count <= CHECKPOINT_EVIDENCE_MAX_ENTRIES,
            "checkpoint evidence count exceeds protocol limit"
        );
        let total = lengths.try_fold(0usize, |total, len| total.checked_add(len));
        anyhow::ensure!(
            total.is_some_and(|total| total <= CHECKPOINT_EVIDENCE_MAX_BYTES),
            "checkpoint evidence bytes exceed protocol limit"
        );
        Ok(())
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

/// Build a proposal, owning the read transaction [`prepare_checkpoint_in_tx`] requires.
///
/// Proposing approves nothing and installs nothing. The bundle it returns is inert until some
/// operator relays its digest out of band and another store is told to expect exactly that value.
pub fn propose_checkpoint(
    conn: &rusqlite::Connection,
    account: AccountId,
    signer: &LocalDevice,
) -> anyhow::Result<CheckpointBundle> {
    // Deferred: this path only reads. The transaction exists to give the evidence scan and the
    // certificate it signs one consistent snapshot, not to write anything, so it is dropped
    // (rolled back) rather than committed.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
    prepare_checkpoint_in_tx(&tx, account, signer)
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

    /// The transport bytes are frozen: a bundle written by one release must decode in the next.
    #[test]
    fn golden_checkpoint_bundle() {
        let (founder, account, genesis, _) = fixture();
        let bundle = prepare_checkpoint(account, &[genesis], &founder.secret).unwrap();
        assert_eq!(
            rag_rat_base::hash::hex_lower(&bundle.encode().unwrap()),
            "8378237261672d7261742f636f6e74726f6c2d636865636b706f696e742d62756e646c652f315901568378237261672d7261742f636f6e74726f6c2d636865636b706f696e742d7369676e65642f3158ec88781c7261672d7261742f636f6e74726f6c2d636865636b706f696e742f31582034d26eede7b519569c485cac41338c03c3e61fd6bd50cd98263ae9057ddc6dc7025820a96cd5ba0219bfc8413c7cfdad50eb31cf49b9c77615158b47cd783a033b4d165820a8a828409eff856f24388d05f86df5f04083f157feea00d970caa9b4d868451458204e12a5b735749810791391f3c600fd98514becbb5736f182549c282a89b0fdc658208a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c5820a96cd5ba0219bfc8413c7cfdad50eb31cf49b9c77615158b47cd783a033b4d1658408c97e9d96dbb1d0a09fa2539ec0b9f27959b706aacff762b1c84a5a662b15da0efbc565843acc6f6bf8ea722917b53deebb48d3124e9d1ce021c35a447feb50e815901238378187261672d7261742f6163636f756e742d7369676e65642f3158c48258678d777261672d7261742f6163636f756e742d656e7472792f31582034d26eede7b519569c485cac41338c03c3e61fd6bd50cd98263ae9057ddc6dc700582034750f98bd59fcfc946da45aaabe933be154a4b5094e1c4abf42866505f3c97e00f6f600010000f6f658588558208a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c582000f81ab0eb0e18ddb5e247d38f25c7a352c39fc77a74cb3f739d309f69ee4878500000000000000000000000000000000000f6584014029bc87a9022fb39d9f806cb349b515d8ad9369b84e6de857e5dcb7e6a7b153463f6a825e021a65862d65e5b9a37bda2ebdf55d9eb8ec4ab5f768544e7a20f",
        );
    }

    /// Wrapping a certificate for transport must not change the digest an operator relays, or a
    /// bundle proposed by one release would not match the digest circulated for it.
    #[test]
    fn the_envelope_does_not_disturb_the_relayed_digest() {
        let (founder, account, genesis, _) = fixture();
        let bundle = prepare_checkpoint(account, &[genesis], &founder.secret).unwrap();
        let round_tripped = CheckpointBundle::decode(&bundle.encode().unwrap()).unwrap();
        assert_eq!(round_tripped, bundle, "the round trip is lossless");
        assert_eq!(
            round_tripped.certificate_digest(),
            bundle.certificate_digest(),
            "the digest covers the certificate alone, not the envelope",
        );
    }

    /// Evidence order is preserved, not sorted. `evidence_digest` sorts internally, so imposing an
    /// order here would be a second rule about bytes nothing reads — and one that could silently
    /// disagree with what a peer wrote.
    #[test]
    fn the_envelope_preserves_evidence_order() {
        let (_founder, account, genesis, _) = fixture();
        // A SECOND device at seq 0, not the founder at seq 1: the envelope enforces `prev_hash is
        // null iff seq == 0`, and chaining would imply a relationship this test is not about. All
        // it needs is two distinct blobs it can present in two orders.
        let other = Dev::new(2);
        let extra = envelope::sign_account_entry(
            &other.secret,
            &AccountEntryHeader {
                account_id: account,
                log_id: 0,
                device_fingerprint: other.fp,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: 99,
                op_version: 1,
                crypto_suite: 0,
                auth_len: 0,
                key_id: None,
                authority_ref: None,
            },
            &[0x81, 0x01],
        )
        .unwrap();
        let forward = CheckpointBundle {
            certificate: vec![0x01, 0x02],
            evidence: vec![genesis.clone(), extra.signed_bytes.clone()],
        };
        let reversed = CheckpointBundle {
            certificate: vec![0x01, 0x02],
            evidence: vec![extra.signed_bytes, genesis],
        };
        assert_ne!(forward.encode().unwrap(), reversed.encode().unwrap());
        assert_eq!(CheckpointBundle::decode(&forward.encode().unwrap()).unwrap(), forward);
        assert_eq!(CheckpointBundle::decode(&reversed.encode().unwrap()).unwrap(), reversed);
    }

    /// The MAXIMUM shape round-trips, which is the case a guessed decode allowance breaks.
    ///
    /// `check_bounds` limits payload lengths, but the envelope adds a bstr header per entry. At the
    /// entry and byte ceilings together that overhead is ~20 KiB, so a pre-check with a flat 1 KiB
    /// slack would refuse a bundle this store had just written — and every ordinary test sits far
    /// below the ceiling, so nothing else would notice.
    #[test]
    fn the_maximum_shape_survives_a_round_trip() {
        let entry = vec![0u8; CHECKPOINT_EVIDENCE_MAX_BYTES / CHECKPOINT_EVIDENCE_MAX_ENTRIES];
        let bundle = CheckpointBundle {
            certificate: vec![0u8; CERTIFICATE_MAX_BYTES],
            evidence: vec![entry; CHECKPOINT_EVIDENCE_MAX_ENTRIES],
        };
        let encoded = bundle.encode().expect("the maximum shape encodes");
        assert_eq!(
            CheckpointBundle::decode(&encoded).expect("and decodes"),
            bundle,
            "a bundle this store can write is one it can read back",
        );
    }

    /// Over-limit bundles built by ANOTHER host are refused by `decode` on its own terms.
    ///
    /// Every other malformed case reaches the decoder through `encode`, which refuses to produce
    /// these at all — so without raw inputs the decoder's own bounds are never exercised by
    /// anything, and the entry-count limit that runs before the evidence vector is allocated has no
    /// test at all.
    #[test]
    fn a_decoder_refuses_over_limit_bundles_it_did_not_write() {
        fn raw(certificate: &[u8], evidence: &[Vec<u8>]) -> Vec<u8> {
            let mut bytes = Vec::new();
            let mut e = Encoder::new(&mut bytes);
            e.put_array(3);
            e.put_str(BUNDLE_DOMAIN);
            e.put_bytes(certificate);
            e.put_array(evidence.len() as u64);
            for entry in evidence {
                e.put_bytes(entry);
            }
            bytes
        }

        assert!(
            CheckpointBundle::decode(&raw(&vec![0; CERTIFICATE_MAX_BYTES + 1], &[])).is_err(),
            "a certificate over its own limit, inside a bundle small enough to reach the parser",
        );
        assert!(
            CheckpointBundle::decode(&raw(&[], &[vec![0; CHECKPOINT_EVIDENCE_MAX_BYTES + 1]]))
                .is_err(),
            "evidence over the byte limit",
        );
        assert!(
            CheckpointBundle::decode(&raw(&[], &vec![
                vec![0x01];
                CHECKPOINT_EVIDENCE_MAX_ENTRIES + 1
            ]))
            .is_err(),
            "evidence over the entry limit",
        );
        assert_eq!(
            CheckpointBundle::decode(&raw(&[], &[])).unwrap(),
            CheckpointBundle { certificate: vec![], evidence: vec![] },
            "while the minimal shape still round-trips",
        );
    }

    /// Every way a transport bundle can be malformed, refused before anything is allocated or
    /// trusted. An accepted alternative encoding of the same content would mean two byte strings
    /// name one checkpoint, which is what the re-encode comparison exists to prevent.
    #[test]
    fn a_malformed_transport_bundle_is_refused() {
        let (founder, account, genesis, _) = fixture();
        let bundle = prepare_checkpoint(account, &[genesis], &founder.secret).unwrap();
        let good = bundle.encode().unwrap();

        assert!(CheckpointBundle::decode(&good[..good.len() - 1]).is_err(), "truncated");
        let mut trailing = good.clone();
        trailing.push(0x00);
        assert!(CheckpointBundle::decode(&trailing).is_err(), "trailing bytes");

        // A different domain is a different format, not a newer one.
        let mut wrong_domain = Vec::new();
        {
            let mut e = Encoder::new(&mut wrong_domain);
            e.put_array(3);
            e.put_str("rag-rat/control-checkpoint-bundle/2");
            e.put_bytes(&bundle.certificate);
            e.put_array(bundle.evidence.len() as u64);
            for entry in &bundle.evidence {
                e.put_bytes(entry);
            }
        }
        assert!(CheckpointBundle::decode(&wrong_domain).is_err(), "wrong domain");

        // Bounds are enforced by ENCODE too, so a bundle this store cannot read is one it also
        // cannot write.
        let oversize_cert =
            CheckpointBundle { certificate: vec![0; CERTIFICATE_MAX_BYTES + 1], evidence: vec![] };
        assert!(oversize_cert.encode().is_err(), "oversize certificate");
        let too_many = CheckpointBundle {
            certificate: bundle.certificate.clone(),
            evidence: vec![vec![0x01]; CHECKPOINT_EVIDENCE_MAX_ENTRIES + 1],
        };
        assert!(too_many.encode().is_err(), "evidence count over limit");
        let too_large = CheckpointBundle {
            certificate: bundle.certificate,
            evidence: vec![vec![0; CHECKPOINT_EVIDENCE_MAX_BYTES + 1]],
        };
        assert!(too_large.encode().is_err(), "evidence bytes over limit");
    }
}
