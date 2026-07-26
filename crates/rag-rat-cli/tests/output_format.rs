//! End-to-end check that the CLI defaults to TOON and `--json` flips it to JSON.
//!
//! `query` emits a uniform-row payload (the hit list), so the default render must produce TOON's
//! dense form, while `--json` must produce parseable JSON. Both run against a tiny throwaway index.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rag_rat_base::config::Config;

mod common;

use common::{ScratchRoot, unique_dir};

/// Build a throwaway index over one Rust file and return the scratch guard + the config path the
/// CLI should use.
fn build_index() -> (ScratchRoot, PathBuf) {
    let root = unique_dir("cli-output");
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

/// Run `rag-rat [--json] <args…>` against the index and return stdout (asserting success).
fn run(config_path: &PathBuf, json: bool, args: &[&str]) -> String {
    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut cmd = Command::new(binary);
    cmd.arg("--config").arg(config_path);
    if json {
        cmd.arg("--json");
    }
    cmd.args(args);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "`{args:?}` failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cli_defaults_to_toon_and_json_flag_flips_to_json() {
    let (_root, config_path) = build_index();

    // Default: TOON. The hit list renders as a TOON object — and is NOT parseable as JSON (TOON's
    // bare-key / tabular form is not a JSON document), which is the strongest single signal that
    // the default is not JSON.
    let toon = run(&config_path, false, &["query", "database"]);
    assert!(
        serde_json::from_str::<serde_json::Value>(&toon).is_err(),
        "default output parsed as JSON — TOON is supposed to be the default:\n{toon}"
    );

    // `--json`: valid, parseable JSON.
    let json = run(&config_path, true, &["query", "database"]);
    let parsed: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|err| panic!("--json output is not valid JSON ({err}):\n{json}"));
    assert!(parsed.is_object() || parsed.is_array(), "unexpected JSON shape:\n{json}");
}

/// Every read command that prints structured output through `print_output` must honor the global
/// format: TOON object by default (not JSON-parseable), valid JSON under `--json`. Covers the
/// per-handler output branches (`doctor`/`brief`/`clusters`) the format flag threads through.
#[test]
fn read_commands_honor_global_format() {
    let (_root, config_path) = build_index();
    for args in [&["doctor"][..], &["brief"][..], &["clusters"][..]] {
        let toon = run(&config_path, false, args);
        assert!(
            serde_json::from_str::<serde_json::Value>(&toon).is_err(),
            "`{args:?}` default output parsed as JSON — should be TOON:\n{toon}"
        );
        let json = run(&config_path, true, args);
        serde_json::from_str::<serde_json::Value>(&json)
            .unwrap_or_else(|err| panic!("`{args:?} --json` is not valid JSON ({err}):\n{json}"));
    }
}

/// `memory list` println'd human text and ignored `--json` before this change; under `--json` it
/// must emit the structured list (here an empty index → a JSON array).
#[test]
fn memory_list_json_flag_emits_json() {
    let (_root, config_path) = build_index();
    let json = run(&config_path, true, &["memory", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|err| panic!("`memory list --json` is not valid JSON ({err}):\n{json}"));
    assert!(parsed.is_array(), "expected a JSON array of memory summaries, got:\n{json}");
}
