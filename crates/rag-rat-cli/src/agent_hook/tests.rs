use rag_rat_core::query::orientation::Orientation;
use rag_rat_query::memory::AnchorHealth;
use rag_rat_query::tree::{DirTree, TreeNode};

use super::*;

// ─── version line ─────────────────────────────────────────────────────────

fn status(latest: Option<&str>, update: bool) -> rag_rat_core::version_check::VersionStatus {
    rag_rat_core::version_check::VersionStatus {
        current_version: "0.5.0".to_string(),
        latest_version: latest.map(str::to_string),
        update_available: update,
        update_command: "cargo install rag-rat --force".to_string(),
        checked_at_ms: latest.map(|_| 1),
    }
}

#[test]
fn version_line_nags_when_behind() {
    let line = version_line(&status(Some("0.6.0"), true)).expect("a behind status renders");
    assert!(line.contains("update available"), "got: {line}");
    assert!(line.contains("0.5.0 → 0.6.0"), "states current → latest: {line}");
    assert!(line.contains("cargo install rag-rat --force"), "states the update command: {line}");
}

#[test]
fn version_line_is_quiet_when_current() {
    let line = version_line(&status(Some("0.5.0"), false)).expect("an up-to-date status renders");
    assert!(line.contains("0.5.0 (latest on crates.io)"), "got: {line}");
    assert!(!line.contains("update available"), "no nag when current: {line}");
}

#[test]
fn version_line_is_none_when_latest_unknown() {
    assert_eq!(version_line(&status(None, false)), None, "no cached check → no line");
}

#[test]
fn version_line_marks_a_local_build_ahead_of_crates_io() {
    // Running 0.5.0 while crates.io latest is 0.4.0 (dev/pre-release): not an update, but not
    // "the crates.io latest" either.
    let line = version_line(&status(Some("0.4.0"), false)).expect("ahead status renders");
    assert!(line.contains("ahead of crates.io latest 0.4.0"), "got: {line}");
    assert!(!line.contains("(latest on crates.io)"), "must not claim it's the latest: {line}");
}

// ─── HookInput parsing ────────────────────────────────────────────────────

#[test]
fn session_start_json_without_tool_fields_deserializes() {
    // SessionStart payloads have no tool_name / tool_input — must still parse.
    let json =
        r#"{"hook_event_name":"SessionStart","source":"startup","cwd":"/x","session_id":"s"}"#;
    let input: HookInput = serde_json::from_str(json).unwrap();
    assert_eq!(input.hook_event_name.as_deref(), Some("SessionStart"));
    assert_eq!(input.source.as_deref(), Some("startup"));
    assert_eq!(input.cwd, "/x");
    // tool_name and tool_input default (empty / null).
    assert!(input.tool_name.is_empty());
    assert!(input.tool_input.is_null());
}

#[test]
fn cursor_events_normalize_to_internal_actions() {
    let session = normalize_hook(
        r#"{"hook_event_name":"sessionStart","conversation_id":"c1","workspace_roots":["/repo"]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(session.harness, Harness::Cursor);
    assert_eq!(session.dispatch, Dispatch::SessionStart);
    assert_eq!(session.input.cwd, "/repo");
    assert_eq!(session.input.session_id, "c1");

    let shell = normalize_hook(
        r#"{"hook_event_name":"beforeShellExecution","command":"rg needle src","cwd":"/repo"}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(shell.dispatch, Dispatch::PreToolUse);
    assert_eq!(shell.output_event, OutputEvent::CursorBeforeShell);
    assert_eq!(shell.input.tool_name, "Bash");
    assert_eq!(shell.input.tool_input["command"], "rg needle src");

    let edit = normalize_hook(
        r#"{"hook_event_name":"afterFileEdit","file_path":"/repo/src/lib.rs","edits":[{"old_string":"a","new_string":"b"}],"workspace_roots":["/repo"]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(edit.dispatch, Dispatch::PostToolUse);
    assert_eq!(edit.input.tool_input["file_path"], "/repo/src/lib.rs");
    assert_eq!(edit.input.tool_input["edits"][0]["new_string"], "b");
}

#[test]
fn cursor_multi_root_events_use_the_root_containing_the_file() {
    let edit = normalize_hook(
        r#"{"hook_event_name":"afterFileEdit","workspace_roots":["/repo-a","/repo-b"],"file_path":"/repo-b/src/lib.rs","edits":[]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(edit.input.cwd, "/repo-b");

    let read = normalize_hook(
        r#"{"hook_event_name":"postToolUse","workspace_roots":["/repo-a","/repo-b"],"tool_name":"Read","tool_input":{"file_path":"/repo-b/src/lib.rs"}}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(read.input.cwd, "/repo-b");
}

#[test]
fn cursor_read_uses_posttool_context_channel_only() {
    let before = normalize_hook(
        r#"{"hook_event_name":"beforeReadFile","file_path":"/repo/src/lib.rs","workspace_roots":["/repo"]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(before.input.tool_name, "Read");
    assert_eq!(before.output_event, OutputEvent::None);

    let after = normalize_hook(
        r#"{"hook_event_name":"postToolUse","tool_name":"Read","tool_input":{"file_path":"/repo/src/lib.rs"},"workspace_roots":["/repo"]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(after.dispatch, Dispatch::CursorPostToolUse);
    assert_eq!(after.output_event, OutputEvent::CursorPostToolUse);
}

#[test]
fn cursor_clone_check_requires_generic_posttool_edit_text() {
    let dedicated = normalize_hook(
        r#"{"hook_event_name":"afterFileEdit","file_path":"/repo/src/lib.rs","edits":[{"new_string":"fn added() {}"}],"workspace_roots":["/repo"]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(dedicated.dispatch, Dispatch::PostToolUse);
    assert_eq!(dedicated.output_event, OutputEvent::None);

    let generic = normalize_hook(
        r#"{"hook_event_name":"postToolUse","tool_name":"Write","tool_input":{"file_path":"/repo/src/lib.rs","edits":[{"new_string":"fn added() {}"}]},"workspace_roots":["/repo"]}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    assert_eq!(generic.dispatch, Dispatch::CursorPostToolUse);
    assert_eq!(generic.input.tool_name, "MultiEdit");
    assert_eq!(generic.input.tool_input["edits"][0]["new_string"], "fn added() {}");
    assert_eq!(generic.output_event, OutputEvent::CursorPostToolUse);
}

#[test]
fn vscode_tool_names_and_camel_case_inputs_normalize() {
    let cases = [
        ("run_in_terminal", serde_json::json!({"command": "rg foo"}), "Bash"),
        ("read_file", serde_json::json!({"filePath": "/repo/a.rs"}), "Read"),
        (
            "create_file",
            serde_json::json!({"filePath": "/repo/a.rs", "content": "fn a() {}"}),
            "Write",
        ),
        (
            "replace_string_in_file",
            serde_json::json!({"filePath": "/repo/a.rs", "newString": "fn b() {}"}),
            "Edit",
        ),
    ];
    for (tool, tool_input, expected) in cases {
        let value = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/repo",
            "tool_name": tool,
            "tool_input": tool_input,
        });
        let normalized = normalize_hook(&value.to_string(), AgentHookHarnessArg::Vscode).unwrap();
        assert_eq!(normalized.input.tool_name, expected, "{tool}");
    }
}

#[test]
fn snake_case_and_camel_case_paths_normalize_identically() {
    for field in ["file_path", "filePath"] {
        let value = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/repo",
            "tool_name": "read_file",
            "tool_input": { (field): "/repo/src/lib.rs" },
        });
        let normalized = normalize_hook(&value.to_string(), AgentHookHarnessArg::Vscode).unwrap();
        assert_eq!(normalized.input.tool_input["file_path"], "/repo/src/lib.rs");
    }
}

#[test]
fn cursor_and_vscode_shell_commands_feed_the_existing_parser() {
    let cursor = normalize_hook(
        r#"{"hook_event_name":"beforeShellExecution","command":"rg -n needle src","cwd":"/repo"}"#,
        AgentHookHarnessArg::Cursor,
    )
    .unwrap();
    let vscode = normalize_hook(
        r#"{"hook_event_name":"PreToolUse","tool_name":"runTerminalCommand","tool_input":{"commandLine":"rg -n needle src"},"cwd":"/repo"}"#,
        AgentHookHarnessArg::Vscode,
    )
    .unwrap();
    for normalized in [cursor, vscode] {
        let search = extract_search(&normalized.input).unwrap();
        assert_eq!(search.pattern, "needle");
        assert_eq!(search.search_path.as_deref(), Some("src"));
    }
}

#[test]
fn irrelevant_tools_and_malformed_input_are_silent() {
    let irrelevant = normalize_hook(
        r#"{"hook_event_name":"PreToolUse","tool_name":"fetch_webpage","tool_input":{"secret":"RAW-PAYLOAD-SENTINEL"}}"#,
        AgentHookHarnessArg::Vscode,
    )
    .unwrap();
    assert_eq!(irrelevant.dispatch, Dispatch::Ignore);
    assert_eq!(irrelevant.output_event, OutputEvent::None);
    assert!(normalize_hook("not json", AgentHookHarnessArg::Cursor).is_none());
}

#[test]
fn context_output_contains_only_intentional_context() {
    let cursor = format_context_output(
        Harness::Cursor,
        OutputEvent::CursorPostToolUse,
        "bounded augmentation",
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&cursor).unwrap(),
        serde_json::json!({"additional_context": "bounded augmentation"})
    );
    assert!(!cursor.contains("RAW-PAYLOAD-SENTINEL"));

    let vscode =
        format_context_output(Harness::Vscode, OutputEvent::PreToolUse, "bounded augmentation")
            .unwrap();
    let value: serde_json::Value = serde_json::from_str(&vscode).unwrap();
    assert_eq!(value["hookSpecificOutput"]["additionalContext"], "bounded augmentation");
    assert!(format_context_output(Harness::Cursor, OutputEvent::None, "ignored").is_none());
}

#[test]
fn hook_manifests_register_one_dispatcher_per_event() {
    for manifest in [
        include_str!("../../../../plugin/hooks/hooks.json"),
        include_str!("../../../../plugin/hooks.vscode.json"),
    ] {
        let value: serde_json::Value = serde_json::from_str(manifest).unwrap();
        for event in ["SessionStart", "PreToolUse", "PostToolUse"] {
            let entries = value["hooks"][event].as_array().expect("event array");
            assert_eq!(entries.len(), 1, "{event} must have one dispatcher");
            assert!(entries[0].get("matcher").is_none(), "{event} must not rely on matchers");
        }
    }

    let vscode: serde_json::Value =
        serde_json::from_str(include_str!("../../../../plugin/hooks.vscode.json")).unwrap();
    for event in ["SessionStart", "PreToolUse", "PostToolUse"] {
        let command = vscode["hooks"][event][0]["command"].as_str().unwrap();
        assert!(command.contains("${PLUGIN_ROOT}/scripts/launch.js"));
        assert!(command.ends_with("agent-hook vscode"));
    }

    let cursor: serde_json::Value =
        serde_json::from_str(include_str!("../../../../plugin/hooks.cursor.json")).unwrap();
    for event in ["sessionStart", "beforeShellExecution", "postToolUse", "afterFileEdit"] {
        let entries = cursor["hooks"][event].as_array().expect("Cursor event array");
        assert_eq!(entries.len(), 1, "Cursor {event} must have one dispatcher");
        assert!(
            entries[0]["command"].as_str().unwrap().ends_with("agent-hook cursor"),
            "Cursor {event} must select the Cursor adapter"
        );
    }
}

#[test]
fn parses_grep_tool_input() {
    let json = r#"{"session_id":"s1","cwd":"/repo","hook_event_name":"PreToolUse",
            "tool_name":"Grep","tool_input":{"pattern":"watcher_main","path":"crates"}}"#;
    let input: HookInput = serde_json::from_str(json).unwrap();
    let search = extract_search(&input).unwrap();
    assert_eq!(search.pattern, "watcher_main");
    assert_eq!(search.search_path.as_deref(), Some("crates"));
    assert_eq!(search.source, "grep_tool");
}

#[test]
fn bash_parser_table() {
    // (command, expected pattern, expected path)
    let positives = [
        ("rg watcher_main", "watcher_main", None),
        ("rg -n 'election retry' crates/", "election retry", Some("crates/")),
        ("grep -rn foo src", "foo", Some("src")),
        ("ag --rust frobnicate", "frobnicate", None),
        ("rg -e 'fn main' --type rust", "fn main", None),
        ("cd crates && rg spawn_listener", "spawn_listener", None),
        ("FOO=1 rg spawn_listener", "spawn_listener", None),
        ("rg -A 3 -B 2 needle haystack/", "needle", Some("haystack/")),
        // grep is the pipeline HEAD (a real search piped into a pager) → still matches.
        ("rg foo | head", "foo", None),
        (r#"rg "quoted pattern" src"#, "quoted pattern", Some("src")),
    ];
    for (cmd, pattern, path) in positives {
        let got = parse_bash_search(cmd).unwrap_or_else(|| panic!("no match for {cmd}"));
        assert_eq!(got.0, pattern, "pattern for {cmd}");
        assert_eq!(got.1.as_deref(), path, "path for {cmd}");
    }
    let negatives = [
        "ls -la",
        "cargo test",
        "rg",                                        // no pattern
        "find . -name '*.rs' -exec grep foo {} \\;", // -exec: ambiguous
        "echo `rg foo`",                             // backticks: ambiguous
        "xargs grep foo",                            // xargs: ambiguous
        "groups",                                    // not grep
        // #138: grep DOWNSTREAM of a pipe is filtering another tool's output, not a code
        // search — must not augment.
        "git log | rg fix",
        "cargo test | grep result",
        "gh run view | grep -i error",
        "cargo clippy | grep -E warning",
        "cargo test |& grep result", // |& pipes stdout+stderr → still a downstream filter
        "cargo clippy |& rg warning",
    ];
    for cmd in negatives {
        assert!(parse_bash_search(cmd).is_none(), "false positive for {cmd}");
    }
}

#[test]
fn extract_search_routes_bash_commands() {
    let json = r#"{"session_id":"s1","cwd":"/repo","hook_event_name":"PreToolUse",
            "tool_name":"Bash","tool_input":{"command":"rg -n watcher_main crates/"}}"#;
    let input: HookInput = serde_json::from_str(json).unwrap();
    let search = extract_search(&input).unwrap();
    assert_eq!(search.pattern, "watcher_main");
    assert_eq!(search.source, "bash");
}

#[test]
fn extract_search_ignores_other_tools() {
    let json = r#"{"session_id":"s1","cwd":"/repo","hook_event_name":"PreToolUse",
            "tool_name":"Read","tool_input":{"path":"/x"}}"#;
    let input: HookInput = serde_json::from_str(json).unwrap();
    assert!(extract_search(&input).is_none());
}

// ─── Read-augmentation path resolution (#756) ───────────────────────────────

#[test]
fn worktree_rel_path_relativizes_an_in_worktree_file_with_slashes() {
    // Relativized against the SESSION worktree root — for a linked worktree that is NOT the
    // main-anchored config.root, so a file under the linked root still resolves (#756).
    assert_eq!(
        worktree_rel_path(
            "/repo/.wt/branch-x/crates/rag-rat-core/src/lib.rs",
            Path::new("/repo/.wt/branch-x"),
        )
        .as_deref(),
        Some("crates/rag-rat-core/src/lib.rs"),
    );
}

#[test]
fn worktree_rel_path_is_none_outside_the_worktree() {
    // A file outside the worktree root has nothing indexed to augment it with.
    assert!(
        worktree_rel_path("/etc/hosts", Path::new("/repo")).is_none(),
        "outside the worktree → None",
    );
}

#[test]
fn rebase_root_swaps_the_worktree_top_and_preserves_the_index_subdir() {
    // root = "." on the main worktree: identity.
    assert_eq!(
        rebase_root(Path::new("/main"), Path::new("/main"), Path::new("/main")),
        Path::new("/main")
    );
    // root = "." in a linked worktree: main top → linked top.
    assert_eq!(
        rebase_root(Path::new("/main"), Path::new("/main"), Path::new("/linked")),
        Path::new("/linked"),
    );
    // [index] root = "crates" on the main worktree: the subdir is preserved.
    assert_eq!(
        rebase_root(Path::new("/main/crates"), Path::new("/main"), Path::new("/main")),
        Path::new("/main/crates"),
    );
    // [index] root = "crates" in a linked worktree: swap the top AND keep the subdir.
    assert_eq!(
        rebase_root(Path::new("/main/crates"), Path::new("/main"), Path::new("/linked")),
        Path::new("/linked/crates"),
    );
    // Config nested below the git root (`/main/project/rag-rat.toml`) + `[index] root = "src"`, in
    // a linked worktree: the WHOLE suffix (`project/src`) is preserved once, no double-count.
    assert_eq!(
        rebase_root(Path::new("/main/project/src"), Path::new("/main"), Path::new("/linked")),
        Path::new("/linked/project/src"),
    );
    // config_root not under main_top (unexpected topology): unchanged, never a wrong prefix.
    assert_eq!(
        rebase_root(Path::new("/elsewhere"), Path::new("/main"), Path::new("/linked")),
        Path::new("/elsewhere"),
    );
}

// ─── PostToolUse edited-path extraction (#661) ──────────────────────────────

#[test]
fn extract_edited_paths_pulls_file_path_from_the_edit_tools() {
    for tool in ["Write", "Edit", "MultiEdit"] {
        let json = format!(
            r#"{{"hook_event_name":"PostToolUse","tool_name":"{tool}",
                "tool_input":{{"file_path":"/repo/src/lib.rs"}}}}"#,
        );
        let input: HookInput = serde_json::from_str(&json).unwrap();
        assert_eq!(
            extract_edited_paths(&input),
            vec![PathBuf::from("/repo/src/lib.rs")],
            "{tool} edited path",
        );
    }
}

#[test]
fn extract_edited_paths_ignores_non_edit_tools_and_missing_paths() {
    // A non-edit tool yields nothing.
    for tool in ["Read", "Grep", "Bash"] {
        let json = format!(
            r#"{{"hook_event_name":"PostToolUse","tool_name":"{tool}",
                "tool_input":{{"file_path":"/repo/x.rs"}}}}"#,
        );
        let input: HookInput = serde_json::from_str(&json).unwrap();
        assert!(extract_edited_paths(&input).is_empty(), "{tool} must not trigger a reindex");
    }
    // An edit tool with no file_path (malformed payload) is a silent no-op, not a panic.
    let json = r#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{}}"#;
    let input: HookInput = serde_json::from_str(json).unwrap();
    assert!(extract_edited_paths(&input).is_empty());
}

#[test]
fn extract_edited_paths_resolves_relative_files_and_apply_patch() {
    let multi = HookInput {
        cwd: "/repo".to_string(),
        tool_name: "MultiEdit".to_string(),
        tool_input: serde_json::json!({"files": ["src/a.rs", "/other/b.rs"]}),
        ..Default::default()
    };
    assert_eq!(extract_edited_paths(&multi), vec![
        PathBuf::from("/repo/src/a.rs"),
        PathBuf::from("/other/b.rs"),
    ]);

    let patch = HookInput {
        cwd: "/repo".to_string(),
        tool_name: "apply_patch".to_string(),
        tool_input: serde_json::json!({
            "command": "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: src/moved.rs\n+fn added() {}\n*** Delete File: src/gone.rs\n*** End Patch\n"
        }),
        ..Default::default()
    };
    assert_eq!(extract_edited_paths(&patch), vec![
        PathBuf::from("/repo/src/lib.rs"),
        PathBuf::from("/repo/src/moved.rs"),
        PathBuf::from("/repo/src/gone.rs"),
    ]);
}

#[test]
fn paths_to_reindex_defers_to_the_watcher_except_for_manifests() {
    let src = PathBuf::from("/repo/src/lib.rs");
    let manifest = PathBuf::from("/repo/Cargo.toml");
    let paths = vec![src.clone(), manifest.clone()];

    // No watcher: the hook reindexes everything.
    assert_eq!(paths_to_reindex(false, &paths), paths);

    // Watcher live: source edits defer to the watcher, but a manifest it never sees does not.
    assert_eq!(paths_to_reindex(true, &paths), vec![manifest]);
    assert!(paths_to_reindex(true, &[src]).is_empty(), "a lone source edit fully defers");
}

// ─── format_digest ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn make_orientation(
    root_title: Option<&str>,
    nodes: Vec<TreeNode>,
    truncated: u32,
    load_bearing: Vec<(&str, u64)>,
    recent: Vec<&str>,
    hot: Vec<&str>,
    memory_titles: Vec<&str>,
    head: &str,
    indexed_head: &str,
    anchor: AnchorHealth,
    parser_failures: u64,
) -> Orientation {
    Orientation {
        tree: DirTree { nodes, root_memory_title: root_title.map(str::to_string), truncated },
        load_bearing: load_bearing.into_iter().map(|(p, fi)| (p.to_string(), fi)).collect(),
        recent_commits: recent.into_iter().map(str::to_string).collect(),
        hot_files: hot.into_iter().map(str::to_string).collect(),
        // Default the total to the shown-title count (no overflow). Tests exercising the
        // `(+N more)` note override `active_memory_total` after construction.
        active_memory_total: memory_titles.len() as u32,
        active_memory_titles: memory_titles.into_iter().map(str::to_string).collect(),
        head: head.to_string(),
        indexed_head: indexed_head.to_string(),
        anchor,
        total_files: 42,
        parser_failures,
    }
}

fn healthy_anchor() -> AnchorHealth {
    AnchorHealth { current: 3, relocated: 1, stale: 0, gone: 0 }
}

fn node(depth: u8, label: &str, path: &str, file_count: u32, title: Option<&str>) -> TreeNode {
    TreeNode {
        depth,
        label: label.to_string(),
        path: path.to_string(),
        file_count,
        memory_title: title.map(str::to_string),
    }
}

#[test]
fn format_digest_contains_attribution_header() {
    let o = make_orientation(
        None,
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    assert!(s.contains("▶ rag-rat repo intelligence"), "missing attribution header");
    assert!(s.contains("semantic_search"), "missing tool nudge");
}

#[test]
fn format_digest_purpose_line_when_root_title_present() {
    let o = make_orientation(
        Some("My project — does amazing things"),
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    assert!(s.contains("My project — does amazing things"), "missing purpose line");
}

#[test]
fn format_digest_no_purpose_line_when_root_title_absent() {
    let o = make_orientation(
        None,
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    // No stray purpose-like line should appear (hard to assert absence of arbitrary content,
    // but the relevant sentinel is not there).
    assert!(!s.contains("does amazing things"));
}

#[test]
fn format_digest_layout_indents_and_annotates_tree() {
    let nodes = vec![
        node(0, "src", "src", 5, None),
        node(1, "actors", "src/actors", 8, Some("per-domain actors")),
        node(1, "data", "src/data", 3, None),
    ];
    let o = make_orientation(
        None,
        nodes,
        0,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    // LAYOUT header renders the scoped file count (make_orientation sets total_files = 42).
    assert!(
        s.contains("LAYOUT  (42 files · ‹…› = directory memory)"),
        "LAYOUT header missing file count; got:\n{s}"
    );
    // depth-0 node: no indentation.
    assert!(s.contains("\nsrc\n"), "depth-0 node should not be indented");
    // depth-1 node with memory: indented 2 spaces + label + title.
    assert!(
        s.contains("  actors  ‹per-domain actors›"),
        "depth-1 node with title missing or malformed"
    );
    // depth-1 node without memory: indented 2 spaces.
    assert!(s.contains("  data\n"), "depth-1 node without title missing");
}

#[test]
fn format_digest_truncated_note() {
    let o = make_orientation(
        None,
        vec![node(0, "src", "src", 5, None)],
        7,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    assert!(s.contains("… (+7 more)"), "missing truncated note");
}

#[test]
fn format_digest_load_bearing_fan_in() {
    let o = make_orientation(
        None,
        vec![],
        0,
        vec![
            ("crates/rag-rat-core/src/index/mod.rs", 2286),
            ("crates/rag-rat-core/src/main.rs", 42),
            ("src/database.rs", 999),
        ],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    assert!(s.contains("load-bearing:"), "missing load-bearing prefix");
    // crates/.../src/ prefix must be stripped.
    assert!(s.contains("index/mod.rs (fan_in 2286)"), "crates path not shortened; got:\n{s}");
    assert!(s.contains("main.rs (fan_in 42)"), "crates path not shortened for main.rs; got:\n{s}");
    // Path without the crates prefix stays unchanged.
    assert!(s.contains("src/database.rs (fan_in 999)"), "non-crates path changed; got:\n{s}");
}

#[test]
fn format_digest_memories_overflow_uses_true_total() {
    // The query truncates to 3 titles, but the repo has 9 active non-dir memories.
    // The overflow note must report the honest remainder (9 - 3 = 6), not 5 - 3 = 2.
    let titles: Vec<&str> = vec!["alpha", "beta", "gamma"];
    let mut o = make_orientation(
        None,
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        titles,
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    o.active_memory_total = 9;
    let s = format_digest(&o, true, true);
    // All 3 shown titles must appear.
    assert!(s.contains("alpha"), "first memory title missing");
    assert!(s.contains("beta"), "second memory title missing");
    assert!(s.contains("gamma"), "third memory title missing");
    // Overflow note reflects the true total (9 - 3 = 6 more).
    assert!(s.contains("(+6 more)"), "overflow note must use true total; got:\n{s}");
}

#[test]
fn format_digest_memories_no_overflow_when_three_or_fewer() {
    let o = make_orientation(
        None,
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        vec!["alpha", "beta", "gamma"],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    assert!(s.contains("alpha · beta · gamma"), "three titles should be shown");
    assert!(!s.contains("more)"), "no overflow note when ≤3 titles");
}

#[test]
fn short_path_strips_crates_prefix() {
    assert_eq!(
        short_path("crates/rag-rat-core/src/index/mod.rs"),
        "index/mod.rs",
        "three-segment crates prefix should be stripped"
    );
    assert_eq!(
        short_path("crates/rag-rat-mcp/src/server.rs"),
        "server.rs",
        "single-file under src should be stripped"
    );
}

#[test]
fn short_path_leaves_non_crates_paths_unchanged() {
    assert_eq!(short_path("src/database.rs"), "src/database.rs");
    assert_eq!(short_path("crates/only-two"), "crates/only-two");
    assert_eq!(
        short_path("crates/foo/not-src/bar.rs"),
        "crates/foo/not-src/bar.rs",
        "second segment is 'not-src', must not strip"
    );
    assert_eq!(short_path(""), "");
}

// ─── Health line table test ───────────────────────────────────────────────

struct HealthCase {
    live: bool,
    enabled: bool,
    head: &'static str,
    indexed: &'static str,
    expected: &'static str,
}

#[test]
fn format_digest_health_watcher_combinations() {
    let cases = [
        HealthCase {
            live: true,
            enabled: true,
            head: "aaa",
            indexed: "aaa",
            expected: "index fresh (watcher live)",
        },
        HealthCase {
            live: true,
            enabled: true,
            head: "aaa",
            indexed: "bbb",
            expected: "index syncing (watcher live)",
        },
        HealthCase {
            live: false,
            enabled: true,
            head: "aaa",
            indexed: "bbb",
            expected: "index stale — start the rag-rat MCP server",
        },
        HealthCase {
            live: false,
            enabled: false,
            head: "aaa",
            indexed: "bbb",
            expected: "watcher off; index stale — run 'rag-rat index'",
        },
        HealthCase {
            live: false,
            enabled: true,
            head: "aaa",
            indexed: "aaa",
            expected: "index fresh",
        },
    ];
    for case in &cases {
        let o = make_orientation(
            None,
            vec![],
            0,
            vec![],
            vec![],
            vec![],
            vec![],
            case.head,
            case.indexed,
            healthy_anchor(),
            0,
        );
        let s = format_digest(&o, case.live, case.enabled);
        assert!(
            s.contains(case.expected),
            "health line mismatch for live={} enabled={} head={}: expected {:?}, got:\n{}",
            case.live,
            case.enabled,
            case.head,
            case.expected,
            s
        );
    }
}

#[test]
fn format_digest_gone_adds_doctor_nudge() {
    let anchor = AnchorHealth { current: 2, relocated: 0, stale: 1, gone: 3 };
    let o =
        make_orientation(None, vec![], 0, vec![], vec![], vec![], vec![], "abc", "abc", anchor, 0);
    let s = format_digest(&o, true, true);
    assert!(s.contains("3 gone → run 'rag-rat memory doctor'"), "missing gone nudge");
}

#[test]
fn format_digest_no_doctor_nudge_when_gone_is_zero() {
    let o = make_orientation(
        None,
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        0,
    );
    let s = format_digest(&o, true, true);
    assert!(!s.contains("memory doctor"), "unexpected doctor nudge when gone=0");
}

#[test]
fn format_digest_parser_failures_shown_when_nonzero() {
    let o = make_orientation(
        None,
        vec![],
        0,
        vec![],
        vec![],
        vec![],
        vec![],
        "abc",
        "abc",
        healthy_anchor(),
        5,
    );
    let s = format_digest(&o, true, true);
    assert!(s.contains("parser failures: 5"), "missing parser failures note");
}

// ─── write-time clone check (#287) ────────────────────────────────────────────

fn write_event(tool: &str, tool_input: serde_json::Value) -> HookInput {
    HookInput {
        hook_event_name: Some("PreToolUse".to_string()),
        tool_name: tool.to_string(),
        tool_input,
        ..Default::default()
    }
}

#[test]
fn extract_clone_inputs_write_edit_multiedit() {
    let root = std::path::Path::new("/repo");

    let write = write_event(
        "Write",
        serde_json::json!({"file_path": "/repo/src/a.rs", "content": "fn f() {}"}),
    );
    let w = super::extract_clone_inputs(&write, root);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].text, "fn f() {}");
    assert_eq!(w[0].path, std::path::PathBuf::from("src/a.rs"), "path relativized to root");

    let edit = write_event(
        "Edit",
        serde_json::json!({"file_path": "/repo/src/a.rs", "new_string": "fn g() {}"}),
    );
    assert_eq!(super::extract_clone_inputs(&edit, root)[0].text, "fn g() {}");

    // MultiEdit fans out to one input per edit (the batch case).
    let multi = write_event(
        "MultiEdit",
        serde_json::json!({"file_path": "/repo/src/a.rs", "edits": [{"new_string": "fn a() {}"}, {"new_string": "fn b() {}"}]}),
    );
    let m = super::extract_clone_inputs(&multi, root);
    assert_eq!(m.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), vec![
        "fn a() {}",
        "fn b() {}"
    ]);
}

#[test]
fn extract_clone_inputs_skips_non_code_and_missing_path() {
    let root = std::path::Path::new("/repo");
    // Unknown extension → no language → nothing to check.
    let txt =
        write_event("Write", serde_json::json!({"file_path": "/repo/notes.txt", "content": "hi"}));
    assert!(super::extract_clone_inputs(&txt, root).is_empty());
    // No file_path → nothing.
    let nofile = write_event("Write", serde_json::json!({"content": "x"}));
    assert!(super::extract_clone_inputs(&nofile, root).is_empty());
}

#[test]
fn extract_clone_inputs_apply_patch_v4a() {
    let root = std::path::Path::new("/repo");
    // Codex/Cursor send the V4A envelope in `tool_input.command`: an Update (only the '+' lines),
    // an Add (whole new file), a Delete (contributes nothing), and a non-code file (dropped for
    // want of a language).
    let patch = "\
*** Begin Patch
*** Update File: /repo/src/a.rs
@@ fn existing()
 fn existing() {
-    old();
+    new_a();
+    new_b();
 }
*** Add File: /repo/src/b.rs
+fn added() {
+    body();
+}
*** Delete File: /repo/src/gone.rs
*** Add File: /repo/notes.txt
+not code, dropped
*** End Patch
";
    let ev = write_event("apply_patch", serde_json::json!({ "command": patch }));
    let got = super::extract_clone_inputs(&ev, root);
    assert_eq!(got.len(), 2, "two code files; .txt dropped, Delete contributes nothing");
    assert_eq!(got[0].path, std::path::PathBuf::from("src/a.rs"), "path relativized to root");
    assert_eq!(got[0].text, "    new_a();\n    new_b();\n", "only the added lines of the Update");
    assert_eq!(got[1].path, std::path::PathBuf::from("src/b.rs"));
    assert_eq!(got[1].text, "fn added() {\n    body();\n}\n", "the whole added file");
}

#[test]
fn format_clone_warning_renders_matches_and_is_silent_when_empty() {
    assert!(super::format_clone_warning(&[]).is_none());
    let m = rag_rat_core::index::TextCloneMatch {
        in_file: "src/a.rs".to_string(),
        name: "foo".to_string(),
        start_line: 3,
        kind: "exact",
        similarity: 1.0,
        clone_of: vec!["src/b.rs::bar".to_string()],
    };
    let out = super::format_clone_warning(&[m]).unwrap();
    assert!(
        out.contains("foo") && out.contains("identical to") && out.contains("src/b.rs::bar"),
        "{out}"
    );
}

#[test]
fn format_clone_warning_caps_the_ref_list() {
    // #292: a single near match can resemble many indexed fns — the hook shows only the first few
    // plus a "+N more" count instead of a wall of refs.
    let clone_of: Vec<String> = (0..12).map(|i| format!("src/m{i}.rs::f{i}")).collect();
    let m = rag_rat_core::index::TextCloneMatch {
        in_file: "src/a.rs".to_string(),
        name: "wide".to_string(),
        start_line: 1,
        kind: "near",
        similarity: 0.9,
        clone_of,
    };
    let out = super::format_clone_warning(&[m]).unwrap();
    assert!(out.contains("src/m0.rs::f0"), "shows the first ref: {out}");
    assert!(
        out.contains(&format!("(+{} more)", 12 - super::MAX_CLONE_REFS)),
        "caps with a count: {out}"
    );
    assert!(!out.contains("src/m11.rs::f11"), "doesn't list every ref: {out}");
}

// ─── #296 phase 4: the 40k size guard is fallback-only ──────────────────────

#[test]
fn clone_check_size_guard_is_fallback_only() {
    // RAM fallback (no eligible postings generation): skip strictly ABOVE the cap, run at/below it.
    assert!(super::clone_check_skipped_for_size(false, super::MAX_CLONE_CHECK_FUNCTIONS + 1));
    assert!(!super::clone_check_skipped_for_size(false, super::MAX_CLONE_CHECK_FUNCTIONS));
    assert!(!super::clone_check_skipped_for_size(false, 0));

    // Indexed (postings fast path eligible): the bounded lookup is corpus-size-independent, so it
    // NEVER skips on size — not even far past the cap.
    assert!(!super::clone_check_skipped_for_size(true, super::MAX_CLONE_CHECK_FUNCTIONS + 1));
    assert!(!super::clone_check_skipped_for_size(true, u64::MAX));
}
