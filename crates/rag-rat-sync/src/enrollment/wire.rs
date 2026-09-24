//! The enrollment and writer-grant frames: canonical CBOR codecs for requests, responses and
//! receipts, and the frame caps they are measured against.

use minicbor::{Decoder, Encoder};
use rag_rat_oplog::{AccountId, ENROLLMENT_HELD_ENTRY_HASHES_MAX, EnrollmentBudget};

use super::error::InviteError;

const REQUEST_DOMAIN: &str = "rag-rat/enrollment-request/1";

const RECEIPT_DOMAIN: &str = "rag-rat/enrollment-receipt/1";

const RESPONSE_DOMAIN: &str = "rag-rat/enrollment-response/1";

pub(super) const MAX_ENROLL_REQUEST_FRAME: u32 = 144 * 1024;

pub(super) const MAX_ENROLL_RESPONSE_FRAME: u32 = crate::codec::MAX_FRAME_BYTES;

pub(super) const MAX_ENROLL_BOOTSTRAP_ENTRIES: u64 = 4_096;

/// An invite's one-time nonce: a bearer secret. Whoever holds it can redeem the invite — enroll a
/// device into the account, or be granted a stream. Its `Debug` is redacted, so the ticket and the
/// requests that carry it can keep deriving `Debug` and be formatted in an error path, a `dbg!` or
/// a tracing field without writing the secret into a log. Reading it is always explicit.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InviteNonce([u8; 32]);

impl InviteNonce {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<[u8; 32]> for InviteNonce {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for InviteNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InviteNonce(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub nonce: InviteNonce,
    /// Account the joiner intends to adopt. The acceptor compares this with the nonce's persisted
    /// account before authoring or consuming the one-time invite.
    pub expected_account: AccountId,
    pub ed25519_pubkey: [u8; 32],
    pub x25519_pubkey: [u8; 32],
    pub transport_node_id: [u8; 32],
    /// The joiner store's remaining admission budget, so the owner can measure its exact receipt
    /// against it BEFORE consuming the one-time nonce (#945). [`connect_and_enroll`] always
    /// recomputes this from the local store; the value a caller sets here is overwritten.
    pub budget: EnrollmentBudget,
    /// Candidate `entry_hash` values the joiner already holds. Enrollment never transfers
    /// unauthenticated parked rows; those remain normal-sync work.
    pub held_entry_hashes: Vec<[u8; 32]>,
}

impl EnrollmentRequest {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(8).expect("owned Vec");
        enc.str(REQUEST_DOMAIN).expect("owned Vec");
        enc.bytes(self.nonce.as_slice()).expect("owned Vec");
        enc.bytes(&self.expected_account.to_bytes()).expect("owned Vec");
        enc.bytes(&self.ed25519_pubkey).expect("owned Vec");
        enc.bytes(&self.x25519_pubkey).expect("owned Vec");
        enc.bytes(&self.transport_node_id).expect("owned Vec");
        enc.array(4).expect("owned Vec");
        enc.u64(self.budget.account_entries_remaining).expect("owned Vec");
        enc.u64(self.budget.account_bytes_remaining).expect("owned Vec");
        enc.u64(self.budget.global_entries_remaining).expect("owned Vec");
        enc.u64(self.budget.global_bytes_remaining).expect("owned Vec");
        enc.array(self.held_entry_hashes.len() as u64).expect("owned Vec");
        for hash in &self.held_entry_hashes {
            enc.bytes(hash).expect("owned Vec");
        }
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 8, "request")?;
        exact_str(&mut dec, REQUEST_DOMAIN, "request domain")?;
        let budget_u64 =
            |dec: &mut Decoder<'_>| -> Result<u64, InviteError> { dec.u64().map_err(decode) };
        let request = Self {
            nonce: InviteNonce::from_bytes(fixed32(dec.bytes().map_err(decode)?, "nonce")?),
            expected_account: AccountId::from_bytes(fixed32(
                dec.bytes().map_err(decode)?,
                "expected account",
            )?),
            ed25519_pubkey: fixed32(dec.bytes().map_err(decode)?, "ed25519 key")?,
            x25519_pubkey: fixed32(dec.bytes().map_err(decode)?, "x25519 key")?,
            transport_node_id: fixed32(dec.bytes().map_err(decode)?, "transport node id")?,
            budget: {
                exact_array(&mut dec, 4, "request budget")?;
                EnrollmentBudget {
                    account_entries_remaining: budget_u64(&mut dec)?,
                    account_bytes_remaining: budget_u64(&mut dec)?,
                    global_entries_remaining: budget_u64(&mut dec)?,
                    global_bytes_remaining: budget_u64(&mut dec)?,
                }
            },
            held_entry_hashes: {
                let count = dec.array().map_err(decode)?.ok_or_else(|| {
                    InviteError::Malformed("held entry hashes must be a definite array".into())
                })?;
                if count > ENROLLMENT_HELD_ENTRY_HASHES_MAX as u64 {
                    return Err(InviteError::Malformed(format!(
                        "held entry hashes {count} over {ENROLLMENT_HELD_ENTRY_HASHES_MAX}"
                    )));
                }
                let mut hashes = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    hashes.push(fixed32(dec.bytes().map_err(decode)?, "held entry hash")?);
                }
                if hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return Err(InviteError::Malformed(
                        "held entry hashes must be strictly sorted and unique".into(),
                    ));
                }
                hashes
            },
        };
        ensure_consumed(&dec, bytes)?;
        if request.encode() != bytes {
            return Err(InviteError::Malformed("request is not canonical CBOR".into()));
        }
        Ok(request)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentReceipt {
    pub device_add_hash: [u8; 32],
    pub device_add_signed: Vec<u8>,
    /// Exact signed account-log bootstrap, ordered as normal account sync would offer it. A fresh
    /// joiner ingests these entries before its first Closed session so it can verify the inviter's
    /// account binding and fold its own DeviceAdd effective.
    pub account_entries: Vec<Vec<u8>>,
}

pub(super) enum EnrollmentResponse {
    Enrolled(EnrollmentReceipt),
    Refused(RefusalCode),
}

/// `EnumIter` so the round-trip test ranges over the enum itself. A hand-listed set would let a
/// new code be added to `as_str` and forgotten in `from_str` — a token the owner can emit and no
/// joiner can read — with nothing failing to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub(super) enum RefusalCode {
    Expired,
    Used,
    Unknown,
    WrongNode,
    AccountMismatch,
    Revoked,
    JoinerCapacity,
    HeldStateConflict,
    CheckpointPinMoved,
    DeviceRemoved,
}

impl EnrollmentReceipt {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(4).expect("owned Vec");
        enc.str(RECEIPT_DOMAIN).expect("owned Vec");
        enc.bytes(&self.device_add_hash).expect("owned Vec");
        enc.bytes(&self.device_add_signed).expect("owned Vec");
        enc.array(self.account_entries.len() as u64).expect("owned Vec");
        for entry in &self.account_entries {
            enc.bytes(entry).expect("owned Vec");
        }
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 4, "receipt")?;
        exact_str(&mut dec, RECEIPT_DOMAIN, "receipt domain")?;
        let device_add_hash = fixed32(dec.bytes().map_err(decode)?, "DeviceAdd hash")?;
        let device_add_signed = dec.bytes().map_err(decode)?.to_vec();
        let entry_count = dec.array().map_err(decode)?.ok_or_else(|| {
            InviteError::Malformed("bootstrap entries must be a definite array".into())
        })?;
        if entry_count > MAX_ENROLL_BOOTSTRAP_ENTRIES {
            return Err(InviteError::Malformed(format!(
                "bootstrap has {entry_count} entries, over {MAX_ENROLL_BOOTSTRAP_ENTRIES}"
            )));
        }
        let mut account_entries = Vec::with_capacity(entry_count as usize);
        for _ in 0..entry_count {
            account_entries.push(dec.bytes().map_err(decode)?.to_vec());
        }
        let receipt = Self { device_add_hash, device_add_signed, account_entries };
        ensure_consumed(&dec, bytes)?;
        if receipt.encode() != bytes {
            return Err(InviteError::Malformed("receipt is not canonical CBOR".into()));
        }
        Ok(receipt)
    }
}

impl EnrollmentResponse {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(3).expect("owned Vec");
        enc.str(RESPONSE_DOMAIN).expect("owned Vec");
        match self {
            Self::Enrolled(receipt) => {
                enc.str("enrolled").expect("owned Vec");
                enc.bytes(&receipt.encode()).expect("owned Vec");
            },
            Self::Refused(code) => {
                enc.str("refused").expect("owned Vec");
                enc.str(code.as_str()).expect("owned Vec");
            },
        }
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 3, "response")?;
        exact_str(&mut dec, RESPONSE_DOMAIN, "response domain")?;
        let response = match dec.str().map_err(decode)? {
            "enrolled" => Self::Enrolled(EnrollmentReceipt::decode(dec.bytes().map_err(decode)?)?),
            "refused" => Self::Refused(RefusalCode::from_str(dec.str().map_err(decode)?)?),
            value =>
                return Err(InviteError::Malformed(format!("unknown enrollment response {value}"))),
        };
        ensure_consumed(&dec, bytes)?;
        if response.encode() != bytes {
            return Err(InviteError::Malformed("response is not canonical CBOR".into()));
        }
        Ok(response)
    }
}

impl RefusalCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Used => "used",
            Self::Unknown => "unknown",
            Self::WrongNode => "wrong_node",
            Self::AccountMismatch => "account_mismatch",
            Self::Revoked => "revoked",
            Self::JoinerCapacity => "joiner_capacity",
            Self::HeldStateConflict => "held_state_conflict",
            Self::CheckpointPinMoved => "checkpoint_pin_moved",
            Self::DeviceRemoved => "device_removed",
        }
    }

    fn from_str(value: &str) -> Result<Self, InviteError> {
        match value {
            "expired" => Ok(Self::Expired),
            "used" => Ok(Self::Used),
            "unknown" => Ok(Self::Unknown),
            "wrong_node" => Ok(Self::WrongNode),
            "account_mismatch" => Ok(Self::AccountMismatch),
            "revoked" => Ok(Self::Revoked),
            "joiner_capacity" => Ok(Self::JoinerCapacity),
            "held_state_conflict" => Ok(Self::HeldStateConflict),
            "checkpoint_pin_moved" => Ok(Self::CheckpointPinMoved),
            "device_removed" => Ok(Self::DeviceRemoved),
            _ => Err(InviteError::Malformed(format!("unknown enrollment refusal {value}"))),
        }
    }

    pub(super) fn into_error(self) -> InviteError {
        match self {
            Self::Expired => InviteError::Expired,
            Self::Used => InviteError::Used,
            Self::Unknown => InviteError::Unknown,
            Self::WrongNode => InviteError::WrongNode,
            Self::AccountMismatch => InviteError::AccountMismatch,
            Self::Revoked => InviteError::Revoked,
            Self::JoinerCapacity => InviteError::JoinerCapacity,
            Self::HeldStateConflict => InviteError::HeldStateConflict,
            Self::CheckpointPinMoved => InviteError::CheckpointPinMoved,
            Self::DeviceRemoved => InviteError::DeviceRemoved,
        }
    }
}

/// Wire domains for the WRITER invite exchange, carried over the enrollment ALPN: the acceptor
/// branches on the request's leading domain string, so one serve loop redeems both kinds and a
/// pre-writer binary refuses the new domain with a legible malformed-request error.
pub(super) const GRANT_REQUEST_DOMAIN: &str = "rag-rat/grant-request/1";

const GRANT_RESPONSE_DOMAIN: &str = "rag-rat/grant-response/1";

/// A contributor's writer-invite redemption: the nonce authorizes, and `contributor_account` is
/// what the owner's `StreamGrant` will name — the piece of the old paste flow that had to travel
/// owner-ward by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterGrantRequest {
    pub nonce: InviteNonce,
    /// The owner account the ticket names; compared against the nonce's persisted account.
    pub expected_account: AccountId,
    pub contributor_account: AccountId,
}

impl WriterGrantRequest {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(4).expect("owned Vec");
        enc.str(GRANT_REQUEST_DOMAIN).expect("owned Vec");
        enc.bytes(self.nonce.as_slice()).expect("owned Vec");
        enc.bytes(&self.expected_account.to_bytes()).expect("owned Vec");
        enc.bytes(&self.contributor_account.to_bytes()).expect("owned Vec");
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 4, "grant request")?;
        exact_str(&mut dec, GRANT_REQUEST_DOMAIN, "grant request domain")?;
        let request = Self {
            nonce: InviteNonce::from_bytes(fixed32(dec.bytes().map_err(decode)?, "nonce")?),
            expected_account: AccountId::from_bytes(fixed32(
                dec.bytes().map_err(decode)?,
                "expected account",
            )?),
            contributor_account: AccountId::from_bytes(fixed32(
                dec.bytes().map_err(decode)?,
                "contributor account",
            )?),
        };
        ensure_consumed(&dec, bytes)?;
        if request.encode() != bytes {
            return Err(InviteError::Malformed("grant request is not canonical CBOR".into()));
        }
        Ok(request)
    }
}

/// What a redeemed writer invite hands back: the authored grant and the stream it grants on, so
/// the contributor can verify the folded fact against the stream its own repo derives — a
/// mismatch means the invite was minted for a different repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterGrantReceipt {
    pub grant_id: [u8; 32],
    pub stream_id: [u8; 32],
}

pub(super) enum WriterGrantResponse {
    Granted(WriterGrantReceipt),
    Refused(RefusalCode),
}

impl WriterGrantResponse {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(3).expect("owned Vec");
        enc.str(GRANT_RESPONSE_DOMAIN).expect("owned Vec");
        match self {
            Self::Granted(receipt) => {
                enc.str("granted").expect("owned Vec");
                enc.array(2).expect("owned Vec");
                enc.bytes(&receipt.grant_id).expect("owned Vec");
                enc.bytes(&receipt.stream_id).expect("owned Vec");
            },
            Self::Refused(code) => {
                enc.str("refused").expect("owned Vec");
                enc.str(code.as_str()).expect("owned Vec");
            },
        }
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 3, "grant response")?;
        exact_str(&mut dec, GRANT_RESPONSE_DOMAIN, "grant response domain")?;
        let response = match dec.str().map_err(decode)? {
            "granted" => {
                exact_array(&mut dec, 2, "grant receipt")?;
                Self::Granted(WriterGrantReceipt {
                    grant_id: fixed32(dec.bytes().map_err(decode)?, "grant id")?,
                    stream_id: fixed32(dec.bytes().map_err(decode)?, "stream id")?,
                })
            },
            "refused" => Self::Refused(RefusalCode::from_str(dec.str().map_err(decode)?)?),
            other =>
                return Err(InviteError::Malformed(format!("unknown grant response `{other}`"))),
        };
        ensure_consumed(&dec, bytes)?;
        if response.encode() != bytes {
            return Err(InviteError::Malformed("grant response is not canonical CBOR".into()));
        }
        Ok(response)
    }
}

/// The leading domain string of a request blob, if it parses far enough to have one — the
/// acceptor's flow dispatch. Full validation stays with each request's own decoder.
pub(super) fn request_blob_domain(bytes: &[u8]) -> Option<&str> {
    let mut dec = Decoder::new(bytes);
    dec.array().ok()??;
    dec.str().ok()
}

pub(super) fn refusal_code(error: &InviteError) -> Option<RefusalCode> {
    match error {
        InviteError::Expired => Some(RefusalCode::Expired),
        InviteError::Used => Some(RefusalCode::Used),
        InviteError::Unknown => Some(RefusalCode::Unknown),
        InviteError::WrongNode => Some(RefusalCode::WrongNode),
        InviteError::AccountMismatch => Some(RefusalCode::AccountMismatch),
        InviteError::Revoked => Some(RefusalCode::Revoked),
        InviteError::JoinerCapacity => Some(RefusalCode::JoinerCapacity),
        InviteError::HeldStateConflict => Some(RefusalCode::HeldStateConflict),
        // The owner holds the pin state, so only the owner can see this; the joiner has to be told
        // or it cannot tell a terminal ticket from a retryable transport failure.
        InviteError::CheckpointPinMoved => Some(RefusalCode::CheckpointPinMoved),
        // Only the owner's log records the removal; the joiner may never have learned of it.
        InviteError::DeviceRemoved => Some(RefusalCode::DeviceRemoved),
        // A transport failure never reached a redemption and has never produced a wire
        // refusal; it is named here rather than left to a wildcard so it cannot start to.
        // Version skew is decided reading a ticket string, before any connection exists, so it
        // has no wire refusal either — named rather than left to a wildcard so it cannot acquire
        // one by accident.
        InviteError::Malformed(_)
        | InviteError::TicketVersionSkew(_)
        | InviteError::Storage(_)
        | InviteError::Io(_)
        | InviteError::Transport(_) => None,
    }
}

pub(super) fn ensure_frame_len(
    len: usize,
    max_len: u32,
    frame_name: &str,
) -> Result<u32, InviteError> {
    let len = u32::try_from(len)
        .map_err(|_| InviteError::Malformed("enrollment frame length overflows u32".into()))?;
    if len > max_len {
        return Err(InviteError::Malformed(format!(
            "enrollment {frame_name} frame exceeds {max_len} bytes"
        )));
    }
    Ok(len)
}

pub(super) fn exact_array(
    dec: &mut Decoder<'_>,
    expected: u64,
    name: &str,
) -> Result<(), InviteError> {
    if dec.array().map_err(decode)? != Some(expected) {
        return Err(InviteError::Malformed(format!("{name} arity")));
    }
    Ok(())
}

pub(super) fn exact_str(
    dec: &mut Decoder<'_>,
    expected: &str,
    name: &str,
) -> Result<(), InviteError> {
    if dec.str().map_err(decode)? != expected {
        return Err(InviteError::Malformed(format!("{name} mismatch")));
    }
    Ok(())
}

pub(super) fn fixed32(bytes: &[u8], name: &str) -> Result<[u8; 32], InviteError> {
    bytes.try_into().map_err(|_| InviteError::Malformed(format!("{name} must be 32 bytes")))
}

pub(super) fn ensure_consumed(dec: &Decoder<'_>, bytes: &[u8]) -> Result<(), InviteError> {
    if dec.position() != bytes.len() {
        return Err(InviteError::Malformed("trailing bytes".into()));
    }
    Ok(())
}

pub(super) fn decode(error: minicbor::decode::Error) -> InviteError {
    InviteError::Malformed(error.to_string())
}
