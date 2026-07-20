//! Memory op-log: the op model, a deterministic projection fold, and the signed hash-chained entry
//! envelope (phase B, layer 1).
//!
//! A pure, in-memory primitive frozen in isolation — the op-log's ordering + integrity semantics
//! without any transport or storage. Parts:
//! - [`op`]: the frozen [`op::MemoryOp`] set, its canonical CBOR wire form
//!   ([`op::encode`]/[`op::decode`]), and the known/unknown split ([`op::DecodedOp`]) that keeps a
//!   forward-version op opaque-but-retained.
//! - [`project`]: the LWW [`project::project`] fold from a Lamport/device-ordered `[op::Entry]` to
//!   a converged [`project::ProjectedState`].
//! - [`device`]: the ed25519 device identity — [`device::DeviceSecret`] signs,
//!   [`device::DevicePublic`] verifies (`verify_strict`) and derives the op model's opaque
//!   `DeviceFingerprint` = `sha256(pubkey)`.
//! - [`entry`]: the signed, hash-chained entry envelope over an OPAQUE op — sign / verify /
//!   per-`(stream, device)` chain-verify. The signature covers the canonical body `[domain,
//!   stream_id, prev_hash, lamport, device_fingerprint, op_bytes]`; `entry_hash = sha256(body)`
//!   links the chain, so an UNKNOWN op still verifies + chains.
//! - [`cbor`]: the shared canonical-CBOR discipline (definite lengths, minimal headers, sorted map
//!   keys, no trailing) both wire layers enforce.
//!
//! - [`store`]: the durable SQLite seam — the layer-1 opaque signed entry log (verified,
//!   chain-continuous, idempotent [`store::append`]) plus the layer-2 full-replay projection into
//!   oplog-owned shadow tables, kept in sync atomically.
//! - [`stream`]: the immutable, content-derived stream identity (#509) — one signed chain,
//!   watermark, and projection exists PER [`stream::StreamId`]; the entry body binds it, so stream
//!   membership is signature-protected.
//! - [`identity`]: the store's ONE persisted ed25519 device identity (#513) —
//!   [`identity::local_device`] mints it from OS entropy on first use and returns it stably
//!   thereafter, so every authored entry signs under one machine fingerprint.
//!
//! Nothing here is wired into the live write path yet (later increments add the append-on-mutation
//! seam, roster/epochs, and transport) — this mirrors the `content_hash` freeze: pin the semantic
//! primitive first, in isolation-testable form.

// The authoring half is wired into the memory write path (#532), but the SYNC-TRANSPORT half —
// `append` (receiving a foreign signed entry), the fork quarantine, `AppendOutcome`, and the C4/C5
// authority primitives that precede their consumers (#607) — is still unconsumed frozen
// scaffolding, so the crate keeps `allow(dead_code)` (carried over from the module's own
// suppression before the #706-phase-8 extraction).
#![allow(dead_code)]

/// Migration hooks for oplog's own tests. The only migration hook oplog's schema uses is its OWN
/// `backfill_authority_projection` (the V064 forward-migration backfill over populated account
/// histories); the other domains' hooks are irrelevant here, so they stay noop. Wiring the crate's
/// own function means the tests need no dev-dependency on rag-rat-core's `migration_hooks()`.
#[cfg(test)]
pub(crate) fn test_hooks() -> rag_rat_db::MigrationHooks {
    rag_rat_db::MigrationHooks {
        backfill_authority_projection: account::backfill_authority_projection,
        ..rag_rat_db::MigrationHooks::noop()
    }
}

mod account;
mod cbor;
mod content_projection;
mod device;
mod entry;
mod identity;
mod op;
mod project;
mod store;
mod stream;

// C1's curated authority seam for the C2 `/3` content envelope and candidate DAG. The account
// implementation stays private; only typed ingest results and snapshot-consistent point queries
// cross the phase boundary.
//
// The C3.4 local-authoring surface `query::memory` reaches for once #664 retargets the live memory
// path onto the owner-bound `/2`//3 substrate:
// - `local_account` (C3.4a, #662): the store-global principal owner-bound `/3` content authors
//   under.
// - `ensure_owned_stream_v2_in_tx` (C3.4b-ii, #676): publish + resolve a repo's owned `/2` stream.
// - `owned_stream_v2_id` / `established_owned_stream_v2` (C3.4b-ii, #676): the pure stream resolver
//   and the effective-ownership fast-path probe.
// - `author_content_batch_in_tx` (C3.4b-i, #663): author a batch of ops as owner-authored `/3`
//   content, verify-accepted in the caller's txn.
// - `content_stream_is_empty` (C3.4b-i, #663): the `/3` genesis-detection reader.
// - `mint_and_author_stream_key_wrap_in_tx` (C4.3a, #607): mint a per-stream content key and author
//   an owner-gated `StreamKeyWrap` sealing it to every effective device, verify-accepted in the
//   caller's txn.
// - `rotate_stream_key_in_tx` / `ensure_stream_key_current_in_tx` / `stream_key_rotation_needed`
//   (C4.4, #607): lazy content-key rotation on device removal — re-seal a fresh higher-epoch key to
//   the remaining roster when a removed device still holds the current key.
// `prepare_content_authoring` / `author_prepared_content_batch_in_tx` / `SealPolicy` (C5, #608):
// policy-aware `/3` authoring prepared before, then executed inside, a caller-owned transaction;
// `author_content_batch` is the self-transacting convenience composition. The
// envelope-layer seal
// primitives (`sign_sealed_content_entry` / `seal_and_sign_content_entry`) take a `&DeviceSecret`,
// so — like `sign_content_entry` — they stay account-crate-internal (never re-exported at the crate
// root, or they would leak the `pub(crate)` `DeviceSecret` past its visibility).
pub use account::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery,
    CapacityScope, CatchUpReport, ContentKey, ContentKeyring, DeviceCut, DeviceRole,
    GrantAuthority, GrantDeviceAuthority, GrantDeviceBoundary, GrantRole, IngestOutcome, KeyId,
    LiveKeyEpoch, LiveKeyTargets, OwnerAuthority, OwnerChainAuthority, PreparedContentAuthoring,
    RosterContentAuthority, RotationOutcome, SealPolicy, SealingKeyOutcome, SelectedWrap,
    account_ingest, auth_len_freshness, author_content_batch, author_content_batch_in_tx,
    author_prepared_content_batch_in_tx, backfill_authority_projection,
    catch_up_stream_keys_for_device_in_tx, content_op_is_authorable,
    content_op_is_sealed_authorable, content_stream_has_sealed_ratchet, content_stream_is_empty,
    current_sealing_key, decode_content_signed, ensure_owned_stream_v2_in_tx,
    ensure_stream_key_current_in_tx, established_owned_stream_v2, grant_effective_for_device,
    historical_content_keyring, live_stream_key_targets_for_device, local_account,
    mint_and_author_stream_key_wrap_in_tx, owned_stream_v2_id, owner_control_authority,
    owner_secrets_authority, prepare_content_authoring, roster_content_authority,
    rotate_stream_key_in_tx, select_current_sealing_wrap, stream_key_rotation_needed,
    stream_owner_effective,
};
// The `/3` content projection's store-global upgrade re-fold (#688): wired into the index
// open/migrate seam by rag-rat-core, so a stale store is rebuilt (every stream, then the one
// stamp) before any per-stream write.
pub use content_projection::rebuild_all_content_projections_if_stale;
// The op-log's first crate-internal API surface (#524): the MINTING primitives + the op
// vocabulary the memory subsystem needs to author + backfill entries. Every submodule above is
// otherwise private, so this curated re-export is the ONE seam `query::memory` reaches through
// — and the only direction of the dependency (`oplog` never depends back on `query::memory`).
pub use identity::{LocalDevice, load_local_device, local_device};
pub use op::{EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus};
// The `/1` shadow-projection read seams (`ProjectedState` / `load_projection`) and the
// standalone (own-txn) `/1` authoring wrappers (`author_batch` / `author_op`) — test-only
// scaffolding for the retained `/1` store. The live memory path now authors owner-bound `/3`
// content (#664), so these `/1` seams have no non-test caller; the re-exports stay `allow`'d
// rather than removed because the `/1` store itself is retained (its history is not migrated,
// per J1).
pub use project::ProjectedState;
pub use store::{author_batch, author_op, load_projection};
pub use stream::StreamId;
