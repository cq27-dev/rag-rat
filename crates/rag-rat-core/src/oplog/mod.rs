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

mod cbor;
mod device;
mod entry;
mod identity;
mod op;
mod project;
mod store;
mod stream;
