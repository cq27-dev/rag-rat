//! Endpoint tests, grouped by subject.

mod dispatch;
mod enrollment;
mod fixtures;
mod limits;
mod peers;
mod reconcile;

use std::collections::HashSet;
use std::str::FromStr;

use fixtures::{NOW, database, direct_addr, loopback_endpoints, test_entry};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use rag_rat_oplog::AccountId;
use rusqlite::Connection;
use tokio::time::timeout;

use super::accept::{
    ACCEPT_BURST, ACCEPT_REFILL_PER_SEC, EGRESS_BURST_BYTES, GRACEFUL_CLOSE_TIMEOUT,
};
use super::dial::{ReconcileStep, RoundTally, reconcile_step};
use super::dispatch::serve_scope_for;
use super::enroll::{enrollment_database_matches, validate_enrollment_request_identity};
use super::*;
use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, PeerAdmission, PeerAuthorization,
    PeerCapability, run_auth_phase,
};
use crate::enrollment::{ENROLL_ALPN, EnrollmentRequest, InviteError};
use crate::session::{DEFAULT_IDLE_TIMEOUT, ServeScope};
use crate::table_session::{TableSessionReport, TableSyncStore, run_table_session};
use crate::table_wire::TABLE_SYNC_ALPN;
use crate::testing::{TableTestStore, TestStore};
use crate::wire::{CONTENT_SYNC_ALPN, SYNC_ALPN};
