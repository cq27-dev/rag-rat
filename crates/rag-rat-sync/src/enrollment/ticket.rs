//! The invite ticket: routing data plus the one-time nonce, and its paste-able encoding.

use std::str::FromStr;

use iroh::{EndpointId, RelayUrl};
use minicbor::{Decoder, Encoder};
use rag_rat_oplog::AccountId;

use super::error::InviteError;
use super::wire::{decode, ensure_consumed, exact_array, exact_str, fixed32};

// `/2` added the kind discriminator (arity 6 -> 7) when the pairing ticket and the writer
// invite merged into one struct; a `/1` binary rejects the new domain legibly.
const TICKET_DOMAIN: &str = "rag-rat/invite-ticket/2";

const MAX_RELAY_URL_BYTES: usize = 2048;

/// What an invite ticket is FOR — a kind discriminator only, never authority. The server-side
/// `sync_invites` row bound to the nonce carries the authoritative role (an enrollment
/// `DeviceRole`, or the writer-grant marker), exactly as before: a tampered kind changes which
/// command accepts the paste, not what the owner authors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteTicketKind {
    /// Device pairing (`sync init` mints, `sync join` redeems).
    Pairing,
    /// A cross-account Writer grant (`sync invite-writer` mints, `sync contribute` redeems).
    Writer,
}

impl InviteTicketKind {
    fn wire_tag(self) -> u8 {
        match self {
            Self::Pairing => 0,
            Self::Writer => 1,
        }
    }

    fn from_wire_tag(tag: u8) -> Result<Self, InviteError> {
        match tag {
            0 => Ok(Self::Pairing),
            1 => Ok(Self::Writer),
            other => Err(InviteError::Malformed(format!("unknown invite ticket kind {other}"))),
        }
    }
}

/// The one human-transferable ticket both invite flows print and redeem: pairing and writer
/// invites share the struct (and the `ragratinvite` string prefix), split by [`InviteTicketKind`]
/// — so pasting the wrong kind is diagnosed from the DECODED ticket ("that is a pairing ticket —
/// use `sync join`"), not three layers down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteTicket {
    pub kind: InviteTicketKind,
    pub account_id: AccountId,
    pub inviter_node_id: [u8; 32],
    pub relay_url: String,
    pub nonce: [u8; 32],
    pub expires_at_ms: i64,
}

impl InviteTicket {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(7).expect("owned Vec");
        enc.str(TICKET_DOMAIN).expect("owned Vec");
        enc.u8(self.kind.wire_tag()).expect("owned Vec");
        enc.bytes(&self.account_id.to_bytes()).expect("owned Vec");
        enc.bytes(&self.inviter_node_id).expect("owned Vec");
        enc.str(&self.relay_url).expect("owned Vec");
        enc.bytes(&self.nonce).expect("owned Vec");
        enc.i64(self.expires_at_ms).expect("owned Vec");
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        exact_array(&mut dec, 7, "ticket")?;
        exact_str(&mut dec, TICKET_DOMAIN, "ticket domain")?;
        let kind = InviteTicketKind::from_wire_tag(dec.u8().map_err(decode)?)?;
        let account_id = AccountId::from_bytes(fixed32(dec.bytes().map_err(decode)?, "account")?);
        let inviter_node_id = fixed32(dec.bytes().map_err(decode)?, "node id")?;
        let relay_url = dec.str().map_err(decode)?.to_owned();
        validate_enrollment_route(&inviter_node_id, &relay_url)?;
        let nonce = fixed32(dec.bytes().map_err(decode)?, "nonce")?;
        let expires_at_ms = dec.i64().map_err(decode)?;
        ensure_consumed(&dec, bytes)?;
        let ticket = Self { kind, account_id, inviter_node_id, relay_url, nonce, expires_at_ms };
        if ticket.encode() != bytes {
            return Err(InviteError::Malformed("ticket is not canonical CBOR".into()));
        }
        Ok(ticket)
    }

    /// The human-transferable ticket string — [`iroh_tickets::Ticket`]'s canonical form: the
    /// `ragratinvite` kind prefix plus lowercase base32 of the canonical CBOR.
    pub fn to_ticket_string(&self) -> String {
        iroh_tickets::Ticket::encode_string(self)
    }

    /// Parse a ticket string produced by [`to_ticket_string`]. Surrounding whitespace is
    /// tolerated (a pasted line often carries it); the prefix, base32, and canonical-CBOR shape
    /// are all validated, so a mistyped or truncated ticket is rejected rather than half-decoded.
    pub fn from_ticket_string(s: &str) -> Result<Self, InviteError> {
        match iroh_tickets::Ticket::decode_string(s.trim()) {
            Ok(ticket) => Ok(ticket),
            Err(iroh_tickets::ParseError::Kind { .. }) => Err(InviteError::Malformed(format!(
                "an invite ticket starts with `{TICKET_KIND_PREFIX}`"
            ))),
            Err(error) => Err(InviteError::Malformed(format!("invalid invite ticket: {error}"))),
        }
    }

    /// Redeem-side kind check: the wrong paste names the command that accepts it.
    pub fn expect_kind(&self, expected: InviteTicketKind) -> Result<(), InviteError> {
        if self.kind == expected {
            return Ok(());
        }
        Err(InviteError::Malformed(match self.kind {
            InviteTicketKind::Pairing =>
                "that is a device-pairing ticket — redeem it with `rag-rat sync join`".into(),
            InviteTicketKind::Writer =>
                "that is a writer invite — redeem it with `rag-rat sync contribute`".into(),
        }))
    }
}

/// The [`iroh_tickets::Ticket`] string prefix. One prefix for both kinds on purpose: the kind is
/// in the payload, so a wrong-kind paste decodes far enough to say which command wants it.
const TICKET_KIND_PREFIX: &str = "ragratinvite";

impl iroh_tickets::Ticket for InviteTicket {
    const KIND: &'static str = TICKET_KIND_PREFIX;

    fn encode_bytes(&self) -> Vec<u8> {
        self.encode()
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, iroh_tickets::ParseError> {
        Self::decode(bytes).map_err(|_| {
            iroh_tickets::ParseError::verification_failed(
                "ticket bytes are not a canonical rag-rat invite",
            )
        })
    }
}

pub(super) fn validate_enrollment_route(
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
