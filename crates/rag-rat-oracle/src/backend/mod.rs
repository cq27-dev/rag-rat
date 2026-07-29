//! The LIVE backend registry: what distinguishes one resident language server from another once
//! the shared client substrate is in place.
//!
//! [`crate::manifest::ToolManifest`] answers "which binary, with what argv, and can it run here?"
//! for every backend, batch and live alike. This module answers the questions only a *live* driver
//! asks: which language's files belong on its worklist, what `languageId` to open them under, and
//! which readiness signal the server actually emits. Adding a backend is one entry here plus one
//! manifest entry — not new protocol code.
//!
//! - `registry.rs` — the [`LiveBackend`] entries and every question the live driver asks of one.
//! - `layout.rs` — the whole-checkout project-marker search and the [`ProjectLayout`] it produces.
//! - `documents.rs` — the searches that find a document whose open would warm a session.

mod documents;
mod layout;
mod registry;
mod scope;
#[cfg(test)]
mod tests;

pub use layout::{LAYOUT_MAX_AGE, ProjectLayout};
pub use registry::LiveBackend;
pub use scope::{CheckoutScope, IndexedCorpus};
