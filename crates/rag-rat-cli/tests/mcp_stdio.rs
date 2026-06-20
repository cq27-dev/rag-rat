use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use rag_rat_core::Config;
use serde_json::{Value, json};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn mcp_stdio_smoke_lists_and_calls_core_tools() {
    let root = unique_temp_root();
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
    assert!(papertrail["github_evidence"].is_array());

    stop(child);
    fs::remove_dir_all(root).unwrap();
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
    let root = unique_temp_root();
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
    fs::remove_dir_all(root).unwrap();

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
    let root = unique_temp_root();
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
    fs::remove_dir_all(root).unwrap();
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

fn unique_temp_root() -> PathBuf {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rag-rat-mcp-stdio-test-{}-{id}", std::process::id()))
}

#[test]
fn mcp_stdio_find_clones_returns_class_for_planted_pair() {
    let root = unique_temp_root();
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
    fs::remove_dir_all(root).unwrap();
}
