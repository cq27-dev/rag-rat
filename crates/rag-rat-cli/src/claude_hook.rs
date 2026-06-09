//! `rag-rat claude-hook`: the Claude Code PreToolUse hook client.
//!
//! Reads the hook JSON from stdin, asks the elected listener (or falls back to a direct
//! read-only query), and prints `additionalContext` JSON. Exit 0 on every path — the hook
//! augments greps and must never block one. Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`.

use serde::Deserialize;

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
    "-A", "-B", "-C", "-m", "-g", "-t", "-T", "-f", "-M", "--glob", "--type", "--type-not",
    "--include", "--exclude", "--exclude-dir", "--max-count", "--max-depth", "--context",
    "--after-context", "--before-context", "--file", "--ignore-file", "--threads", "--colors",
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
            "rg",                       // no pattern
            "find . -name '*.rs' -exec grep foo {} \\;", // -exec: ambiguous
            "echo `rg foo`",            // backticks: ambiguous
            "xargs grep foo",           // xargs: ambiguous
            "groups",                   // not grep
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
