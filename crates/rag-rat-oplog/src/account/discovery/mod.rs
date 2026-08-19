//! Sealing a discovery announcement to the account's roster-effective devices (#1080).
//!
//! A discovery announcement carries this node's iroh node id so the account's other devices can
//! dial it. Published in the clear, that hands the shared discovery service — and anyone who can
//! compute the account's tag, including a device removed from the roster — a list of dialable
//! identities. Sealed per recipient, it hands them opaque bytes.
//!
//! **Why per recipient rather than under one shared account key.** A device that has been offline
//! for a month and a device that has been removed look identical to the discovery service, so any
//! scheme keyed on a shared rotating secret revokes the removed device and strands the offline one
//! in the same stroke. They do not look identical to the PUBLISHER: the offline device is still
//! roster-effective and its long-term X25519 key never rotates, while the removed device is simply
//! not in the roster the publisher reads at seal time. Sealing per recipient therefore cuts exactly
//! along roster membership, with no key epoch, no rotation, and nothing to distribute.
//!
//! Revocation applies to what is sealed NEXT. An announcement sealed before a device was removed
//! stays openable by it until that announcement expires.
//!
//! Both halves live here, in the op-log crate, because neither the device's X25519 secret nor the
//! roster's public keys may leave it — see [`crate::identity`].

use anyhow::Context;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::keywrap::{self, ContentKey, SealedKeyWrap, WrapContext};
use super::{bootstrap, storage};
use crate::device::DeviceX25519Public;
use crate::identity::{self, LocalDevice};
use crate::op::DeviceFingerprint;

/// The envelope's leading byte. Bumping it is a wire break; the fetch side drops what it does not
/// recognise rather than guessing.
///
/// **Version 1 is the first format that ships.** The cleartext 32-byte node id this replaced never
/// appeared in a released version, so there is no deployed client to stay compatible with and no
/// transition to provide. That will not be true of the next change: publishers and fetchers share
/// one tag and one ALPN with no negotiation, so a future format that simply bumped this byte would
/// partition a mixed fleet — each side silently discarding the other's announcements — for as long
/// as the upgrade took, with discovery falling back to `server_peers` and nothing reporting why.
/// A later format therefore needs a transition: publish both encodings for a release, or key the
/// tag by version so the two populations do not share a namespace.
pub const ANNOUNCEMENT_VERSION: u8 = 1;

/// One sealed wrap on the wire: the ephemeral public key then the tagged ciphertext.
///
/// Public because the envelope's size law — one version byte plus this per roster-effective device
/// — is what the publish side's recipient ceiling is computed from. A publisher that spelled `80`
/// beside its own byte limit would keep sealing envelopes the service refuses the moment the wrap
/// layout changed here, and the refusal reads like any other transient publish error.
pub const WRAP_LEN: usize = 32 + 48;

/// The `key_epoch` slot of the wrap context. Discovery has no epochs — the whole point of sealing
/// per recipient is that nothing rotates — so it is pinned at zero and covered by the golden vector
/// rather than left to drift.
const DISCOVERY_KEY_EPOCH: u64 = 0;

/// A digest of the recipient set an announcement was sealed to.
///
/// The reason this exists: **sealing is not deterministic.** Every wrap carries a fresh ephemeral,
/// so two seals of the same node id to an unchanged roster produce different bytes. A caller that
/// wants to re-seal only when the roster actually moved therefore cannot compare envelopes — that
/// predicate is true every time, and acting on it means republishing on every tick, which is the
/// self-inflicted tag exhaustion the whole seal-once design exists to avoid.
///
/// Compare these instead. It is a digest, not the roster: fingerprints and public keys stay in this
/// crate.
pub type RosterStamp = [u8; 32];

/// A sealed announcement, with the recipient count the caller needs for policy it owns.
///
/// It carries no roster stamp: whether a re-seal is owed is decided by comparing [`roster_stamp`]
/// across sessions, not by anything a single seal reports — see [`RosterStamp`] for why the
/// envelope bytes cannot serve that purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedAnnouncement {
    /// `version || wrap*`, raw concatenation. Not CBOR: the payload already travels as an opaque
    /// byte string, so a second CBOR layer would add nothing and cost about 30% against a size
    /// budget that decides how many announcements fit one response frame.
    pub bytes: Vec<u8>,
    /// How many devices can open it — the whole roster-effective set, including this one.
    pub recipients: usize,
}

/// Seal `node_id` to every roster-effective device, bound to `tag`.
///
/// `Ok(None)` when this store has no account: there is no roster to seal to, and nothing to
/// discover. Errors only on a real database or crypto failure.
///
/// **This node is among the recipients.** Not for its own sake but because a publisher is also a
/// device: it fetches on its own device-side pass, and a host that could not open announcements it
/// had itself published would be a confusing hole the first time anyone looked.
pub fn seal_discovery_announcement(
    conn: &Connection,
    tag: &[u8; 32],
    node_id: &[u8; 32],
) -> anyhow::Result<Option<SealedAnnouncement>> {
    let Some(account) = bootstrap::read_local_account(conn)? else {
        return Ok(None);
    };
    let recipients = storage::list_effective_roster_x25519_pubkeys(conn, account)?;
    if recipients.is_empty() {
        return Ok(None);
    }

    // The node id is 32 bytes of material to seal; `ContentKey` is the crate's 32-byte sealable
    // payload and carries the zeroize-on-drop this wants anyway. It is not a content key.
    let payload = ContentKey::from_seed(node_id);
    let mut bytes = Vec::with_capacity(1 + recipients.len() * WRAP_LEN);
    bytes.push(ANNOUNCEMENT_VERSION);
    for (fingerprint, recipient_pub) in &recipients {
        let ctx = wrap_context(account, tag, &recipient_pub.to_bytes());
        let sealed = keywrap::seal_content_key(&payload, &ctx, recipient_pub)
            .with_context(|| format!("sealing a discovery announcement to device {fingerprint}"))?;
        push_wrap(&mut bytes, &sealed);
    }
    Ok(Some(SealedAnnouncement { bytes, recipients: recipients.len() }))
}

/// The current recipient set's stamp, without sealing anything.
///
/// Cheap enough to call on a cadence — it is one roster read — which is the point: a long-running
/// host checks this between sessions and only pays for a re-seal when it changes.
pub fn roster_stamp(conn: &Connection) -> anyhow::Result<Option<RosterStamp>> {
    let Some(account) = bootstrap::read_local_account(conn)? else {
        return Ok(None);
    };
    Ok(Some(stamp_of(&storage::list_effective_roster_x25519_pubkeys(conn, account)?)))
}

/// Digest the recipient set. Order is already stable — the roster read sorts by fingerprint — and
/// both the fingerprint and the key go in, so a device whose enrolment key changed counts as a
/// different recipient even at the same fingerprint.
fn stamp_of(recipients: &[(DeviceFingerprint, DeviceX25519Public)]) -> RosterStamp {
    let mut hasher = Sha256::new();
    hasher.update(b"rag-rat/discovery-roster-stamp/1");
    for (fingerprint, public) in recipients {
        hasher.update(fingerprint.to_bytes());
        hasher.update(public.to_bytes());
    }
    hasher.finalize().into()
}

/// Everything opening an announcement needs that does not vary with the announcement: this
/// device's X25519 key and the wrap context the account and tag determine.
///
/// **Why the state is loaded up front rather than per announcement.** One fetch returns up to the
/// service's per-response cap of payloads, and a payload that does not open spends no peer budget,
/// so the opening path runs for every one of them. Only the unwrap depends on the envelope; the
/// account read, the device load — which re-derives and re-validates the stored keys — and the
/// context are the same answer each time, so they are paid for once and carried.
///
/// It holds no connection, so a loaded opener cannot reach the database again. That is the point:
/// hold one for the length of a fetch pass and drop it after, since a roster or identity change
/// mid-pass is not something the pass is expected to observe.
pub struct AnnouncementOpener {
    device: LocalDevice,
    ctx: WrapContext,
}

impl AnnouncementOpener {
    /// Load the per-pass state, or `None` when this store has no account or no local device — in
    /// which case nothing it fetches under `tag` is openable, for the whole pass.
    pub fn load(conn: &Connection, tag: &[u8; 32]) -> anyhow::Result<Option<Self>> {
        let Some(account) = bootstrap::read_local_account(conn)? else {
            return Ok(None);
        };
        let Some(device) = identity::load_local_device(conn)? else {
            return Ok(None);
        };
        let ctx = wrap_context(account, tag, &device.x25519_public().to_bytes());
        Ok(Some(Self { device, ctx }))
    }

    /// Recover the node id from an announcement sealed to this device, or `None`.
    ///
    /// `None` covers every "not for us, or not ours to read" case: an unrecognised version, a
    /// malformed length, and — the ordinary case — no wrap that opens under this device's key.
    ///
    /// **Silent when a wrap fails its tag.** Every announcement carries one wrap per roster device,
    /// so all but one are expected to fail here. The content path records unwrap failures as
    /// security events; doing that here would write a security event per foreign wrap per
    /// announcement per discovery pass, burying the real ones.
    pub fn open(&self, envelope: &[u8]) -> Option<[u8; 32]> {
        parse_wraps(envelope)?.iter().find_map(|wrap| {
            // Failure here is the expected case, not an error: this device matches at most one
            // wrap.
            let opened =
                keywrap::unwrap_content_key(wrap, self.device.x25519_secret(), &self.ctx).ok()?;
            let mut node_id = [0u8; 32];
            node_id.copy_from_slice(opened.as_slice());
            Some(node_id)
        })
    }
}

/// The AAD every wrap is bound to.
///
/// The slot that carries a stream id for content keys carries the discovery TAG here, so a wrap
/// opens only under the tag it was published for and cannot be replayed under another. The account
/// id is included for the same reason it is there for content keys: one device can belong to
/// several accounts.
fn wrap_context(
    account: super::AccountId,
    tag: &[u8; 32],
    recipient_pub: &[u8; 32],
) -> WrapContext {
    WrapContext {
        account_id: account.to_bytes(),
        stream_id: *tag,
        key_epoch: DISCOVERY_KEY_EPOCH,
        recipient_pub: *recipient_pub,
    }
}

fn push_wrap(bytes: &mut Vec<u8>, sealed: &SealedKeyWrap) {
    bytes.extend_from_slice(&sealed.ephemeral_pubkey);
    bytes.extend_from_slice(&sealed.ciphertext);
}

/// Split an envelope into its wraps, or `None` if it is not one of ours.
///
/// Rejects a wrong version and any length that is not exactly one version byte plus a whole number
/// of wraps — including the empty case, which no publisher produces. A legacy raw 32-byte node id
/// fails here on length even if its first byte happens to equal the version.
fn parse_wraps(envelope: &[u8]) -> Option<Vec<SealedKeyWrap>> {
    let (&version, rest) = envelope.split_first()?;
    if version != ANNOUNCEMENT_VERSION || rest.is_empty() || rest.len() % WRAP_LEN != 0 {
        return None;
    }
    Some(
        rest.as_chunks::<WRAP_LEN>()
            .0
            .iter()
            .map(|chunk| {
                let mut ephemeral_pubkey = [0u8; 32];
                let mut ciphertext = [0u8; 48];
                ephemeral_pubkey.copy_from_slice(&chunk[..32]);
                ciphertext.copy_from_slice(&chunk[32..]);
                SealedKeyWrap { ephemeral_pubkey, ciphertext }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
