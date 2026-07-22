//! The canonical account projection and its `folded_state_hash` (C6b, #609).
//!
//! A snapshot manifest claims "I folded exactly this prefix and got this state". The state itself
//! never rides the wire — the §18a envelope could not hold it — so the claim is a 32-byte hash and
//! a peer verifies by re-folding the covered prefix and recomputing. That makes this encoding a
//! **frozen wire in every sense that matters**: two honest devices must produce byte-identical
//! bytes from the same fold, or verification fails between correct implementations. It is pinned by
//! golden vectors exactly like the manifest wire, and changing it means bumping
//! `SNAPSHOT_STATE_FORMAT_V1`.
//!
//! # Determinism is the whole job
//!
//! Every collection in [`super::super::fold::AccountAuthHistory`] is a `HashMap`/`HashSet`, whose
//! iteration order varies run to run. Nothing here may iterate one directly into the encoder: each
//! collection is materialized, sorted by a total key, and only then written. `encoded` is the only
//! place that ordering is established, and `a_shuffled_fold_produces_identical_bytes` is what keeps
//! it honest — it builds the same logical account through different arrival orders and asserts the
//! bytes match.
//!
//! # What it binds, and why that set
//!
//! The authority projection in full: the account classification and its recovery successor, the
//! effective set (each entry with the `auth_epoch` it took), roster facts, owner incarnations,
//! stream ownership, grants and their cuts, and the tombstone set. Two of those deserve a note.
//!
//! The **effective set** is bound explicitly rather than left implicit in the derived facts. The
//! facts are a function of it, but not an injective one — two folds could agree on every roster and
//! grant fact while disagreeing about which entries were effective (an entry that mints nothing
//! moves between `parked` and `condemned` without touching a fact). Since the whole purpose is
//! convergence cross-checking, the set that must converge is bound directly.
//!
//! The **tombstone set** is bound because I4 (a tombstoned fingerprint never re-enrolls) is
//! otherwise unverifiable from a snapshot: a manifest that omitted tombstones would let a peer
//! bootstrap into a state that re-admits a removed device. The set is total, never windowed —
//! growth is `DeviceRemove`-rate at 32 bytes each, so a pathological account is kilobytes.
//!
//! What it does NOT bind: per-entry outcomes for non-effective entries. Park reasons are
//! recoverable local state (a park heals when the missing input arrives) rather than converged
//! verdicts, and binding them would make the hash disagree between two peers that are both correct
//! and merely differently supplied.

use minicbor::Encoder;

use super::super::fold::{AccountAuthHistory, AccountClassification, AuthorityBoundary};
use super::super::ops::DeviceCut;
use crate::cbor;

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible).
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// Domain tag for the canonical projection. Changing what this encoding covers is a
/// `SNAPSHOT_STATE_FORMAT_V1` bump, and this tag is what makes a hash from one encoding
/// unmistakable for a hash from another.
const PROJECTION_DOMAIN: &str = "rag-rat/account-projection/1";

/// The hash a snapshot manifest claims for a covered prefix: `sha256` of [`encoded`].
pub(in crate::account) fn folded_state_hash(history: &AccountAuthHistory) -> [u8; 32] {
    cbor::sha256(&encoded(history))
}

/// The canonical bytes. Deterministic by construction: every collection is sorted by a total key
/// before it is written, and no `HashMap`/`HashSet` is ever iterated straight into the encoder.
pub(in crate::account) fn encoded(history: &AccountAuthHistory) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
    let mut enc = Encoder::new(&mut buf);
    // ELEVEN top-level items, and the count is load-bearing: a short array makes a conforming
    // decoder treat the tail as trailing values outside the frame, even though every byte is still
    // hashed. Keep this in step with the writes below — domain, classification, successor,
    // effective_count, then the seven collections.
    enc.array(11).expect(INFALLIBLE);
    enc.str(PROJECTION_DOMAIN).expect(INFALLIBLE);

    // 1. Classification. `Contested` carries the depth its pre-contest state was frozen at, which
    //    is part of the state a peer must agree on, not a local detail.
    match history.classification() {
        AccountClassification::Live => {
            enc.array(1).expect(INFALLIBLE);
            enc.u8(0).expect(INFALLIBLE);
        },
        AccountClassification::Contested { state_before_depth } => {
            enc.array(2).expect(INFALLIBLE);
            enc.u8(1).expect(INFALLIBLE);
            enc.u64(state_before_depth as u64).expect(INFALLIBLE);
        },
    }

    // 2. The deterministic recovery successor (§12), when contested.
    match history.contested_successor() {
        Some(account) => enc.bytes(&account.to_bytes()).expect(INFALLIBLE),
        None => enc.null().expect(INFALLIBLE),
    };

    enc.u64(history.effective_count()).expect(INFALLIBLE);

    // 3. The effective set, sorted by entry hash.
    let mut effective: Vec<([u8; 32], u64)> = history.effective_entries().collect();
    effective.sort_unstable();
    enc.array(effective.len() as u64).expect(INFALLIBLE);
    for (hash, auth_epoch) in effective {
        enc.array(2).expect(INFALLIBLE);
        enc.bytes(&hash).expect(INFALLIBLE);
        enc.u64(auth_epoch).expect(INFALLIBLE);
    }

    // 4. Roster facts, sorted by the entry hash that minted them.
    let mut roster: Vec<_> = history.roster_facts().collect();
    roster.sort_unstable_by_key(|(hash, _)| **hash);
    enc.array(roster.len() as u64).expect(INFALLIBLE);
    for (hash, fact) in roster {
        enc.array(7).expect(INFALLIBLE);
        enc.bytes(hash).expect(INFALLIBLE);
        enc.bytes(&fact.authority.device_fingerprint.to_bytes()).expect(INFALLIBLE);
        enc.u8(fact.authority.current_role.as_u8()).expect(INFALLIBLE);
        write_window(&mut enc, fact.effective_at, fact.closed_at);
        write_boundary(&mut enc, fact.control_boundary);
        write_boundary(&mut enc, fact.secrets_boundary);
        let mut content: Vec<_> = fact.content_boundaries.iter().collect();
        content.sort_unstable_by_key(|(stream, _)| stream.to_bytes());
        enc.array(content.len() as u64).expect(INFALLIBLE);
        for (stream, boundary) in content {
            enc.array(2).expect(INFALLIBLE);
            enc.bytes(&stream.to_bytes()).expect(INFALLIBLE);
            write_boundary(&mut enc, *boundary);
        }
    }

    // 5. Owner incarnations, sorted by mint hash.
    let mut incarnations: Vec<_> = history.owner_incarnation_facts().collect();
    incarnations.sort_unstable_by_key(|(hash, _)| **hash);
    enc.array(incarnations.len() as u64).expect(INFALLIBLE);
    for (hash, fact) in incarnations {
        enc.array(5).expect(INFALLIBLE);
        enc.bytes(hash).expect(INFALLIBLE);
        enc.bytes(&fact.authority.device_fingerprint.to_bytes()).expect(INFALLIBLE);
        write_window(&mut enc, fact.effective_at, fact.closed_at);
        write_boundary(&mut enc, fact.control_boundary);
        write_boundary(&mut enc, fact.secrets_boundary);
    }

    // 6. Stream ownership, sorted by stream.
    let mut ownership: Vec<_> = history.stream_ownership_facts().collect();
    ownership.sort_unstable_by_key(|(stream, _)| stream.to_bytes());
    enc.array(ownership.len() as u64).expect(INFALLIBLE);
    for (stream, fact) in ownership {
        enc.array(3).expect(INFALLIBLE);
        enc.bytes(&stream.to_bytes()).expect(INFALLIBLE);
        enc.bytes(&fact.own_id).expect(INFALLIBLE);
        enc.u64(fact.effective_at).expect(INFALLIBLE);
    }

    // 7. Grants, sorted by grant id.
    let mut grants: Vec<_> = history.grant_facts().collect();
    grants.sort_unstable_by_key(|(id, _)| **id);
    enc.array(grants.len() as u64).expect(INFALLIBLE);
    for (grant_id, fact) in grants {
        // FIVE items: grant_id, stream, grantee, role, window — `write_window` emits ONE array, not
        // two values. A miscounted nested arity makes the decoder consume following top-level items
        // to fill this one, which is how a malformed frame hides behind a stable hash.
        enc.array(5).expect(INFALLIBLE);
        enc.bytes(grant_id).expect(INFALLIBLE);
        enc.bytes(&fact.authority.stream_id.to_bytes()).expect(INFALLIBLE);
        enc.bytes(&fact.authority.grantee_account_id.to_bytes()).expect(INFALLIBLE);
        enc.u8(fact.authority.role.as_u8()).expect(INFALLIBLE);
        write_window(&mut enc, fact.effective_at, fact.closed_at);
    }

    // 8. Grant cuts, sorted by grant id then by the device each cut names.
    let mut grant_cuts: Vec<_> = history.grant_cuts().collect();
    grant_cuts.sort_unstable_by_key(|(id, _)| **id);
    enc.array(grant_cuts.len() as u64).expect(INFALLIBLE);
    for (grant_id, cuts) in grant_cuts {
        enc.array(2).expect(INFALLIBLE);
        enc.bytes(grant_id).expect(INFALLIBLE);
        let mut sorted: Vec<&DeviceCut> = cuts.iter().collect();
        sorted.sort_unstable_by_key(|cut| (cut.device_fingerprint.to_bytes(), cut.seq, cut.hash));
        enc.array(sorted.len() as u64).expect(INFALLIBLE);
        for cut in sorted {
            enc.array(3).expect(INFALLIBLE);
            enc.bytes(&cut.device_fingerprint.to_bytes()).expect(INFALLIBLE);
            enc.u64(cut.seq).expect(INFALLIBLE);
            enc.bytes(&cut.hash).expect(INFALLIBLE);
        }
    }

    // 9. Tombstones, sorted. Total, never windowed — I4 depends on exact membership.
    let mut tombstoned: Vec<[u8; 32]> =
        history.tombstoned().map(|fingerprint| fingerprint.to_bytes()).collect();
    tombstoned.sort_unstable();
    enc.array(tombstoned.len() as u64).expect(INFALLIBLE);
    for fingerprint in tombstoned {
        enc.bytes(&fingerprint).expect(INFALLIBLE);
    }

    buf
}

/// An `(effective_at, closed_at)` validity window. Written as a pair so a closed fact can never
/// encode identically to an open one at the same epoch.
fn write_window(enc: &mut Encoder<&mut Vec<u8>>, effective_at: u64, closed_at: Option<u64>) {
    enc.array(2).expect(INFALLIBLE);
    enc.u64(effective_at).expect(INFALLIBLE);
    match closed_at {
        Some(at) => enc.u64(at).expect(INFALLIBLE),
        None => enc.null().expect(INFALLIBLE),
    };
}

/// `Open`/`Closed` are discriminants alone; `Cut` carries its coordinate. Encoded as a tagged array
/// rather than a bare integer so a future boundary variant cannot collide with an existing one.
fn write_boundary(enc: &mut Encoder<&mut Vec<u8>>, boundary: AuthorityBoundary) {
    match boundary {
        AuthorityBoundary::Open => {
            enc.array(1).expect(INFALLIBLE);
            enc.u8(0).expect(INFALLIBLE);
        },
        AuthorityBoundary::Cut { seq, hash } => {
            enc.array(3).expect(INFALLIBLE);
            enc.u8(1).expect(INFALLIBLE);
            enc.u64(seq).expect(INFALLIBLE);
            enc.bytes(&hash).expect(INFALLIBLE);
        },
        AuthorityBoundary::Closed => {
            enc.array(1).expect(INFALLIBLE);
            enc.u8(2).expect(INFALLIBLE);
        },
    }
}
