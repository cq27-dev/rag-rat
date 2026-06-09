//! End-to-end tests for `rag-rat claude-hook` (Unix only for socket paths; the no-op
//! contract tests run everywhere).

use std::{
    io::Write,
    process::{Command, Stdio},
};

fn run_hook(stdin_body: &str, cwd: &std::path::Path) -> (String, std::process::ExitStatus) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("claude-hook")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.stdin.as_mut().unwrap().write_all(stdin_body.as_bytes());
    let out = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status)
}

#[test]
fn no_rag_rat_toml_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-noindex-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir, "hook_event_name": "PreToolUse",
        "tool_name": "Grep", "tool_input": {"pattern": "anything"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty(), "must print nothing without an index, got: {stdout}");
}

#[test]
fn garbage_stdin_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-garbage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (stdout, status) = run_hook("this is not json", &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}

#[test]
fn non_search_tool_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-read-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir, "hook_event_name": "PreToolUse",
        "tool_name": "Read", "tool_input": {"path": "/x"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}
