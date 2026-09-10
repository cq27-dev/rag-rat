//! End to end coverage for the native CLI projection of the MCP tool catalog.
//!
//! The public surface is `rag-rat tools <tool> ...`; command names and flags are generated
//! from the canonical catalog and schema, while execution still uses the same dispatcher by name
//! as MCP. Help and schema discovery must work without a configured repository.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use rag_rat_base::config::Config;
use serde_json::{Value, json};

mod common;

use common::{ScratchRoot, unique_dir};

fn build_index() -> (ScratchRoot, Config, PathBuf) {
    let root = unique_dir("cli_tools_dispatch");
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
    (root, config, config_path)
}

fn run(args: &[&str], cwd: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rag-rat")).args(args).current_dir(cwd).output().unwrap()
}

#[test]
fn tools_help_is_configless_and_lists_the_complete_generated_catalog() {
    let root = unique_dir("cli_tools_help");
    fs::create_dir_all(&root).unwrap();

    let output = run(&["tools", "--help"], &root);
    assert!(
        output.status.success(),
        "tools --help failed outside a configured repo: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    for tool in rag_rat_mcp::tools::TOOL_NAMES {
        let cli_name = tool.replace('_', "-");
        assert!(stdout.contains(&cli_name), "missing generated command {cli_name}:\n{stdout}");
    }
    assert!(stdout.contains("does not start an MCP server or client"));
}

#[test]
fn tools_namespace_preserves_global_version_behavior() {
    let root = unique_dir("cli_tools_version");
    fs::create_dir_all(&root).unwrap();

    let output = run(&["tools", "--version"], &root);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "unexpected stdout: {stdout}");
}

#[test]
fn per_tool_help_is_generated_from_the_canonical_schema() {
    let root = unique_dir("cli_tools_subcommand_help");
    fs::create_dir_all(&root).unwrap();

    let output = run(&["tools", "find-callers", "--help"], &root);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    for flag in ["--symbol", "--resolution", "--limit", "--include", "--edge-kinds", "--worktree"] {
        assert!(stdout.contains(flag), "missing generated flag {flag}:\n{stdout}");
    }
    assert!(stdout.contains("reverse call graph"));

    let output = run(&["tools", "help", "find-callers"], &root);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: rag-rat tools find-callers"));
    assert!(stdout.contains("--edge-kinds"));
}

#[test]
fn schema_discovery_is_configless_and_uses_the_canonical_catalog() {
    let root = unique_dir("cli_tools_schema");
    fs::create_dir_all(&root).unwrap();

    let output = run(&["--json", "tools", "--schema"], &root);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual, rag_rat_mcp::tools::list_tools());

    // Every tool, including those with required fields, supports either global flag position.
    for &tool in rag_rat_mcp::tools::TOOL_NAMES {
        let cli_name = tool.replace('_', "-");
        for args in [["--json", "tools", "--schema", cli_name.as_str()], [
            "--json",
            "tools",
            cli_name.as_str(),
            "--schema",
        ]] {
            let output = run(&args, &root);
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                actual,
                json!({
                    "name": tool,
                    "description": rag_rat_mcp::tools::description(tool),
                    "inputSchema": rag_rat_mcp::tools::schema(tool),
                }),
                "{args:?}"
            );
        }
    }
}

#[test]
fn schema_discovery_still_rejects_invalid_arguments() {
    let root = unique_dir("cli_tools_schema_invalid");
    fs::create_dir_all(&root).unwrap();

    for args in [
        vec!["tools", "--schema", "unknown-tool"],
        vec!["tools", "--schema", "semantic-search", "--unknown-flag"],
        vec!["tools", "semantic-search", "--schema", "--unknown-flag"],
        vec!["tools", "--schema", "semantic-search", "--include", "invalid"],
        vec!["tools", "--schema", "semantic-search", "--query"],
        vec!["tools", "--schema", "semantic-search", "--query", "text", "--arguments-json", "{}"],
    ] {
        let output = run(&args, &root);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn native_tool_invocation_matches_the_canonical_dispatcher() {
    let (_root, config, config_path) = build_index();
    let arguments = json!({"symbol": "open_database", "limit": 5});
    let expected =
        rag_rat_mcp::tools::call_tool_for_config(&config, "symbol_lookup", arguments).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .args(["--json", "tools", "symbol-lookup", "--symbol", "open_database", "--limit", "5"])
        .arg("--config")
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn arguments_json_preserves_the_full_wire_shape() {
    let (_root, config, config_path) = build_index();
    let arguments = json!({"symbol":"open_database","include":[],"limit":5});
    let expected =
        rag_rat_mcp::tools::call_tool_for_config(&config, "symbol_lookup", arguments.clone())
            .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("--config")
        .arg(&config_path)
        .args(["--json", "tools", "symbol-lookup", "--arguments-json"])
        .arg(arguments.to_string())
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn required_schema_fields_are_required_by_the_generated_cli() {
    let root = unique_dir("cli_tools_required");
    fs::create_dir_all(&root).unwrap();

    for args in [
        vec!["tools", "semantic-search"],
        vec!["tools", "--config=--schema", "semantic-search"],
        vec!["tools", "semantic-search", "--config=--schema"],
    ] {
        let output = run(&args, &root);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("--query"), "{args:?}: {stderr}");
    }
}
