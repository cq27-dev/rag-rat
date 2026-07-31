//! The discovery protocol's frame types — a CROSS-REPOSITORY wire contract.
//!
//! The peer-discovery service is a separate program in a separate repository, so these types are a
//! MIRROR, not a shared crate. Nothing in this repository's CI can build or run the service, so the
//! golden vectors below are the only pin: treat a diff there as a protocol break on the wire, never
//! as a stale expectation to re-bless.
//!
//! **The counterpart is derive-generated; this side is hand-written, and that is deliberate.** The
//! two repositories are on different major versions of the CBOR library, so sharing the derive
//! would not have made compatibility structural — it would only have moved the assumption. Instead
//! the encoding here was MEASURED against the counterpart's library version and every vector below
//! is that measurement; the `Fetch` vector is additionally byte-identical to the golden the
//! service's own test suite pins, which is what ties this mirror to the real thing.
//!
//! The shape the service expects, in CBOR terms:
//! - an enum is `[variant_index, [fields...]]`; a unit variant's field array is empty;
//! - a struct is a bare `[fields...]`;
//! - `[u8; 32]` and `Vec<u8>` are arrays of integers, NOT byte strings — the single easiest thing
//!   to get wrong here, and invisible until a real service rejects the frame.
//!
//! The service is a BLIND `tag -> announcements` store: it hexes the 32 bytes and uses them as a
//! map key, learning nothing about what a tag means. That is what lets an account keep its device
//! list private from the operator (see [`super::account_tag`]).

use minicbor::{Decoder, Encoder};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Writing CBOR into a `Vec` cannot fail, mirroring the op-log's envelope style.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// A discovery tag: an opaque 32-byte key the service stores announcements under.
pub const TAG_LEN: usize = 32;

/// Max bytes of one framed message, length prefix excluded. Mirrors the service's own cap; a larger
/// declared length is refused before allocating.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Most announcements decoded from one response. The service caps this too, but its cap is an
/// environment-overridable deployment default this client cannot observe, so the length prefix is
/// not something to trust.
const MAX_ANNOUNCEMENTS_DECODED: u64 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryRequest {
    Publish {
        tag: [u8; TAG_LEN],
        payload: Vec<u8>,
        ttl_seconds: u32,
    },
    Fetch {
        tag: [u8; TAG_LEN],
    },
    /// Part of the mirrored contract and encodable, though nothing in rag-rat withdraws yet —
    /// announcements are allowed to lapse by TTL. Kept so the next reader does not have to
    /// re-derive it from the other repository.
    Withdraw {
        tag: [u8; TAG_LEN],
        withdraw_token: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireAnnouncement {
    pub payload: Vec<u8>,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryResponse {
    Published {
        withdraw_token: Vec<u8>,
    },
    Fetched {
        announcements: Vec<WireAnnouncement>,
    },
    Withdrawn,
    /// `code` is [`DecodedErrorCode`] rather than a closed enum so a code a NEWER service adds
    /// decodes instead of failing. Every code means the same thing to this client — the request did
    /// not take effect — so it is diagnostic, never control flow.
    Error {
        code: DecodedErrorCode,
        message: String,
    },
}

/// Why the service refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryErrorCode {
    RateLimited,
    PayloadTooLarge,
    PerTagCapExceeded,
    TtlInvalid,
    TagInvalid,
    WithdrawTokenMismatch,
    WithdrawUnknownTag,
    Internal,
}

impl DiscoveryErrorCode {
    /// The wire index. Frozen: these ARE the contract.
    pub fn index(self) -> u32 {
        match self {
            Self::RateLimited => 0,
            Self::PayloadTooLarge => 1,
            Self::PerTagCapExceeded => 2,
            Self::TtlInvalid => 3,
            Self::TagInvalid => 4,
            Self::WithdrawTokenMismatch => 5,
            Self::WithdrawUnknownTag => 6,
            Self::Internal => 7,
        }
    }

    fn from_index(index: u32) -> Option<Self> {
        Some(match index {
            0 => Self::RateLimited,
            1 => Self::PayloadTooLarge,
            2 => Self::PerTagCapExceeded,
            3 => Self::TtlInvalid,
            4 => Self::TagInvalid,
            5 => Self::WithdrawTokenMismatch,
            6 => Self::WithdrawUnknownTag,
            7 => Self::Internal,
            _ => return None,
        })
    }
}

/// An error code as this binary understands it: a known variant, or the raw index a newer service
/// sent. Keeps an added code from turning into a decode failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedErrorCode {
    Known(DiscoveryErrorCode),
    Unknown(u32),
}

/// A malformed frame from the service. Never fatal to a discovery pass — see [`super`].
#[derive(Debug)]
pub struct WireError(String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed discovery frame: {}", self.0)
    }
}

impl std::error::Error for WireError {}

impl From<minicbor::decode::Error> for WireError {
    fn from(error: minicbor::decode::Error) -> Self {
        Self(error.to_string())
    }
}

/// Encode an integer array — how the counterpart renders `[u8; N]` and `Vec<u8>`. NOT `bytes()`.
fn encode_byte_array(enc: &mut Encoder<&mut Vec<u8>>, bytes: &[u8]) {
    enc.array(bytes.len() as u64).expect(INFALLIBLE);
    for byte in bytes {
        enc.u8(*byte).expect(INFALLIBLE);
    }
}

fn decode_byte_vec(dec: &mut Decoder<'_>, cap: u64) -> Result<Vec<u8>, WireError> {
    let len = dec.array()?.ok_or_else(|| WireError("indefinite-length array".into()))?;
    if len > cap {
        return Err(WireError(format!("array of {len} exceeds the {cap} cap")));
    }
    (0..len).map(|_| dec.u8().map_err(WireError::from)).collect()
}

impl DiscoveryRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        let mut enc = Encoder::new(&mut buf);
        // `[variant_index, [fields...]]`.
        enc.array(2).expect(INFALLIBLE);
        match self {
            Self::Publish { tag, payload, ttl_seconds } => {
                enc.u32(0).expect(INFALLIBLE);
                enc.array(3).expect(INFALLIBLE);
                encode_byte_array(&mut enc, tag);
                encode_byte_array(&mut enc, payload);
                enc.u32(*ttl_seconds).expect(INFALLIBLE);
            },
            Self::Fetch { tag } => {
                enc.u32(1).expect(INFALLIBLE);
                enc.array(1).expect(INFALLIBLE);
                encode_byte_array(&mut enc, tag);
            },
            Self::Withdraw { tag, withdraw_token } => {
                enc.u32(2).expect(INFALLIBLE);
                enc.array(2).expect(INFALLIBLE);
                encode_byte_array(&mut enc, tag);
                encode_byte_array(&mut enc, withdraw_token);
            },
        }
        buf
    }
}

impl DiscoveryResponse {
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut dec = Decoder::new(bytes);
        let outer = dec.array()?.ok_or_else(|| WireError("indefinite-length response".into()))?;
        if outer != 2 {
            return Err(WireError(format!("response envelope has {outer} items, expected 2")));
        }
        let variant = dec.u32()?;
        let fields = dec.array()?.ok_or_else(|| WireError("indefinite-length fields".into()))?;
        Ok(match variant {
            0 => {
                expect_fields(fields, 1, "Published")?;
                Self::Published { withdraw_token: decode_byte_vec(&mut dec, MAX_FRAME_LEN as u64)? }
            },
            1 => {
                expect_fields(fields, 1, "Fetched")?;
                let count =
                    dec.array()?.ok_or_else(|| WireError("indefinite announcements".into()))?;
                if count > MAX_ANNOUNCEMENTS_DECODED {
                    return Err(WireError(format!(
                        "{count} announcements exceeds the {MAX_ANNOUNCEMENTS_DECODED} cap"
                    )));
                }
                let mut announcements = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let items =
                        dec.array()?.ok_or_else(|| WireError("indefinite announcement".into()))?;
                    expect_fields(items, 2, "WireAnnouncement")?;
                    announcements.push(WireAnnouncement {
                        payload: decode_byte_vec(&mut dec, MAX_FRAME_LEN as u64)?,
                        expires_at_ms: dec.i64()?,
                    });
                }
                Self::Fetched { announcements }
            },
            2 => {
                expect_fields(fields, 0, "Withdrawn")?;
                Self::Withdrawn
            },
            3 => {
                expect_fields(fields, 2, "Error")?;
                // The code is itself an enum: `[index, []]`.
                let code_outer =
                    dec.array()?.ok_or_else(|| WireError("indefinite error code".into()))?;
                if code_outer != 2 {
                    return Err(WireError("error code envelope is not a 2-item array".into()));
                }
                let index = dec.u32()?;
                // SKIP the code's field array rather than assuming it is empty. Every code the
                // counterpart defines today is unit-shaped, so this reads zero elements — but the
                // counterpart is in another repository and a code that carries a payload would
                // otherwise leave the decoder pointing at that payload, failing the whole response
                // and turning "the service told me why" into "the service is broken" for every
                // older client. That is the failure this forward-compatibility exists to prevent,
                // so it must not depend on the shape of a variant nobody has written yet.
                let code_fields = dec.array()?.unwrap_or(0);
                for _ in 0..code_fields {
                    dec.skip()?;
                }
                Self::Error {
                    code: DiscoveryErrorCode::from_index(index)
                        .map_or(DecodedErrorCode::Unknown(index), DecodedErrorCode::Known),
                    message: dec.str()?.to_owned(),
                }
            },
            // A variant a newer service added. Reported, not decoded — a response this client
            // cannot interpret means only that the request did not take effect.
            other => return Err(WireError(format!("unknown response variant {other}"))),
        })
    }
}

/// The SERVICE's half of the contract, for the in-process stub the tests run against.
///
/// Test-only on purpose: rag-rat is a client and never serves discovery, so shipping these would be
/// dead production surface. They exist because a stub that spoke a convenient encoding instead of
/// the real one would let the client's own bugs pass — the stub has to be as literal as the
/// service. `the_service_side_encoder_reproduces_the_measured_response_bytes` pins that by
/// re-deriving the measured golden vectors through this encoder.
#[cfg(test)]
impl DiscoveryResponse {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        let mut enc = Encoder::new(&mut buf);
        enc.array(2).expect(INFALLIBLE);
        match self {
            Self::Published { withdraw_token } => {
                enc.u32(0).expect(INFALLIBLE);
                enc.array(1).expect(INFALLIBLE);
                encode_byte_array(&mut enc, withdraw_token);
            },
            Self::Fetched { announcements } => {
                enc.u32(1).expect(INFALLIBLE);
                enc.array(1).expect(INFALLIBLE);
                enc.array(announcements.len() as u64).expect(INFALLIBLE);
                for announcement in announcements {
                    enc.array(2).expect(INFALLIBLE);
                    encode_byte_array(&mut enc, &announcement.payload);
                    enc.i64(announcement.expires_at_ms).expect(INFALLIBLE);
                }
            },
            Self::Withdrawn => {
                enc.u32(2).expect(INFALLIBLE);
                enc.array(0).expect(INFALLIBLE);
            },
            Self::Error { code, message } => {
                enc.u32(3).expect(INFALLIBLE);
                enc.array(2).expect(INFALLIBLE);
                // The code is itself an enum on the wire: `[index, []]`.
                enc.array(2).expect(INFALLIBLE);
                enc.u32(match code {
                    DecodedErrorCode::Known(known) => known.index(),
                    DecodedErrorCode::Unknown(index) => *index,
                })
                .expect(INFALLIBLE);
                enc.array(0).expect(INFALLIBLE);
                enc.str(message).expect(INFALLIBLE);
            },
        }
        buf
    }
}

#[cfg(test)]
impl DiscoveryRequest {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut dec = Decoder::new(bytes);
        let outer = dec.array()?.ok_or_else(|| WireError("indefinite-length request".into()))?;
        if outer != 2 {
            return Err(WireError(format!("request envelope has {outer} items, expected 2")));
        }
        let variant = dec.u32()?;
        let fields = dec.array()?.ok_or_else(|| WireError("indefinite-length fields".into()))?;
        let tag = |dec: &mut Decoder<'_>| -> Result<[u8; TAG_LEN], WireError> {
            let bytes = decode_byte_vec(dec, TAG_LEN as u64)?;
            <[u8; TAG_LEN]>::try_from(bytes.as_slice())
                .map_err(|_| WireError("tag is not 32 bytes".into()))
        };
        Ok(match variant {
            0 => {
                expect_fields(fields, 3, "Publish")?;
                Self::Publish {
                    tag: tag(&mut dec)?,
                    payload: decode_byte_vec(&mut dec, MAX_FRAME_LEN as u64)?,
                    ttl_seconds: dec.u32()?,
                }
            },
            1 => {
                expect_fields(fields, 1, "Fetch")?;
                Self::Fetch { tag: tag(&mut dec)? }
            },
            2 => {
                expect_fields(fields, 2, "Withdraw")?;
                Self::Withdraw {
                    tag: tag(&mut dec)?,
                    withdraw_token: decode_byte_vec(&mut dec, MAX_FRAME_LEN as u64)?,
                }
            },
            other => return Err(WireError(format!("unknown request variant {other}"))),
        })
    }
}

fn expect_fields(actual: u64, expected: u64, what: &str) -> Result<(), WireError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WireError(format!("{what} has {actual} fields, expected {expected}")))
    }
}

/// Write a length-prefixed frame: 4-byte big-endian length, then the body.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {} > {MAX_FRAME_LEN}", body.len()),
        ));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

/// Read one length-prefixed frame, refusing an over-cap declared length BEFORE allocating — the
/// length is peer-supplied, so trusting it is a trivial memory-exhaustion lever.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} > {MAX_FRAME_LEN}"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// The vector the SERVICE's own test suite pins. This one line is what ties this hand-written
    /// mirror to the real counterpart; the rest of the vectors below were measured against the
    /// counterpart's CBOR library and are only as good as this one proving the method.
    const SERVICE_GOLDEN_FETCH: &str = "820181982018ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab\
                                        18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab\
                                        18ab18ab18ab18ab18ab";

    const PUBLISH_MEASURED: &str = "820083982001010101010101010101010101010101010101010101010101010101010101018218de18ad190384";
    const WITHDRAW_MEASURED: &str =
        "820282982001010101010101010101010101010101010101010101010101010101010101018107";

    #[test]
    fn fetch_matches_the_vector_the_service_itself_pins() {
        assert_eq!(
            hex(&DiscoveryRequest::Fetch { tag: [0xab; TAG_LEN] }.encode()),
            SERVICE_GOLDEN_FETCH
        );
    }

    /// A `[u8; 32]` is an ARRAY OF INTEGERS on this wire, not a CBOR byte string. Encoding it as
    /// bytes produces a frame the service rejects, and nothing local would notice.
    #[test]
    fn a_tag_is_an_integer_array_not_a_byte_string() {
        let encoded = DiscoveryRequest::Fetch { tag: [0xab; TAG_LEN] }.encode();
        assert!(encoded[3..].starts_with(&[0x98, 0x20]), "array(32) header: {}", hex(&encoded));
        assert!(!encoded.contains(&0x58), "a byte-string header must not appear");
    }

    /// Measured against the counterpart's CBOR library, single-line on purpose: hand-wrapping a hex
    /// literal is how a wrong byte count gets into a "golden" vector unnoticed.
    #[test]
    fn every_request_variant_encodes_to_its_measured_bytes() {
        assert_eq!(
            hex(&DiscoveryRequest::Publish {
                tag: [0x01; TAG_LEN],
                payload: vec![0xde, 0xad],
                ttl_seconds: 900,
            }
            .encode()),
            PUBLISH_MEASURED,
        );
        assert_eq!(
            hex(&DiscoveryRequest::Withdraw { tag: [0x01; TAG_LEN], withdraw_token: vec![0x07] }
                .encode()),
            WITHDRAW_MEASURED,
        );
    }

    #[test]
    fn every_response_variant_decodes_from_its_measured_bytes() {
        assert_eq!(
            DiscoveryResponse::decode(&unhex("820081820102")).unwrap(),
            DiscoveryResponse::Published { withdraw_token: vec![1, 2] },
        );
        assert_eq!(
            DiscoveryResponse::decode(&unhex("82018180")).unwrap(),
            DiscoveryResponse::Fetched { announcements: Vec::new() },
        );
        assert_eq!(
            DiscoveryResponse::decode(&unhex("820181818281091b0000018bcfe56800")).unwrap(),
            DiscoveryResponse::Fetched {
                announcements: vec![WireAnnouncement {
                    payload: vec![0x09],
                    expires_at_ms: 1_700_000_000_000,
                }],
            },
        );
        assert_eq!(
            DiscoveryResponse::decode(&unhex("820280")).unwrap(),
            DiscoveryResponse::Withdrawn
        );
        assert_eq!(
            DiscoveryResponse::decode(&unhex("82038282008069736c6f7720646f776e")).unwrap(),
            DiscoveryResponse::Error {
                code: DecodedErrorCode::Known(DiscoveryErrorCode::RateLimited),
                message: "slow down".into(),
            },
        );
    }

    /// The stub the transport tests run against answers with the test-only service-side encoder, so
    /// that encoder has to produce the SAME bytes the real service does — otherwise those tests
    /// validate the client against a fiction. Re-deriving the measured vectors through it is the
    /// pin. Mutate any arm of `DiscoveryResponse::encode` and this fails.
    #[test]
    fn the_service_side_encoder_reproduces_the_measured_response_bytes() {
        for (measured, response) in [
            ("820081820102", DiscoveryResponse::Published { withdraw_token: vec![1, 2] }),
            ("82018180", DiscoveryResponse::Fetched { announcements: Vec::new() }),
            ("820181818281091b0000018bcfe56800", DiscoveryResponse::Fetched {
                announcements: vec![WireAnnouncement {
                    payload: vec![0x09],
                    expires_at_ms: 1_700_000_000_000,
                }],
            }),
            ("820280", DiscoveryResponse::Withdrawn),
            ("82038282008069736c6f7720646f776e", DiscoveryResponse::Error {
                code: DecodedErrorCode::Known(DiscoveryErrorCode::RateLimited),
                message: "slow down".into(),
            }),
        ] {
            assert_eq!(hex(&response.encode()), measured, "re-encoding {response:?}");
        }
    }

    /// The stub's request decoder is the other half of that mirror: it must read exactly what this
    /// client writes, including the integer-array encoding of a tag.
    #[test]
    fn the_service_side_decoder_reads_every_request_this_client_writes() {
        for request in [
            DiscoveryRequest::Publish {
                tag: [0x01; TAG_LEN],
                payload: vec![0xde, 0xad],
                ttl_seconds: 900,
            },
            DiscoveryRequest::Fetch { tag: [0xab; TAG_LEN] },
            DiscoveryRequest::Withdraw { tag: [0x01; TAG_LEN], withdraw_token: vec![0x07] },
        ] {
            assert_eq!(DiscoveryRequest::decode(&request.encode()).unwrap(), request);
        }
    }

    /// A future code that carries a PAYLOAD must decode too, not just a unit-shaped one.
    ///
    /// The counterpart is in another repository and every code it defines today is unit-shaped, so
    /// the empty-array case is the only one exercised in practice — which is exactly why this is
    /// pinned: assuming the array is empty leaves the decoder pointing at the payload, and the
    /// message field then fails the whole response. Every refusal would read as a broken service to
    /// every older client. Vector: variant 3 (Error), code index 8 carrying `[42]`, message "no".
    #[test]
    fn a_future_error_code_carrying_a_payload_still_decodes() {
        let decoded = DiscoveryResponse::decode(&unhex("820382820881182a626e6f")).unwrap();
        assert_eq!(decoded, DiscoveryResponse::Error {
            code: DecodedErrorCode::Unknown(8),
            message: "no".into(),
        });
    }

    /// A code a NEWER service adds must decode, not fail. Otherwise "the service told me why it
    /// said no" becomes "the service is broken", and a deployment that adds one code blinds every
    /// older client to every refusal.
    #[test]
    fn an_unknown_future_error_code_decodes_rather_than_failing() {
        // Variant 3 (Error) carrying code index 99.
        let decoded = DiscoveryResponse::decode(&unhex("8203828218638060")).unwrap();
        assert_eq!(decoded, DiscoveryResponse::Error {
            code: DecodedErrorCode::Unknown(99),
            message: String::new(),
        });
    }

    #[test]
    fn every_known_error_code_holds_its_frozen_index() {
        for (code, index) in [
            (DiscoveryErrorCode::RateLimited, 0u32),
            (DiscoveryErrorCode::PayloadTooLarge, 1),
            (DiscoveryErrorCode::PerTagCapExceeded, 2),
            (DiscoveryErrorCode::TtlInvalid, 3),
            (DiscoveryErrorCode::TagInvalid, 4),
            (DiscoveryErrorCode::WithdrawTokenMismatch, 5),
            (DiscoveryErrorCode::WithdrawUnknownTag, 6),
            (DiscoveryErrorCode::Internal, 7),
        ] {
            assert_eq!(code.index(), index, "index moved for {code:?}");
            assert_eq!(DiscoveryErrorCode::from_index(index), Some(code));
        }
    }

    /// A peer-declared announcement count is refused before the allocation it would cause.
    #[test]
    fn an_over_cap_announcement_count_is_refused() {
        // Fetched, declaring 1000 announcements it does not carry.
        let error = DiscoveryResponse::decode(&unhex("8201819903e8"))
            .expect_err("an over-cap count is refused");
        assert!(error.to_string().contains("cap"), "{error}");
    }

    /// The framing is part of the contract too: a 4-byte BIG-endian length.
    #[tokio::test]
    async fn a_frame_is_a_four_byte_big_endian_length_then_the_body() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[0xaa, 0xbb, 0xcc]).await.unwrap();
        assert_eq!(buf, vec![0x00, 0x00, 0x00, 0x03, 0xaa, 0xbb, 0xcc]);
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).await.unwrap(), vec![0xaa, 0xbb, 0xcc]);
    }

    #[tokio::test]
    async fn an_over_cap_declared_length_is_refused_without_allocating() {
        let mut framed = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&[0u8; 8]);
        let mut cursor = std::io::Cursor::new(framed);
        let error = read_frame(&mut cursor).await.expect_err("an over-cap length is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
