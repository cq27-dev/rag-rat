//! Wiring for the #585 migration gate: opening/migrating an `Older` GLOBAL store from a dev build
//! must be refused through the real open paths (both funnels — `open_and_migrate` and
//! `apply_schema_under_lock`), while an explicit override lets it through. The decision matrix
//! itself is unit-tested in `index::migration_gate`; these tests pin that the live paths CALL it.
//!
//! The test process is itself a dev build (`cfg!(debug_assertions)` + a `target/` exe), so the gate
//! sees `is_dev_build == true` without any faking — exactly the incident configuration.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rusqlite::Connection;

use super::unique_temp_root;
use crate::index::{IndexDatabase, schema};

/// Serializes the env-mutating global-store tests (`RAG_RAT_DATA_DIR` / `RAG_RAT_ALLOW_MIGRATE` are
/// process-global). nextest's process-per-test isolation makes it moot in CI; the mutex keeps a
/// thread-based `cargo test` honest.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Point `data_dir::global_database_path` at a fresh temp dir and run `body` with the global store
/// path, optionally with `RAG_RAT_ALLOW_MIGRATE=1`. Restores the prior environment afterward.
fn with_global_store(allow_migrate: bool, body: impl FnOnce(&Path)) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let data_dir = unique_temp_root();
    std::fs::create_dir_all(&data_dir).unwrap();
    let global = data_dir.join("rag-rat.sqlite");

    let saved_data_dir = std::env::var_os("RAG_RAT_DATA_DIR");
    let saved_allow = std::env::var_os("RAG_RAT_ALLOW_MIGRATE");
    // SAFETY: env access is serialized by ENV_LOCK for the duration of this call.
    unsafe {
        std::env::set_var("RAG_RAT_DATA_DIR", &data_dir);
        if allow_migrate {
            std::env::set_var("RAG_RAT_ALLOW_MIGRATE", "1");
        } else {
            std::env::remove_var("RAG_RAT_ALLOW_MIGRATE");
        }
    }

    body(&global);

    // SAFETY: same serialized scope; restore the prior values.
    unsafe {
        match saved_data_dir {
            Some(value) => std::env::set_var("RAG_RAT_DATA_DIR", value),
            None => std::env::remove_var("RAG_RAT_DATA_DIR"),
        }
        match saved_allow {
            Some(value) => std::env::set_var("RAG_RAT_ALLOW_MIGRATE", value),
            None => std::env::remove_var("RAG_RAT_ALLOW_MIGRATE"),
        }
    }
}

/// Create the global store at `path` and roll it back to look `Older` (drop the newest migration
/// row so `current_version < LATEST`). First-time init (`Missing`) is never gated, so this succeeds
/// even from a dev build.
fn older_global_store(path: &Path) {
    IndexDatabase::migrate(path).expect("first-time global-store init is not gated");
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "DELETE FROM schema_version WHERE id = (SELECT id FROM schema_version ORDER BY id DESC \
         LIMIT 1)",
        [],
    )
    .unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().state,
        schema::SchemaState::Older,
        "fixture must be Older before the gated open"
    );
}

#[test]
fn dev_build_refuses_to_migrate_an_older_global_store_via_migrate() {
    with_global_store(false, |global| {
        older_global_store(global);
        let err = IndexDatabase::migrate(global)
            .expect_err("apply_schema_under_lock funnel must refuse the global store");
        assert!(err.to_string().contains("global"), "got: {err}");
    });
}

#[test]
fn dev_build_refuses_to_open_an_older_global_store() {
    with_global_store(false, |global| {
        older_global_store(global);
        let err = IndexDatabase::open(global)
            .map(|_| ())
            .expect_err("open_and_migrate funnel must refuse the global store");
        assert!(err.to_string().contains("global"), "got: {err}");
    });
}

#[test]
fn allow_migrate_override_lets_a_dev_build_migrate_the_global_store() {
    with_global_store(true, |global| {
        older_global_store(global);
        let status = IndexDatabase::migrate(global)
            .expect("RAG_RAT_ALLOW_MIGRATE=1 lets a dev build migrate the global store");
        assert_eq!(status.state, schema::SchemaState::Compatible);
    });
}
