//! Graph/edge/logical-symbol index lifecycle: resolve edges, (re)build logical symbols, graph
//! coverage, and graph-index freshness.

mod drift_heal;
mod freshness;
mod logical_key;
mod references;

pub(super) use drift_heal::LogicalKeyDriftRow;
pub(super) use logical_key::{
    KeyVersionStamp, LogicalGroupingUpkeep, LogicalSymbolKey, LogicalSymbolMemberRow,
};
pub(crate) use references::{realign_logical_symbol_ids, resolve_synced_symbol_anchors};
