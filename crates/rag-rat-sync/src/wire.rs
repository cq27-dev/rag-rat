//! The sync session wire protocol (phase D, #406).
//!
//! One symmetric protocol for device↔device and device↔server: both peers send [`Frame::Hello`]
//! naming the account and every account-log entry hash they already hold, then each streams the
//! entries the other lacks as [`Frame::Entries`] pages, ending with [`Frame::Done`]. A role-ordered
//! [`Frame::Ack`] exchange then proves both receivers consumed their peer's complete stream before
//! the dialer closes the connection. The receiver feeds every entry through the op-log ingest seam,
//! which re-verifies signature, canonicity, and chain continuity from scratch — the sender is never
//! trusted, so a malformed or forged frame is rejected exactly as a malformed local write would be.
//!
//! Frames are canonical CBOR arrays tagged by a leading discriminant, encoded by hand with
//! `minicbor::Encoder` to match the op-log's envelope style (no derive). The byte layout is a
//! frozen wire — a golden-vector test pins it.

use minicbor::{Decoder, Encoder};

/// The ALPN for the ACCOUNT-LOG stream. Versioned: a breaking wire change bumps the suffix so an
/// old peer declines the handshake instead of misreading frames. Bumped to `/4` for the
/// post-binding capability grant: a `/3` sender considers only its own role and can upload when the
/// receiver has granted it read-only access.
pub const SYNC_ALPN: &[u8] = b"rag-rat/sync/4";

/// The ALPN for the `/3` CONTENT stream — the memories themselves (#907). Same frame protocol and
/// same account-level auth phase as [`SYNC_ALPN`]; only the STORE differs (`OplogContentSyncStore`
/// vs `OplogSyncStore`). The stream is discriminated by the ALPN, not a frame field, so the
/// acceptor picks the store from `conn.alpn()` and the account-log wire stays byte-identical. A
/// peer that does not speak this ALPN simply never exchanges content — account-log sync is
/// unaffected.
/// Bumped to `/3` alongside [`SYNC_ALPN`] because content uses the same auth state machine.
pub const CONTENT_SYNC_ALPN: &[u8] = b"rag-rat/content/3";

/// Domain tag committed into every frame's leading array element, so a frame from another protocol
/// (or a truncated one) cannot be mistaken for a valid frame.
const FRAME_DOMAIN: &str = "rag-rat/sync-frame/1";

/// The maximum entries a single [`Frame::Entries`] page carries — a hard cap so one frame can never
/// force an unbounded allocation on the receiver (#406: bounded frames, no amplification).
pub const MAX_ENTRIES_PER_PAGE: usize = 256;

/// The maximum entry hashes a single [`Frame::Hello`] inventory carries. Bounds the hello frame so
/// a peer cannot force an unbounded allocation before any authentication. A sender with more
/// entries than this advertises a bounded subset — still correct (the peer's extra sends are
/// idempotently re-ingested), just less efficient; see `session::bounded_inventory`. For the
/// accounts D targets the cap is never reached.
pub const MAX_HELLO_HASHES: usize = 65_536;

/// The maximum bytes an [`Frame::Auth`] binding carries. A node binding is a small fixed shape
/// (~224 bytes: domain + three 32-byte keys + a timestamp + a 64-byte signature); this is a
/// decode-time sanity bound on that field. The actual PRE-ALLOCATION bound for an unauthenticated
/// peer is the auth phase's frame-level cap, enforced from the length prefix before the body is
/// allocated (`auth::MAX_AUTH_FRAME_BYTES` via `codec::read_frame_within`) — not this, which is
/// checked only after the frame is read.
pub const MAX_AUTH_BINDING_BYTES: usize = 512;

type Hash = [u8; 32];

/// One protocol frame. The discriminant is the second array element (after the domain tag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Authorizes the sender to the peer BEFORE any inventory is revealed (#881): names the account
    /// and carries the sender's signed transport-node ↔ account-device binding. The peer verifies
    /// it against its roster and the connection's authenticated remote node id under its own
    /// admission policy, and reveals nothing (not even account confirmation) until it passes.
    Auth { account_id: Hash, binding: Vec<u8> },
    /// Reports the capability this sender granted the peer after verifying its binding. Each side
    /// intersects the grant it receives with its own authority before the data phase, so stale
    /// local roster state cannot make it upload to a receiver that admitted it read-only.
    AuthGrant { can_push: bool },
    /// Opens the data phase: the account being synced and every account-log entry hash the sender
    /// holds. The peer replies with the entries in ITS store that are not in this set.
    Hello { account_id: Hash, have: Vec<Hash> },
    /// A page of `signed_bytes` the peer lacked. `more` is true when further pages follow.
    Entries { entries: Vec<Vec<u8>>, more: bool },
    /// The sender has streamed every entry the peer lacked. A session ends when both directions are
    /// `Done`, followed by the role-ordered acknowledgement exchange.
    Done,
    /// Confirms that the sender consumed the peer's complete stream through [`Frame::Done`]. The
    /// dialer sends first; the acceptor replies only after reading it, so the dialer can close
    /// after the reply without truncating either direction.
    Ack,
}

mod tag {
    pub const HELLO: u8 = 0;
    pub const ENTRIES: u8 = 1;
    pub const DONE: u8 = 2;
    pub const AUTH: u8 = 3;
    pub const ACK: u8 = 4;
    pub const AUTH_GRANT: u8 = 5;
}

/// A frame that failed to decode. Kept distinct from an I/O error so the session can treat a
/// protocol violation (drop the peer) differently from a transport hiccup.
#[derive(Debug)]
pub enum WireError {
    /// The bytes are not a well-formed frame of this protocol.
    Malformed(String),
    /// A field exceeded its hard cap — a bounded-frame violation, treated as hostile.
    OverCap(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Malformed(m) => write!(f, "malformed sync frame: {m}"),
            WireError::OverCap(m) => write!(f, "sync frame over cap: {m}"),
        }
    }
}

impl std::error::Error for WireError {}

impl Frame {
    /// Encode to canonical CBOR. Infallible for the in-memory shapes the session builds (the caps
    /// are enforced by the builders), so encoding errors are a programmer bug, not a wire
    /// condition.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        // `[domain, tag, ..variant fields]`.
        match self {
            Frame::Auth { account_id, binding } => {
                enc.array(4).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::AUTH).expect(INFALLIBLE);
                enc.bytes(account_id).expect(INFALLIBLE);
                enc.bytes(binding).expect(INFALLIBLE);
            },
            Frame::AuthGrant { can_push } => {
                enc.array(3).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::AUTH_GRANT).expect(INFALLIBLE);
                enc.bool(*can_push).expect(INFALLIBLE);
            },
            Frame::Hello { account_id, have } => {
                enc.array(3).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::HELLO).expect(INFALLIBLE);
                enc.array(2).expect(INFALLIBLE);
                enc.bytes(account_id).expect(INFALLIBLE);
                enc.array(have.len() as u64).expect(INFALLIBLE);
                for h in have {
                    enc.bytes(h).expect(INFALLIBLE);
                }
            },
            Frame::Entries { entries, more } => {
                enc.array(4).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::ENTRIES).expect(INFALLIBLE);
                enc.array(entries.len() as u64).expect(INFALLIBLE);
                for e in entries {
                    enc.bytes(e).expect(INFALLIBLE);
                }
                enc.bool(*more).expect(INFALLIBLE);
            },
            Frame::Done => {
                enc.array(2).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::DONE).expect(INFALLIBLE);
            },
            Frame::Ack => {
                enc.array(2).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::ACK).expect(INFALLIBLE);
            },
        }
        buf
    }

    /// Decode a frame, enforcing the domain tag and every hard cap. A caller treats `Err` as a
    /// protocol violation and drops the peer.
    pub fn decode(bytes: &[u8]) -> Result<Frame, WireError> {
        let mut dec = Decoder::new(bytes);
        let outer = expect_len(dec.array().map_err(m)?, "frame")?;
        let domain = dec.str().map_err(m)?;
        if domain != FRAME_DOMAIN {
            return Err(WireError::Malformed(format!("unknown frame domain {domain:?}")));
        }
        let tag = dec.u8().map_err(m)?;
        // Pin the outer arity per variant: the wire is frozen, so a `Done` with extra fields or a
        // `Hello` of the wrong length is a malformed frame, not a tolerated superset.
        let expected_outer = match tag {
            tag::HELLO => 3,
            tag::ENTRIES => 4,
            tag::DONE => 2,
            tag::AUTH => 4,
            tag::ACK => 2,
            tag::AUTH_GRANT => 3,
            other => return Err(WireError::Malformed(format!("unknown frame tag {other}"))),
        };
        if outer != expected_outer {
            return Err(WireError::Malformed(format!(
                "frame tag {tag} arity {outer}, expected {expected_outer}"
            )));
        }
        let frame = Self::decode_body(tag, &mut dec)?;
        // A canonical frame consumes ALL its bytes: trailing CBOR after a valid frame is malformed,
        // never silently ignored.
        if dec.position() != bytes.len() {
            return Err(WireError::Malformed("trailing bytes after frame".into()));
        }
        Ok(frame)
    }

    fn decode_body(tag: u8, dec: &mut Decoder<'_>) -> Result<Frame, WireError> {
        match tag {
            tag::AUTH => {
                let account_id = fixed_hash(dec.bytes().map_err(m)?)?;
                let binding = dec.bytes().map_err(m)?;
                if binding.len() > MAX_AUTH_BINDING_BYTES {
                    return Err(WireError::OverCap(format!(
                        "auth binding {} > {MAX_AUTH_BINDING_BYTES}",
                        binding.len()
                    )));
                }
                Ok(Frame::Auth { account_id, binding: binding.to_vec() })
            },
            tag::AUTH_GRANT => Ok(Frame::AuthGrant { can_push: dec.bool().map_err(m)? }),
            tag::HELLO => {
                let inner = dec.array().map_err(m)?;
                if inner != Some(2) {
                    return Err(WireError::Malformed("hello payload arity".into()));
                }
                let account_id = fixed_hash(dec.bytes().map_err(m)?)?;
                let n = expect_len(dec.array().map_err(m)?, "hello.have")?;
                if n > MAX_HELLO_HASHES as u64 {
                    return Err(WireError::OverCap(format!("hello.have {n} > {MAX_HELLO_HASHES}")));
                }
                let mut have = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    have.push(fixed_hash(dec.bytes().map_err(m)?)?);
                }
                Ok(Frame::Hello { account_id, have })
            },
            tag::ENTRIES => {
                let n = expect_len(dec.array().map_err(m)?, "entries")?;
                if n > MAX_ENTRIES_PER_PAGE as u64 {
                    return Err(WireError::OverCap(format!(
                        "entries page {n} > {MAX_ENTRIES_PER_PAGE}"
                    )));
                }
                let mut entries = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    entries.push(dec.bytes().map_err(m)?.to_vec());
                }
                let more = dec.bool().map_err(m)?;
                Ok(Frame::Entries { entries, more })
            },
            tag::DONE => Ok(Frame::Done),
            tag::ACK => Ok(Frame::Ack),
            other => Err(WireError::Malformed(format!("unknown frame tag {other}"))),
        }
    }
}

const INFALLIBLE: &str = "encoding into an owned Vec cannot fail";

fn m(e: minicbor::decode::Error) -> WireError {
    WireError::Malformed(e.to_string())
}

fn expect_len(len: Option<u64>, field: &str) -> Result<u64, WireError> {
    len.ok_or_else(|| WireError::Malformed(format!("indefinite-length {field} array")))
}

fn fixed_hash(bytes: &[u8]) -> Result<Hash, WireError> {
    Hash::try_from(bytes)
        .map_err(|_| WireError::Malformed(format!("expected 32-byte hash, got {}", bytes.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: &Frame) {
        let bytes = frame.encode();
        assert_eq!(&Frame::decode(&bytes).unwrap(), frame, "encode∘decode is identity");
    }

    /// The ALPN identifiers are a frozen wire contract — an accidental rename would silently split
    /// peers (an old peer declines the handshake). Pin both, and that they are distinct so the
    /// acceptor can route account-log vs content by ALPN.
    #[test]
    fn alpn_identifiers_are_frozen() {
        assert_eq!(SYNC_ALPN, b"rag-rat/sync/4");
        assert_eq!(CONTENT_SYNC_ALPN, b"rag-rat/content/3");
        assert_eq!(crate::enrollment::ENROLL_ALPN, b"rag-rat/enroll/1");
        assert_ne!(SYNC_ALPN, CONTENT_SYNC_ALPN);
        assert_ne!(SYNC_ALPN, crate::enrollment::ENROLL_ALPN);
        assert_ne!(CONTENT_SYNC_ALPN, crate::enrollment::ENROLL_ALPN);
    }

    #[test]
    fn every_frame_roundtrips() {
        roundtrip(&Frame::Auth { account_id: [0xbb; 32], binding: vec![1, 2, 3, 4] });
        roundtrip(&Frame::Auth { account_id: [0; 32], binding: vec![] });
        roundtrip(&Frame::AuthGrant { can_push: false });
        roundtrip(&Frame::AuthGrant { can_push: true });
        roundtrip(&Frame::Hello { account_id: [0xaa; 32], have: vec![[1; 32], [2; 32]] });
        roundtrip(&Frame::Hello { account_id: [0; 32], have: vec![] });
        roundtrip(&Frame::Entries { entries: vec![vec![1, 2, 3], vec![]], more: true });
        roundtrip(&Frame::Entries { entries: vec![], more: false });
        roundtrip(&Frame::Done);
        roundtrip(&Frame::Ack);
    }

    #[test]
    fn an_over_cap_auth_binding_is_rejected() {
        // An Auth frame whose binding exceeds the cap is hostile — bounds the first-frame
        // allocation an unauthenticated peer can force.
        let frame =
            Frame::Auth { account_id: [3; 32], binding: vec![0u8; MAX_AUTH_BINDING_BYTES + 1] };
        assert!(matches!(Frame::decode(&frame.encode()), Err(WireError::OverCap(_))));
    }

    #[test]
    fn a_foreign_domain_is_rejected() {
        // A CBOR array that is well-formed but not our protocol.
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.array(2).unwrap();
        enc.str("some/other-protocol/1").unwrap();
        enc.u8(0).unwrap();
        assert!(matches!(Frame::decode(&buf), Err(WireError::Malformed(_))));
    }

    #[test]
    fn an_over_cap_entries_page_is_rejected_as_hostile() {
        // Hand-build an Entries frame claiming more elements than the cap allows, without
        // allocating them — the decoder must reject on the declared length alone.
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.array(4).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::ENTRIES).unwrap();
        enc.array((MAX_ENTRIES_PER_PAGE + 1) as u64).unwrap();
        // no elements written; the length prefix alone must trip the cap
        assert!(matches!(Frame::decode(&buf), Err(WireError::OverCap(_))));
    }

    #[test]
    fn a_truncated_frame_is_malformed_not_a_panic() {
        let full = Frame::Hello { account_id: [7; 32], have: vec![[9; 32]] }.encode();
        for cut in 0..full.len() {
            // Every prefix either decodes (unlikely) or errors — never panics.
            let _ = Frame::decode(&full[..cut]);
        }
    }

    #[test]
    fn a_wrong_outer_arity_is_rejected() {
        // A Done tag inside a 3-element outer array (Hello's arity) is malformed, not tolerated.
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.array(3).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::DONE).unwrap();
        enc.u8(0).unwrap(); // an extra field a lax decoder might ignore
        assert!(matches!(Frame::decode(&buf), Err(WireError::Malformed(_))));
    }

    #[test]
    fn trailing_bytes_after_a_valid_frame_are_rejected() {
        let mut buf = Frame::Done.encode();
        buf.push(0xff); // one byte of garbage after an otherwise-valid frame
        assert!(matches!(Frame::decode(&buf), Err(WireError::Malformed(_))));
    }

    /// The wire is frozen: pin the exact bytes of a representative frame so an accidental layout
    /// change (field order, tag value, domain string) breaks the build rather than a live peer.
    #[test]
    fn done_frame_bytes_are_frozen() {
        let bytes = Frame::Done.encode();
        assert_eq!(
            bytes,
            // array(2), text "rag-rat/sync-frame/1", u8 2
            [
                0x82, 0x74, b'r', b'a', b'g', b'-', b'r', b'a', b't', b'/', b's', b'y', b'n', b'c',
                b'-', b'f', b'r', b'a', b'm', b'e', b'/', b'1', 0x02,
            ],
        );
    }

    #[test]
    fn ack_frame_bytes_are_frozen() {
        let bytes = Frame::Ack.encode();
        assert_eq!(
            bytes,
            // array(2), text "rag-rat/sync-frame/1", u8 4
            [
                0x82, 0x74, b'r', b'a', b'g', b'-', b'r', b'a', b't', b'/', b's', b'y', b'n', b'c',
                b'-', b'f', b'r', b'a', b'm', b'e', b'/', b'1', 0x04,
            ],
        );
    }

    #[test]
    fn auth_grant_frame_bytes_are_frozen() {
        let bytes = Frame::AuthGrant { can_push: true }.encode();
        assert_eq!(
            bytes,
            // array(3), text "rag-rat/sync-frame/1", u8 5, true
            [
                0x83, 0x74, b'r', b'a', b'g', b'-', b'r', b'a', b't', b'/', b's', b'y', b'n', b'c',
                b'-', b'f', b'r', b'a', b'm', b'e', b'/', b'1', 0x05, 0xf5,
            ],
        );
    }
}
