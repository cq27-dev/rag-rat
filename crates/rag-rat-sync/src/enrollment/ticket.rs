//! The invite ticket: routing data plus the one-time nonce, and its paste-able encoding.

use std::str::FromStr;

use iroh::{EndpointId, RelayUrl};
use minicbor::{Decoder, Encoder};
use rag_rat_oplog::AccountId;

use super::error::InviteError;
use super::wire::{decode, ensure_consumed, fixed32};

// `/2` added the kind discriminator (arity 6 -> 7) when the pairing ticket and the writer
// invite merged into one struct; a `/1` binary rejects the new domain legibly. `/3` appends the
// checkpoint digest (arity 8). `/4` changes no byte of the layout: it fences a change in what the
// JOINER accepts. From `/4` any owner may mint, and the DeviceAdd a redemption authors is signed by
// whichever owner redeemed it. A `/3` joiner verifies only a founder-signed DeviceAdd, and it would
// refuse the receipt after the owner had already spent the nonce — identically on every replay. A
// joiner that cannot decode the ticket never dials, so it is told to upgrade before any nonce
// exists.
//
// The fence runs one way only. This binary still DECODES `/3`: the layout is identical, and every
// `/3` ticket was minted under the founder-only gate, so its DeviceAdd is one this verifier accepts
// too. Refusing it would strand a joiner that upgraded while holding a `/3` ticket whose enrollment
// the owner had already committed — its lost response is recovered only by replaying that ticket,
// and a fresh one cannot re-add a device the owner already enrolled.
//
// A domain bump rather than the additive-by-omission trick `StreamSpecV2` uses: that exists there
// because `stream_id = sha256(spec)` and changing bytes would move identities, while a ticket is
// content-addressed by nothing and is TTL'd and single-use, so a revision needs decoding only as
// long as its tickets can still be outstanding — which is why `/3` still decodes. And for a
// SECURITY field silent compatibility is the wrong default — an old binary that ignored the digest
// would enrol into a pinned account without installing its pin. Legible rejection is correct.
pub(super) const TICKET_DOMAIN: &str = "rag-rat/invite-ticket/4";
/// Shared by every revision of the domain, so a ticket from another release is told apart from
/// arbitrary bytes that merely decoded as a string in that position.
pub(super) const TICKET_DOMAIN_STEM: &str = "rag-rat/invite-ticket/";
/// This binary's revision of the ticket format — the one it mints.
pub(super) const TICKET_VERSION: u32 = 4;
/// The oldest revision this binary still decodes; `/3` shares `/4`'s layout exactly.
pub(super) const OLDEST_DECODED_VERSION: u32 = 3;
pub(super) const OLDEST_DECODED_DOMAIN: &str = "rag-rat/invite-ticket/3";

/// Which way the version skews, so the operator is told the action that can actually work.
///
/// Direction matters and the common case is NEWER, not older: the owner runs `sync init`, so the
/// minting side upgrades first. Telling that operator to ask for a re-mint sends them after the one
/// thing that cannot help — the owner would re-mint the same unreadable revision forever.
fn version_skew(domain: &str) -> InviteError {
    let Some(revision) =
        domain.strip_prefix(TICKET_DOMAIN_STEM).and_then(|v| v.parse::<u32>().ok())
    else {
        return InviteError::Malformed("ticket domain mismatch".into());
    };
    if revision < OLDEST_DECODED_VERSION {
        InviteError::TicketVersionSkew(
            "this ticket was minted by an older rag-rat — ask the owner to re-mint it",
        )
    } else if revision > TICKET_VERSION {
        InviteError::TicketVersionSkew(
            "this ticket was minted by a newer rag-rat — upgrade rag-rat on this machine",
        )
    } else {
        // `04`, `+3`: a revision this binary decodes, spelled in a way nothing mints. Corrupt, not
        // skewed — and blaming a release the operator cannot change sends them nowhere.
        InviteError::Malformed("ticket domain mismatch".into())
    }
}

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
    /// The digest of the control checkpoint the inviting account is pinned to, or `None` when it
    /// is unpinned. This is the pin's trusted channel: the ticket is what an operator carries
    /// between machines by hand, so the digest travels here while the proof travels over the
    /// enrollment connection, and the joiner verifies one against the other.
    ///
    /// Always `None` on a [`InviteTicketKind::Writer`] ticket: a writer grant is cross-account and
    /// pins nothing. Enforced at decode AND in [`Self::expect_kind`], which every writer consumer
    /// passes through — the fields are public, so an in-process caller can build a writer ticket
    /// carrying a digest without ever round-tripping through decode. This is not a type-level
    /// guarantee; making it unrepresentable means moving the field into the `Pairing` variant.
    pub checkpoint_digest: Option<[u8; 32]>,
}

impl InviteTicket {
    /// The canonical bytes, always under THIS binary's revision. A ticket decoded from `/3`
    /// therefore re-encodes as `/4`: the struct does not remember which revision it came from.
    // ponytail: nothing re-emits a decoded ticket — the one production `to_ticket_string` prints a
    // freshly minted one — so relaying a decoded `/3` ticket to a `/3` reader is unsupported. Carry
    // the decoded revision in the struct if a relay path ever appears.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_as(TICKET_DOMAIN)
    }

    /// The canonical bytes under `domain`. Only a revision sharing this layout may be named here;
    /// the decoder uses it to check a `/3` ticket's canonicality against its OWN domain, since
    /// re-encoding it as `/4` would make every valid `/3` ticket look non-canonical.
    fn encode_as(&self, domain: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        enc.array(8).expect("owned Vec");
        enc.str(domain).expect("owned Vec");
        enc.u8(self.kind.wire_tag()).expect("owned Vec");
        enc.bytes(&self.account_id.to_bytes()).expect("owned Vec");
        enc.bytes(&self.inviter_node_id).expect("owned Vec");
        enc.str(&self.relay_url).expect("owned Vec");
        enc.bytes(&self.nonce).expect("owned Vec");
        enc.i64(self.expires_at_ms).expect("owned Vec");
        match self.checkpoint_digest {
            Some(digest) => enc.bytes(&digest).expect("owned Vec"),
            None => enc.null().expect("owned Vec"),
        };
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InviteError> {
        let mut dec = Decoder::new(bytes);
        // Read the array header WITHOUT asserting its length, and diagnose the domain first: every
        // revision of this format has its own arity (`/1` was 6, `/2` was 7, `/3` and `/4` are 8),
        // so asserting arity up front means a genuinely older ticket dies on arity and
        // never reaches the message written for it. Which is the only input that message
        // exists to serve.
        let arity = dec.array().map_err(decode)?;
        let domain = dec.str().map_err(decode)?;
        if domain != TICKET_DOMAIN && domain != OLDEST_DECODED_DOMAIN {
            return Err(version_skew(domain));
        }
        if arity != Some(8) {
            return Err(InviteError::Malformed("ticket arity".into()));
        }
        let kind = InviteTicketKind::from_wire_tag(dec.u8().map_err(decode)?)?;
        let account_id = AccountId::from_bytes(fixed32(dec.bytes().map_err(decode)?, "account")?);
        let inviter_node_id = fixed32(dec.bytes().map_err(decode)?, "node id")?;
        let relay_url = dec.str().map_err(decode)?.to_owned();
        validate_enrollment_route(&inviter_node_id, &relay_url)?;
        let nonce = fixed32(dec.bytes().map_err(decode)?, "nonce")?;
        let expires_at_ms = dec.i64().map_err(decode)?;
        let checkpoint_digest = match dec.datatype().map_err(decode)? {
            minicbor::data::Type::Null => {
                dec.null().map_err(decode)?;
                None
            },
            _ => Some(fixed32(dec.bytes().map_err(decode)?, "checkpoint digest")?),
        };
        if kind == InviteTicketKind::Writer && checkpoint_digest.is_some() {
            return Err(InviteError::Malformed(
                "a writer invite pins no checkpoint and must not carry a digest".into(),
            ));
        }
        ensure_consumed(&dec, bytes)?;
        let ticket = Self {
            kind,
            account_id,
            inviter_node_id,
            relay_url,
            nonce,
            expires_at_ms,
            checkpoint_digest,
        };
        if ticket.encode_as(domain) != bytes {
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
            // Unwrap the verification message rather than nesting it: the actionable sentence was
            // arriving four prefixes deep by the time the CLI printed it.
            Err(iroh_tickets::ParseError::Verify { message, .. }) =>
                Err(InviteError::Malformed(message.to_string())),
            Err(error) => Err(InviteError::Malformed(format!("invalid invite ticket: {error}"))),
        }
    }

    /// Redeem-side kind check: the wrong paste names the command that accepts it.
    pub fn expect_kind(&self, expected: InviteTicketKind) -> Result<(), InviteError> {
        // A self-invalid ticket never reaches a redemption path: decode refuses this shape, and an
        // in-process caller that skipped decode is refused here, at the seam every consumer uses.
        if self.kind == InviteTicketKind::Writer && self.checkpoint_digest.is_some() {
            return Err(InviteError::Malformed(
                "a writer invite pins no checkpoint and must not carry a digest".into(),
            ));
        }
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
pub const TICKET_KIND_PREFIX: &str = "ragratinvite";

impl iroh_tickets::Ticket for InviteTicket {
    const KIND: &'static str = TICKET_KIND_PREFIX;

    fn encode_bytes(&self) -> Vec<u8> {
        self.encode()
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, iroh_tickets::ParseError> {
        // Carry the reason through. Flattening every decode failure into one string made "this is
        // from an older release" and "you pasted junk" byte-identical where operators stand.
        // `ParseError::Verify` carries only a `&'static str`, so the reason cannot be threaded
        // through verbatim — but the cases an operator can ACT on are worth telling apart from
        // "you pasted junk", which is the whole value of bumping the domain. Selected by MATCHING
        // the variant: keying it off formatted text reverts silently when the text is reworded.
        Self::decode(bytes).map_err(|error| {
            iroh_tickets::ParseError::verification_failed(match error {
                InviteError::TicketVersionSkew(message) => message,
                _ => "ticket bytes are not a canonical rag-rat invite",
            })
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
