//! Memory op-log: the op model + a deterministic projection fold (phase B, §5.4).
//!
//! A pure, in-memory, crypto/network-free primitive frozen in isolation — the op-log's ordering
//! semantics without any of its transport. It has two parts:
//! - [`op`]: the frozen [`op::MemoryOp`] set, its canonical CBOR wire form
//!   ([`op::encode`]/[`op::decode`]), and the known/unknown split ([`op::DecodedOp`]) that keeps a
//!   forward-version op opaque-but-retained.
//! - [`project`]: the LWW [`project::project`] fold from a Lamport/device-ordered `[Entry]` to a
//!   converged [`project::ProjectedState`].
//!
//! Nothing here is wired into the live write path yet (a later increment materializes the fold into
//! the SQL tables) — this mirrors the `content_hash` freeze: pin the semantic primitive first, in
//! isolation-testable form. See `issue-489-plan.md`.

mod op;
mod project;
