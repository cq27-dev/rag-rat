//! The `lens` read module: HTTP-agnostic compositions backing the editor-lens
//! endpoint contract (#216). Each method mirrors one `/api/*` shape consumed by
//! the editor extension; the serve layer (MCP thread / `rag-rat serve`) is a
//! thin serializer over these.
//!
//! `mod.rs` is the curated index; compositions live in cohesive siblings.

mod chunks;
pub(crate) mod clones;
mod enrichments;
mod files;
mod handles;
mod hops;
mod status;
mod treemap;

pub use chunks::LensChunkText;
pub use clones::LensCloneGraphCache;
pub use enrichments::{
    LensCouplingPartner, LensDecisionRecord, LensFileCoupling, LensFileMemories, LensFileMemory,
    LensFilePapertrail, LensPapertrailRef,
};
pub use files::{
    LensDispatchDetail, LensFileAnswer, LensFileGraph, LensFileSymbolGraph, LensFileSymbols,
    LensGraphCallerCounts, LensSymbol,
};
pub use hops::{LensCallees, LensCallers, LensHopResolvedBy, LensHopSelector, LensSymbolHop};
pub use status::{LensLaneVersions, LensStatus, LensVersion};
pub use treemap::{LensTreemap, LensTreemapFile};

#[cfg(test)]
mod tests;
