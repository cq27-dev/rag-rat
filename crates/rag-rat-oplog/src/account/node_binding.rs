//! Signed transport-node ↔ account-device binding for peer sync authorization (phase D, #881).
//!
//! The iroh transport key is a per-install identity deliberately distinct from any account device
//! signing key (key hygiene — the long-term device key never doubles as a TLS key). So proving that
//! a connecting node belongs to the account needs an explicit, signed statement: a roster device
//! vouches "transport node N may sync account A". This module mints and verifies that statement.
//!
//! The verifier's guarantee rests on THREE independent facts, all required:
//! 1. the statement is signed by a device whose fingerprint is **roster-effective** in the current
//!    fold — proves a real account device authored it;
//! 2. the bound `node_pubkey` equals the connection's **iroh-authenticated** remote node id —
//!    proves the *connector* actually holds that transport key, so a captured/leaked binding is
//!    inert to anyone who does not also hold node N's transport secret (this is what defeats
//!    replay);
//! 3. the statement is fresh (`issued_at_ms` within a bounded window) — bounds the one residual, a
//!    transport seed stolen without the device key, to that window rather than forever.
//!
//! The signed bytes are DOMAIN-TAGGED (`NODE_BINDING_DOMAIN`), separate from the account/content
//! entry domains and the sync frame domain, so a binding signature can never collide with an op or
//! frame preimage (a cross-protocol forgery the node-id check would not catch).

use minicbor::{Decoder, Encoder};

use super::storage;
use crate::account::AccountId;
use crate::device::DevicePublic;
use crate::op::DeviceFingerprint;
use crate::{cbor, identity};

/// The signature domain for a node binding. Distinct from `FRAME_DOMAIN`, the account/content entry
/// domains, and every other signed preimage in the system — a binding signature and an op signature
/// can never be confused.
const NODE_BINDING_DOMAIN: &str = "rag-rat/node-binding/1";

/// A binding older than this (relative to the verifier's clock) is rejected. The honest client
/// mints a fresh binding per dial — it holds both keys locally — so a tight window costs nothing
/// and bounds the stolen-transport-seed residual to a day rather than forever.
pub const MAX_BINDING_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// A binding whose `issued_at_ms` is more than this far in the FUTURE is rejected — tolerates
/// modest clock skew between peers without opening a meaningful pre-dating window.
pub const MAX_BINDING_FUTURE_SKEW_MS: i64 = 60 * 60 * 1000;

const INFALLIBLE: &str = "encoding into an owned Vec cannot fail";

/// Why a node binding failed authorization. Typed for the caller's logging and future retry logic
/// (a `NotRosterDevice` can be retried after more of the account log syncs; a `BadSignature` never
/// can) — but the transport MUST collapse every variant to ONE uniform wire refusal, so a peer
/// cannot use the distinction as an oracle ("does this server host account A / know device D").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeAuthError {
    /// The binding bytes are not a well-formed, domain-tagged node binding.
    Malformed,
    /// The signature does not verify under the embedded device pubkey.
    BadSignature,
    /// The signing device is not roster-effective in the current fold (removed, or not yet synced).
    NotRosterDevice,
    /// The bound `node_pubkey` is not the connection's authenticated remote node id.
    NodeMismatch,
    /// The binding names a different account than the session is scoped to.
    WrongAccount,
    /// The binding is stale, or dated implausibly far in the future.
    Expired,
    /// No local device exists to sign a binding (this store has no account yet).
    NoLocalDevice,
}

/// The canonical, domain-tagged bytes a device signs to vouch for a node. Kept identical between
/// the mint and verify paths (and stable for a future persisted `NodeBind` op to embed), so the
/// signature covers exactly `[domain, account_id, node_pubkey, device_pubkey, issued_at_ms]`.
fn signing_bytes(
    account_id: AccountId,
    node_pubkey: &[u8; 32],
    device_pubkey: &[u8; 32],
    issued_at_ms: i64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(160);
    let mut enc = Encoder::new(&mut buf);
    enc.array(5).expect(INFALLIBLE);
    enc.str(NODE_BINDING_DOMAIN).expect(INFALLIBLE);
    enc.bytes(&account_id.to_bytes()).expect(INFALLIBLE);
    enc.bytes(node_pubkey).expect(INFALLIBLE);
    enc.bytes(device_pubkey).expect(INFALLIBLE);
    enc.i64(issued_at_ms).expect(INFALLIBLE);
    buf
}

/// Mint a signed binding vouching that THIS install's transport `node_pubkey` belongs to the local
/// account device, for `account_id`. Loads the local device WITHOUT minting (a store with no
/// account cannot authorize a sync); returns [`NodeAuthError::NoLocalDevice`] if absent. The
/// returned bytes are what the transport puts on the wire in its `Auth` frame.
pub fn sign_local_node_binding(
    conn: &rusqlite::Connection,
    account_id: AccountId,
    node_pubkey: &[u8; 32],
    now_ms: i64,
) -> anyhow::Result<Result<Vec<u8>, NodeAuthError>> {
    let Some(local) = identity::load_local_device(conn)? else {
        return Ok(Err(NodeAuthError::NoLocalDevice));
    };
    let device_pubkey = local.public().to_bytes();
    let signature =
        local.secret().sign(&signing_bytes(account_id, node_pubkey, &device_pubkey, now_ms));

    let mut buf = Vec::with_capacity(224);
    let mut enc = Encoder::new(&mut buf);
    enc.array(6).expect(INFALLIBLE);
    enc.str(NODE_BINDING_DOMAIN).expect(INFALLIBLE);
    enc.bytes(&account_id.to_bytes()).expect(INFALLIBLE);
    enc.bytes(node_pubkey).expect(INFALLIBLE);
    enc.bytes(&device_pubkey).expect(INFALLIBLE);
    enc.i64(now_ms).expect(INFALLIBLE);
    enc.bytes(&signature).expect(INFALLIBLE);
    Ok(Ok(buf))
}

struct DecodedBinding {
    account_id: [u8; 32],
    node_pubkey: [u8; 32],
    device_pubkey: [u8; 32],
    issued_at_ms: i64,
    signature: [u8; 64],
}

fn decode(bytes: &[u8]) -> Result<DecodedBinding, NodeAuthError> {
    let mut dec = Decoder::new(bytes);
    let outer = dec.array().map_err(|_| NodeAuthError::Malformed)?;
    if outer != Some(6) {
        return Err(NodeAuthError::Malformed);
    }
    let domain = dec.str().map_err(|_| NodeAuthError::Malformed)?;
    if domain != NODE_BINDING_DOMAIN {
        return Err(NodeAuthError::Malformed);
    }
    let account_id = fixed32(&mut dec)?;
    let node_pubkey = fixed32(&mut dec)?;
    let device_pubkey = fixed32(&mut dec)?;
    let issued_at_ms = dec.i64().map_err(|_| NodeAuthError::Malformed)?;
    let signature = fixed64(&mut dec)?;
    // A canonical binding consumes ALL its bytes — trailing CBOR is malformed, never ignored.
    if dec.position() != bytes.len() {
        return Err(NodeAuthError::Malformed);
    }
    Ok(DecodedBinding { account_id, node_pubkey, device_pubkey, issued_at_ms, signature })
}

/// Verify a peer's node binding for `account_id` against the current fold and the connection's
/// authenticated `remote_node_pubkey`. Returns `Ok(())` iff every check passes — see the module doc
/// for why all three (roster, node match, freshness) are required. The caller re-runs this PER
/// connection (never caches) so a device removed since a prior session is refused, and collapses
/// any `Err` to a single uniform wire refusal.
pub fn verify_node_binding(
    conn: &rusqlite::Connection,
    account_id: AccountId,
    binding_bytes: &[u8],
    remote_node_pubkey: &[u8; 32],
    now_ms: i64,
) -> anyhow::Result<Result<(), NodeAuthError>> {
    let b = match decode(binding_bytes) {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    // Scope: the binding must name the session's account.
    if b.account_id != account_id.to_bytes() {
        return Ok(Err(NodeAuthError::WrongAccount));
    }
    // Node match: the binding must name the connector's AUTHENTICATED transport key. This is what
    // makes a stolen binding useless — iroh will not let anyone but node N present remote id N.
    if &b.node_pubkey != remote_node_pubkey {
        return Ok(Err(NodeAuthError::NodeMismatch));
    }
    // Signature: verify over the exact canonical preimage, under the embedded (self-certifying)
    // device pubkey.
    let Ok(pubkey) = DevicePublic::from_bytes(&b.device_pubkey) else {
        return Ok(Err(NodeAuthError::BadSignature));
    };
    let preimage = signing_bytes(account_id, &b.node_pubkey, &b.device_pubkey, b.issued_at_ms);
    if pubkey.verify(&preimage, &b.signature).is_err() {
        return Ok(Err(NodeAuthError::BadSignature));
    }
    // Roster membership: the signer's fingerprint (self-certified as `sha256(device_pubkey)`, never
    // resolved from a fold-independent key store) must be effective in the CURRENT fold. Any role
    // is allowed — the roster gate is read access, not authoring authority; gating on Owner
    // would break a member device restoring its own account.
    let fingerprint = DeviceFingerprint::from_bytes(cbor::sha256(&b.device_pubkey));
    let effective = storage::list_effective_roster_fingerprints(conn, account_id)?;
    if !effective.contains(&fingerprint) {
        return Ok(Err(NodeAuthError::NotRosterDevice));
    }
    // Freshness: bounds the stolen-transport-seed residual. The binding may not be older than
    // MAX_BINDING_AGE_MS nor dated more than MAX_BINDING_FUTURE_SKEW_MS ahead of the verifier.
    if b.issued_at_ms < now_ms.saturating_sub(MAX_BINDING_AGE_MS)
        || b.issued_at_ms > now_ms.saturating_add(MAX_BINDING_FUTURE_SKEW_MS)
    {
        return Ok(Err(NodeAuthError::Expired));
    }
    Ok(Ok(()))
}

fn fixed32(dec: &mut Decoder<'_>) -> Result<[u8; 32], NodeAuthError> {
    dec.bytes()
        .map_err(|_| NodeAuthError::Malformed)?
        .try_into()
        .map_err(|_| NodeAuthError::Malformed)
}

fn fixed64(dec: &mut Decoder<'_>) -> Result<[u8; 64], NodeAuthError> {
    dec.bytes()
        .map_err(|_| NodeAuthError::Malformed)?
        .try_into()
        .map_err(|_| NodeAuthError::Malformed)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::account::local_account;
    use crate::device::DeviceSecret;

    const NOW: i64 = 1_700_000_000_000;
    const NODE: [u8; 32] = [7u8; 32];

    fn db_with_account() -> (Connection, AccountId) {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = local_account(&conn, NOW).unwrap();
        (conn, account)
    }

    fn sign_local(conn: &Connection, account: AccountId, node: &[u8; 32], now: i64) -> Vec<u8> {
        sign_local_node_binding(conn, account, node, now).unwrap().unwrap()
    }

    /// Assemble a binding signed by an ARBITRARY device (not necessarily on the roster), for the
    /// non-roster-device test — the production mint only ever uses the local device.
    fn sign_with(secret: &DeviceSecret, account: AccountId, node: &[u8; 32], now: i64) -> Vec<u8> {
        let device_pubkey = secret.public().to_bytes();
        let signature = secret.sign(&signing_bytes(account, node, &device_pubkey, now));
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.array(6).unwrap();
        enc.str(NODE_BINDING_DOMAIN).unwrap();
        enc.bytes(&account.to_bytes()).unwrap();
        enc.bytes(node).unwrap();
        enc.bytes(&device_pubkey).unwrap();
        enc.i64(now).unwrap();
        enc.bytes(&signature).unwrap();
        buf
    }

    #[test]
    fn a_valid_binding_from_a_roster_device_verifies() {
        let (conn, account) = db_with_account();
        let binding = sign_local(&conn, account, &NODE, NOW);
        assert_eq!(verify_node_binding(&conn, account, &binding, &NODE, NOW).unwrap(), Ok(()));
    }

    #[test]
    fn a_binding_for_a_different_node_is_refused() {
        // A captured binding is inert when presented from a different transport key — the crux of
        // the replay defense.
        let (conn, account) = db_with_account();
        let binding = sign_local(&conn, account, &NODE, NOW);
        assert_eq!(
            verify_node_binding(&conn, account, &binding, &[8u8; 32], NOW).unwrap(),
            Err(NodeAuthError::NodeMismatch),
        );
    }

    #[test]
    fn a_binding_naming_a_different_account_is_refused() {
        let (conn, account) = db_with_account();
        let binding = sign_local(&conn, account, &NODE, NOW);
        let other = AccountId::from_bytes([9u8; 32]);
        assert_eq!(
            verify_node_binding(&conn, other, &binding, &NODE, NOW).unwrap(),
            Err(NodeAuthError::WrongAccount),
        );
    }

    #[test]
    fn a_binding_from_a_non_roster_device_is_refused() {
        // Correct account + node + a valid signature, but the signer is not a roster device.
        let (conn, account) = db_with_account();
        let stranger = DeviceSecret::from_seed(&[0x5a; 32]);
        let binding = sign_with(&stranger, account, &NODE, NOW);
        assert_eq!(
            verify_node_binding(&conn, account, &binding, &NODE, NOW).unwrap(),
            Err(NodeAuthError::NotRosterDevice),
        );
    }

    #[test]
    fn a_stale_or_future_binding_is_refused() {
        let (conn, account) = db_with_account();
        let binding = sign_local(&conn, account, &NODE, NOW);
        assert_eq!(
            verify_node_binding(&conn, account, &binding, &NODE, NOW + MAX_BINDING_AGE_MS + 1)
                .unwrap(),
            Err(NodeAuthError::Expired),
            "older than the max age",
        );
        let future = sign_local(&conn, account, &NODE, NOW + MAX_BINDING_FUTURE_SKEW_MS + 10_000);
        assert_eq!(
            verify_node_binding(&conn, account, &future, &NODE, NOW).unwrap(),
            Err(NodeAuthError::Expired),
            "dated too far in the future",
        );
    }

    #[test]
    fn a_tampered_signature_is_refused() {
        let (conn, account) = db_with_account();
        let mut binding = sign_local(&conn, account, &NODE, NOW);
        *binding.last_mut().unwrap() ^= 0x01; // flip a bit in the trailing signature
        assert_eq!(
            verify_node_binding(&conn, account, &binding, &NODE, NOW).unwrap(),
            Err(NodeAuthError::BadSignature),
        );
    }

    #[test]
    fn malformed_bytes_are_refused_not_panicked() {
        let (conn, account) = db_with_account();
        for bad in [vec![], vec![0xff; 4], vec![0u8; 200]] {
            assert_eq!(
                verify_node_binding(&conn, account, &bad, &NODE, NOW).unwrap(),
                Err(NodeAuthError::Malformed),
            );
        }
    }

    #[test]
    fn a_wrong_domain_binding_is_malformed() {
        // A structurally valid 6-array whose domain tag is not ours — the cross-protocol guard.
        let (conn, account) = db_with_account();
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.array(6).unwrap();
        enc.str("some/other-protocol/1").unwrap();
        enc.bytes(&account.to_bytes()).unwrap();
        enc.bytes(&NODE).unwrap();
        enc.bytes(&[0u8; 32]).unwrap();
        enc.i64(NOW).unwrap();
        enc.bytes(&[0u8; 64]).unwrap();
        assert_eq!(
            verify_node_binding(&conn, account, &buf, &NODE, NOW).unwrap(),
            Err(NodeAuthError::Malformed),
        );
    }

    #[test]
    fn trailing_bytes_after_a_binding_are_malformed() {
        let (conn, account) = db_with_account();
        let mut binding = sign_local(&conn, account, &NODE, NOW);
        binding.push(0xff); // one byte past a valid binding
        assert_eq!(
            verify_node_binding(&conn, account, &binding, &NODE, NOW).unwrap(),
            Err(NodeAuthError::Malformed),
        );
    }

    #[test]
    fn signing_without_a_local_device_reports_no_local_device() {
        // A store with schema but no account has no device to sign with.
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = AccountId::from_bytes([1u8; 32]);
        assert_eq!(
            sign_local_node_binding(&conn, account, &NODE, NOW).unwrap(),
            Err(NodeAuthError::NoLocalDevice),
        );
    }
}
