//! End-to-end check that the CLI defaults to TOON and `--json` flips it to JSON.
//!
//! `query` emits a uniform-row payload (the hit list), so the default render must produce TOON's
//! dense form, while `--json` must produce parseable JSON. Both run against a tiny throwaway index.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rag_rat_core::Config;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rag-rat-cli-output-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

/// Build a throwaway index over one Rust file and return the config path the CLI should use.
fn build_index() -> (PathBuf, PathBuf) {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\npub fn close_database() {}\n")
        .unwrap();
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

fn run_query(config_path: &PathBuf, json: bool) -> String {
    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut cmd = Command::new(binary);
    cmd.arg("--config").arg(config_path);
    if json {
        cmd.arg("--json");
    }
    cmd.arg("query").arg("database");
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "query failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cli_defaults_to_toon_and_json_flag_flips_to_json() {
    let (root, config_path) = build_index();

    // Default: TOON. The hit list renders as a TOON object — and is NOT parseable as JSON (TOON's
    // bare-key / tabular form is not a JSON document), which is the strongest single signal that
    // the default is not JSON.
    let toon = run_query(&config_path, false);
    assert!(
        serde_json::from_str::<serde_json::Value>(&toon).is_err(),
        "default output parsed as JSON — TOON is supposed to be the default:\n{toon}"
    );

    // `--json`: valid, parseable JSON.
    let json = run_query(&config_path, true);
    let parsed: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|err| panic!("--json output is not valid JSON ({err}):\n{json}"));
    assert!(parsed.is_object() || parsed.is_array(), "unexpected JSON shape:\n{json}");

    let _ = fs::remove_dir_all(&root);
}
