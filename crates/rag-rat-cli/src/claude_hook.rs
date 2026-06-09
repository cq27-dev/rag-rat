//! `rag-rat claude-hook`: the Claude Code PreToolUse hook client.
//!
//! Reads the hook JSON from stdin, asks the elected listener (or falls back to a direct
//! read-only query), and prints `additionalContext` JSON. Exit 0 on every path — the hook
//! augments greps and must never block one. Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    time::Duration,
};

use rag_rat_core::{config::Config, locks, query::grep_augment, storage::IndexConnection};
use serde::Deserialize;

const SOCKET_BUDGET: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub cwd: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
}

pub struct Search {
    pub pattern: String,
    pub search_path: Option<String>,
    pub source: &'static str,
}

/// Pull a search intent out of the hook input; `None` means "not a grep, stay silent".
pub fn extract_search(input: &HookInput) -> Option<Search> {
    match input.tool_name.as_str() {
        "Grep" => {
            let pattern = input.tool_input.get("pattern")?.as_str()?.to_string();
            let search_path =
                input.tool_input.get("path").and_then(|v| v.as_str()).map(str::to_string);
            Some(Search { pattern, search_path, source: "grep_tool" })
        },
        "Bash" => {
            let command = input.tool_input.get("command")?.as_str()?;
            let (pattern, search_path) = parse_bash_search(command)?;
            Some(Search { pattern, search_path, source: "bash" })
        },
        _ => None,
    }
}

const SEARCH_COMMANDS: &[&str] = &["grep", "rg", "ag"];
/// Flags whose *next* token is a value, not the pattern. Conservative superset across the
/// three tools — a missed flag only costs a wrong-pattern no-op downstream, never a block.
const ARG_FLAGS: &[&str] = &[
    "-A",
    "-B",
    "-C",
    "-m",
    "-g",
    "-t",
    "-T",
    "-f",
    "-M",
    "--glob",
    "--type",
    "--type-not",
    "--include",
    "--exclude",
    "--exclude-dir",
    "--max-count",
    "--max-depth",
    "--context",
    "--after-context",
    "--before-context",
    "--file",
    "--ignore-file",
    "--threads",
    "--colors",
];

/// Extract (pattern, path) from a shell command that runs grep/rg/ag, or `None` when the
/// command doesn't or parsing would have to guess. False negatives are fine; false
/// positives are not (spec: Bash command parsing).
pub fn parse_bash_search(command: &str) -> Option<(String, Option<String>)> {
    if command.contains('`') || command.contains("$(") {
        return None; // substitution: ambiguous
    }
    // Split into pipeline/sequence segments; examine each for a search command.
    for segment in split_top_level(command) {
        let tokens = shell_tokens(&segment)?;
        let mut tokens = tokens.as_slice();
        // Skip env-var prefixes (FOO=bar) before the command word.
        while tokens.first().is_some_and(|t| t.contains('=') && !t.starts_with('-')) {
            tokens = &tokens[1..];
        }
        let Some(command_word) = tokens.first() else { continue };
        let base = command_word.rsplit('/').next().unwrap_or(command_word);
        if base == "xargs" || base == "find" {
            return None; // grep as an argument of these is ambiguous
        }
        if !SEARCH_COMMANDS.contains(&base) {
            continue;
        }
        let mut pattern: Option<String> = None;
        let mut path: Option<String> = None;
        let mut rest = tokens[1..].iter();
        while let Some(token) = rest.next() {
            if let Some(value) = token.strip_prefix("--regexp=") {
                pattern.get_or_insert_with(|| value.to_string());
            } else if token == "-e" || token == "--regexp" {
                if let Some(value) = rest.next() {
                    pattern.get_or_insert_with(|| value.to_string());
                }
            } else if ARG_FLAGS.contains(&token.as_str()) {
                rest.next(); // consume the flag's value
            } else if token.starts_with('-') && token.len() > 1 {
                // value-less flag (or unknown): skip
            } else if pattern.is_none() {
                pattern = Some(token.to_string());
            } else if path.is_none() {
                path = Some(token.to_string());
            }
        }
        return pattern.map(|p| (p, path));
    }
    None
}

/// Split on top-level `|`, `&&`, `||`, `;` (quote-aware); also drop a leading `cd …` segment.
///
/// Quote characters are preserved verbatim into the segment so that [`shell_tokens`] can
/// strip them itself — a top-level separator inside quotes must not split, and a quoted
/// pattern (`rg "quoted pattern" src`) must survive intact for re-tokenization.
fn split_top_level(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            // Closing quote: keep the quote char so shell_tokens sees a balanced pair.
            (Some(q), c) if c == q => {
                quote = None;
                current.push(c);
            },
            (Some(_), c) => current.push(c),
            // Opening quote: keep the quote char verbatim.
            (None, '\'' | '"') => {
                quote = Some(ch);
                current.push(ch);
            },
            (None, '|' | ';') => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                segments.push(std::mem::take(&mut current));
            },
            (None, '&') => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                segments.push(std::mem::take(&mut current));
            },
            (None, c) => current.push(c),
        }
    }
    segments.push(current);
    segments
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("cd ") && *s != "cd")
        .collect()
}

/// Quote-aware tokenization of one segment. `None` on unbalanced quotes (ambiguous).
fn shell_tokens(segment: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    for ch in segment.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                quoted = true;
            },
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() || quoted {
                    tokens.push(std::mem::take(&mut current));
                    quoted = false;
                }
            },
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() || quoted {
        tokens.push(current);
    }
    Some(tokens)
}

/// Entry point for `rag-rat claude-hook`. Every failure path prints nothing and returns
/// Ok(()) — the hook must never block a grep (spec: error posture).
pub fn run() -> anyhow::Result<()> {
    let _ = run_inner(); // swallow: silence is the contract
    Ok(())
}

fn run_inner() -> anyhow::Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: HookInput = serde_json::from_str(&raw)?;
    let Some(search) = extract_search(&input) else { return Ok(()) };
    let Some(config) = find_config(Path::new(&input.cwd)) else { return Ok(()) };

    let context = ask_listener(&config, &input.session_id, &search)
        .unwrap_or_else(|| fallback_compose(&config, &search));
    if let Some(context) = context {
        // PreToolUse contract: allow + additionalContext; plain stdout is debug-only.
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "additionalContext": context,
                }
            })
        );
    }
    Ok(())
}

/// Walk up from the hook's cwd to the nearest rag-rat.toml. `None` ⇒ not a rag-rat repo ⇒
/// silent no-op (what makes `--global` install safe).
fn find_config(start: &Path) -> Option<Config> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let candidate = current.join("rag-rat.toml");
        if candidate.is_file() {
            return Config::load(&candidate).ok();
        }
        dir = current.parent();
    }
    None
}

/// Outer Option: did the listener answer at all (None ⇒ fall back). Inner Option: did it
/// have anything new to say.
fn ask_listener(config: &Config, session_id: &str, search: &Search) -> Option<Option<String>> {
    #[cfg(unix)]
    {
        use std::{
            io::{BufRead, BufReader, Write as _},
            os::unix::net::UnixStream,
        };
        let socket = socket_path(config);
        // SOCKET_BUDGET covers both read and write. Unix-domain connect() completes into
        // the listener's backlog immediately (no network round-trip), so no separate connect
        // timeout is needed.
        let stream = UnixStream::connect(&socket).ok()?;
        stream.set_read_timeout(Some(SOCKET_BUDGET)).ok()?;
        stream.set_write_timeout(Some(SOCKET_BUDGET)).ok()?;
        let request = serde_json::json!({
            "v": 1, "kind": "grep_augment", "session_id": session_id,
            "pattern": search.pattern, "search_path": search.search_path,
            "source": search.source,
        });
        let mut writer = stream.try_clone().ok()?;
        writeln!(writer, "{request}").ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).ok()?;
        let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
        if reply.get("v")?.as_u64()? != 1 {
            return None;
        }
        Some(reply.get("context")?.as_str().map(str::to_string))
    }
    #[cfg(not(unix))]
    {
        let _ = (config, session_id, search);
        None
    }
}

/// Single source of truth via `locks::hook_socket_path_for`; same computation as the MCP
/// listener's `socket_path_for`, guaranteed not to diverge.
fn socket_path(config: &Config) -> PathBuf {
    locks::hook_socket_path_for(config)
}

/// Stateless direct read (no dedupe — spec: fallback path). Any error ⇒ silence.
fn fallback_compose(config: &Config, search: &Search) -> Option<String> {
    let conn = IndexConnection::open_read_only(&config.database).ok()?;
    grep_augment::compose(
        conn.connection(),
        &search.pattern,
        search.search_path.as_deref(),
        &grep_augment::DedupeFilter::default(),
    )
    .ok()
    .flatten()
    .map(|out| out.context)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            ("git log | rg fix", "fix", None),
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
}
