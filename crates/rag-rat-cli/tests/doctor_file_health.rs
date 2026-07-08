//! `doctor` must surface the database file-health block (#482): WAL sidecar size and freelist
//! dead space were previously invisible, so an oversized `-wal` or a mostly-free file could only
//! be found by inspecting the data dir by hand.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rag_rat_core::Config;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a throwaway index over one Rust file and return (root, config path).
fn build_index() -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "rag-rat-cli-doctor-health-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn doctor_probe() {}\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();
    (root, config_path)
}

#[test]
fn doctor_reports_database_file_health() {
    let (root, config_path) = build_index();

    let out = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("--config")
        .arg(&config_path)
        .arg("--json")
        .arg("doctor")
        .output()
        .unwrap();
    assert!(out.status.success(), "doctor failed: {}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();

    let health = &report["file_health"];
    assert!(health.is_object(), "doctor must report a file_health block, got: {report}");
    assert!(health["main_bytes"].as_u64().unwrap() > 0);
    assert!(health["page_count"].as_i64().unwrap() > 0);
    // A one-file fixture is nowhere near either warn threshold — flags present and quiet.
    assert_eq!(health["wal_oversized"], serde_json::Value::Bool(false));
    assert_eq!(health["freelist_excessive"], serde_json::Value::Bool(false));
    assert!(health["note"].is_null(), "no advisory on a healthy file: {health}");

    let _ = fs::remove_dir_all(&root);
}
