//! Migration provenance gate (#585): a dev/test build must not silently forward-migrate the SHARED
//! GLOBAL store.
//!
//! On a box with one consolidated global DB and many long-lived processes (watcher, git hooks, MCP
//! servers on the installed binary), whichever binary knows the newest migration wins the schema
//! race for everyone — the moment a dev/test build opens the global store write-capable and applies
//! a forward migration, every process still on an older binary refuses the now-newer schema
//! (`SchemaState::Newer`). Migrations are forward-only, so there is no undo: the result is a
//! fleet-wide loss of index access until a matching binary is installed.
//!
//! So an `Older` global store is migrated only by an installed release binary, or under an explicit
//! `RAG_RAT_ALLOW_MIGRATE=1` operator override. Per-repo / temp DBs (the whole test suite,
//! single-binary machines) and first-time `Missing` init keep migrating automatically — there is no
//! fleet to strand in those cases.

use std::path::{Path, PathBuf};

use super::schema::SchemaState;

/// Operator escape hatch: set truthy to let a dev/test build migrate the global store anyway.
const ALLOW_MIGRATE_ENV: &str = "RAG_RAT_ALLOW_MIGRATE";

/// The inputs the migration gate decides on. INJECTED (not read from the ambient environment inside
/// the decision) so the refusal is unit-testable without mutating process-global env or faking
/// `current_exe`; [`MigrationGate::from_env`] builds the real-world instance.
pub(crate) struct MigrationGate {
    /// This binary is a development build, not a pristine release. Three signals feed it in
    /// [`from_env`](Self::from_env): `debug_assertions` and [`running_from_target_dir`] catch
    /// `cargo run`/`test`/`build`, and [`version_indicates_dev_build`] catches a release-profile
    /// dev binary installed OUTSIDE a `target/` dir (`cargo install --path .` off a branch) by its
    /// `+g<hash>` version stamp.
    pub is_dev_build: bool,
    /// `RAG_RAT_ALLOW_MIGRATE` is set truthy.
    pub allow_override: bool,
    /// The consolidated global store path, if one resolves (`data_dir::global_database_path`).
    pub global_db_path: Option<PathBuf>,
}

impl MigrationGate {
    /// Build the gate from the real environment (the wiring the live open paths use).
    pub(crate) fn from_env() -> Self {
        Self {
            is_dev_build: cfg!(debug_assertions)
                || running_from_target_dir()
                || version_indicates_dev_build(crate::binary_version()),
            allow_override: env_flag(ALLOW_MIGRATE_ENV),
            global_db_path: rag_rat_base::data_dir::global_database_path(),
        }
    }

    /// Refuse a schema apply that ADVANCES an existing global store from a dev/test build. `Older`
    /// (forward migration) and `Dirty` (an `index --full` recovery, which re-applies the ladder to
    /// this binary's latest) both advance the schema and can strand the fleet; `Missing` is
    /// first-time init (no fleet to strand) and `Compatible`/`Newer` never apply forward, so those
    /// pass. Returns `Ok(())` when the apply may proceed.
    pub(crate) fn ensure_migration_permitted(
        &self,
        db_path: &Path,
        state: SchemaState,
    ) -> anyhow::Result<()> {
        // Both `Older` (auto forward-migrate) and `Dirty` (`create_or_migrate` → `schema::apply`
        // recovery) run the ladder to THIS binary's LATEST on an existing store, so a dev binary
        // doing either bumps the shared schema past installed releases. `Missing` is first-time
        // init (nothing to strand); `Compatible`/`Newer` never apply forward. An installed
        // release binary, or an explicit operator override, always proceeds.
        let advances_existing_schema = matches!(state, SchemaState::Older | SchemaState::Dirty);
        if !advances_existing_schema || !self.is_dev_build || self.allow_override {
            return Ok(());
        }
        let Some(global) = self.global_db_path.as_deref() else {
            return Ok(());
        };
        if !same_file(db_path, global) {
            return Ok(());
        }
        anyhow::bail!(
            "refusing to auto-migrate the shared global index ({}) from a development build: it \
             would forward-migrate the schema for every rag-rat process on this machine and \
             strand any still on an older binary (#585). Install a released rag-rat and let it \
             migrate, or re-run with {ALLOW_MIGRATE_ENV}=1 to override.",
            global.display()
        )
    }
}

/// Whether the running executable lives under a Cargo `target/` build directory — the signal that
/// distinguishes a `cargo run` / `cargo test` / `cargo build` binary from an installed one. Matches
/// a path COMPONENT named `target` (not a substring), and FAILS OPEN — an unreadable `current_exe`
/// is treated as installed, so a platform that can't resolve it never over-refuses a real release.
fn running_from_target_dir() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|exe| exe.components().any(|component| component.as_os_str() == "target"))
}

/// Whether a git-stamped version string denotes a development build rather than a pristine release.
/// `build.rs` stamps `<pkg>+g<hash>` (a semver local-version segment) for anything not built at an
/// exact release tag; a real release is the bare `<pkg>`. So the presence of `+` is the build
/// provenance that catches a dev binary installed OUTSIDE a `target/` dir (`cargo install --path .`
/// off a branch) — which `debug_assertions` and the exe-path check miss (#585, Codex P1).
fn version_indicates_dev_build(version: &str) -> bool {
    version.contains('+')
}

/// A truthy env flag: set, non-empty, and not `0`/`false`.
fn env_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

/// Whether `a` and `b` name the same on-disk file, comparing CANONICAL paths — a bare/MCP open
/// arrives as a client-supplied string, so a symlinked `$HOME` (or `/tmp`) would break literal
/// equality. Both sides are canonicalized (per the `strip_prefix` two-sided-canonicalize rule);
/// falls back to literal comparison when either path can't be canonicalized.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
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
        assert!(
            err.to_string().contains("global"),
            "refusal should name the global store; got: {err}"
        );
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
}
