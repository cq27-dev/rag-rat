//! Pins complete maintenance JSON objects, including omitted fields and explicit nulls.
mod common;

use std::fs;
use std::process::Command;

use rag_rat_base::config::Config;
use rag_rat_base::locks::{self, FileLock, FlightKind};
use rag_rat_core::IndexDatabase;

#[test]
fn maintenance_json_shapes_are_stable() {
    let root = common::unique_dir("maintenance-output");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn example() {}\n").unwrap();
    let path = root.join("rag-rat.toml");
    fs::write(
        &path,
        "[index]\nroot = \".\"\ndatabase = \"index.sqlite\"\n[llm.embedding]\nmodel = \
         \"none\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let config = Config::load(&path).unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());

    let run = |name: &str, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
            .args(["--json", "--config"])
            .arg(&path)
            .args(["maintenance", "--max-seconds", "0", "--old-head", "old"])
            .args(args)
            .current_dir(&*root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let raw = String::from_utf8(output.stdout).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Preserve the historical pretty JSON rendering as well as field presence and order.
        assert_eq!(raw, format!("{}\n", serde_json::to_string_pretty(&value).unwrap()));
        if let Some(elapsed) = value.get_mut("elapsed_seconds") {
            *elapsed = serde_json::json!(0.0);
        }
        if let Some(bytes) = value.pointer_mut("/wal_checkpoint/wal_bytes_before") {
            *bytes = serde_json::json!(0);
        }
        let normalized = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        let snapshot = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/maintenance")
            .join(format!("{name}.json"));
        if std::env::var_os("RAG_RAT_UPDATE_MAINTENANCE_SNAPSHOTS").is_some() {
            fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
            fs::write(&snapshot, &normalized).unwrap();
        }
        assert_eq!(normalized, fs::read_to_string(snapshot).unwrap(), "{name}");
    };

    run("complete", &[]);
    run("file-checkout", &["--trigger", "post-checkout", "--branch-checkout", "0"]);
    let watcher = FileLock::try_acquire(&locks::election_lock_path_for(&config)).unwrap().unwrap();
    run("watcher-live", &["--trigger", "post-merge"]);
    drop(watcher);
    let repo = locks::write_lock_repo_id(&config);
    let flight = FileLock::try_acquire(&FlightKind::Maintenance.lock_path(&config.database, &repo))
        .unwrap()
        .unwrap();
    run("coalesced", &[]);
    run("coalesced-hook", &["--trigger", "post-commit"]);
    drop(flight);

    // A new empty registration defers; it must not inherit the completed pass's nullable fields.
    fs::write(
        &path,
        "[index]\nroot = \".\"\ndatabase = \"empty.sqlite\"\n[llm.embedding]\nmodel = \"none\"\n",
    )
    .unwrap();
    run("deferred", &[]);
}
