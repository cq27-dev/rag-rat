use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use rag_rat_base::config::Config;
use serde_json::{Value, json};

mod common;

use common::unique_dir;

#[test]
fn mcp_stdio_smoke_lists_and_calls_core_tools() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("docs/search.md"), "# Search\n\nSemantic recall uses sqlite.\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\nmarkdown = [\"docs\"]\n",
    )
    .unwrap();

    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "rag-rat-test", "version": "0.1"}
            }
        }),
    );
    let initialize = recv(&mut reader);
    assert_eq!(initialize["id"], 1);

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = recv(&mut reader);
    let tool_names = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    for name in ["semantic_search", "read_chunk", "papertrail_for_symbol"] {
        assert!(tool_names.contains(&name), "missing tool {name}");
    }

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "semantic_search", "arguments": {"query": "search", "limit": 1}}
        }),
    );
    let hits = response_text_json(recv(&mut reader));
    let chunk_id = hits.as_array().unwrap()[0]["chunk_id"].as_i64().unwrap();

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "read_chunk", "arguments": {"chunk_id": chunk_id}}
        }),
    );
    let chunk = response_text_json(recv(&mut reader));
    assert_eq!(chunk["chunk_id"], chunk_id);
    assert!(chunk["text"].as_str().unwrap().contains("Semantic recall"));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "papertrail_for_symbol",
                "arguments": {"symbol": "open_database", "lang": "rust", "limit": 1}
            }
        }),
    );
    let papertrail = response_text_json(recv(&mut reader));
    assert!(papertrail["current_source"].is_object());
    assert!(papertrail["evidence"].is_array());

    stop(child);
}

/// A `rag-rat mcp` launched OUTSIDE any rag-rat repo (no `rag-rat.toml` at or above cwd) must NOT
/// exit — a globally-registered MCP server is spawned in EVERY project, so dying in a non-rag-rat
/// one would take repo intelligence down for the whole session (#603). It serves a DORMANT server:
/// `initialize` + `tools/list` (full catalog) succeed, every `tools/call` returns the informative
/// dormant notice as a NON-error result, and the process stays alive across calls.
#[test]
fn mcp_stdio_serves_dormant_without_a_config() {
    let root = unique_dir("mcp-stdio");
    // A bare directory — deliberately NO rag-rat.toml here or (temp roots have none) above it.
    fs::create_dir_all(&root).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp") // NO --config: discovery from this cwd finds nothing → dormant.
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "rag-rat-test", "version": "0.1"}
            }
        }),
    );
    let initialize = recv(&mut reader);
    assert_eq!(initialize["id"], 1, "a dormant server must still answer initialize");

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = recv(&mut reader);
    let tool_names = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        tool_names.contains(&"semantic_search"),
        "a dormant server still advertises the full tool catalog",
    );

    // A tool call returns the dormant notice as a NON-error result (the agent reads it as a normal
    // response), not an MCP error and not a dead pipe.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "semantic_search", "arguments": {"query": "anything"}}
        }),
    );
    let response = recv(&mut reader);
    assert_ne!(
        response["result"]["isError"],
        json!(true),
        "the dormant tool result must not be an error",
    );
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("no_index"),
        "the dormant tool result must carry the no-index notice, got: {text}",
    );

    // An UNKNOWN tool name is still rejected as an error — dormancy must not mask a typo/stale tool
    // as `no_index` (a client can't otherwise tell an invalid call from genuine dormancy).
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "definitely_not_a_tool", "arguments": {}}
        }),
    );
    let unknown = recv(&mut reader);
    assert!(
        unknown["error"]["message"].as_str().unwrap_or_default().contains("unknown tool"),
        "a dormant server must reject an unknown tool, got: {unknown}",
    );

    // The server is still alive after those calls — a further request is answered.
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}));
    assert_eq!(recv(&mut reader)["id"], 5, "the dormant server stays alive across calls");

    stop(child);
}

/// The dormant tool result honors `--json` mode: its text block must be directly parseable as JSON
/// (the JSON-output contract holds even with no repo), not the default TOON prose. A JSON-parsing
/// MCP client would otherwise fail ONLY in dormant mode.
#[test]
fn mcp_stdio_dormant_result_is_valid_json_in_json_mode() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(&root).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .arg("--json")
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rag-rat-test", "version": "0.1"}}
        }),
    );
    assert_eq!(recv(&mut reader)["id"], 1);
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "semantic_search", "arguments": {"query": "anything"}}
        }),
    );
    let text = recv(&mut reader)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let parsed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("dormant --json result must be valid JSON ({err}): {text}"));
    assert_eq!(parsed["status"], "no_index", "dormant JSON payload carries the no-index status");

    stop(child);
}

/// Self-healing (#603): a server that started DORMANT activates its tools as soon as the directory
/// becomes a rag-rat repo mid-session — a `rag-rat init` + index run reachable through the dormant
/// notice actually works, with NO MCP restart. The server re-discovers the config on each call.
/// Dormancy is BINARY and decided at launch: a server that started dormant does NOT half-activate
/// when the directory becomes a rag-rat repo mid-session — it keeps returning the notice (which
/// tells the user to restart) rather than serving through a config it discovered per-call. A
/// per-call-discovered server would lack the active lifecycle (watcher / git-hook freshness) and
/// could return results not validated against current source, breaking rag-rat's core guarantee
/// (#603). Activation requires a restart; the smoke test covers the launched-with-config path.
#[test]
fn mcp_stdio_dormant_server_stays_dormant_until_restart() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn healed_symbol_marker() {}\n").unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .current_dir(&root) // NO --config, NO rag-rat.toml yet → starts dormant.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rag-rat-test", "version": "0.1"}}
        }),
    );
    assert_eq!(recv(&mut reader)["id"], 1);
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );

    // Before init: a tool call is dormant.
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "semantic_search", "arguments": {"query": "healed", "limit": 3}}}),
    );
    let before = recv(&mut reader)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(before.contains("no_index"), "must be dormant before the repo is indexed: {before}");

    // Turn the dir into an indexed rag-rat repo mid-session — but do NOT restart the server.
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let config = Config::load(root.join("rag-rat.toml")).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    // Without a restart the SAME server stays dormant — it does not half-activate against the new
    // config; the notice (restart to activate) still stands.
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "semantic_search", "arguments": {"query": "healed", "limit": 3}}}),
    );
    let after = recv(&mut reader)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(
        after.contains("no_index"),
        "a launched-dormant server must stay dormant until restart, got: {after}",
    );

    stop(child);
}

/// Regression guard: every tool advertised by `tools/list` (built from `TOOL_NAMES`) must be
/// ROUTABLE by `tools/call`. The two are built from different sources — `list_tools` from
/// `TOOL_NAMES`, the call router from the `#[tool]`-annotated methods — so a tool added to one but
/// not the other is advertised yet uncallable (the SDK answers "tool not found"). `memory_rebind`
/// regressed exactly this way: it was in `TOOL_NAMES` + handlers + schema but had no `#[tool]`
/// method. Calling each tool with empty args is fine here: a routable tool answers with a result or
/// an argument/validation error — only an unroutable one yields "tool not found".
#[test]
fn mcp_stdio_every_advertised_tool_is_routable() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rag-rat-test", "version": "0.1"}}
        }),
    );
    let _ = recv(&mut reader);
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = recv(&mut reader);
    let advertised = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        advertised.contains(&"memory_rebind".to_string()),
        "memory_rebind should be advertised"
    );

    let mut unroutable = Vec::new();
    for (offset, name) in advertised.iter().enumerate() {
        let id = 100 + offset as i64;
        send(
            &mut stdin,
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": {}}
            }),
        );
        let response = recv(&mut reader);
        if response["error"]["message"].as_str() == Some("tool not found") {
            unroutable.push(name.clone());
        }
    }
    stop(child);

    assert!(
        unroutable.is_empty(),
        "tools advertised by tools/list but not routable by tools/call: {unroutable:?}"
    );
}

#[test]
fn mcp_stdio_json_flag_emits_json_not_toon() {
    // `rag-rat mcp --json` is the MCP-side escape hatch: MCP has no per-call flag, so the output
    // format is chosen once at launch. With it, tool results are JSON (parseable directly), not the
    // default TOON (which `serde_json::from_str` would reject).
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .arg("--json")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rag-rat-test", "version": "0.1"}}
        }),
    );
    let _ = recv(&mut reader);
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "semantic_search", "arguments": {"query": "database", "limit": 1}}
        }),
    );
    let response = recv(&mut reader);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    // The escape hatch: this parses as JSON directly (a TOON `[N]{cols}:` payload would not).
    let parsed: Value = serde_json::from_str(text)
        .unwrap_or_else(|err| panic!("MCP result under --json is not valid JSON ({err}):\n{text}"));
    assert!(parsed.is_array() || parsed.is_object(), "expected a JSON array/object, got: {text}");

    stop(child);
}

fn send(stdin: &mut impl Write, value: Value) {
    writeln!(stdin, "{}", serde_json::to_string(&value).unwrap()).unwrap();
    stdin.flush().unwrap();
}

fn recv(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "mcp server closed stdout");
    serde_json::from_str(&line).unwrap()
}

/// Decode an MCP tool result into a JSON `Value`. Tool results are rendered as TOON (the server
/// default), so the text is TOON, not JSON — decode it back through `toon_format` to assert on it.
fn response_text_json(response: Value) -> Value {
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    toon_format::decode(text, &toon_format::DecodeOptions::default())
        .unwrap_or_else(|err| panic!("MCP result is not valid TOON ({err}):\n{text}"))
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn mcp_stdio_find_clones_returns_class_for_planted_pair() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("src")).unwrap();
    // Two identical functions → struct_hash fast path produces a clone pair.
    let clone_body = "pub fn cloned_helper(x: i32, y: i32) -> i32 {\n    x + y + 42\n}\n";
    fs::write(root.join("src/lib.rs"), format!("{clone_body}pub mod a;\npub mod b;\n")).unwrap();
    fs::write(root.join("src/a.rs"), clone_body).unwrap();
    fs::write(root.join("src/b.rs"), clone_body).unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();

    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rag-rat-test", "version": "0.1"}}
        }),
    );
    let _ = recv(&mut reader);
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "find_clones", "arguments": {"min_copies": 2}}
        }),
    );
    let response = recv(&mut reader);
    let result = response_text_json(response);
    let classes = result["classes"].as_array().expect("find_clones returns classes array");
    assert!(
        classes.iter().any(|c| c["member_count"].as_u64().unwrap_or(0) >= 2),
        "find_clones must return at least one class with member_count >= 2 for the planted clone \
         pair: {result:?}"
    );

    stop(child);
}

/// End-to-end (#215 Plan 4b Task 8): a Type-2 clone pair — two functions whose ONLY difference is
/// a single literal whose KIND differs (int `10` vs float `2.5`) — refines through the MCP
/// `find_clones` path and surfaces the anti-unify payload (`template`, `variation_points`,
/// `anti_unify_coverage`). After baseline normalization the local identifiers collapse to `ID<n>`
/// and equal-KIND literals collapse to the same `LIT_<KIND>` bucket, so only a DIFFERING literal
/// kind makes the column variant — int vs float keeps the two `struct_hash`es distinct → a
/// candidate pair that refines to a `value_param` variation point. Uses `--json` for plain JSON.
#[test]
fn mcp_stdio_find_clones_returns_refined_payload_for_type2_clone() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("src")).unwrap();
    // Two functions differing ONLY in the literal KIND (int 10 vs float 2.5). Baseline
    // normalization buckets literals by kind (`LIT_INTEGER_LITERAL` vs `LIT_FLOAT_LITERAL`), so the
    // differing KIND — not the differing value — is what yields a variant column + distinct
    // struct_hashes. Same-value-different-value integers would bucket identically (coverage 1.0).
    fs::write(
        root.join("src/a.rs"),
        "pub fn process_a(input: i32) -> i32 {\n    let factor = 10;\n    input * factor + 1\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn process_b(input: i32) -> i32 {\n    let factor = 2.5;\n    input * factor + 1\n}\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();

    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");
    let mut child = Command::new(binary)
        .arg("mcp")
        .arg("--config")
        .arg(&config_path)
        .arg("--json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rag-rat-test", "version": "0.1"}}
        }),
    );
    let _ = recv(&mut reader);
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "find_clones", "arguments": {"min_copies": 2}}
        }),
    );
    let response = recv(&mut reader);
    // `--json` server → tool result text is plain JSON.
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text)
        .unwrap_or_else(|err| panic!("find_clones result is not valid JSON ({err}):\n{text}"));

    let classes = result["classes"].as_array().expect("find_clones returns classes array");
    let refined =
        classes.iter().find(|c| c["refined"].as_bool().unwrap_or(false)).unwrap_or_else(|| {
            panic!("expected a refined class for the Type-2 clone pair: {result:?}")
        });

    // The anti-unify payload is surfaced on the refined class.
    assert!(
        refined["template"].as_str().is_some_and(|t| !t.is_empty()),
        "refined class must carry a non-empty template: {refined:?}"
    );
    assert!(
        refined["anti_unify_coverage"].as_f64().is_some(),
        "refined class must carry anti_unify_coverage: {refined:?}"
    );
    let vps = refined["variation_points"]
        .as_array()
        .expect("refined class must carry a variation_points array");
    assert!(
        vps.iter().any(|vp| vp["extraction_role"].as_str() == Some("value_param")),
        "the differing literal must surface as a value_param variation point: {vps:?}"
    );

    // Codex #5: `medoid_symbol_id` is INTERNAL (a reindex-unstable rowid) — `#[serde(skip)]` keeps
    // it out of the API payload so clients can't cache an id that breaks on rebuild.
    assert!(
        refined.get("medoid_symbol_id").is_none(),
        "medoid_symbol_id (a rowid) must NOT be serialized in the find_clones payload: {refined:?}"
    );

    stop(child);
}

/// CLI surface (#215 Plan 4b Task 8): `rag-rat clones --explain <CLASS_KEY>` prints the refined
/// class's template + variation points instead of the listing. The key isn't known ahead of time,
/// so first run `clones --json` to read a refined class's `class_key`, then re-run with
/// `--explain <key>` and assert the human output names a metavar (`m0`) and the value_param role.
#[test]
fn cli_clones_explain_prints_template_and_metavar_for_type2_clone() {
    let root = unique_dir("mcp-stdio");
    fs::create_dir_all(root.join("src")).unwrap();
    // Same Type-2 fixture as the MCP refined-payload test: bodies identical except a differing
    // literal KIND (int 10 vs float 2.5), which is what forces a value_param variation point.
    fs::write(
        root.join("src/a.rs"),
        "pub fn process_a(input: i32) -> i32 {\n    let factor = 10;\n    input * factor + 1\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn process_b(input: i32) -> i32 {\n    let factor = 2.5;\n    input * factor + 1\n}\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
    fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();

    let config_path = root.join("rag-rat.toml");
    let config = Config::load(&config_path).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    let binary = env!("CARGO_BIN_EXE_rag-rat");

    // Step 1: `clones --json` → find a refined class's class_key.
    let listing = Command::new(binary)
        .arg("clones")
        .arg("--config")
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "clones --json must succeed: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let listing_json: Value = serde_json::from_slice(&listing.stdout).unwrap_or_else(|err| {
        panic!(
            "clones --json is not valid JSON ({err}):\n{}",
            String::from_utf8_lossy(&listing.stdout)
        )
    });
    let classes = listing_json["classes"].as_array().expect("clones returns classes array");
    let key = classes
        .iter()
        .find(|c| c["refined"].as_bool().unwrap_or(false))
        .and_then(|c| c["class_key"].as_str())
        .unwrap_or_else(|| panic!("expected a refined class with a class_key: {listing_json:?}"))
        .to_string();

    // Step 2: `clones --explain <key>` → human-readable breakdown.
    let explain = Command::new(binary)
        .arg("clones")
        .arg("--config")
        .arg(&config_path)
        .arg("--explain")
        .arg(&key)
        .output()
        .unwrap();
    assert!(
        explain.status.success(),
        "clones --explain must succeed: {}",
        String::from_utf8_lossy(&explain.stderr)
    );
    let out = String::from_utf8_lossy(&explain.stdout);
    assert!(out.contains(&key), "explain output must echo the class key:\n{out}");
    assert!(out.contains("Template:"), "explain output must have a Template section:\n{out}");
    assert!(out.contains("Variation points"), "explain output must list variation points:\n{out}");
    assert!(out.contains("m0"), "explain output must name the m0 metavar:\n{out}");
    assert!(out.contains("value_param"), "explain output must name the value_param role:\n{out}");

    // Member-key fix (#215 Plan 4b): each per-member value must be printed ALONGSIDE the member it
    // belongs to (`identity=value`), so a reader can map a value back to a member. Both clone
    // members' refs must appear, paired with their differing literal value (`10` / `2.5`).
    assert!(
        out.contains("process_a") && out.contains("process_b"),
        "explain output must name both member refs next to their values:\n{out}"
    );
    // Codex #4: the per-member identity is LOCATION-BEARING (`ref@path:start-end`), so the printed
    // label is `…process_a@…:<lines>=<value>`, not the bare `process_a=value`. Assert the member
    // ref and its value appear in the SAME `=`-joined token (location-bearing identity →
    // value), so a class with duplicate refs would still be disambiguated.
    assert!(
        out.lines().any(|line| {
            line.contains("process_a@")
                && (line.contains("=10") || line.contains("=2.5"))
                && line.contains("process_a")
        }),
        "explain output must pair the LOCATION-BEARING member identity (process_a@path:lines) \
         with its per-member value:\n{out}"
    );
    assert!(
        out.contains("process_a@") && out.contains("process_b@"),
        "explain output must carry location-bearing member identities (ref@path:start-end):\n{out}"
    );
    // The variation-points line carries the `identity=value` pairing separated by ` | `.
    assert!(
        out.contains("=10") && out.contains("=2.5") && out.contains(" | "),
        "explain output must zip per-member values with member identity (identity=value | \
         identity=value):\n{out}"
    );
}
