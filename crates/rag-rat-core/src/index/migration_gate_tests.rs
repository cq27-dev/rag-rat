use std::fs;

use tempfile::TempDir;

use super::*;

/// A gate whose global store is `global`, with the dev/override knobs set explicitly.
fn gate(global: &Path, is_dev_build: bool, allow_override: bool) -> MigrationGate {
    MigrationGate { is_dev_build, allow_override, global_db_path: Some(global.to_path_buf()) }
}

/// Create the global store file so `canonicalize` resolves it.
fn global_store() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rag-rat.sqlite");
    fs::write(&path, b"db").unwrap();
    (dir, path)
}

#[test]
fn a_release_version_is_not_a_dev_build() {
    assert!(!version_indicates_dev_build("0.15.0"));
    assert!(!version_indicates_dev_build("1.2.3"));
}

#[test]
fn a_git_stamped_version_is_a_dev_build() {
    assert!(version_indicates_dev_build("0.15.0+g7eb89f28b5ae"));
    assert!(version_indicates_dev_build("0.15.0+g7eb89f28b5ae.dirty"));
}

#[test]
fn dev_build_refuses_to_migrate_the_global_store() {
    let (_dir, global) = global_store();
    let err = gate(&global, true, false)
        .ensure_migration_permitted(&global, SchemaState::Older)
        .expect_err("a dev build must refuse to migrate the global store");
    assert!(err.to_string().contains("global"), "refusal should name the global store; got: {err}");
}

#[test]
fn override_lets_a_dev_build_migrate_the_global_store() {
    let (_dir, global) = global_store();
    gate(&global, true, true)
        .ensure_migration_permitted(&global, SchemaState::Older)
        .expect("RAG_RAT_ALLOW_MIGRATE overrides the gate");
}

#[test]
fn installed_build_migrates_the_global_store() {
    let (_dir, global) = global_store();
    gate(&global, false, false)
        .ensure_migration_permitted(&global, SchemaState::Older)
        .expect("an installed release binary may migrate the global store");
}

#[test]
fn dev_build_migrates_a_per_repo_store() {
    let (_dir, global) = global_store();
    let per_repo = _dir.path().join("index.sqlite");
    fs::write(&per_repo, b"db").unwrap();
    gate(&global, true, false)
        .ensure_migration_permitted(&per_repo, SchemaState::Older)
        .expect("a per-repo/temp DB is never gated");
}

#[test]
fn dev_build_may_initialize_a_missing_global_store() {
    let (_dir, global) = global_store();
    gate(&global, true, false)
        .ensure_migration_permitted(&global, SchemaState::Missing)
        .expect("first-time Missing init is not gated — no fleet to strand");
}

#[test]
fn dev_build_refuses_to_recover_a_dirty_global_store() {
    // `index --full` recovery (`create_or_migrate` → `schema::apply`) advances a Dirty store to
    // this binary's latest, so it strands the fleet exactly like an Older forward-migration.
    let (_dir, global) = global_store();
    let err = gate(&global, true, false)
        .ensure_migration_permitted(&global, SchemaState::Dirty)
        .expect_err("a dev build must not recover (advance) a dirty global store");
    assert!(err.to_string().contains("global"), "got: {err}");
}

#[test]
fn dev_build_may_open_a_newer_global_store() {
    // A Newer store was migrated by a FUTURE binary; this (older) dev binary can't advance it,
    // so there is nothing to gate — the open refuses for a different reason (unknown
    // migration).
    let (_dir, global) = global_store();
    gate(&global, true, false)
        .ensure_migration_permitted(&global, SchemaState::Newer)
        .expect("Newer is not gated — an older dev binary cannot advance the schema");
}

#[test]
fn no_global_store_path_never_gates() {
    let (_dir, some_db) = global_store();
    let no_global =
        MigrationGate { is_dev_build: true, allow_override: false, global_db_path: None };
    no_global
        .ensure_migration_permitted(&some_db, SchemaState::Older)
        .expect("without a resolvable global store, nothing is gated");
}
