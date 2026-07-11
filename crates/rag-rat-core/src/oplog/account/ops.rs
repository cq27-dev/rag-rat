//! The control-log op payloads (`log_id = 0`) and their canonical-CBOR wire (§10).
//!
//! Each op is a domain-less fixed-length CBOR array carried as the `payload` bstr of an account
//! entry (the domain + versioning live in the envelope header, §6). The op is discriminated by the
//! header's `entry_type`, whose frozen tag set is [`entry_type`]. An UNKNOWN `entry_type` is
//! RETAINED, never rejected ([`DecodedAccountOp::Unknown`]) — a forward-version op still stores +
//! chains, exactly as `super::super::op` keeps a forward op opaque.
//!
//! This layer owns STRUCTURAL wire validity: arity, canonicity (`encode(decode) == bytes`), closed
//! role/kind tokens, §18a cut-array bounds, sorted-unique cut arrays, the `CutExtend` stream_id ⇔
//! kind coupling, and public-key point-validity (an ed25519 non-point / identity-or-small-order
//! X25519 key is refused here). The CRYPTO-CONSISTENCY + authority rules that need other entries or
//! the header (genesis self-hash, roster/grant citation, last-owner, StreamOwn spec match) live in
//! the fold ([`super::fold`]).

use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};

use super::AccountId;
use super::cut::Cut;
use super::limits::{CONTENT_CUTS_MAX, DEVICE_CUTS_MAX};
use crate::oplog::cbor;
use crate::oplog::device::{DevicePublic, DeviceX25519Public};
use crate::oplog::op::DeviceFingerprint;
use crate::oplog::stream::StreamId;

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::super`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// The frozen `entry_type` tag set (header part 7). A tag is a wire constant — a renumber is a wire
/// bump. Deferred control ops land ADDITIVELY at 10+ (`StandoffResolve` §12, `PublicStreamConfig`,
/// `ModerationOp` — C3/E). NOTE: §10 lists `StandoffResolve` before `AccountReRoot`, but
/// `AccountReRoot` is frozen at 9 here, so `StandoffResolve` takes a 10+ slot, never 9.
pub(super) mod entry_type {
    pub(in crate::oplog::account) const ACCOUNT_GENESIS: u32 = 0;
    pub(in crate::oplog::account) const DEVICE_ADD: u32 = 1;
    pub(in crate::oplog::account) const DEVICE_REMOVE: u32 = 2;
    pub(in crate::oplog::account) const OWNER_PROMOTE: u32 = 3;
    pub(in crate::oplog::account) const OWNER_DEMOTE: u32 = 4;
    pub(in crate::oplog::account) const CUT_EXTEND: u32 = 5;
    pub(in crate::oplog::account) const STREAM_OWN: u32 = 6;
    pub(in crate::oplog::account) const STREAM_GRANT: u32 = 7;
    pub(in crate::oplog::account) const STREAM_REVOKE: u32 = 8;
    pub(in crate::oplog::account) const ACCOUNT_REROOT: u32 = 9;
}

/// A device's role in an account roster (§9). Owner holds all control/secrets ops; a member authors
/// content on granted streams only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeviceRole {
    Member,
    Owner,
}

impl DeviceRole {
    fn as_u8(self) -> u8 {
        match self {
            DeviceRole::Member => 1,
            DeviceRole::Owner => 2,
        }
    }

    fn from_u8(value: u8) -> Result<Self, CborError> {
        match value {
            1 => Ok(DeviceRole::Member),
            2 => Ok(DeviceRole::Owner),
            other => Err(CborError::message(format!("unknown device role {other}"))),
        }
    }
}

/// A cross-account grant's role on a stream (§9). Reader = wrap recipient (sealed) / free (public);
/// writer = reader + content accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GrantRole {
    Reader,
    Writer,
}

impl GrantRole {
    fn as_u8(self) -> u8 {
        match self {
            GrantRole::Reader => 1,
            GrantRole::Writer => 2,
        }
    }

    fn from_u8(value: u8) -> Result<Self, CborError> {
        match value {
            1 => Ok(GrantRole::Reader),
            2 => Ok(GrantRole::Writer),
            other => Err(CborError::message(format!("unknown grant role {other}"))),
        }
    }
}

/// Which chain a `CutExtend` extends (§10). Content (2) requires a `stream_id`; ctrl/secrets do
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChainKind {
    Ctrl,
    Secrets,
    Content,
}

impl ChainKind {
    fn as_u8(self) -> u8 {
        match self {
            ChainKind::Ctrl => 0,
            ChainKind::Secrets => 1,
            ChainKind::Content => 2,
        }
    }

    fn from_u8(value: u8) -> Result<Self, CborError> {
        match value {
            0 => Ok(ChainKind::Ctrl),
            1 => Ok(ChainKind::Secrets),
            2 => Ok(ChainKind::Content),
            other => Err(CborError::message(format!("unknown chain kind {other}"))),
        }
    }
}

/// One content-chain cut inside a `DeviceRemove`: `[stream_id, seq, entry_hash]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContentCut {
    pub(super) stream_id: StreamId,
    pub(super) seq: u64,
    pub(super) hash: [u8; 32],
}

/// One device-chain cut inside a `StreamRevoke`: `[device_fingerprint, seq, entry_hash]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeviceCut {
    pub(super) device_fingerprint: DeviceFingerprint,
    pub(super) seq: u64,
    pub(super) hash: [u8; 32],
}

/// The ten control-log ops (§10). Field order is the frozen wire order; goldens pin the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AccountOp {
    AccountGenesis {
        ed25519_pubkey: [u8; 32],
        x25519_pubkey: [u8; 32],
        nonce16: [u8; 16],
        created_at_ms: u64,
        label: Option<String>,
    },
    DeviceAdd {
        device_fingerprint: DeviceFingerprint,
        ed25519_pubkey: [u8; 32],
        x25519_pubkey: [u8; 32],
        role: DeviceRole,
        label: Option<String>,
    },
    DeviceRemove {
        device_fingerprint: DeviceFingerprint,
        control_cut: Cut,
        secrets_cut: Cut,
        content_cuts: Vec<ContentCut>,
        reason: String,
    },
    OwnerPromote {
        device_fingerprint: DeviceFingerprint,
    },
    OwnerDemote {
        device_fingerprint: DeviceFingerprint,
        owner_id: [u8; 32],
        control_cut: Cut,
        secrets_cut: Cut,
        reason: String,
    },
    CutExtend {
        chain_kind: ChainKind,
        stream_id: Option<StreamId>,
        incarnation_id: Option<[u8; 32]>,
        subject_account_id: AccountId,
        device_fingerprint: DeviceFingerprint,
        new_seq: u64,
        new_entry_hash: [u8; 32],
    },
    StreamOwn {
        stream_id: StreamId,
        stream_spec_bytes: Vec<u8>,
    },
    StreamGrant {
        stream_id: StreamId,
        grantee_account_id: AccountId,
        grant_role: GrantRole,
    },
    StreamRevoke {
        stream_id: StreamId,
        grantee_account_id: AccountId,
        grant_id: [u8; 32],
        device_cuts: Vec<DeviceCut>,
        reason: String,
    },
    AccountReRoot {
        successor_account_id: AccountId,
        note: Option<String>,
    },
}

/// The result of decoding an op payload against its header `entry_type`: a known op or a retained
/// opaque forward-version op (never an error for an unrecognized `entry_type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DecodedAccountOp {
    Known(AccountOp),
    Unknown { entry_type: u32, bytes: Vec<u8> },
}

/// The `entry_type` tag an op carries in its header (§10).
pub(super) fn entry_type_of(op: &AccountOp) -> u32 {
    match op {
        AccountOp::AccountGenesis { .. } => entry_type::ACCOUNT_GENESIS,
        AccountOp::DeviceAdd { .. } => entry_type::DEVICE_ADD,
        AccountOp::DeviceRemove { .. } => entry_type::DEVICE_REMOVE,
        AccountOp::OwnerPromote { .. } => entry_type::OWNER_PROMOTE,
        AccountOp::OwnerDemote { .. } => entry_type::OWNER_DEMOTE,
        AccountOp::CutExtend { .. } => entry_type::CUT_EXTEND,
        AccountOp::StreamOwn { .. } => entry_type::STREAM_OWN,
        AccountOp::StreamGrant { .. } => entry_type::STREAM_GRANT,
        AccountOp::StreamRevoke { .. } => entry_type::STREAM_REVOKE,
        AccountOp::AccountReRoot { .. } => entry_type::ACCOUNT_REROOT,
    }
}

/// Encode an op to its canonical-CBOR payload (§10). Sorted cut arrays are emitted in binary-id
/// order (the decoder rejects an unsorted wire, so `encode(decode) == bytes`).
pub(super) fn encode(op: &AccountOp) -> Result<Vec<u8>, CborError> {
    Ok(encode_canonical(&canonicalize(op)?))
}

/// Validate + canonicalize an op so its encoding ALWAYS round-trips through [`decode`] — the
/// authoring path must never sign a payload the sorted-unique / bounded / self-consistent decoder
/// (and peers) would reject. Derives the `DeviceAdd` fingerprint from its own key, rejects invalid
/// public keys, sorts + dedups + reject-conflicting + bound-checks the cut arrays, and enforces the
/// `CutExtend` kind ⇔ stream_id coupling. Everything decode enforces at the op level is enforced
/// here too, so `encode` cannot emit dead bytes.
fn canonicalize(op: &AccountOp) -> Result<AccountOp, CborError> {
    Ok(match op {
        AccountOp::AccountGenesis { ed25519_pubkey, x25519_pubkey, .. } => {
            validate_keys(ed25519_pubkey, x25519_pubkey)?;
            op.clone()
        },
        AccountOp::DeviceAdd { ed25519_pubkey, x25519_pubkey, role, label, .. } => {
            validate_keys(ed25519_pubkey, x25519_pubkey)?;
            AccountOp::DeviceAdd {
                // The fingerprint is DERIVED data — sha256 of the added device's own ed25519 key. A
                // caller-supplied value is ignored, so a stale/placeholder fp can never be signed.
                device_fingerprint: DeviceFingerprint::from_bytes(cbor::sha256(ed25519_pubkey)),
                ed25519_pubkey: *ed25519_pubkey,
                x25519_pubkey: *x25519_pubkey,
                role: *role,
                label: label.clone(),
            }
        },
        AccountOp::DeviceRemove {
            device_fingerprint,
            control_cut,
            secrets_cut,
            content_cuts,
            reason,
        } => AccountOp::DeviceRemove {
            device_fingerprint: *device_fingerprint,
            control_cut: control_cut.clone(),
            secrets_cut: secrets_cut.clone(),
            content_cuts: canonical_content_cuts(content_cuts)?,
            reason: reason.clone(),
        },
        AccountOp::StreamRevoke {
            stream_id,
            grantee_account_id,
            grant_id,
            device_cuts,
            reason,
        } => AccountOp::StreamRevoke {
            stream_id: *stream_id,
            grantee_account_id: *grantee_account_id,
            grant_id: *grant_id,
            device_cuts: canonical_device_cuts(device_cuts)?,
            reason: reason.clone(),
        },
        AccountOp::CutExtend { chain_kind, stream_id, .. } => {
            if (*chain_kind == ChainKind::Content) != stream_id.is_some() {
                return Err(CborError::message(
                    "CutExtend stream_id is present iff chain_kind is content",
                ));
            }
            op.clone()
        },
        _ => op.clone(),
    })
}

/// Serialize an ALREADY-canonical op (see [`canonicalize`]) to CBOR. Infallible — the cut arrays
/// are pre-sorted-unique and every invariant has been checked, so the writers only emit bytes.
fn encode_canonical(op: &AccountOp) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    {
        let mut enc = Encoder::new(&mut buf);
        match op {
            AccountOp::AccountGenesis {
                ed25519_pubkey,
                x25519_pubkey,
                nonce16,
                created_at_ms,
                label,
            } => {
                enc.array(5).expect(INFALLIBLE);
                enc.bytes(ed25519_pubkey).expect(INFALLIBLE);
                enc.bytes(x25519_pubkey).expect(INFALLIBLE);
                enc.bytes(nonce16).expect(INFALLIBLE);
                enc.u64(*created_at_ms).expect(INFALLIBLE);
                encode_opt_str(&mut enc, label.as_deref());
            },
            AccountOp::DeviceAdd {
                device_fingerprint,
                ed25519_pubkey,
                x25519_pubkey,
                role,
                label,
            } => {
                enc.array(5).expect(INFALLIBLE);
                enc.bytes(&device_fingerprint.to_bytes()).expect(INFALLIBLE);
                enc.bytes(ed25519_pubkey).expect(INFALLIBLE);
                enc.bytes(x25519_pubkey).expect(INFALLIBLE);
                enc.u8(role.as_u8()).expect(INFALLIBLE);
                encode_opt_str(&mut enc, label.as_deref());
            },
            AccountOp::DeviceRemove {
                device_fingerprint,
                control_cut,
                secrets_cut,
                content_cuts,
                reason,
            } => {
                enc.array(5).expect(INFALLIBLE);
                enc.bytes(&device_fingerprint.to_bytes()).expect(INFALLIBLE);
                control_cut.encode_into(&mut enc);
                secrets_cut.encode_into(&mut enc);
                write_content_cuts(&mut enc, content_cuts);
                enc.str(reason).expect(INFALLIBLE);
            },
            AccountOp::OwnerPromote { device_fingerprint } => {
                enc.array(1).expect(INFALLIBLE);
                enc.bytes(&device_fingerprint.to_bytes()).expect(INFALLIBLE);
            },
            AccountOp::OwnerDemote {
                device_fingerprint,
                owner_id,
                control_cut,
                secrets_cut,
                reason,
            } => {
                enc.array(5).expect(INFALLIBLE);
                enc.bytes(&device_fingerprint.to_bytes()).expect(INFALLIBLE);
                enc.bytes(owner_id).expect(INFALLIBLE);
                control_cut.encode_into(&mut enc);
                secrets_cut.encode_into(&mut enc);
                enc.str(reason).expect(INFALLIBLE);
            },
            AccountOp::CutExtend {
                chain_kind,
                stream_id,
                incarnation_id,
                subject_account_id,
                device_fingerprint,
                new_seq,
                new_entry_hash,
            } => {
                enc.array(7).expect(INFALLIBLE);
                enc.u8(chain_kind.as_u8()).expect(INFALLIBLE);
                encode_opt_b32(&mut enc, stream_id.map(StreamId::to_bytes));
                encode_opt_b32(&mut enc, *incarnation_id);
                enc.bytes(&subject_account_id.to_bytes()).expect(INFALLIBLE);
                enc.bytes(&device_fingerprint.to_bytes()).expect(INFALLIBLE);
                enc.u64(*new_seq).expect(INFALLIBLE);
                enc.bytes(new_entry_hash).expect(INFALLIBLE);
            },
            AccountOp::StreamOwn { stream_id, stream_spec_bytes } => {
                enc.array(2).expect(INFALLIBLE);
                enc.bytes(&stream_id.to_bytes()).expect(INFALLIBLE);
                enc.bytes(stream_spec_bytes).expect(INFALLIBLE);
            },
            AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role } => {
                enc.array(3).expect(INFALLIBLE);
                enc.bytes(&stream_id.to_bytes()).expect(INFALLIBLE);
                enc.bytes(&grantee_account_id.to_bytes()).expect(INFALLIBLE);
                enc.u8(grant_role.as_u8()).expect(INFALLIBLE);
            },
            AccountOp::StreamRevoke {
                stream_id,
                grantee_account_id,
                grant_id,
                device_cuts,
                reason,
            } => {
                enc.array(5).expect(INFALLIBLE);
                enc.bytes(&stream_id.to_bytes()).expect(INFALLIBLE);
                enc.bytes(&grantee_account_id.to_bytes()).expect(INFALLIBLE);
                enc.bytes(grant_id).expect(INFALLIBLE);
                write_device_cuts(&mut enc, device_cuts);
                enc.str(reason).expect(INFALLIBLE);
            },
            AccountOp::AccountReRoot { successor_account_id, note } => {
                enc.array(2).expect(INFALLIBLE);
                enc.bytes(&successor_account_id.to_bytes()).expect(INFALLIBLE);
                encode_opt_str(&mut enc, note.as_deref());
            },
        }
    }
    buf
}

/// Decode an op payload against its header `entry_type`. A known type is fully validated
/// (structure, canonicity via re-encode, closed tokens, bounds); an unknown type is retained
/// opaque.
pub(super) fn decode(entry_type: u32, bytes: &[u8]) -> Result<DecodedAccountOp, CborError> {
    let op = match entry_type {
        entry_type::ACCOUNT_GENESIS => decode_account_genesis(bytes)?,
        entry_type::DEVICE_ADD => decode_device_add(bytes)?,
        entry_type::DEVICE_REMOVE => decode_device_remove(bytes)?,
        entry_type::OWNER_PROMOTE => decode_owner_promote(bytes)?,
        entry_type::OWNER_DEMOTE => decode_owner_demote(bytes)?,
        entry_type::CUT_EXTEND => decode_cut_extend(bytes)?,
        entry_type::STREAM_OWN => decode_stream_own(bytes)?,
        entry_type::STREAM_GRANT => decode_stream_grant(bytes)?,
        entry_type::STREAM_REVOKE => decode_stream_revoke(bytes)?,
        entry_type::ACCOUNT_REROOT => decode_account_reroot(bytes)?,
        other => {
            // A forward-version op is RETAINED opaque (we can't re-encode it), but it must STILL be
            // exactly one canonical CBOR item — otherwise a future binary that learns this
            // entry_type would run the `encode(decode) == bytes` check below and REJECT an entry an
            // older peer accepted + chained, splitting consensus on a signed log. The envelope
            // validates the payload only as an opaque bstr (it never recurses into it), so this is
            // the ONLY place the payload interior is checked. (Mirrors `super::super::op::decode`.)
            cbor::require_canonical_cbor(bytes)?;
            // Every account op is a fixed-length CBOR ARRAY (every known decoder starts with an
            // array header). A future binary that learns this entry_type would reject a
            // non-array payload on shape, so require the array here too — else an old
            // peer accepts bytes a newer peer cannot fold, splitting consensus on a
            // signed log.
            cbor::expect_definite_len(&mut Decoder::new(bytes))?;
            return Ok(DecodedAccountOp::Unknown { entry_type: other, bytes: bytes.to_vec() });
        },
    };
    // Canonicity guarantee for a KNOWN op: the decoded value must re-encode to the exact wire (this
    // rejects non-minimal ints, unsorted cut arrays, trailing bytes, etc. in one check). A decoded
    // op already satisfies every invariant `canonicalize` checks, so the re-encode cannot fail.
    if encode(&op)? != bytes {
        return Err(CborError::message("op payload is not canonical (re-encode differs)"));
    }
    Ok(DecodedAccountOp::Known(op))
}

fn decode_account_genesis(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 5)?;
    let ed25519_pubkey = cbor::fixed_bytes::<32>(d.bytes()?, "ed25519_pubkey")?;
    let x25519_pubkey = cbor::fixed_bytes::<32>(d.bytes()?, "x25519_pubkey")?;
    validate_keys(&ed25519_pubkey, &x25519_pubkey)?;
    let nonce16 = cbor::fixed_bytes::<16>(d.bytes()?, "nonce16")?;
    let created_at_ms = d.u64()?;
    let label = decode_opt_str(&mut d)?;
    Ok(AccountOp::AccountGenesis { ed25519_pubkey, x25519_pubkey, nonce16, created_at_ms, label })
}

fn decode_device_add(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 5)?;
    let device_fingerprint =
        DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device_fingerprint")?);
    let ed25519_pubkey = cbor::fixed_bytes::<32>(d.bytes()?, "ed25519_pubkey")?;
    let x25519_pubkey = cbor::fixed_bytes::<32>(d.bytes()?, "x25519_pubkey")?;
    validate_keys(&ed25519_pubkey, &x25519_pubkey)?;
    // Self-contained consistency: the fingerprint must be sha256 of the ed25519 key (both are in
    // the payload). The roster/authority checks are the fold's.
    if device_fingerprint.to_bytes() != cbor::sha256(&ed25519_pubkey) {
        return Err(CborError::message("device_fingerprint is not sha256(ed25519_pubkey)"));
    }
    let role = DeviceRole::from_u8(d.u8()?)?;
    let label = decode_opt_str(&mut d)?;
    Ok(AccountOp::DeviceAdd { device_fingerprint, ed25519_pubkey, x25519_pubkey, role, label })
}

fn decode_device_remove(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 5)?;
    let device_fingerprint =
        DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device_fingerprint")?);
    let control_cut = Cut::decode(&mut d)?;
    let secrets_cut = Cut::decode(&mut d)?;
    let content_cuts = decode_content_cuts(&mut d)?;
    let reason = d.str()?.to_string();
    Ok(AccountOp::DeviceRemove {
        device_fingerprint,
        control_cut,
        secrets_cut,
        content_cuts,
        reason,
    })
}

fn decode_owner_promote(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 1)?;
    let device_fingerprint =
        DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device_fingerprint")?);
    Ok(AccountOp::OwnerPromote { device_fingerprint })
}

fn decode_owner_demote(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 5)?;
    let device_fingerprint =
        DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device_fingerprint")?);
    let owner_id = cbor::fixed_bytes::<32>(d.bytes()?, "owner_id")?;
    let control_cut = Cut::decode(&mut d)?;
    let secrets_cut = Cut::decode(&mut d)?;
    let reason = d.str()?.to_string();
    Ok(AccountOp::OwnerDemote { device_fingerprint, owner_id, control_cut, secrets_cut, reason })
}

fn decode_cut_extend(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 7)?;
    let chain_kind = ChainKind::from_u8(d.u8()?)?;
    let stream_id = decode_opt_b32(&mut d, "stream_id")?.map(StreamId::from_bytes);
    let incarnation_id = decode_opt_b32(&mut d, "incarnation_id")?;
    // A content cut names a stream; a ctrl/secrets cut does not (§10).
    if (chain_kind == ChainKind::Content) != stream_id.is_some() {
        return Err(CborError::message("CutExtend stream_id is present iff chain_kind is content"));
    }
    let subject_account_id =
        AccountId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "subject_account_id")?);
    let device_fingerprint =
        DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device_fingerprint")?);
    let new_seq = d.u64()?;
    let new_entry_hash = cbor::fixed_bytes::<32>(d.bytes()?, "new_entry_hash")?;
    Ok(AccountOp::CutExtend {
        chain_kind,
        stream_id,
        incarnation_id,
        subject_account_id,
        device_fingerprint,
        new_seq,
        new_entry_hash,
    })
}

fn decode_stream_own(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 2)?;
    let stream_id = StreamId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "stream_id")?);
    let stream_spec_bytes = d.bytes()?.to_vec();
    Ok(AccountOp::StreamOwn { stream_id, stream_spec_bytes })
}

fn decode_stream_grant(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 3)?;
    let stream_id = StreamId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "stream_id")?);
    let grantee_account_id =
        AccountId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "grantee_account_id")?);
    let grant_role = GrantRole::from_u8(d.u8()?)?;
    Ok(AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role })
}

fn decode_stream_revoke(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 5)?;
    let stream_id = StreamId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "stream_id")?);
    let grantee_account_id =
        AccountId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "grantee_account_id")?);
    let grant_id = cbor::fixed_bytes::<32>(d.bytes()?, "grant_id")?;
    let device_cuts = decode_device_cuts(&mut d)?;
    let reason = d.str()?.to_string();
    Ok(AccountOp::StreamRevoke { stream_id, grantee_account_id, grant_id, device_cuts, reason })
}

fn decode_account_reroot(bytes: &[u8]) -> Result<AccountOp, CborError> {
    let mut d = decoder(bytes, 2)?;
    let successor_account_id =
        AccountId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "successor_account_id")?);
    let note = decode_opt_str(&mut d)?;
    Ok(AccountOp::AccountReRoot { successor_account_id, note })
}

/// Open a decoder over `bytes`, requiring a canonical `arity`-element array header. Trailing bytes
/// and non-canonicity are caught by the top-level `encode(decode) == bytes` re-encode check.
fn decoder(bytes: &[u8], arity: u64) -> Result<Decoder<'_>, CborError> {
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, arity)?;
    Ok(d)
}

fn validate_keys(ed25519: &[u8; 32], x25519: &[u8; 32]) -> Result<(), CborError> {
    DevicePublic::from_bytes(ed25519)
        .map_err(|_| CborError::message("ed25519_pubkey is not a valid curve point"))?;
    DeviceX25519Public::from_bytes(x25519)
        .map_err(|_| CborError::message("x25519_pubkey is a small-order / identity point"))?;
    Ok(())
}

/// Validate + canonicalize a content-cut array so it matches exactly what the sorted-unique decoder
/// accepts: sort by stream_id, drop EXACT duplicates, reject two CONFLICTING cuts for one stream (a
/// silent drop would lose a revocation, so this errors), and enforce the §18a bound.
fn canonical_content_cuts(cuts: &[ContentCut]) -> Result<Vec<ContentCut>, CborError> {
    let mut sorted = cuts.to_vec();
    sorted.sort_by_key(|cut| cut.stream_id.to_bytes());
    sorted.dedup();
    if sorted.windows(2).any(|pair| pair[0].stream_id == pair[1].stream_id) {
        return Err(CborError::message("content_cuts has conflicting entries for one stream_id"));
    }
    if sorted.len() > CONTENT_CUTS_MAX {
        return Err(CborError::message("content_cuts exceeds the §18a bound"));
    }
    Ok(sorted)
}

/// Emit an ALREADY-canonical content-cut array (see [`canonical_content_cuts`]).
fn write_content_cuts(enc: &mut Encoder<&mut Vec<u8>>, cuts: &[ContentCut]) {
    enc.array(cuts.len() as u64).expect(INFALLIBLE);
    for cut in cuts {
        enc.array(3).expect(INFALLIBLE);
        enc.bytes(&cut.stream_id.to_bytes()).expect(INFALLIBLE);
        enc.u64(cut.seq).expect(INFALLIBLE);
        enc.bytes(&cut.hash).expect(INFALLIBLE);
    }
}

fn decode_content_cuts(d: &mut Decoder<'_>) -> Result<Vec<ContentCut>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    if len > CONTENT_CUTS_MAX as u64 {
        return Err(CborError::message("content_cuts exceeds the §18a bound"));
    }
    let mut cuts = Vec::with_capacity(len as usize);
    let mut prev: Option<[u8; 32]> = None;
    for _ in 0..len {
        cbor::expect_array(d, 3)?;
        let stream_bytes = cbor::fixed_bytes::<32>(d.bytes()?, "content_cut stream_id")?;
        // Strictly ascending by stream_id ⇒ sorted AND unique in one check.
        if prev.is_some_and(|p| stream_bytes <= p) {
            return Err(CborError::message("content_cuts not sorted-unique by stream_id"));
        }
        prev = Some(stream_bytes);
        let seq = d.u64()?;
        let hash = cbor::fixed_bytes::<32>(d.bytes()?, "content_cut hash")?;
        cuts.push(ContentCut { stream_id: StreamId::from_bytes(stream_bytes), seq, hash });
    }
    Ok(cuts)
}

/// Validate + canonicalize a device-cut array (see [`canonical_content_cuts`]) — sort by
/// device_fingerprint, drop exact duplicates, reject conflicting entries, enforce the §18a bound.
fn canonical_device_cuts(cuts: &[DeviceCut]) -> Result<Vec<DeviceCut>, CborError> {
    let mut sorted = cuts.to_vec();
    sorted.sort_by_key(|cut| cut.device_fingerprint.to_bytes());
    sorted.dedup();
    if sorted.windows(2).any(|pair| pair[0].device_fingerprint == pair[1].device_fingerprint) {
        return Err(CborError::message("device_cuts has conflicting entries for one device"));
    }
    if sorted.len() > DEVICE_CUTS_MAX {
        return Err(CborError::message("device_cuts exceeds the §18a bound"));
    }
    Ok(sorted)
}

/// Emit an ALREADY-canonical device-cut array (see [`canonical_device_cuts`]).
fn write_device_cuts(enc: &mut Encoder<&mut Vec<u8>>, cuts: &[DeviceCut]) {
    enc.array(cuts.len() as u64).expect(INFALLIBLE);
    for cut in cuts {
        enc.array(3).expect(INFALLIBLE);
        enc.bytes(&cut.device_fingerprint.to_bytes()).expect(INFALLIBLE);
        enc.u64(cut.seq).expect(INFALLIBLE);
        enc.bytes(&cut.hash).expect(INFALLIBLE);
    }
}

fn decode_device_cuts(d: &mut Decoder<'_>) -> Result<Vec<DeviceCut>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    if len > DEVICE_CUTS_MAX as u64 {
        return Err(CborError::message("device_cuts exceeds the §18a bound"));
    }
    let mut cuts = Vec::with_capacity(len as usize);
    let mut prev: Option<[u8; 32]> = None;
    for _ in 0..len {
        cbor::expect_array(d, 3)?;
        let fp_bytes = cbor::fixed_bytes::<32>(d.bytes()?, "device_cut fingerprint")?;
        if prev.is_some_and(|p| fp_bytes <= p) {
            return Err(CborError::message("device_cuts not sorted-unique by device_fingerprint"));
        }
        prev = Some(fp_bytes);
        let seq = d.u64()?;
        let hash = cbor::fixed_bytes::<32>(d.bytes()?, "device_cut hash")?;
        cuts.push(DeviceCut {
            device_fingerprint: DeviceFingerprint::from_bytes(fp_bytes),
            seq,
            hash,
        });
    }
    Ok(cuts)
}

/// `Some(text)` → a CBOR text string; `None` → CBOR `null`.
fn encode_opt_str(enc: &mut Encoder<&mut Vec<u8>>, text: Option<&str>) {
    match text {
        Some(text) => {
            enc.str(text).expect(INFALLIBLE);
        },
        None => {
            enc.null().expect(INFALLIBLE);
        },
    }
}

fn decode_opt_str(d: &mut Decoder<'_>) -> Result<Option<String>, CborError> {
    if d.datatype()? == Type::Null {
        d.null()?;
        Ok(None)
    } else {
        Ok(Some(d.str()?.to_string()))
    }
}

/// `Some(b32)` → a 32-byte bstr; `None` → CBOR `null`.
fn encode_opt_b32(enc: &mut Encoder<&mut Vec<u8>>, value: Option<[u8; 32]>) {
    match value {
        Some(value) => {
            enc.bytes(&value).expect(INFALLIBLE);
        },
        None => {
            enc.null().expect(INFALLIBLE);
        },
    }
}

fn decode_opt_b32(d: &mut Decoder<'_>, field: &str) -> Result<Option<[u8; 32]>, CborError> {
    if d.datatype()? == Type::Null {
        d.null()?;
        Ok(None)
    } else {
        Ok(Some(cbor::fixed_bytes::<32>(d.bytes()?, field)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::device::{DeviceSecret, DeviceX25519Secret};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn ed25519() -> [u8; 32] {
        DeviceSecret::from_seed(&[7u8; 32]).public().to_bytes()
    }

    fn x25519() -> [u8; 32] {
        DeviceX25519Secret::from_seed(&[7u8; 32]).public().to_bytes()
    }

    fn founder_fp() -> DeviceFingerprint {
        DeviceSecret::from_seed(&[7u8; 32]).public().fingerprint()
    }

    fn account(byte: u8) -> AccountId {
        AccountId::from_bytes([byte; 32])
    }

    fn stream(byte: u8) -> StreamId {
        StreamId::from_bytes([byte; 32])
    }

    /// One representative op per variant, deterministic and distinctive.
    fn sample(entry_type: u32) -> AccountOp {
        match entry_type {
            entry_type::ACCOUNT_GENESIS => AccountOp::AccountGenesis {
                ed25519_pubkey: ed25519(),
                x25519_pubkey: x25519(),
                nonce16: [0xcc; 16],
                created_at_ms: 1_700_000_000_000,
                label: None,
            },
            entry_type::DEVICE_ADD => AccountOp::DeviceAdd {
                device_fingerprint: founder_fp(),
                ed25519_pubkey: ed25519(),
                x25519_pubkey: x25519(),
                role: DeviceRole::Owner,
                label: Some("laptop".to_string()),
            },
            entry_type::DEVICE_REMOVE => AccountOp::DeviceRemove {
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                control_cut: Cut::At { seq: 3, hash: [0x11; 32] },
                secrets_cut: Cut::Empty,
                content_cuts: vec![ContentCut {
                    stream_id: stream(0x33),
                    seq: 7,
                    hash: [0x44; 32],
                }],
                reason: "left".to_string(),
            },
            entry_type::OWNER_PROMOTE => AccountOp::OwnerPromote {
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
            },
            entry_type::OWNER_DEMOTE => AccountOp::OwnerDemote {
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                owner_id: [0x55; 32],
                control_cut: Cut::At { seq: 2, hash: [0x66; 32] },
                secrets_cut: Cut::Empty,
                reason: "demoted".to_string(),
            },
            entry_type::CUT_EXTEND => AccountOp::CutExtend {
                chain_kind: ChainKind::Ctrl,
                stream_id: None,
                incarnation_id: Some([0x77; 32]),
                subject_account_id: account(0xaa),
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                new_seq: 9,
                new_entry_hash: [0x88; 32],
            },
            entry_type::STREAM_OWN => AccountOp::StreamOwn {
                stream_id: stream(0x33),
                stream_spec_bytes: vec![0x85, 0x01, 0x02],
            },
            entry_type::STREAM_GRANT => AccountOp::StreamGrant {
                stream_id: stream(0x33),
                grantee_account_id: account(0x99),
                grant_role: GrantRole::Writer,
            },
            entry_type::STREAM_REVOKE => AccountOp::StreamRevoke {
                stream_id: stream(0x33),
                grantee_account_id: account(0x99),
                grant_id: [0xaa; 32],
                device_cuts: vec![DeviceCut {
                    device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                    seq: 41,
                    hash: [0xcc; 32],
                }],
                reason: "revoked".to_string(),
            },
            entry_type::ACCOUNT_REROOT => AccountOp::AccountReRoot {
                successor_account_id: account(0xdd),
                note: Some("compromised".to_string()),
            },
            other => panic!("no sample for entry_type {other}"),
        }
    }

    fn round_trip(op: &AccountOp) {
        let bytes = encode(op).unwrap();
        assert_eq!(decode(entry_type_of(op), &bytes).unwrap(), DecodedAccountOp::Known(op.clone()));
    }

    #[test]
    fn every_op_round_trips_and_re_encodes_canonically() {
        for et in 0..=9u32 {
            round_trip(&sample(et));
        }
    }

    #[test]
    fn golden_account_genesis() {
        assert_eq!(hex(&encode(&sample(entry_type::ACCOUNT_GENESIS)).unwrap()), "855820ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c582013be4feaeaf204c7fd3358fc9c00721881d174278128227ec674f37f7fe97b6d50cccccccccccccccccccccccccccccccc1b0000018bcfe56800f6");
    }

    #[test]
    fn golden_device_add() {
        assert_eq!(hex(&encode(&sample(entry_type::DEVICE_ADD)).unwrap()), "855820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638895820ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c582013be4feaeaf204c7fd3358fc9c00721881d174278128227ec674f37f7fe97b6d02666c6170746f70");
    }

    #[test]
    fn golden_device_remove() {
        assert_eq!(hex(&encode(&sample(entry_type::DEVICE_REMOVE)).unwrap()), "855820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb820358201111111111111111111111111111111111111111111111111111111111111111f68183582033333333333333333333333333333333333333333333333333333333333333330758204444444444444444444444444444444444444444444444444444444444444444646c656674");
    }

    #[test]
    fn golden_owner_promote() {
        assert_eq!(
            hex(&encode(&sample(entry_type::OWNER_PROMOTE)).unwrap()),
            "815820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn golden_owner_demote() {
        assert_eq!(hex(&encode(&sample(entry_type::OWNER_DEMOTE)).unwrap()), "855820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb58205555555555555555555555555555555555555555555555555555555555555555820258206666666666666666666666666666666666666666666666666666666666666666f66764656d6f746564");
    }

    #[test]
    fn golden_cut_extend() {
        assert_eq!(hex(&encode(&sample(entry_type::CUT_EXTEND)).unwrap()), "8700f6582077777777777777777777777777777777777777777777777777777777777777775820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0958208888888888888888888888888888888888888888888888888888888888888888");
    }

    #[test]
    fn golden_stream_own() {
        assert_eq!(
            hex(&encode(&sample(entry_type::STREAM_OWN)).unwrap()),
            "825820333333333333333333333333333333333333333333333333333333333333333343850102"
        );
    }

    #[test]
    fn golden_stream_grant() {
        assert_eq!(hex(&encode(&sample(entry_type::STREAM_GRANT)).unwrap()), "83582033333333333333333333333333333333333333333333333333333333333333335820999999999999999999999999999999999999999999999999999999999999999902");
    }

    #[test]
    fn golden_stream_revoke() {
        assert_eq!(hex(&encode(&sample(entry_type::STREAM_REVOKE)).unwrap()), "8558203333333333333333333333333333333333333333333333333333333333333333582099999999999999999999999999999999999999999999999999999999999999995820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa81835820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb18295820cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc677265766f6b6564");
    }

    #[test]
    fn golden_account_reroot() {
        assert_eq!(hex(&encode(&sample(entry_type::ACCOUNT_REROOT)).unwrap()), "825820dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd6b636f6d70726f6d69736564");
    }

    #[test]
    fn unknown_entry_type_is_retained_not_rejected() {
        let bytes = vec![0x81, 0x00];
        let decoded = decode(999, &bytes).unwrap();
        assert_eq!(decoded, DecodedAccountOp::Unknown { entry_type: 999, bytes });
    }

    #[test]
    fn unknown_entry_type_with_a_non_canonical_payload_is_rejected() {
        // A forward op is retained opaque, but its payload must STILL be canonical — else a future
        // binary that learns the type would reject (via encode(decode)==bytes) an entry an older
        // peer accepted + chained, splitting consensus on a signed log.
        let non_minimal_uint = vec![0x81, 0x18, 0x05]; // [uint 5] with a non-minimal 1-byte header
        assert!(decode(999, &non_minimal_uint).is_err(), "non-canonical unknown payload rejected");
        let trailing = vec![0x81, 0x00, 0xff]; // a valid item + a trailing byte
        assert!(decode(999, &trailing).is_err(), "trailing bytes after an unknown op rejected");
        // A canonical NON-array payload (a bare uint) is rejected: every account op is an array, so
        // a future binary that learns this entry_type would reject it on shape.
        assert!(decode(999, &[0x00]).is_err(), "a non-array unknown payload is rejected");
        assert!(decode(999, &[0xa0]).is_err(), "an empty map unknown payload is rejected");
    }

    #[test]
    fn entry_type_tag_set_is_frozen() {
        // The tags live in the envelope header (part 7), so the op goldens don't pin them — pin
        // them here. A renumber is a wire bump.
        assert_eq!(entry_type::ACCOUNT_GENESIS, 0);
        assert_eq!(entry_type::DEVICE_ADD, 1);
        assert_eq!(entry_type::DEVICE_REMOVE, 2);
        assert_eq!(entry_type::OWNER_PROMOTE, 3);
        assert_eq!(entry_type::OWNER_DEMOTE, 4);
        assert_eq!(entry_type::CUT_EXTEND, 5);
        assert_eq!(entry_type::STREAM_OWN, 6);
        assert_eq!(entry_type::STREAM_GRANT, 7);
        assert_eq!(entry_type::STREAM_REVOKE, 8);
        assert_eq!(entry_type::ACCOUNT_REROOT, 9);
    }

    #[test]
    fn closed_tokens_reject_unknown_values() {
        // role 3, grant_role 9, chain_kind 5 are all rejected.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(1).unwrap();
            enc.bytes(&[0xbb; 32]).unwrap();
        }
        // OwnerPromote decodes fine (control) — sanity that the fixture wire is valid shape.
        assert!(decode(entry_type::OWNER_PROMOTE, &buf).is_ok());

        let mut bad_role = Vec::new();
        {
            let mut enc = Encoder::new(&mut bad_role);
            enc.array(5).unwrap();
            enc.bytes(&founder_fp().to_bytes()).unwrap();
            enc.bytes(&ed25519()).unwrap();
            enc.bytes(&x25519()).unwrap();
            enc.u8(3).unwrap();
            enc.null().unwrap();
        }
        assert!(decode(entry_type::DEVICE_ADD, &bad_role).is_err(), "role 3 is rejected");
    }

    #[test]
    fn device_add_rejects_a_fingerprint_that_is_not_sha256_of_the_key() {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(5).unwrap();
            enc.bytes(&[0x00; 32]).unwrap(); // wrong fingerprint
            enc.bytes(&ed25519()).unwrap();
            enc.bytes(&x25519()).unwrap();
            enc.u8(2).unwrap();
            enc.null().unwrap();
        }
        assert!(
            decode(entry_type::DEVICE_ADD, &buf).is_err(),
            "fingerprint must be sha256(ed25519)"
        );
    }

    #[test]
    fn cut_extend_stream_id_kind_coupling_is_enforced() {
        // Content kind (2) WITHOUT a stream_id is rejected.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(7).unwrap();
            enc.u8(2).unwrap(); // content
            enc.null().unwrap(); // stream_id absent — illegal for content
            enc.null().unwrap();
            enc.bytes(&[0xaa; 32]).unwrap();
            enc.bytes(&[0xbb; 32]).unwrap();
            enc.u64(1).unwrap();
            enc.bytes(&[0x88; 32]).unwrap();
        }
        assert!(decode(entry_type::CUT_EXTEND, &buf).is_err(), "content cut needs a stream_id");
    }

    #[test]
    fn unsorted_device_cuts_are_rejected() {
        // Two device_cuts out of ascending order — a re-encode would reorder, so decode rejects.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(5).unwrap();
            enc.bytes(&[0x33; 32]).unwrap();
            enc.bytes(&[0x99; 32]).unwrap();
            enc.bytes(&[0xaa; 32]).unwrap();
            enc.array(2).unwrap();
            enc.array(3).unwrap();
            enc.bytes(&[0xff; 32]).unwrap(); // higher fp first — out of order
            enc.u64(1).unwrap();
            enc.bytes(&[0x00; 32]).unwrap();
            enc.array(3).unwrap();
            enc.bytes(&[0x11; 32]).unwrap();
            enc.u64(2).unwrap();
            enc.bytes(&[0x00; 32]).unwrap();
            enc.str("r").unwrap();
        }
        assert!(decode(entry_type::STREAM_REVOKE, &buf).is_err(), "unsorted device_cuts rejected");
    }

    #[test]
    fn exact_duplicate_cuts_are_collapsed_on_encode() {
        // A caller supplying the same cut twice must still produce a payload the sorted-unique
        // decoder accepts — encode collapses exact duplicates rather than emitting a dup key that
        // decode (and peers) would reject. (encode_content_cuts shares the codepath.)
        let cut = DeviceCut {
            device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
            seq: 41,
            hash: [0xcc; 32],
        };
        let doubled = AccountOp::StreamRevoke {
            stream_id: stream(0x33),
            grantee_account_id: account(0x99),
            grant_id: [0xaa; 32],
            device_cuts: vec![cut.clone(), cut.clone()],
            reason: "revoked".to_string(),
        };
        let single = AccountOp::StreamRevoke {
            stream_id: stream(0x33),
            grantee_account_id: account(0x99),
            grant_id: [0xaa; 32],
            device_cuts: vec![cut],
            reason: "revoked".to_string(),
        };
        // The duplicate encodes to the same bytes as the single, and round-trips to the single.
        assert_eq!(
            encode(&doubled).unwrap(),
            encode(&single).unwrap(),
            "exact-duplicate cuts collapse on encode"
        );
        assert_eq!(
            decode(entry_type::STREAM_REVOKE, &encode(&doubled).unwrap()).unwrap(),
            DecodedAccountOp::Known(single),
        );
    }

    fn revoke_with(device_cuts: Vec<DeviceCut>) -> AccountOp {
        AccountOp::StreamRevoke {
            stream_id: stream(0x33),
            grantee_account_id: account(0x99),
            grant_id: [0xaa; 32],
            device_cuts,
            reason: "r".to_string(),
        }
    }

    #[test]
    fn encode_rejects_conflicting_and_over_bound_cuts() {
        // Two DIFFERENT cuts for one device ⇒ encode errors (a silent drop would lose a
        // revocation).
        let conflicting = revoke_with(vec![
            DeviceCut {
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                seq: 1,
                hash: [0x01; 32],
            },
            DeviceCut {
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                seq: 2,
                hash: [0x02; 32],
            },
        ]);
        assert!(encode(&conflicting).is_err(), "conflicting device_cuts rejected at encode");

        // DEVICE_CUTS_MAX + 1 DISTINCT cuts ⇒ encode errors (decode would reject them too).
        let many: Vec<DeviceCut> = (0..=DEVICE_CUTS_MAX as u32)
            .map(|i| {
                let mut fp = [0u8; 32];
                fp[..4].copy_from_slice(&i.to_be_bytes());
                DeviceCut {
                    device_fingerprint: DeviceFingerprint::from_bytes(fp),
                    seq: 1,
                    hash: [0u8; 32],
                }
            })
            .collect();
        assert!(encode(&revoke_with(many)).is_err(), "over-bound device_cuts rejected at encode");
    }

    #[test]
    fn encode_derives_the_device_add_fingerprint_and_validates_keys() {
        // A DeviceAdd carrying a WRONG fingerprint encodes to the same bytes as the derived-fp op,
        // and round-trips to the derived one — a stale/placeholder fp can never be signed.
        let wrong = AccountOp::DeviceAdd {
            device_fingerprint: DeviceFingerprint::from_bytes([0x00; 32]),
            ed25519_pubkey: ed25519(),
            x25519_pubkey: x25519(),
            role: DeviceRole::Owner,
            label: Some("laptop".to_string()),
        };
        let derived = sample(entry_type::DEVICE_ADD); // fingerprint == sha256(ed25519())
        assert_eq!(
            encode(&wrong).unwrap(),
            encode(&derived).unwrap(),
            "fingerprint is derived, not trusted"
        );
        assert_eq!(
            decode(entry_type::DEVICE_ADD, &encode(&wrong).unwrap()).unwrap(),
            DecodedAccountOp::Known(derived),
        );
        // An identity X25519 key ⇒ encode errors (validate_keys, mirroring decode).
        let bad_key = AccountOp::DeviceAdd {
            device_fingerprint: founder_fp(),
            ed25519_pubkey: ed25519(),
            x25519_pubkey: [0u8; 32],
            role: DeviceRole::Owner,
            label: None,
        };
        assert!(encode(&bad_key).is_err(), "identity x25519 key rejected at encode");
    }

    #[test]
    fn encode_rejects_a_cut_extend_coupling_violation() {
        // Content kind without a stream_id ⇒ encode errors, matching the decode-side rejection.
        let bad = AccountOp::CutExtend {
            chain_kind: ChainKind::Content,
            stream_id: None,
            incarnation_id: None,
            subject_account_id: account(0xaa),
            device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
            new_seq: 1,
            new_entry_hash: [0x88; 32],
        };
        assert!(encode(&bad).is_err(), "content CutExtend without stream_id rejected at encode");
    }
}
