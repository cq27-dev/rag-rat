//! End-to-end tests for the SCIP-oracle join. SCIP `Index` objects are built **programmatically**
//! via the `scip` crate's types (no rust-analyzer, no network) and serialized, then fed through the
//! real `run_oracle` path against a DB seeded with synthetic files/symbols/edges. This keeps the
//! join deterministic and exercises the exact code eval uses.

use ::protobuf::{EnumOrUnknown, Message};
use ::scip::types::{Document, Index, Occurrence, PositionEncoding, SymbolRole};
use rag_rat_db::schema;
use rusqlite::{Connection, params};

use super::*;
use crate::store::EdgeOracleRow;
use crate::test_support::*;

mod edge_view;
mod join_tests;
mod library_usage;
mod monikers;
mod persisted_enums;
mod pre_spawn;
mod production;
mod reports;
mod resolution;
mod run_eval;
mod schema_tests;
mod scip_parse;
mod scope;
mod status_tests;
mod store_io;
mod surfacing;
mod tool_defaults;
