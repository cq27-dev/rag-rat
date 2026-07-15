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
// C3.4a's local-account mint (#662): the store-global principal later C3.4 slices author
// owner-bound /3 content under. Unused until that caller lands, so the re-export is `expect`'d
// like the seam above.
// C3.4b-i's in-tx `/3` content-author seam (#663): the local writer that authors owner-bound
// `/3` content on a `/2` stream, verify-accepted in the caller's txn. Frozen until #664
// retargets the live memory path onto it.
#[expect(
    unused_imports,
    reason = "C3.4b-i content-author seam is frozen before its caller lands"
)]
pub(crate) use account::author_content_batch_in_tx;
// C3.4b-ii's in-tx `/2`-ownership ensure seam (#676): the idempotent
// ensure-the-repo's-`/2`-stream-is-owned primitive #664 calls before authoring owner-bound
// `/3` content. Frozen until that caller lands.
#[expect(
    unused_imports,
    reason = "C3.4b-ii /2-ownership ensure seam is frozen before its caller lands"
)]
pub(crate) use account::ensure_owned_stream_v2_in_tx;
#[expect(
    unused_imports,
    reason = "C3.4a local account mint is frozen before its caller lands"
)]
pub(crate) use account::local_account;
#[expect(unused_imports, reason = "C2 authority seam is frozen before its caller lands")]
pub(crate) use account::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery,
    CapacityScope, DeviceCut, DeviceRole, GrantAuthority, GrantDeviceAuthority,
    GrantDeviceBoundary, GrantRole, IngestOutcome, OwnerAuthority, OwnerChainAuthority,
    RosterContentAuthority, account_ingest, auth_len_freshness, backfill_authority_projection,
    grant_effective_for_device, owner_control_authority, owner_secrets_authority,
    roster_content_authority, stream_owner_effective,
};
// The op-log's first crate-internal API surface (#524): the MINTING primitives + the op
// vocabulary the memory subsystem needs to author + backfill entries. Every submodule above is
// otherwise private, so this curated re-export is the ONE seam `query::memory` reaches through
// — and the only direction of the dependency (`oplog` never depends back on `query::memory`).
pub(crate) use identity::local_device;
pub(crate) use op::{EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus};
// The reconcile's tests read the materialized shadow projection as a `ProjectedState` via
// `load_projection` (below) — test-only today (the live path folds in-txn), so `allow`'d.
#[allow(unused_imports)]
pub(crate) use project::ProjectedState;
// `author_batch_in_tx` is the gate-free batch-author primitive (#541) the reconcile authors
// its ghost batch through (`query::memory::authoring::sync_owner_stream`).
pub(crate) use store::author_batch_in_tx;
// `load_projection` materializes a `ProjectedState` from the shadow tables — the
// reconcile-test read seam above; only test-used for now, so `allow`'d.
#[allow(unused_imports)]
pub(crate) use store::load_projection;
// `author_batch` / `author_op` are the standalone (own-txn) wrappers — `author_batch` is only
// test-used and `author_op` is only reached from a test helper, so their re-export stays
// `allow(unused_imports)`. The live write path uses the in-txn primitives.
#[allow(unused_imports)]
pub(crate) use store::{author_batch, author_op};
pub(crate) use store::{author_in_tx, chain_tail};
pub(crate) use stream::{StreamId, owner_stream};
