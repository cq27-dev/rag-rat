//! One-time owner↔joiner enrollment protocol (#945).
//!
//! The ticket is routing data plus an opaque nonce; the granted role and label stay in the
//! inviter's durable row, so a joiner cannot escalate by editing the ticket. Redemption consumes
//! the nonce, authors the exact `DeviceAdd`, and catches up stream keys in one IMMEDIATE
//! transaction. Any failure rolls all three effects back.

use std::mem::MaybeUninit;
use std::str::FromStr;
use std::time::Duration;

use iroh::{EndpointId, RelayUrl};
use minicbor::{Decoder, Encoder};
use rag_rat_oplog::{
    AccountId, AuthoredDurability, AuthorityBoundary, AuthorityQuery, CatchUpReport,
    DeviceFingerprint, DeviceRole, ENROLLMENT_HELD_ENTRY_HASHES_MAX, EnrollingDevice,
    EnrollmentBudget, account_entries_for_enrollment, author_enrollment_device_add_in_tx,
    enroll_stream_keys_for_device_in_tx, enrollment_authoring_fits, load_local_device,
    owned_streams_for_account, owner_control_authority_in_snapshot, read_local_account,
    retry_enrollment_pre_verify, validate_device_add_label, verify_enrollment_device_add,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ENROLL_ALPN: &[u8] = b"rag-rat/enroll/1";
const TICKET_DOMAIN: &str = "rag-rat/enrollment-ticket/1";
const REQUEST_DOMAIN: &str = "rag-rat/enrollment-request/1";
const RECEIPT_DOMAIN: &str = "rag-rat/enrollment-receipt/1";
const RESPONSE_DOMAIN: &str = "rag-rat/enrollment-response/1";
const MAX_ENROLL_REQUEST_FRAME: u32 = 144 * 1024;
const MAX_ENROLL_RESPONSE_FRAME: u32 = crate::codec::MAX_FRAME_BYTES;
const MAX_ENROLL_BOOTSTRAP_ENTRIES: u64 = 4_096;
const MAX_RELAY_URL_BYTES: usize = 2048;
const RECEIPT_REPLAY_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
/// Per-chunk progress window for enrollment frame IO: every 64 KiB slice of a frame body must
/// arrive (or drain) within this window. Mirrors the session's per-frame idle reset
/// (`read_frame_before`), so a slow but progressing multi-megabyte receipt never times out while
/// a stalled peer dies within one window.
pub const ENROLL_PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);
/// Chunk size for progress-tracked frame IO: 64 KiB per window tolerates ~1 KiB/s links, and a
/// byte-at-a-time trickle can never complete a chunk, so the slow-loris floor is one window.
const ENROLL_PROGRESS_CHUNK_BYTES: usize = 64 * 1024;
/// The tighter window applied to frames any UNAUTHENTICATED peer can reach — the request and a
/// refusal response (a random nonce gets that far). Both are at most a couple of chunks, so a
/// bogus peer's hold on the serial accept loop stays in seconds, not minutes; the full
/// [`ENROLL_PROGRESS_TIMEOUT`] applies only once a valid nonce has authorized the receipt.
const ENROLL_UNAUTH_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
/// One byte the dialer sends the moment it has DECODED the response — the acceptor's delivery
/// signal, letting it close without a peer-controlled graceful-close wait (#945). Not a
/// length-prefixed blob: it is the terminal byte of the exchange, written after the response
/// frame and read raw by the acceptor.
pub(crate) const RESPONSE_ACK: u8 = 0x01;
/// Shared bound for sending or receiving the terminal response acknowledgement. Ack delivery is
/// best-effort after the response is decoded, so a stalled reverse stream cannot mask the result.
pub(crate) const RESPONSE_ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentTicket {
    pub account_id: AccountId,
    pub inviter_node_id: [u8; 32],
    pub relay_url: String,
    pub nonce: [u8; 32],
    pub expires_at_ms: i64,
}

impl EnrollmentTicket {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(6).expect("owned Vec");
        enc.str(TICKET_DOMAIN).expect("owned Vec");
        enc.bytes(&self.account_id.to_bytes()).expect("owned Vec");
        enc.bytes(&self.inviter_node_id).expect("owned Vec");
        enc.str(&self.relay_url).expect("owned Vec");
        enc.bytes(&self.nonce).expect("owned Vec");
        enc.i64(self.expires_at_ms).expect("owned Vec");
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 6, "ticket")?;
        exact_str(&mut dec, TICKET_DOMAIN, "ticket domain")?;
        let account_id = AccountId::from_bytes(fixed32(dec.bytes().map_err(decode)?, "account")?);
        let inviter_node_id = fixed32(dec.bytes().map_err(decode)?, "node id")?;
        let relay_url = dec.str().map_err(decode)?.to_owned();
        validate_enrollment_route(&inviter_node_id, &relay_url)?;
        let nonce = fixed32(dec.bytes().map_err(decode)?, "nonce")?;
        let expires_at_ms = dec.i64().map_err(decode)?;
        ensure_consumed(&dec, bytes)?;
        let ticket = Self { account_id, inviter_node_id, relay_url, nonce, expires_at_ms };
        if ticket.encode() != bytes {
            return Err(InviteError::Malformed("ticket is not canonical CBOR".into()));
        }
        Ok(ticket)
    }
}

fn validate_enrollment_route(
    inviter_node_id: &[u8; 32],
    relay_url: &str,
) -> Result<(), InviteError> {
    EndpointId::from_bytes(inviter_node_id)
        .map_err(|error| InviteError::Malformed(format!("invalid inviter node id: {error}")))?;
    if relay_url.len() > MAX_RELAY_URL_BYTES {
        return Err(InviteError::Malformed("relay URL exceeds 2048 bytes".into()));
    }
    RelayUrl::from_str(relay_url.trim())
        .map_err(|error| InviteError::Malformed(format!("invalid relay URL: {error}")))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub nonce: [u8; 32],
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
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(8).expect("owned Vec");
        enc.str(REQUEST_DOMAIN).expect("owned Vec");
        enc.bytes(&self.nonce).expect("owned Vec");
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

    fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 8, "request")?;
        exact_str(&mut dec, REQUEST_DOMAIN, "request domain")?;
        let budget_u64 =
            |dec: &mut Decoder<'_>| -> Result<u64, InviteError> { dec.u64().map_err(decode) };
        let request = Self {
            nonce: fixed32(dec.bytes().map_err(decode)?, "nonce")?,
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

pub struct InviteSpec<'a> {
    pub account_id: AccountId,
    pub inviter_node_id: [u8; 32],
    pub relay_url: String,
    pub role: DeviceRole,
    pub label: Option<&'a str>,
    /// Mint clock. Read ONCE after the writer transaction is acquired — a pre-lock timestamp can
    /// go stale behind SQLite's busy timeout and would mint an already-expired ticket.
    pub now_ms: &'a dyn Fn() -> i64,
    pub ttl: Duration,
}

struct StoredInvite {
    account_bytes: Vec<u8>,
    role: String,
    label: Option<String>,
    expires_at_ms: i64,
    used_at_ms: Option<i64>,
    used_transport_node: Option<Vec<u8>>,
    used_ed25519_pubkey: Option<Vec<u8>>,
    used_x25519_pubkey: Option<Vec<u8>>,
    receipt_hash: Option<Vec<u8>>,
    receipt_signed: Option<Vec<u8>>,
    receipt_entries: Option<Vec<u8>>,
    /// Legacy full-receipt copy (pre-V092). Never written anymore; retained so invites consumed
    /// before V092 keep replaying through their 24h window. The manifest form is preferred.
    receipt_bytes: Option<Vec<u8>>,
}

/// Owner-side result after a complete enrollment request. Refusals are sent over the wire before
/// being returned, so the caller may gracefully close the QUIC connection without hiding the
/// semantic error from the joiner.
pub enum EnrollmentAcceptorOutcome {
    Enrolled(EnrollmentReceipt, CatchUpReport),
    Refused(InviteError),
}

enum EnrollmentResponse {
    Enrolled(EnrollmentReceipt),
    Refused(RefusalCode),
}

#[derive(Clone, Copy)]
enum RefusalCode {
    Expired,
    Used,
    Unknown,
    WrongNode,
    AccountMismatch,
    Revoked,
    JoinerCapacity,
    HeldStateConflict,
}

impl EnrollmentReceipt {
    fn encode(&self) -> Vec<u8> {
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

    fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
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
    fn encode(&self) -> Vec<u8> {
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

    fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
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
            _ => Err(InviteError::Malformed(format!("unknown enrollment refusal {value}"))),
        }
    }

    fn into_error(self) -> InviteError {
        match self {
            Self::Expired => InviteError::Expired,
            Self::Used => InviteError::Used,
            Self::Unknown => InviteError::Unknown,
            Self::WrongNode => InviteError::WrongNode,
            Self::AccountMismatch => InviteError::AccountMismatch,
            Self::Revoked => InviteError::Revoked,
            Self::JoinerCapacity => InviteError::JoinerCapacity,
            Self::HeldStateConflict => InviteError::HeldStateConflict,
        }
    }
}

#[derive(Debug)]
pub enum InviteError {
    Malformed(String),
    Expired,
    Used,
    Unknown,
    WrongNode,
    /// The request's expected account differs from the account bound to its one-time nonce.
    AccountMismatch,
    /// The exact-request replay found the acknowledged DeviceAdd no longer roster-effective —
    /// the owner removed the device inside the replay window, so the stored receipt (and its
    /// stream-key wraps) must not be released again.
    Revoked,
    /// The exact receipt does not fit the admission budget the joiner declared. Refused BEFORE
    /// the nonce is consumed: candidate capacity is grow-only, so committing the redemption
    /// would burn the enrollment on a receipt the joiner can never hold.
    JoinerCapacity,
    /// The joiner claims a candidate the owner's authenticated snapshot does not hold. Refused
    /// BEFORE the nonce is consumed: adopting the receipt into the union with that unreconciled
    /// history could make the acknowledged DeviceAdd ineffective, and every exact replay would
    /// fail identically.
    HeldStateConflict,
    Storage(anyhow::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for InviteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(message) => write!(f, "malformed enrollment data: {message}"),
            Self::Expired => write!(f, "enrollment invite expired"),
            Self::Used => write!(f, "enrollment invite was already used"),
            Self::Unknown => write!(f, "enrollment invite is unknown"),
            Self::WrongNode =>
                write!(f, "join request transport node does not match the connection"),
            Self::AccountMismatch => write!(f, "enrollment invite belongs to a different account"),
            Self::Revoked => write!(f, "the enrolled device was removed from the roster"),
            Self::JoinerCapacity =>
                write!(f, "the enrollment receipt does not fit the joiner's declared capacity"),
            Self::HeldStateConflict => write!(
                f,
                "the joiner holds account candidates the inviter's snapshot cannot reconcile"
            ),
            Self::Storage(error) => write!(f, "enrollment storage: {error}"),
            Self::Io(error) => write!(f, "enrollment stream: {error}"),
        }
    }
}

impl std::error::Error for InviteError {}

impl From<anyhow::Error> for InviteError {
    fn from(value: anyhow::Error) -> Self {
        Self::Storage(value)
    }
}

impl From<std::io::Error> for InviteError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn mint_invite(
    conn: &Connection,
    spec: InviteSpec<'_>,
) -> Result<EnrollmentTicket, InviteError> {
    let InviteSpec { account_id, inviter_node_id, relay_url, role, label, now_ms, ttl } = spec;
    validate_enrollment_route(&inviter_node_id, &relay_url)?;
    validate_device_add_label(label).map_err(|error| InviteError::Malformed(error.to_string()))?;
    let ttl_ms = i64::try_from(ttl.as_millis())
        .map_err(|_| InviteError::Malformed("invite TTL is too large".into()))?;
    // A zero or sub-millisecond TTL mints a ticket that is already expired — redemption treats
    // `now_ms >= expires_at_ms` as expired, so this is a deterministic failure that must not
    // cross the invite-issuance boundary.
    if ttl_ms == 0 {
        return Err(InviteError::Malformed("invite TTL must be at least one millisecond".into()));
    }
    let mut nonce_bytes = [MaybeUninit::uninit(); 32];
    let nonce = getrandom::fill_uninit(&mut nonce_bytes)
        .map_err(|error| InviteError::Storage(anyhow::anyhow!("invite entropy: {error}")))?
        .try_into()
        .map(|nonce: &mut [u8; 32]| *nonce)
        .expect("the fixed-size destination preserves its length");
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    // BEGIN IMMEDIATE can wait out the busy timeout behind another writer: read the mint clock
    // ONLY NOW, or a ticket minted after a long lock wait would carry a pre-wait expiry and be
    // born already unusable (mirrors redemption's post-lock clock, #945).
    let now_ms = now_ms();
    let expires_at_ms = now_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| InviteError::Malformed("invite expiry overflows i64".into()))?;
    require_founder_enrollment_authority(&tx, account_id)?;
    // The candidate store is grow-only — capacity never drains — so if it cannot fit THIS
    // redemption's DeviceAdd plus its stream-key wraps, the ticket would be permanently
    // unusable: a deterministic failure that must gate the invite-issuance boundary (#945),
    // checked in the same commit snapshot as the authority preflight. Minting then RESERVES
    // that exact requirement against the shared candidate counters, so ordinary ingest or a
    // second mint cannot consume the headroom this ticket was measured against; the reservation
    // is released only by redemption (under the writer lock) or expiry.
    let streams = owned_streams_for_account(&tx, account_id)?;
    enrollment_authoring_fits(&tx, account_id, &streams, role, label, now_ms)?;
    let (reserved_entries, reserved_bytes) =
        rag_rat_oplog::enrollment_authoring_requirements(&tx, account_id, &streams, role, label)?;
    rag_rat_oplog::upsert_account_candidate_reservation_in_tx(
        &tx,
        account_id,
        nonce,
        reserved_entries,
        reserved_bytes,
        reserved_entries.saturating_sub(1),
        expires_at_ms,
    )?;
    prune_expired_invites_in_tx(&tx, now_ms)?;
    tx.execute(
        "INSERT INTO sync_invites(
             nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![
            nonce.as_slice(),
            account_id.to_bytes().as_slice(),
            role.as_db_str(),
            label,
            expires_at_ms,
            now_ms,
        ],
    )
    .map_err(|error| InviteError::Storage(error.into()))?;
    tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
    Ok(EnrollmentTicket { account_id, inviter_node_id, relay_url, nonce, expires_at_ms })
}

fn require_founder_enrollment_authority(
    conn: &Connection,
    account_id: AccountId,
) -> Result<(), InviteError> {
    if read_local_account(conn)
        .map_err(InviteError::from)?
        .filter(|local| *local == account_id)
        .is_none()
    {
        return Err(InviteError::Storage(anyhow::anyhow!(
            "invite account is not the local account"
        )));
    }
    let local_device = load_local_device(conn)
        .map_err(InviteError::from)?
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("local device identity is missing")))?;
    let genesis_bytes: Vec<u8> = conn
        .query_row("SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0", [], |row| {
            row.get(0)
        })
        .map_err(|error| InviteError::Storage(error.into()))?;
    let genesis_hash = genesis_bytes.try_into().map_err(|_| {
        InviteError::Storage(anyhow::anyhow!("local account genesis hash is not 32 bytes"))
    })?;
    let authority = owner_control_authority_in_snapshot(
        conn,
        account_id,
        genesis_hash,
        local_device.fingerprint(),
    )
    .map_err(InviteError::from)?;
    if !matches!(
        authority,
        AuthorityQuery::Effective(authority)
            if authority.device_boundary == AuthorityBoundary::Open
                && authority.incarnation_boundary == AuthorityBoundary::Open
    ) {
        return Err(InviteError::Storage(anyhow::anyhow!(
            "local device lacks open founder authority to enroll devices"
        )));
    }
    Ok(())
}

pub fn redeem_invite(
    conn: &Connection,
    request: EnrollmentRequest,
    authenticated_remote_node: [u8; 32],
    now_ms: &dyn Fn() -> i64,
) -> Result<(EnrollmentReceipt, CatchUpReport), InviteError> {
    if request.transport_node_id != authenticated_remote_node {
        return Err(InviteError::WrongNode);
    }
    // Reject random unauthenticated nonces without taking SQLite's database-wide writer
    // reservation. A valid candidate is re-read after BEGIN IMMEDIATE below.
    let Some(invite) = stored_invite(conn, request.nonce)? else {
        return Err(InviteError::Unknown);
    };
    let account_id = stored_invite_account(&invite)?;
    if request.expected_account != account_id {
        return Err(InviteError::AccountMismatch);
    }
    let arrival_ms = now_ms();
    if receipt_replay_expired(&invite, arrival_ms) {
        prune_expired_invites(conn, arrival_ms)?;
        return Err(InviteError::Used);
    }
    if let Some(receipt) = replay_receipt(conn, &invite, &request)? {
        return Ok((receipt, empty_catch_up(&request)));
    }
    if arrival_ms >= invite.expires_at_ms {
        return Err(InviteError::Expired);
    }
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    // BEGIN IMMEDIATE can wait out the busy timeout behind another writer: re-read the clock
    // NOW that the writer lock is held, or an invite that expired during the wait would be
    // consumed against the stale pre-wait timestamp.
    let commit_ms = now_ms();
    let Some(invite) = stored_invite(&tx, request.nonce)? else {
        return Err(InviteError::Unknown);
    };
    let account_id = stored_invite_account(&invite)?;
    if request.expected_account != account_id {
        return Err(InviteError::AccountMismatch);
    }
    if receipt_replay_expired(&invite, commit_ms) {
        prune_expired_invites_in_tx(&tx, commit_ms)?;
        tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
        return Err(InviteError::Used);
    }
    if let Some(receipt) = replay_receipt(&tx, &invite, &request)? {
        return Ok((receipt, empty_catch_up(&request)));
    }
    if commit_ms >= invite.expires_at_ms {
        return Err(InviteError::Expired);
    }
    prune_expired_invites_in_tx(&tx, commit_ms)?;
    if read_local_account(&tx)
        .map_err(InviteError::from)?
        .filter(|local| *local == account_id)
        .is_none()
    {
        return Err(InviteError::Storage(anyhow::anyhow!(
            "invite account is not the local account"
        )));
    }
    let role = DeviceRole::from_db_str(&invite.role).map_err(InviteError::from)?;
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into());
    // Release THIS invite's reservation under the writer lock, then RE-MEASURE the mandatory
    // requirement against current state: key targets may have grown since minting, and the
    // reservation covered only the mint-time set. The fits check runs with our reservation
    // released (other outstanding invites' reservations still count), so it passes only if the
    // DeviceAdd plus the CURRENT wraps genuinely fit — a shortfall rolls back, preserving the
    // nonce and restoring the reservation instead of stranding the ticket mid-redemption.
    rag_rat_oplog::release_account_candidate_reservation_in_tx(&tx, request.nonce)?;
    // Resolve ownership in this same redemption snapshot. A long-running server can ingest
    // StreamOwn/StreamRevoke entries after startup; caching this set would either omit a newly
    // owned stream's key wrap or make a stale, no-longer-owned stream abort the whole enrollment.
    let streams = owned_streams_for_account(&tx, account_id)?;
    enrollment_authoring_fits(&tx, account_id, &streams, role, invite.label.as_deref(), commit_ms)?;
    let device_add = author_enrollment_device_add_in_tx(
        &tx,
        EnrollingDevice {
            ed25519_pubkey: request.ed25519_pubkey,
            x25519_pubkey: request.x25519_pubkey,
            label: invite.label,
        },
        role,
        commit_ms,
    )?;
    let device_add_signed = tx
        .query_row(
            "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
            [device_add.as_slice()],
            |row| row.get(0),
        )
        .map_err(|error| InviteError::Storage(error.into()))?;
    let catch_up = enroll_stream_keys_for_device_in_tx(&tx, fingerprint, &streams, commit_ms)?;
    let bootstrap_entries = account_entries_for_enrollment(&tx, account_id)?;
    // Every candidate the joiner claims to hold MUST be one the owner's authenticated snapshot
    // also holds. An unrepresented hash means the joiner carries history this receipt cannot
    // reconcile (a competing control branch, or a false claim); adoption would refold the union
    // of the receipt and those grow-only extras and could leave the acknowledged DeviceAdd
    // ineffective — burning the nonce on a bootstrap that can never succeed. Refuse BEFORE the
    // consume boundary, rolling back the authored entries and restoring the reservation.
    let receipt_hashes: std::collections::HashSet<&[u8; 32]> =
        bootstrap_entries.iter().map(|entry| &entry.entry_hash).collect();
    if request.held_entry_hashes.iter().any(|hash| !receipt_hashes.contains(hash)) {
        return Err(InviteError::HeldStateConflict);
    }
    let account_entries =
        bootstrap_entries.iter().map(|entry| entry.signed_bytes.clone()).collect();
    let receipt =
        EnrollmentReceipt { device_add_hash: device_add, device_add_signed, account_entries };
    // The receipt must FIT the state the joiner declared: candidate capacity is grow-only, so
    // consuming the one-time nonce for a receipt the joiner can never hold would burn the
    // enrollment — a deterministic failure checked BEFORE the consume boundary (#945).
    //
    // The charge is the joiner's ACTUAL adoption cost, not the receipt's raw size:
    // - entries whose hash the joiner already holds are FREE (`insert_candidate` returns
    //   `AlreadyPresent`) — only the confirmed intersection the joiner proved is credited;
    // - every NEW authenticated candidate is charged against the declared candidate budgets.
    let held: std::collections::HashSet<&[u8; 32]> = request.held_entry_hashes.iter().collect();
    let mut new_entries = 0u64;
    let mut new_bytes = 0u64;
    for entry in &bootstrap_entries {
        if held.contains(&entry.entry_hash) {
            continue;
        }
        new_entries += 1;
        new_bytes += entry.signed_bytes.len() as u64;
    }
    let budget = &request.budget;
    if new_entries > budget.account_entries_remaining
        || new_entries > budget.global_entries_remaining
        || new_bytes > budget.account_bytes_remaining
        || new_bytes > budget.global_bytes_remaining
    {
        return Err(InviteError::JoinerCapacity);
    }
    ensure_frame_len(
        EnrollmentResponse::Enrolled(receipt.clone()).encode().len(),
        MAX_ENROLL_RESPONSE_FRAME,
        "response",
    )?;
    // Only the joiner-specific DeviceAdd plus the manifest of receipt entry hashes is persisted:
    // the bootstrap is already durable in the grow-only candidate DAG, and replay reconstructs
    // the EXACT acknowledged receipt from it rather than storing one full copy per invite
    // (quadratic across a fleet, #945).
    let mut receipt_manifest = Vec::with_capacity(32 * bootstrap_entries.len());
    for entry in &bootstrap_entries {
        receipt_manifest.extend_from_slice(&entry.entry_hash);
    }
    let changed = tx
        .execute(
            "UPDATE sync_invites
                SET used_at_ms = ?2,
                    used_transport_node = ?3,
                    used_ed25519_pubkey = ?4,
                    used_x25519_pubkey = ?5,
                    receipt_hash = ?6,
                    receipt_signed = ?7,
                    receipt_entries = ?8
              WHERE nonce = ?1 AND used_at_ms IS NULL AND expires_at_ms > ?2",
            params![
                request.nonce.as_slice(),
                commit_ms,
                request.transport_node_id.as_slice(),
                request.ed25519_pubkey.as_slice(),
                request.x25519_pubkey.as_slice(),
                receipt.device_add_hash.as_slice(),
                receipt.device_add_signed,
                receipt_manifest,
            ],
        )
        .map_err(|error| InviteError::Storage(error.into()))?;
    if changed != 1 {
        return Err(InviteError::Used);
    }
    tx.commit().map_err(|error| InviteError::Storage(error.into()))?;
    if let Err(error) = retry_enrollment_pre_verify(conn, account_id, commit_ms) {
        tracing::warn!(%error, "post-enrollment pre-verify retry failed");
    }
    Ok((receipt, catch_up))
}

fn stored_invite(conn: &Connection, nonce: [u8; 32]) -> Result<Option<StoredInvite>, InviteError> {
    conn.query_row(
        "SELECT account_id, role, label, expires_at_ms, used_at_ms,
                used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
                receipt_hash, receipt_signed, receipt_entries, receipt_bytes
           FROM sync_invites WHERE nonce = ?1",
        [nonce.as_slice()],
        |row| {
            Ok(StoredInvite {
                account_bytes: row.get(0)?,
                role: row.get(1)?,
                label: row.get(2)?,
                expires_at_ms: row.get(3)?,
                used_at_ms: row.get(4)?,
                used_transport_node: row.get(5)?,
                used_ed25519_pubkey: row.get(6)?,
                used_x25519_pubkey: row.get(7)?,
                receipt_hash: row.get(8)?,
                receipt_signed: row.get(9)?,
                receipt_entries: row.get(10)?,
                receipt_bytes: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(|error| InviteError::Storage(error.into()))
}

fn stored_invite_account(invite: &StoredInvite) -> Result<AccountId, InviteError> {
    Ok(AccountId::from_bytes(invite.account_bytes.as_slice().try_into().map_err(|_| {
        InviteError::Storage(anyhow::anyhow!("sync_invites account_id is not 32 bytes"))
    })?))
}

fn replay_receipt(
    conn: &Connection,
    invite: &StoredInvite,
    request: &EnrollmentRequest,
) -> Result<Option<EnrollmentReceipt>, InviteError> {
    if invite.used_at_ms.is_none() {
        return Ok(None);
    }
    let same_request = invite.used_transport_node.as_deref()
        == Some(request.transport_node_id.as_slice())
        && invite.used_ed25519_pubkey.as_deref() == Some(request.ed25519_pubkey.as_slice())
        && invite.used_x25519_pubkey.as_deref() == Some(request.x25519_pubkey.as_slice());
    if !same_request {
        return Err(InviteError::Used);
    }
    let device_add_hash: [u8; 32] = invite
        .receipt_hash
        .as_deref()
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("used invite has no receipt hash")))?
        .try_into()
        .map_err(|_| {
            InviteError::Storage(anyhow::anyhow!("stored receipt hash is not 32 bytes"))
        })?;
    let device_add_signed = invite
        .receipt_signed
        .clone()
        .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("used invite has no DeviceAdd")))?;
    let account_id = stored_invite_account(invite)?;
    let receipt = if let Some(manifest) = invite.receipt_entries.as_deref() {
        if manifest.len() % 32 != 0 || manifest.len() / 32 > MAX_ENROLL_BOOTSTRAP_ENTRIES as usize {
            return Err(InviteError::Storage(anyhow::anyhow!(
                "stored receipt manifest is not a bounded hash list"
            )));
        }
        // Reconstruct the EXACT acknowledged receipt from the grow-only candidate DAG: the
        // manifest pins the original entry set and order, so the capacity and frame checks the
        // original redemption measured stay valid no matter how much history arrived since —
        // the replay never ships an unadoptable or oversized response.
        let mut signed_by_hash: std::collections::HashMap<[u8; 32], Vec<u8>> =
            account_entries_for_enrollment(conn, account_id)?
                .into_iter()
                .map(|entry| (entry.entry_hash, entry.signed_bytes))
                .collect();
        let mut account_entries = Vec::with_capacity(manifest.len() / 32);
        for hash in manifest.chunks_exact(32) {
            let hash: [u8; 32] = hash.try_into().expect("chunked by 32");
            let bytes = signed_by_hash.remove(&hash).ok_or_else(|| {
                InviteError::Storage(anyhow::anyhow!(
                    "receipt entry missing from the candidate DAG"
                ))
            })?;
            account_entries.push(bytes);
        }
        EnrollmentReceipt { device_add_hash, device_add_signed, account_entries }
    } else {
        // Pre-V092 legacy form: the exact receipt was stored whole and replays as-is through the
        // remainder of its 24h window.
        let receipt_bytes = invite
            .receipt_bytes
            .as_deref()
            .ok_or_else(|| InviteError::Storage(anyhow::anyhow!("used invite has no receipt")))?;
        let receipt = EnrollmentReceipt::decode(receipt_bytes)?;
        if receipt.device_add_hash != device_add_hash
            || receipt.device_add_signed != device_add_signed
        {
            return Err(InviteError::Storage(anyhow::anyhow!(
                "stored receipt replay columns disagree"
            )));
        }
        receipt
    };
    // A replay must not outlive the enrollment it replays: if the owner removed the device
    // after redemption, re-releasing the stored bootstrap and its stream-key wraps would arm a
    // revoked device and report success for an enrollment Closed sync can no longer authorize.
    let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into());
    let still_effective: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM account_roster_history
                  WHERE account_id = ?1
                    AND device_fingerprint = ?2
                    AND roster_ref = ?3
                    AND closed_at IS NULL
            )",
            params![
                account_id.to_bytes().as_slice(),
                fingerprint.to_bytes().as_slice(),
                receipt.device_add_hash.as_slice(),
            ],
            |row| row.get(0),
        )
        .map_err(|error| InviteError::Storage(error.into()))?;
    if !still_effective {
        return Err(InviteError::Revoked);
    }
    Ok(Some(receipt))
}

fn receipt_replay_expired(invite: &StoredInvite, now_ms: i64) -> bool {
    invite
        .used_at_ms
        .is_some_and(|used_at_ms| now_ms >= used_at_ms.saturating_add(RECEIPT_REPLAY_RETENTION_MS))
}

fn prune_expired_invites(conn: &Connection, now_ms: i64) -> Result<(), InviteError> {
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|error| InviteError::Storage(error.into()))?;
    prune_expired_invites_in_tx(&tx, now_ms)?;
    tx.commit().map_err(|error| InviteError::Storage(error.into()))
}

fn prune_expired_invites_in_tx(conn: &Connection, now_ms: i64) -> Result<(), InviteError> {
    let replay_cutoff_ms = now_ms.saturating_sub(RECEIPT_REPLAY_RETENTION_MS);
    conn.execute(
        "DELETE FROM sync_invites
          WHERE (used_at_ms IS NULL AND expires_at_ms <= ?1)
             OR (used_at_ms IS NOT NULL AND used_at_ms <= ?2)",
        params![now_ms, replay_cutoff_ms],
    )
    .map_err(|error| InviteError::Storage(error.into()))?;
    rag_rat_oplog::prune_account_candidate_reservations_in_tx(conn, now_ms)?;
    Ok(())
}

fn empty_catch_up(request: &EnrollmentRequest) -> CatchUpReport {
    CatchUpReport {
        target: DeviceFingerprint::from_bytes(Sha256::digest(request.ed25519_pubkey).into()),
        authored: Vec::new(),
        already_covered: Vec::new(),
    }
}

pub async fn run_enrollment_acceptor<R, W, F>(
    recv: &mut R,
    send: &mut W,
    conn: &Connection,
    authenticated_remote_node: [u8; 32],
    now_ms: F,
) -> Result<EnrollmentAcceptorOutcome, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn() -> i64,
{
    run_enrollment_acceptor_with_progress(
        recv,
        send,
        conn,
        authenticated_remote_node,
        now_ms,
        ENROLL_PROGRESS_TIMEOUT,
    )
    .await
}

pub async fn run_enrollment_acceptor_with_progress<R, W, F>(
    recv: &mut R,
    send: &mut W,
    conn: &Connection,
    authenticated_remote_node: [u8; 32],
    now_ms: F,
    progress: Duration,
) -> Result<EnrollmentAcceptorOutcome, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn() -> i64,
{
    // The request and a refusal are reachable by ANY peer (a random nonce gets both): read and
    // write them under the tighter unauthenticated window so a bogus peer's hold on the serial
    // accept loop stays in seconds. The enrolled receipt — gated on a valid nonce — gets the
    // full progress window.
    let unauth = progress.min(ENROLL_UNAUTH_PROGRESS_TIMEOUT);
    let request = EnrollmentRequest::decode(
        &read_blob(recv, MAX_ENROLL_REQUEST_FRAME, "request", unauth).await?,
    )?;
    // Evaluate expiry only after the complete peer-controlled request arrives. A timestamp read
    // before this await would let a peer hold the stream open past expiry and still redeem.
    match redeem_invite(conn, request, authenticated_remote_node, &now_ms) {
        Ok((receipt, catch_up)) => {
            write_blob(
                send,
                &EnrollmentResponse::Enrolled(receipt.clone()).encode(),
                MAX_ENROLL_RESPONSE_FRAME,
                "response",
                progress,
            )
            .await?;
            Ok(EnrollmentAcceptorOutcome::Enrolled(receipt, catch_up))
        },
        Err(error) if refusal_code(&error).is_some() => {
            let code = refusal_code(&error).expect("guarded above");
            write_blob(
                send,
                &EnrollmentResponse::Refused(code).encode(),
                MAX_ENROLL_RESPONSE_FRAME,
                "response",
                unauth,
            )
            .await?;
            Ok(EnrollmentAcceptorOutcome::Refused(error))
        },
        Err(error) => Err(error),
    }
}

pub async fn run_enrollment_dialer<R, W>(
    recv: &mut R,
    send: &mut W,
    expected_account: AccountId,
    request: &EnrollmentRequest,
) -> Result<EnrollmentReceipt, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_enrollment_dialer_with_progress(
        recv,
        send,
        expected_account,
        request,
        ENROLL_PROGRESS_TIMEOUT,
    )
    .await
}

pub async fn run_enrollment_dialer_with_progress<R, W>(
    recv: &mut R,
    send: &mut W,
    expected_account: AccountId,
    request: &EnrollmentRequest,
    progress: Duration,
) -> Result<EnrollmentReceipt, InviteError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if request.expected_account != expected_account {
        return Err(InviteError::AccountMismatch);
    }
    write_blob(send, &request.encode(), MAX_ENROLL_REQUEST_FRAME, "request", progress).await?;
    let response = EnrollmentResponse::decode(
        &read_blob(recv, MAX_ENROLL_RESPONSE_FRAME, "response", progress).await?,
    )?;
    // Ack the response BEFORE any post-processing (verification, adoption): the byte tells the
    // acceptor the response reached the application, so it can close immediately instead of
    // waiting on us to finish local work and tear down.
    let ack_window = progress.min(RESPONSE_ACK_TIMEOUT);
    if write_within(send, &[RESPONSE_ACK], ack_window).await.is_ok() {
        let _ = flush_within(send, ack_window).await;
    }
    match response {
        EnrollmentResponse::Enrolled(receipt) => {
            verify_enrollment_device_add(
                &receipt.account_entries,
                expected_account,
                receipt.device_add_hash,
                &receipt.device_add_signed,
                request.ed25519_pubkey,
                request.x25519_pubkey,
            )
            .map_err(|error| {
                InviteError::Malformed(format!("invalid enrollment receipt: {error}"))
            })?;
            Ok(receipt)
        },
        EnrollmentResponse::Refused(code) => Err(code.into_error()),
    }
}

fn refusal_code(error: &InviteError) -> Option<RefusalCode> {
    match error {
        InviteError::Expired => Some(RefusalCode::Expired),
        InviteError::Used => Some(RefusalCode::Used),
        InviteError::Unknown => Some(RefusalCode::Unknown),
        InviteError::WrongNode => Some(RefusalCode::WrongNode),
        InviteError::AccountMismatch => Some(RefusalCode::AccountMismatch),
        InviteError::Revoked => Some(RefusalCode::Revoked),
        InviteError::JoinerCapacity => Some(RefusalCode::JoinerCapacity),
        InviteError::HeldStateConflict => Some(RefusalCode::HeldStateConflict),
        InviteError::Malformed(_) | InviteError::Storage(_) | InviteError::Io(_) => None,
    }
}

async fn write_blob<W: AsyncWrite + Unpin>(
    send: &mut W,
    body: &[u8],
    max_len: u32,
    frame_name: &str,
    progress: Duration,
) -> Result<(), InviteError> {
    let len = ensure_frame_len(body.len(), max_len, frame_name)?;
    write_within(send, &len.to_be_bytes(), progress).await?;
    // Chunked so each slice gets its own progress window: a slow-reading peer back-pressures
    // `write_all`, and a monolithic write would turn the whole-exchange deadline into a
    // total-transfer deadline the peer controls.
    for chunk in body.chunks(ENROLL_PROGRESS_CHUNK_BYTES) {
        write_within(send, chunk, progress).await?;
    }
    flush_within(send, progress).await?;
    Ok(())
}

async fn flush_within<W: AsyncWrite + Unpin>(
    send: &mut W,
    window: Duration,
) -> Result<(), InviteError> {
    tokio::time::timeout(window, send.flush()).await.map_err(|_| {
        InviteError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "enrollment frame flush stalled",
        ))
    })??;
    Ok(())
}

async fn write_within<W: AsyncWrite + Unpin>(
    send: &mut W,
    bytes: &[u8],
    window: Duration,
) -> Result<(), InviteError> {
    tokio::time::timeout(window, send.write_all(bytes)).await.map_err(|_| {
        InviteError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "enrollment frame write stalled",
        ))
    })??;
    Ok(())
}

fn ensure_frame_len(len: usize, max_len: u32, frame_name: &str) -> Result<u32, InviteError> {
    let len = u32::try_from(len)
        .map_err(|_| InviteError::Malformed("enrollment frame length overflows u32".into()))?;
    if len > max_len {
        return Err(InviteError::Malformed(format!(
            "enrollment {frame_name} frame exceeds {max_len} bytes"
        )));
    }
    Ok(len)
}

async fn read_blob<R: AsyncRead + Unpin>(
    recv: &mut R,
    max_len: u32,
    frame_name: &str,
    progress: Duration,
) -> Result<Vec<u8>, InviteError> {
    let mut prefix = [0u8; 4];
    read_within(recv, &mut prefix, progress).await?;
    let len = u32::from_be_bytes(prefix);
    if len > max_len {
        return Err(InviteError::Malformed(format!(
            "enrollment {frame_name} frame exceeds {max_len} bytes"
        )));
    }
    let mut body = vec![0; len as usize];
    for chunk in body.chunks_mut(ENROLL_PROGRESS_CHUNK_BYTES) {
        read_within(recv, chunk, progress).await?;
    }
    Ok(body)
}

async fn read_within<R: AsyncRead + Unpin>(
    recv: &mut R,
    buf: &mut [u8],
    window: Duration,
) -> Result<(), InviteError> {
    tokio::time::timeout(window, recv.read_exact(buf)).await.map_err(|_| {
        InviteError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "enrollment frame read stalled",
        ))
    })??;
    Ok(())
}

fn exact_array(dec: &mut Decoder<'_>, expected: u64, name: &str) -> Result<(), InviteError> {
    if dec.array().map_err(decode)? != Some(expected) {
        return Err(InviteError::Malformed(format!("{name} arity")));
    }
    Ok(())
}

fn exact_str(dec: &mut Decoder<'_>, expected: &str, name: &str) -> Result<(), InviteError> {
    if dec.str().map_err(decode)? != expected {
        return Err(InviteError::Malformed(format!("{name} mismatch")));
    }
    Ok(())
}

fn fixed32(bytes: &[u8], name: &str) -> Result<[u8; 32], InviteError> {
    bytes.try_into().map_err(|_| InviteError::Malformed(format!("{name} must be 32 bytes")))
}

fn ensure_consumed(dec: &Decoder<'_>, bytes: &[u8]) -> Result<(), InviteError> {
    if dec.position() != bytes.len() {
        return Err(InviteError::Malformed("trailing bytes".into()));
    }
    Ok(())
}

fn decode(error: minicbor::decode::Error) -> InviteError {
    InviteError::Malformed(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::task::{Context, Poll};

    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    struct StallAfterBytes {
        remaining: usize,
    }

    impl AsyncWrite for StallAfterBytes {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Pending;
            }
            let written = self.remaining.min(bytes.len());
            self.remaining -= written;
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn
    }

    fn joiner_keys() -> ([u8; 32], [u8; 32]) {
        let conn = db();
        rag_rat_oplog::local_account(&conn, NOW).unwrap();
        conn.query_row(
            "SELECT public_key, x25519_public FROM oplog_device_identity WHERE id = 0",
            [],
            |row| {
                let ed: Vec<u8> = row.get(0)?;
                let x: Vec<u8> = row.get(1)?;
                Ok((ed.try_into().unwrap(), x.try_into().unwrap()))
            },
        )
        .unwrap()
    }

    fn ticket(conn: &Connection, account: AccountId, role: DeviceRole) -> EnrollmentTicket {
        mint_invite(conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role,
            label: Some("laptop"),
            now_ms: &|| NOW,
            ttl: Duration::from_secs(60),
        })
        .unwrap()
    }

    fn invalid_endpoint_id() -> [u8; 32] {
        (0u64..1_000)
            .map(|ordinal| Sha256::digest(ordinal.to_be_bytes()).into())
            .find(|bytes| EndpointId::from_bytes(bytes).is_err())
            .expect("random-looking bytes include an invalid compressed Edwards point")
    }

    /// A budget no honest redemption can exceed, for tests that are not exercising the capacity
    /// check.
    fn generous_budget() -> EnrollmentBudget {
        EnrollmentBudget {
            account_entries_remaining: u64::MAX,
            account_bytes_remaining: u64::MAX,
            global_entries_remaining: u64::MAX,
            global_bytes_remaining: u64::MAX,
        }
    }

    #[test]
    fn request_is_canonical_and_exactly_bound() {
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: [7; 32],
            expected_account: AccountId::from_bytes([8; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: EnrollmentBudget {
                account_entries_remaining: 10,
                account_bytes_remaining: 20,
                global_entries_remaining: 30,
                global_bytes_remaining: 40,
            },
            held_entry_hashes: vec![[3; 32], [4; 32]],
        };
        let bytes = request.encode();
        assert!(
            bytes.len() <= MAX_ENROLL_REQUEST_FRAME as usize,
            "the request still fits the unauthenticated frame cap"
        );
        assert_eq!(EnrollmentRequest::decode(&bytes).unwrap(), request);

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(EnrollmentRequest::decode(&trailing), Err(InviteError::Malformed(_))));
        // A non-minimal u64 in the budget fails the canonical re-encode check.
        let mut non_minimal = bytes.clone();
        non_minimal.truncate(bytes.len() - 2); // drop the final u8-form u64 (0x18 60)
        non_minimal.push(0x1b); // and re-encode the same value in the 8-byte form
        non_minimal.extend_from_slice(&60u64.to_be_bytes());
        assert!(matches!(EnrollmentRequest::decode(&non_minimal), Err(InviteError::Malformed(_))));

        let hashes = (0..ENROLLMENT_HELD_ENTRY_HASHES_MAX)
            .map(|ordinal| {
                let mut hash = [0u8; 32];
                hash[24..].copy_from_slice(&(ordinal as u64).to_be_bytes());
                hash
            })
            .collect::<Vec<_>>();
        let maximal = EnrollmentRequest { held_entry_hashes: hashes, ..request.clone() };
        let maximal_bytes = maximal.encode();
        assert!(maximal_bytes.len() <= MAX_ENROLL_REQUEST_FRAME as usize);
        assert_eq!(EnrollmentRequest::decode(&maximal_bytes).unwrap(), maximal);

        let mut unsorted = request.clone();
        unsorted.held_entry_hashes = vec![[2; 32], [1; 32]];
        assert!(matches!(
            EnrollmentRequest::decode(&unsorted.encode()),
            Err(InviteError::Malformed(_))
        ));
        let mut duplicate = request;
        duplicate.held_entry_hashes = vec![[2; 32], [2; 32]];
        assert!(matches!(
            EnrollmentRequest::decode(&duplicate.encode()),
            Err(InviteError::Malformed(_))
        ));
    }

    #[test]
    fn ticket_is_canonical_and_exactly_bound() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::ReadOnly);
        let bytes = ticket.encode();
        assert_eq!(EnrollmentTicket::decode(&bytes).unwrap(), ticket);

        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(EnrollmentTicket::decode(&trailing), Err(InviteError::Malformed(_))));

        let invalid_node =
            EnrollmentTicket { inviter_node_id: invalid_endpoint_id(), ..ticket.clone() };
        assert!(matches!(
            EnrollmentTicket::decode(&invalid_node.encode()),
            Err(InviteError::Malformed(message)) if message.contains("node id")
        ));
        let invalid_relay = EnrollmentTicket { relay_url: "not a relay URL".into(), ..ticket };
        assert!(matches!(
            EnrollmentTicket::decode(&invalid_relay.encode()),
            Err(InviteError::Malformed(message)) if message.contains("relay URL")
        ));
    }

    #[test]
    fn mint_requires_live_founder_authority_and_leaves_no_invite_on_failure() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        conn.execute(
            "UPDATE account_owner_incarnations
                SET closed_at = ?2,
                    control_boundary = 'closed',
                    control_seq = NULL,
                    control_hash = NULL
              WHERE account_id = ?1",
            params![account.to_bytes().as_slice(), NOW + 1],
        )
        .unwrap();
        conn.execute(
            "UPDATE account_roster_history
                SET closed_at = ?2,
                    control_boundary = 'closed',
                    control_seq = NULL,
                    control_hash = NULL
              WHERE account_id = ?1",
            params![account.to_bytes().as_slice(), NOW + 1],
        )
        .unwrap();

        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: Some("laptop"),
            now_ms: &|| NOW + 2,
            ttl: Duration::from_secs(60),
        });

        assert!(matches!(result, Err(InviteError::Storage(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "a device unable to redeem must not distribute an invite");
        let synchronous: i64 =
            conn.pragma_query_value(None, "synchronous", |row| row.get(0)).unwrap();
        assert_eq!(synchronous, 1, "the authored-durability guard restores NORMAL");
    }

    #[test]
    fn mint_rejects_founder_authority_bounded_by_a_cut() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let genesis_hash: Vec<u8> = conn
            .query_row(
                "SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE account_roster_history
                SET control_boundary = 'cut',
                    control_seq = ?2,
                    control_hash = ?3
              WHERE account_id = ?1",
            params![account.to_bytes().as_slice(), 0_u64.to_be_bytes().as_slice(), genesis_hash,],
        )
        .unwrap();

        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: Some("laptop"),
            now_ms: &|| NOW + 1,
            ttl: Duration::from_secs(60),
        });

        assert!(matches!(result, Err(InviteError::Storage(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "bounded founder authority must not mint an invite");
    }

    #[test]
    fn mint_rejects_an_unauthorable_label_before_persisting_the_nonce() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let label = "x".repeat(64 * 1024);

        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: Some(&label),
            now_ms: &|| NOW,
            ttl: Duration::from_secs(60),
        });

        assert!(matches!(result, Err(InviteError::Malformed(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "an unusable invite must never cross the mint boundary");
    }

    #[test]
    fn mint_rejects_an_unparseable_relay_url_before_persisting_the_nonce() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "not a relay URL".into(),
            role: DeviceRole::Member,
            label: None,
            now_ms: &|| NOW,
            ttl: Duration::from_secs(60),
        });
        assert!(matches!(result, Err(InviteError::Malformed(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0);
    }

    #[test]
    fn mint_rejects_an_invalid_inviter_node_before_persisting_the_nonce() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: invalid_endpoint_id(),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: None,
            now_ms: &|| NOW,
            ttl: Duration::from_secs(60),
        });
        assert!(matches!(result, Err(InviteError::Malformed(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "an undialable node id must fail before invite persistence");
    }

    #[test]
    fn mint_rejects_a_live_key_the_founder_cannot_recover() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let stream =
            rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "recovery-test", NOW).unwrap();
        rag_rat_oplog::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
        tx.commit().unwrap();

        let other = db();
        rag_rat_oplog::local_account(&other, NOW).unwrap();
        let (secret, public): (Vec<u8>, Vec<u8>) = other
            .query_row(
                "SELECT x25519_secret, x25519_public FROM oplog_device_identity WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        conn.execute(
            "UPDATE oplog_device_identity SET x25519_secret = ?1, x25519_public = ?2 WHERE id = 0",
            params![secret, public],
        )
        .unwrap();

        let result = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: None,
            now_ms: &|| NOW + 1,
            ttl: Duration::from_secs(60),
        });
        assert!(matches!(result, Err(InviteError::Storage(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "an unrecoverable live key must gate invite issuance");
    }

    #[test]
    fn mint_refuses_a_ttl_that_is_already_expired() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        for ttl in [Duration::ZERO, Duration::from_nanos(999_999)] {
            let result = mint_invite(&conn, InviteSpec {
                account_id: account,
                inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
                relay_url: "https://relay.example".into(),
                role: DeviceRole::Member,
                label: None,
                now_ms: &|| NOW,
                ttl,
            });
            assert!(
                matches!(result, Err(InviteError::Malformed(_))),
                "a {ttl:?} TTL mints a ticket redemption always rejects as expired"
            );
        }
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "an unusable invite must never cross the mint boundary");
    }

    #[test]
    fn every_budget_scope_gates_the_consume() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        type BudgetScope = (&'static str, fn(&mut EnrollmentBudget));
        let scopes: [BudgetScope; 4] = [
            ("account_entries_remaining", |budget| budget.account_entries_remaining = 0),
            ("account_bytes_remaining", |budget| budget.account_bytes_remaining = 0),
            ("global_entries_remaining", |budget| budget.global_entries_remaining = 0),
            ("global_bytes_remaining", |budget| budget.global_bytes_remaining = 0),
        ];
        for (name, zero) in scopes {
            let ticket = ticket(&conn, account, DeviceRole::Member);
            let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
            let mut budget = generous_budget();
            zero(&mut budget);
            let request = EnrollmentRequest {
                nonce: ticket.nonce,
                expected_account: account,
                ed25519_pubkey,
                x25519_pubkey,
                transport_node_id: [9; 32],
                budget,
                held_entry_hashes: Vec::new(),
            };
            assert!(
                matches!(
                    redeem_invite(&conn, request, [9; 32], &|| NOW + 1),
                    Err(InviteError::JoinerCapacity)
                ),
                "zeroing {name} must refuse"
            );
            let used: Option<i64> = conn
                .query_row(
                    "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                    [ticket.nonce.as_slice()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(used, None, "the {name} refusal must not consume the nonce");
        }
    }

    #[test]
    fn redemption_charges_only_receipt_entries_the_joiner_does_not_hold() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        // Enroll one device so the next receipt carries genesis + AddB + AddJoiner.
        let first_ticket = ticket(&conn, account, DeviceRole::Member);
        let (first_ed, first_x) = joiner_keys();
        let first = EnrollmentRequest {
            nonce: first_ticket.nonce,
            expected_account: account,
            ed25519_pubkey: first_ed,
            x25519_pubkey: first_x,
            transport_node_id: [8; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let _ = redeem_invite(&conn, first, [8; 32], &|| NOW + 1).unwrap();

        // The joiner proves it already holds every prior entry (genesis + AddB): only the new
        // DeviceAdd is charged, so a one-entry budget fits.
        let prior = rag_rat_oplog::account_entries_for_sync(&conn, account).unwrap();
        assert_eq!(prior.len(), 2, "genesis plus the first DeviceAdd");
        let mut held = vec![prior[0].entry_hash, prior[1].entry_hash];
        held.sort_unstable();
        let second_ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: second_ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: EnrollmentBudget { account_entries_remaining: 1, ..generous_budget() },
            held_entry_hashes: held.clone(),
        };
        let _ = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2).unwrap();
    }

    #[test]
    fn redemption_preserves_the_nonce_when_the_receipt_exceeds_the_declared_budget() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        // Enroll one device so the next receipt carries history beyond genesis + its DeviceAdd.
        let first_ticket = ticket(&conn, account, DeviceRole::Member);
        let (first_ed, first_x) = joiner_keys();
        let first = EnrollmentRequest {
            nonce: first_ticket.nonce,
            expected_account: account,
            ed25519_pubkey: first_ed,
            x25519_pubkey: first_x,
            transport_node_id: [8; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let _ = redeem_invite(&conn, first, [8; 32], &|| NOW + 1).unwrap();

        let second_ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let mut request = EnrollmentRequest {
            nonce: second_ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        // The receipt is genesis + two DeviceAdds; a budget for only two entries cannot hold it.
        request.budget.account_entries_remaining = 2;
        assert!(matches!(
            redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2),
            Err(InviteError::JoinerCapacity)
        ));
        let used: Option<i64> = conn
            .query_row(
                "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                [second_ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(used, None, "a capacity refusal must not consume the nonce");
        // The same invite redeems once the joiner declares honest headroom.
        request.budget = generous_budget();
        let _ = redeem_invite(&conn, request, [9; 32], &|| NOW + 3).unwrap();
    }

    #[test]
    fn mint_reserves_and_redemption_releases_the_mandatory_candidate_capacity() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (reserved_entries, reserved_bytes): (i64, i64) = conn
            .query_row(
                "SELECT reserved_entries, reserved_bytes
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
                [ticket.nonce.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("minting persists a reservation for the mandatory redemption entries");
        assert!(reserved_entries >= 1, "the DeviceAdd itself must be reserved");
        assert!(reserved_bytes > 0, "the reserved entries must carry their byte cost");

        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let _ = redeem_invite(&conn, request, [9; 32], &|| NOW + 1).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_candidate_reservations WHERE reservation_id = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "redemption releases its own reservation under the writer lock");
    }

    #[test]
    fn an_outstanding_invite_reservation_gates_the_next_mint_until_expiry() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        // Simulate an already-minted invite whose reservation consumes the whole per-account
        // candidate budget: the next mint must refuse rather than distribute a ticket the first
        // redemption would strand.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::upsert_account_candidate_reservation_in_tx(
            &tx,
            account,
            [0x77; 32],
            4_096,
            0,
            0,
            NOW + 1_000,
        )
        .unwrap();
        tx.commit().unwrap();

        let spec = |now_ms: &'static dyn Fn() -> i64| InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: None,
            now_ms,
            ttl: Duration::from_secs(60),
        };
        assert!(matches!(mint_invite(&conn, spec(&|| NOW)), Err(InviteError::Storage(_))));
        let invites: i64 =
            conn.query_row("SELECT COUNT(*) FROM sync_invites", [], |row| row.get(0)).unwrap();
        assert_eq!(invites, 0, "no second invite may be minted against reserved capacity");
        // After the outstanding reservation expires, minting prunes it and succeeds.
        let _ = mint_invite(&conn, spec(&|| NOW + 1_001)).expect("expiry frees the reservation");
    }

    #[test]
    fn mint_reads_the_clock_once_after_acquiring_the_writer_lock() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let clock_reads = AtomicI64::new(0);
        let ticket = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: None,
            now_ms: &|| NOW + clock_reads.fetch_add(1, Ordering::SeqCst),
            ttl: Duration::from_secs(60),
        })
        .unwrap();
        assert_eq!(
            clock_reads.load(Ordering::SeqCst),
            1,
            "one post-lock read: a pre-lock timestamp cannot mint an already-expired ticket"
        );
        let created_at_ms: i64 = conn
            .query_row(
                "SELECT created_at_ms FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ticket.expires_at_ms, created_at_ms + 60_000);
    }

    #[test]
    fn new_mandatory_key_targets_grow_an_outstanding_invites_reservation() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let reservation_of = |nonce: [u8; 32]| {
            conn.query_row(
                "SELECT reserved_entries, reserved_bytes
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
                [nonce.as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap()
        };
        let (entries0, bytes0) = reservation_of(ticket.nonce);
        assert_eq!(entries0, 1, "no live keys yet: only the DeviceAdd is reserved");

        // Authoring a new live key target grows the outstanding invite's reservation in the same
        // transaction, so ordinary candidate writes cannot consume the headroom redemption will
        // need for the catch-up wrap.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW + 1).unwrap();
        rag_rat_oplog::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW + 1).unwrap();
        tx.commit().unwrap();
        let (entries1, bytes1) = reservation_of(ticket.nonce);
        assert_eq!(entries1, entries0 + 1, "one new live key target is one reserved wrap");
        assert!(bytes1 > bytes0, "the reserved wrap carries its byte cost");

        // Redemption re-measures the CURRENT requirement and succeeds against the grown
        // reservation, delivering the post-mint key's wrap to the joiner.
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (_, catch_up) = redeem_invite(&conn, request, [9; 32], &|| NOW + 2).unwrap();
        assert_eq!(catch_up.authored.len(), 1, "the post-mint key is wrapped for the joiner");
    }

    #[test]
    fn synced_key_target_growth_tops_up_the_outstanding_reservation() {
        // Another device of the same account authors a StreamOwn + key; the entries arrive here
        // through ordinary (untrusted) account sync. The fold-time top-up must grow the
        // outstanding invite's reservation to cover the new mandatory catch-up wrap — the local
        // authoring hooks never see this transition.
        // Enroll this store's device so it holds a roster wrap recipient: key recovery targets
        // are measured for the local device, exactly as they are on a real inviter.
        let inviter = db();
        let account = rag_rat_oplog::local_account(&inviter, NOW).unwrap();
        let conn = db();
        let _ = rag_rat_oplog::local_device(&conn, NOW).unwrap();
        let (ed25519_pubkey, x25519_pubkey): ([u8; 32], [u8; 32]) = conn
            .query_row(
                "SELECT public_key, x25519_public FROM oplog_device_identity WHERE id = 0",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map(|(ed, x)| (ed.try_into().unwrap(), x.try_into().unwrap()))
            .unwrap();
        let ticket = ticket(&inviter, account, DeviceRole::Member);
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (receipt, _) = redeem_invite(&inviter, request, [9; 32], &|| NOW + 1).unwrap();
        let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
            &receipt.account_entries,
            account,
            receipt.device_add_hash,
            &receipt.device_add_signed,
            ed25519_pubkey,
            x25519_pubkey,
        )
        .unwrap();
        let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(ed25519_pubkey).into());
        rag_rat_oplog::adopt_enrollment_bootstrap(&conn, rag_rat_oplog::EnrollmentBootstrap {
            account_entries: &receipt.account_entries,
            account_id: account,
            genesis_hash,
            device_fingerprint: fingerprint,
            device_add_hash: receipt.device_add_hash,
            now_ms: NOW + 1,
        })
        .unwrap();

        // The founder authors the new stream + key AFTER this device enrolled; the wrap names it.
        let tx = Transaction::new_unchecked(&inviter, TransactionBehavior::Immediate).unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW + 2).unwrap();
        rag_rat_oplog::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW + 2).unwrap();
        tx.commit().unwrap();
        let entries = rag_rat_oplog::account_entries_for_sync(&inviter, account).unwrap();

        // An outstanding invite on this store reserved only the DeviceAdd (no targets yet).
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::upsert_account_candidate_reservation_in_tx(
            &tx,
            account,
            [0x88; 32],
            1,
            200,
            0,
            NOW + 10_000,
        )
        .unwrap();
        tx.commit().unwrap();
        // The StreamOwn + wrap arrive through ordinary untrusted account sync.
        for entry in &entries {
            let _ = rag_rat_oplog::account_ingest(&conn, &entry.signed_bytes, NOW + 2).unwrap();
        }
        let (reserved_entries, reserved_targets): (i64, i64) = conn
            .query_row(
                "SELECT reserved_entries, reserved_targets
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
                [[0x88; 32].as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reserved_targets, 1, "the synced key target is now covered");
        assert_eq!(reserved_entries, 2, "DeviceAdd plus the new mandatory wrap");

        // A sealed /3 entry authored under the FIRST key, then a rotation to a second key: the
        // accepted content pins the historical key as a live catch-up target only when the
        // CONTENT settles — a path no account fold traverses (#949).
        rag_rat_oplog::author_content_batch(
            &inviter,
            stream,
            &[rag_rat_oplog::MemoryOp::NodeCreate {
                node_id: rag_rat_oplog::NodeId::from("n1"),
                content: rag_rat_oplog::NodeContent {
                    kind: "Invariant".into(),
                    title: "t".into(),
                    body: "body".into(),
                    confidence: "high".into(),
                    source: "agent".into(),
                    tags: Vec::new(),
                    payload: None,
                },
            }],
            rag_rat_oplog::SealPolicy::Sealed,
            NOW + 3,
        )
        .unwrap();
        let sealed = rag_rat_oplog::content_entries_for_sync(&inviter, account).unwrap();
        let sealed = sealed.last().expect("one sealed content entry").signed_bytes.clone();
        let tx = Transaction::new_unchecked(&inviter, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::rotate_stream_key_in_tx(&tx, stream, NOW + 4).unwrap();
        tx.commit().unwrap();
        // The rotation's wrap arrives by account sync; with no accepted content yet, the live
        // set is still only the selected key.
        for entry in rag_rat_oplog::account_entries_for_sync(&inviter, account).unwrap() {
            let _ = rag_rat_oplog::account_ingest(&conn, &entry.signed_bytes, NOW + 4).unwrap();
        }
        let (_, targets_after_rotation): (i64, i64) = conn
            .query_row(
                "SELECT reserved_entries, reserved_targets
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
                [[0x88; 32].as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(targets_after_rotation, 1, "rotation alone does not pin the historical key");
        // The sealed entry settles through content sync — acceptance pins the historical key and
        // the reservation grows without any account fold.
        rag_rat_oplog::content_ingest(&conn, &sealed, NOW + 5).unwrap();
        rag_rat_oplog::settle_pending_content_refolds(
            &conn,
            &rag_rat_oplog::ContentRefoldBudget::unbounded(),
            NOW + 5,
        )
        .unwrap();
        let (entries_after_content, targets_after_content): (i64, i64) = conn
            .query_row(
                "SELECT reserved_entries, reserved_targets
                   FROM account_candidate_reservations WHERE reservation_id = ?1",
                [[0x88; 32].as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(targets_after_content, 2, "settled content pins its sealing key as a target");
        assert_eq!(entries_after_content, 3, "both live wraps are now reserved");
    }

    #[test]
    fn replay_reconstructs_the_exact_acknowledged_receipt_from_the_manifest() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let first_ticket = ticket(&conn, account, DeviceRole::Member);
        let (first_ed, first_x) = joiner_keys();
        let first = EnrollmentRequest {
            nonce: first_ticket.nonce,
            expected_account: account,
            ed25519_pubkey: first_ed,
            x25519_pubkey: first_x,
            transport_node_id: [8; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (original, _) = redeem_invite(&conn, first.clone(), [8; 32], &|| NOW + 1).unwrap();

        // A second enrollment advances the account snapshot AFTER the first receipt was stored.
        let second_ticket = ticket(&conn, account, DeviceRole::Member);
        let (second_ed, second_x) = joiner_keys();
        let second = EnrollmentRequest {
            nonce: second_ticket.nonce,
            expected_account: account,
            ed25519_pubkey: second_ed,
            x25519_pubkey: second_x,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let _ = redeem_invite(&conn, second, [9; 32], &|| NOW + 2).unwrap();

        // Replaying the first request reconstructs the EXACT acknowledged receipt from the hash
        // manifest — never a superset the joiner's measured capacity might not fit, and never a
        // second stored copy of the bootstrap bytes.
        let (replayed, _) = redeem_invite(&conn, first, [8; 32], &|| NOW + 3).unwrap();
        assert_eq!(replayed, original);
        let duplicated: bool = conn
            .query_row(
                "SELECT receipt_bytes IS NOT NULL FROM sync_invites WHERE nonce = ?1",
                [first_ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!duplicated, "new redemptions persist only the DeviceAdd plus the hash manifest");

        // The reconstructed receipt adopts cleanly on the original joiner.
        let joiner_db = db();
        let _ = rag_rat_oplog::local_device(&joiner_db, NOW).unwrap();
        let genesis_hash = rag_rat_oplog::verify_enrollment_device_add(
            &replayed.account_entries,
            account,
            replayed.device_add_hash,
            &replayed.device_add_signed,
            first_ed,
            first_x,
        )
        .unwrap();
        let fingerprint = DeviceFingerprint::from_bytes(Sha256::digest(first_ed).into());
        rag_rat_oplog::adopt_enrollment_bootstrap(&joiner_db, rag_rat_oplog::EnrollmentBootstrap {
            account_entries: &replayed.account_entries,
            account_id: account,
            genesis_hash,
            device_fingerprint: fingerprint,
            device_add_hash: replayed.device_add_hash,
            now_ms: NOW + 3,
        })
        .unwrap();
    }

    #[test]
    fn redemption_rejects_joiner_candidates_absent_from_the_owner_snapshot() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        // The joiner claims a candidate the owner's authenticated snapshot does not hold — a
        // competing branch or a false claim. Adoption would refold the union, so redemption
        // refuses BEFORE consuming the nonce (and before releasing the reservation).
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: vec![[0xee; 32]],
        };
        assert!(matches!(
            redeem_invite(&conn, request, [9; 32], &|| NOW + 1),
            Err(InviteError::HeldStateConflict)
        ));
        let unused: bool = conn
            .query_row(
                "SELECT used_at_ms IS NULL FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(unused, "an unreconciled held-state claim must not consume the nonce");
        let reservation: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM account_candidate_reservations WHERE reservation_id = ?1)",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(reservation, "the rolled-back redemption restores the invite's reservation");
    }

    #[test]
    fn a_replayed_receipt_ignores_the_declared_budget() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let mut request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
        // The retry declares zero headroom: replay returns the stored receipt regardless — the
        // capacity check gates only the one-time consume.
        request.budget = EnrollmentBudget {
            account_entries_remaining: 0,
            account_bytes_remaining: 0,
            global_entries_remaining: 0,
            global_bytes_remaining: 0,
        };
        let (replayed, _) = redeem_invite(&conn, request, [9; 32], &|| NOW + 2).unwrap();
        assert_eq!(replayed, receipt);
    }

    #[tokio::test]
    async fn joiner_capacity_refusal_travels_the_wire() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: EnrollmentBudget { account_entries_remaining: 0, ..generous_budget() },
            held_entry_hashes: Vec::new(),
        };
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
        let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
        let (dial, accept) = tokio::join!(
            run_enrollment_dialer(&mut dial_recv, &mut dial_send, account, &request),
            run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
        );
        assert!(matches!(dial, Err(InviteError::JoinerCapacity)));
        assert!(matches!(
            accept,
            Ok(EnrollmentAcceptorOutcome::Refused(InviteError::JoinerCapacity))
        ));
    }

    #[test]
    fn replay_is_refused_once_the_enrolled_device_is_removed() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::ReadOnly);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
        // While the acknowledged DeviceAdd is roster-effective, the exact request replays.
        let (replayed, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2).unwrap();
        assert_eq!(replayed, receipt);
        // The owner removes the device inside the replay window: the stored bootstrap and its
        // stream-key wraps must not be released again.
        conn.execute(
            "UPDATE account_roster_history SET closed_at = ?2 WHERE roster_ref = ?1",
            params![receipt.device_add_hash.as_slice(), NOW + 3],
        )
        .unwrap();
        assert!(matches!(
            redeem_invite(&conn, request, [9; 32], &|| NOW + 3),
            Err(InviteError::Revoked)
        ));
    }

    #[test]
    fn redemption_replays_only_the_same_request_and_uses_the_server_side_role() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::ReadOnly);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (receipt, catch_up) =
            redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
        assert_eq!(catch_up.authored.len(), 0);
        let role: String = conn
            .query_row(
                "SELECT role FROM account_roster_history
                 WHERE account_id = ?1 AND roster_ref = ?2",
                params![account.to_bytes().as_slice(), receipt.device_add_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(role, "read_only");
        let (response_account, response_hash) =
            rag_rat_oplog::account_entry_ref(&receipt.device_add_signed).unwrap();
        assert_eq!(response_account, account);
        assert_eq!(response_hash, receipt.device_add_hash);
        assert!(
            receipt.account_entries.len() >= 2,
            "the receipt bootstraps genesis plus the DeviceAdd"
        );
        verify_enrollment_device_add(
            &receipt.account_entries,
            account,
            receipt.device_add_hash,
            &receipt.device_add_signed,
            ed25519_pubkey,
            x25519_pubkey,
        )
        .unwrap();
        assert!(
            verify_enrollment_device_add(
                &receipt.account_entries,
                AccountId::from_bytes([0xff; 32]),
                receipt.device_add_hash,
                &receipt.device_add_signed,
                ed25519_pubkey,
                x25519_pubkey,
            )
            .is_err(),
            "a receipt for another account is not trusted"
        );
        assert!(
            verify_enrollment_device_add(
                &receipt.account_entries,
                account,
                receipt.device_add_hash,
                &receipt.device_add_signed,
                [0xee; 32],
                x25519_pubkey,
            )
            .is_err(),
            "a receipt enrolling different request keys is not trusted"
        );
        let mut forged_signed = receipt.device_add_signed.clone();
        forged_signed[0] ^= 1;
        assert!(
            verify_enrollment_device_add(
                &receipt.account_entries,
                account,
                receipt.device_add_hash,
                &forged_signed,
                ed25519_pubkey,
                x25519_pubkey,
            )
            .is_err(),
            "the acknowledged signed bytes must be the verified bootstrap entry"
        );

        let joiner = db();
        let genesis_hash = verify_enrollment_device_add(
            &receipt.account_entries,
            account,
            receipt.device_add_hash,
            &receipt.device_add_signed,
            ed25519_pubkey,
            x25519_pubkey,
        )
        .unwrap();
        let joiner_fingerprint =
            DeviceFingerprint::from_bytes(Sha256::digest(ed25519_pubkey).into());
        rag_rat_oplog::adopt_enrollment_bootstrap(&joiner, rag_rat_oplog::EnrollmentBootstrap {
            account_entries: &receipt.account_entries,
            account_id: account,
            genesis_hash,
            device_fingerprint: joiner_fingerprint,
            device_add_hash: receipt.device_add_hash,
            now_ms: NOW + 2,
        })
        .unwrap();
        assert_eq!(rag_rat_oplog::read_local_account(&joiner).unwrap(), Some(account));
        let folded_role: String = joiner
            .query_row(
                "SELECT role FROM account_roster_history
                 WHERE account_id = ?1 AND roster_ref = ?2 AND closed_at IS NULL",
                params![account.to_bytes().as_slice(), receipt.device_add_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            folded_role, "read_only",
            "bootstrap ingestion makes the joiner roster-effective before closed sync"
        );

        let interrupted_joiner = db();
        let mut invalid_bootstrap = receipt.account_entries.clone();
        invalid_bootstrap.push(vec![0xff]);
        assert!(
            rag_rat_oplog::adopt_enrollment_bootstrap(
                &interrupted_joiner,
                rag_rat_oplog::EnrollmentBootstrap {
                    account_entries: &invalid_bootstrap,
                    account_id: account,
                    genesis_hash,
                    device_fingerprint: joiner_fingerprint,
                    device_add_hash: receipt.device_add_hash,
                    now_ms: NOW + 2,
                },
            )
            .is_err()
        );
        let retained_entries: i64 = interrupted_joiner
            .query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained_entries, 0, "a later bootstrap failure rolls back earlier entries");
        assert_eq!(
            rag_rat_oplog::read_local_account(&interrupted_joiner).unwrap(),
            None,
            "a failed bootstrap cannot publish the local-account pointer"
        );
        let (replayed, replay_catch_up) =
            redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 2).unwrap();
        assert_eq!(replayed, receipt);
        assert!(replay_catch_up.authored.is_empty());
        assert!(replay_catch_up.already_covered.is_empty());

        let mut different = request;
        different.x25519_pubkey = [7; 32];
        assert!(matches!(
            redeem_invite(&conn, different, [9; 32], &|| NOW + 2),
            Err(InviteError::Used)
        ));
    }

    #[test]
    fn redemption_rechecks_expiry_with_the_post_lock_clock() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        // The optimistic pre-lock read sees the invite still valid; the writer-lock wait then
        // crosses the TTL, so the in-tx check must refuse with the refreshed clock instead of
        // consuming against the stale pre-wait one.
        let reads = AtomicI64::new(0);
        let clock = || {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 { NOW + 1 } else { ticket.expires_at_ms }
        };
        assert!(matches!(
            redeem_invite(&conn, request.clone(), [9; 32], &clock),
            Err(InviteError::Expired)
        ));
        let used: Option<i64> = conn
            .query_row(
                "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(used, None, "an expiry-crossing redemption must not consume the nonce");
        // The still-valid invite redeems normally afterwards.
        let _ = redeem_invite(&conn, request, [9; 32], &|| NOW + 2).unwrap();
    }

    #[test]
    fn replay_within_the_retention_window_survives_the_post_lock_clock() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1).unwrap();
        // A retry arriving before expiry but checked after the TTL (and within the 24h replay
        // window) must still replay the stored receipt: the in-tx replay check runs before the
        // refreshed expiry check.
        let reads = AtomicI64::new(0);
        let clock = || {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                NOW + 2
            } else {
                ticket.expires_at_ms + 60_000
            }
        };
        let (replayed, _) = redeem_invite(&conn, request, [9; 32], &clock).unwrap();
        assert_eq!(replayed, receipt);
    }

    #[test]
    fn expiry_and_node_binding_fail_before_consumption() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        assert!(matches!(
            redeem_invite(&conn, request.clone(), [8; 32], &|| NOW + 1),
            Err(InviteError::WrongNode)
        ));
        assert!(matches!(
            redeem_invite(&conn, request, [9; 32], &|| ticket.expires_at_ms),
            Err(InviteError::Expired)
        ));
        let used: Option<i64> = conn
            .query_row(
                "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(used, None);
    }

    #[test]
    fn expected_account_mismatch_fails_before_authoring_or_consumption() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let mut request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: AccountId::from_bytes([0xa5; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let roster_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_roster_history", [], |row| row.get(0))
            .unwrap();

        assert!(matches!(
            redeem_invite(&conn, request.clone(), [9; 32], &|| NOW + 1),
            Err(InviteError::AccountMismatch)
        ));
        let (used_at_ms, roster_after): (Option<i64>, i64) = (
            conn.query_row(
                "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM account_roster_history", [], |row| row.get(0))
                .unwrap(),
        );
        assert_eq!(used_at_ms, None);
        assert_eq!(roster_after, roster_before);

        request.expected_account = account;
        redeem_invite(&conn, request, [9; 32], &|| NOW + 2)
            .expect("the account-mismatch refusal leaves the invite redeemable");
    }

    #[test]
    fn receipt_replay_is_retained_for_one_day_then_pruned() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let used_at_ms = NOW + 1;
        let (receipt, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| used_at_ms).unwrap();
        let (replayed, _) = redeem_invite(&conn, request.clone(), [9; 32], &|| {
            used_at_ms + RECEIPT_REPLAY_RETENTION_MS - 1
        })
        .unwrap();
        assert_eq!(replayed, receipt);

        assert!(matches!(
            redeem_invite(&conn, request, [9; 32], &|| used_at_ms + RECEIPT_REPLAY_RETENTION_MS),
            Err(InviteError::Used)
        ));
        let retained: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_invites WHERE nonce = ?1)",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!retained, "the expired multi-megabyte receipt row is deleted");
    }

    #[test]
    fn failed_device_add_rolls_back_invite_consumption() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let bad = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey: [0; 32],
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        assert!(matches!(
            redeem_invite(&conn, bad, [9; 32], &|| NOW + 1),
            Err(InviteError::Storage(_))
        ));
        let good = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        redeem_invite(&conn, good, [9; 32], &|| NOW + 2)
            .expect("the failed transaction must leave the invite redeemable");
    }

    #[test]
    fn account_entry_sized_receipt_fits_the_bootstrap_frame() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let label = "x".repeat(8 * 1024);
        let ticket = mint_invite(&conn, InviteSpec {
            account_id: account,
            inviter_node_id: crate::endpoint::node_id_from_secret([2; 32]),
            relay_url: "https://relay.example".into(),
            role: DeviceRole::Member,
            label: Some(&label),
            now_ms: &|| NOW,
            ttl: Duration::from_secs(60),
        })
        .unwrap();
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };

        redeem_invite(&conn, request, [9; 32], &|| NOW + 1)
            .expect("the enrollment frame cap covers account-entry-sized receipts");
        let used: Option<i64> = conn
            .query_row(
                "SELECT used_at_ms FROM sync_invites WHERE nonce = ?1",
                [ticket.nonce.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(used, Some(NOW + 1));
    }

    #[test]
    fn simultaneous_redemption_has_exactly_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enrollment.sqlite");
        let conn = Connection::open(&path).unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        drop(conn);

        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for transport_node_id in [[8; 32], [9; 32]] {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            let nonce = ticket.nonce;
            handles.push(std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
                let request = EnrollmentRequest {
                    nonce,
                    expected_account: account,
                    ed25519_pubkey,
                    x25519_pubkey,
                    transport_node_id,
                    budget: generous_budget(),
                    held_entry_hashes: Vec::new(),
                };
                barrier.wait();
                redeem_invite(&conn, request, transport_node_id, &|| NOW + 1)
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results.iter().filter(|result| matches!(result, Err(InviteError::Used))).count(),
            1
        );

        let conn = Connection::open(path).unwrap();
        let additions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_roster_history
                 WHERE account_id = ?1",
                [account.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        // AccountGenesis contributes the owner row; exactly one DeviceAdd may join it.
        assert_eq!(additions, 2);
    }

    #[tokio::test]
    async fn duplex_exchange_returns_the_authored_device_add() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
        let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
        let (dial, accept) = tokio::join!(
            run_enrollment_dialer(&mut dial_recv, &mut dial_send, account, &request),
            run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
        );
        let dial = dial.unwrap();
        let EnrollmentAcceptorOutcome::Enrolled(accept, _) = accept.unwrap() else {
            panic!("valid enrollment must succeed");
        };
        assert_eq!(dial, accept);
    }

    #[tokio::test]
    async fn a_slow_but_progressing_response_completes() {
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: [42; 32],
            expected_account: AccountId::from_bytes([1; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(1024);
        let (mut accept_send, mut dial_recv) = tokio::io::duplex(1024);
        // Drip the refusal in small writes with gaps — comfortably inside each per-chunk window,
        // past what a whole-exchange deadline of the same size would tolerate.
        let server = async move {
            let mut prefix = [0u8; 4];
            accept_recv.read_exact(&mut prefix).await.unwrap();
            let mut req = vec![0u8; u32::from_be_bytes(prefix) as usize];
            accept_recv.read_exact(&mut req).await.unwrap();
            let response = EnrollmentResponse::Refused(RefusalCode::Unknown).encode();
            let framed = [(response.len() as u32).to_be_bytes().as_slice(), &response].concat();
            for piece in framed.chunks(8) {
                accept_send.write_all(piece).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (dial, _) = tokio::join!(
            run_enrollment_dialer_with_progress(
                &mut dial_recv,
                &mut dial_send,
                AccountId::from_bytes([1; 32]),
                &request,
                Duration::from_millis(500),
            ),
            server,
        );
        assert!(matches!(dial, Err(InviteError::Unknown)));
    }

    #[tokio::test]
    async fn a_stalled_response_times_out_within_one_window() {
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: [42; 32],
            expected_account: AccountId::from_bytes([1; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(1024);
        let (mut accept_send, mut dial_recv) = tokio::io::duplex(1024);
        let server = tokio::spawn(async move {
            let mut prefix = [0u8; 4];
            accept_recv.read_exact(&mut prefix).await.unwrap();
            let mut req = vec![0u8; u32::from_be_bytes(prefix) as usize];
            accept_recv.read_exact(&mut req).await.unwrap();
            // A valid prefix and half the body, then silence forever.
            accept_send.write_all(&128u32.to_be_bytes()).await.unwrap();
            accept_send.write_all(&[0u8; 64]).await.unwrap();
            accept_send.flush().await.unwrap();
            std::future::pending::<()>().await;
        });
        let start = std::time::Instant::now();
        let dial = run_enrollment_dialer_with_progress(
            &mut dial_recv,
            &mut dial_send,
            AccountId::from_bytes([1; 32]),
            &request,
            Duration::from_millis(100),
        )
        .await;
        server.abort();
        assert!(
            matches!(dial, Err(InviteError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut),
            "a stalled frame must die with a timeout: {dial:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(2), "the stall dies in one window");
    }

    #[tokio::test]
    async fn a_stalled_reader_times_out_the_frame_write() {
        let (mut send, _recv) = tokio::io::duplex(64);
        let body = vec![0u8; 256 * 1024];
        let result = write_blob(
            &mut send,
            &body,
            MAX_ENROLL_RESPONSE_FRAME,
            "response",
            Duration::from_millis(100),
        )
        .await;
        assert!(
            matches!(result, Err(InviteError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut),
            "a back-pressured write must die with a timeout: {result:?}"
        );
    }

    #[tokio::test]
    async fn dialer_acks_the_response_once_decoded() {
        let conn = db();
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: [42; 32],
            expected_account: AccountId::from_bytes([1; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
        let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
        let (dial, accept) = tokio::join!(
            run_enrollment_dialer(
                &mut dial_recv,
                &mut dial_send,
                AccountId::from_bytes([1; 32]),
                &request,
            ),
            run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
        );
        assert!(matches!(dial, Err(InviteError::Unknown)));
        assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::Refused(_))));
        let mut ack = [0u8; 1];
        accept_recv.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack, [RESPONSE_ACK], "the dialer acks even a refused response");
    }

    #[tokio::test]
    async fn a_stalled_response_ack_is_bounded_and_best_effort() {
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: [42; 32],
            expected_account: AccountId::from_bytes([1; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let response = EnrollmentResponse::Refused(RefusalCode::Unknown).encode();
        let framed = [(response.len() as u32).to_be_bytes().as_slice(), &response].concat();
        let (mut response_send, mut dial_recv) = tokio::io::duplex(4096);
        response_send.write_all(&framed).await.unwrap();
        let mut dial_send = StallAfterBytes { remaining: 4 + request.encode().len() };

        let start = std::time::Instant::now();
        let dial = run_enrollment_dialer_with_progress(
            &mut dial_recv,
            &mut dial_send,
            AccountId::from_bytes([1; 32]),
            &request,
            Duration::from_millis(100),
        )
        .await;

        assert!(matches!(dial, Err(InviteError::Unknown)));
        assert!(start.elapsed() < Duration::from_secs(2), "the stalled ack is bounded");
    }

    #[tokio::test]
    async fn duplex_exchange_returns_a_semantic_refusal() {
        let conn = db();
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: [42; 32],
            expected_account: AccountId::from_bytes([1; 32]),
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
        let (mut accept_send, mut dial_recv) = tokio::io::duplex(4096);
        let (dial, accept) = tokio::join!(
            run_enrollment_dialer(
                &mut dial_recv,
                &mut dial_send,
                AccountId::from_bytes([1; 32]),
                &request,
            ),
            run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || NOW + 1,),
        );
        assert!(matches!(dial, Err(InviteError::Unknown)));
        assert!(matches!(accept, Ok(EnrollmentAcceptorOutcome::Refused(InviteError::Unknown))));
    }

    #[tokio::test]
    async fn acceptor_checks_expiry_after_receiving_the_request() {
        let conn = db();
        let account = rag_rat_oplog::local_account(&conn, NOW).unwrap();
        let ticket = ticket(&conn, account, DeviceRole::Member);
        let (ed25519_pubkey, x25519_pubkey) = joiner_keys();
        let request = EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account,
            ed25519_pubkey,
            x25519_pubkey,
            transport_node_id: [9; 32],
            budget: generous_budget(),
            held_entry_hashes: Vec::new(),
        };
        let clock = AtomicI64::new(NOW + 1);
        let (mut dial_send, mut accept_recv) = tokio::io::duplex(4096);
        let (mut accept_send, _dial_recv) = tokio::io::duplex(4096);
        let send_request = async {
            write_blob(
                &mut dial_send,
                &request.encode(),
                MAX_ENROLL_REQUEST_FRAME,
                "request",
                ENROLL_PROGRESS_TIMEOUT,
            )
            .await
            .unwrap();
            clock.store(ticket.expires_at_ms, Ordering::SeqCst);
        };
        let accept =
            run_enrollment_acceptor(&mut accept_recv, &mut accept_send, &conn, [9; 32], || {
                clock.load(Ordering::SeqCst)
            });

        let (_, result) = tokio::join!(send_request, accept);
        assert!(matches!(result, Ok(EnrollmentAcceptorOutcome::Refused(InviteError::Expired))));
    }

    #[tokio::test]
    async fn request_length_is_capped_before_allocating_its_body() {
        let (mut send, mut recv) = tokio::io::duplex(16);
        send.write_all(&(MAX_ENROLL_REQUEST_FRAME + 1).to_be_bytes()).await.unwrap();
        let error =
            read_blob(&mut recv, MAX_ENROLL_REQUEST_FRAME, "request", ENROLL_PROGRESS_TIMEOUT)
                .await
                .unwrap_err();
        assert!(
            matches!(error, InviteError::Malformed(message) if message.contains("request")),
            "the unauthenticated request uses its small cap"
        );
    }
}
