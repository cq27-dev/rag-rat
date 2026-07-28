//! `rag-rat agent-hook`: coding-agent hook adapters for Claude/Codex, Cursor, and VS Code.
//!
//! Reads the hook JSON from stdin and branches on `hook_event_name`:
//! - `"SessionStart"`: injects a read-only repo orientation digest into the model context.
//! - successful edit events trigger a scoped reindex in a DETACHED child (`edit_reindex`),
//!   watcher-aware — see [`posttooluse`].
//! - tool events run grep/read augmentation or a write-time clone check after harness-specific tool
//!   names and camel/snake-case fields have been normalized.
//! - context is serialized only through the selected harness's documented output field; raw stdin
//!   is never copied into output.
//!
//! Exit 0 on every path — the hook must never block a tool call or session start.

use std::io::Read as _;
use std::path::{Path, PathBuf};
// Duration (SOCKET_BUDGET) backs the unix-only listener path.
#[cfg(unix)]
use std::time::Duration;

use rag_rat_base::config::Config;
use rag_rat_base::language::Language;
use rag_rat_base::locks;
use rag_rat_core::index::{CloneCheckInput, IndexDatabase, TextCloneMatch};
use rag_rat_core::query::orientation::Orientation;
use rag_rat_core::query::{grep_augment, read_augment};
use rag_rat_db::storage::IndexConnection;
use serde::Deserialize;

use crate::cli::AgentHookHarnessArg;

pub(crate) mod edit_reindex;

/// Skip the write-time clone check above this many fingerprinted functions — but ONLY in the RAM
/// FALLBACK mode, which builds an in-RAM inverted index over all of them (O(functions)) and so
/// would add a perceptible delay to a write on a very large repo. When the persisted-postings fast
/// path is eligible (#296), the check is a bounded indexed lookup independent of corpus size, so
/// this guard does not apply — see [`clone_check_skipped_for_size`].
const MAX_CLONE_CHECK_FUNCTIONS: u64 = 40_000;

/// Minimum similarity for a NEAR clone to be surfaced by the WRITE-TIME hook (#292). Higher than
/// `find_clones`' 0.7: boilerplate-heavy code (esp. across a whole repo) pushes unrelated functions
/// to ~0.7-0.8 token overlap, so a lower bar floods the agent with non-actionable matches. Exact
/// (struct_hash) matches are always surfaced regardless of this.
const HOOK_NEAR_THRESHOLD: f64 = 0.85;

/// Cap the existing-function refs listed per clone match in the hook output — a single new function
/// can be similar to many indexed ones; listing them all is noise. Show the first few + a count.
const MAX_CLONE_REFS: usize = 5;

// Only the unix Unix-socket listener path uses this; dead on Windows (which has no warm listener).
#[cfg(unix)]
const SOCKET_BUDGET: Duration = Duration::from_millis(250);

/// Parsed hook input.  Fields absent on `SessionStart` (`tool_name`, `tool_input`) are
/// `#[serde(default)]` so deserialization succeeds for every event type.
#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    /// `"PreToolUse"`, `"SessionStart"`, etc. — absent on some older hook payloads.
    pub hook_event_name: Option<String>,
    /// `"startup"` | `"resume"` | `"clear"` | `"compact"` (SessionStart only).
    pub source: Option<String>,
    /// Present on PreToolUse only.
    #[serde(default)]
    pub tool_name: String,
    /// Present on PreToolUse only.
    #[serde(default)]
    pub tool_input: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Harness {
    Native,
    Cursor,
    Vscode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    SessionStart,
    PreToolUse,
    PostToolUse,
    CursorPostToolUse,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputEvent {
    SessionStart,
    PreToolUse,
    CursorBeforeShell,
    CursorPostToolUse,
    None,
}

struct NormalizedHook {
    harness: Harness,
    input: HookInput,
    dispatch: Dispatch,
    output_event: OutputEvent,
}

fn normalize_hook(raw: &str, requested: AgentHookHarnessArg) -> Option<NormalizedHook> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let harness = match requested {
        AgentHookHarnessArg::Cursor => Harness::Cursor,
        AgentHookHarnessArg::Vscode => Harness::Vscode,
        AgentHookHarnessArg::Auto => detect_harness(&value),
    };
    match harness {
        Harness::Native => normalize_native(value),
        Harness::Cursor => normalize_cursor(&value),
        Harness::Vscode => normalize_vscode(&value),
    }
}

fn detect_harness(value: &serde_json::Value) -> Harness {
    let event = value.get("hook_event_name").and_then(|field| field.as_str()).unwrap_or_default();
    if value.get("cursor_version").is_some() || event.chars().next().is_some_and(char::is_lowercase)
    {
        return Harness::Cursor;
    }
    let tool = value.get("tool_name").and_then(|field| field.as_str()).unwrap_or_default();
    if value.get("source").and_then(|field| field.as_str()) == Some("new") || is_vscode_tool(tool) {
        return Harness::Vscode;
    }
    Harness::Native
}

fn normalize_native(value: serde_json::Value) -> Option<NormalizedHook> {
    let input: HookInput = serde_json::from_value(value).ok()?;
    let (dispatch, output_event) = match input.hook_event_name.as_deref() {
        Some("SessionStart") => (Dispatch::SessionStart, OutputEvent::SessionStart),
        Some("PostToolUse") => (Dispatch::PostToolUse, OutputEvent::None),
        Some("PreToolUse") | None => (Dispatch::PreToolUse, OutputEvent::PreToolUse),
        _ => (Dispatch::Ignore, OutputEvent::None),
    };
    Some(NormalizedHook { harness: Harness::Native, input, dispatch, output_event })
}

fn normalize_cursor(value: &serde_json::Value) -> Option<NormalizedHook> {
    let event = value.get("hook_event_name").and_then(|field| field.as_str())?;
    let cwd = cursor_hook_cwd(value);
    let session_id = value
        .get("session_id")
        .or_else(|| value.get("conversation_id"))
        .and_then(|field| field.as_str())
        .unwrap_or_default()
        .to_string();
    let mut input = HookInput { session_id, cwd, ..Default::default() };
    let (dispatch, output_event) = match event {
        "sessionStart" => {
            input.hook_event_name = Some("SessionStart".to_string());
            input.source = Some("startup".to_string());
            (Dispatch::SessionStart, OutputEvent::SessionStart)
        },
        "beforeShellExecution" => {
            input.hook_event_name = Some("PreToolUse".to_string());
            input.tool_name = "Bash".to_string();
            input.tool_input = normalize_tool_input("Bash", value);
            (Dispatch::PreToolUse, OutputEvent::CursorBeforeShell)
        },
        "beforeReadFile" => {
            input.hook_event_name = Some("PreToolUse".to_string());
            input.tool_name = "Read".to_string();
            input.tool_input = normalize_tool_input("Read", value);
            // Cursor documents no model-context field for beforeReadFile. The native manifest uses
            // postToolUse for read augmentation; an explicitly invoked beforeReadFile stays quiet.
            (Dispatch::PreToolUse, OutputEvent::None)
        },
        "afterFileEdit" => {
            input.hook_event_name = Some("PostToolUse".to_string());
            input.tool_name = "Edit".to_string();
            input.tool_input = normalize_tool_input("Edit", value);
            (Dispatch::PostToolUse, OutputEvent::None)
        },
        "postToolUse" => {
            let raw_tool =
                value.get("tool_name").and_then(|field| field.as_str()).unwrap_or_default();
            let tool_input = value.get("tool_input").unwrap_or(&serde_json::Value::Null);
            let Some(tool_name) = normalize_tool_name(raw_tool, tool_input) else {
                return Some(NormalizedHook {
                    harness: Harness::Cursor,
                    input,
                    dispatch: Dispatch::Ignore,
                    output_event: OutputEvent::None,
                });
            };
            input.hook_event_name = Some("PostToolUse".to_string());
            input.tool_input = normalize_tool_input(&tool_name, tool_input);
            input.tool_name = tool_name;
            (Dispatch::CursorPostToolUse, OutputEvent::CursorPostToolUse)
        },
        // MCP hooks and every unsupported lifecycle event are intentionally inert.
        _ => (Dispatch::Ignore, OutputEvent::None),
    };
    Some(NormalizedHook { harness: Harness::Cursor, input, dispatch, output_event })
}

fn normalize_vscode(value: &serde_json::Value) -> Option<NormalizedHook> {
    let event = value.get("hook_event_name").and_then(|field| field.as_str())?;
    let mut input = HookInput {
        session_id: value
            .get("session_id")
            .and_then(|field| field.as_str())
            .unwrap_or_default()
            .to_string(),
        cwd: hook_cwd(value),
        hook_event_name: Some(event.to_string()),
        ..Default::default()
    };
    let (dispatch, output_event) = match event {
        "SessionStart" => {
            input.source = value
                .get("source")
                .and_then(|field| field.as_str())
                .map(|source| if source == "new" { "startup" } else { source }.to_string());
            (Dispatch::SessionStart, OutputEvent::SessionStart)
        },
        "PreToolUse" | "PostToolUse" => {
            let raw_tool =
                value.get("tool_name").and_then(|field| field.as_str()).unwrap_or_default();
            let tool_input = value.get("tool_input").unwrap_or(&serde_json::Value::Null);
            let Some(tool_name) = normalize_tool_name(raw_tool, tool_input) else {
                return Some(NormalizedHook {
                    harness: Harness::Vscode,
                    input,
                    dispatch: Dispatch::Ignore,
                    output_event: OutputEvent::None,
                });
            };
            input.tool_input = normalize_tool_input(&tool_name, tool_input);
            input.tool_name = tool_name;
            if event == "PreToolUse" {
                (Dispatch::PreToolUse, OutputEvent::PreToolUse)
            } else {
                (Dispatch::PostToolUse, OutputEvent::None)
            }
        },
        _ => (Dispatch::Ignore, OutputEvent::None),
    };
    Some(NormalizedHook { harness: Harness::Vscode, input, dispatch, output_event })
}

fn hook_cwd(value: &serde_json::Value) -> String {
    value
        .get("cwd")
        .and_then(|field| field.as_str())
        .or_else(|| {
            value
                .get("workspace_roots")
                .and_then(|field| field.as_array())
                .and_then(|roots| roots.first())
                .and_then(|root| root.as_str())
        })
        .unwrap_or_default()
        .to_string()
}

fn cursor_hook_cwd(value: &serde_json::Value) -> String {
    if let Some(cwd) = value.get("cwd").and_then(|field| field.as_str()) {
        return cwd.to_string();
    }
    let event_path = string_field(value, &["file_path", "filePath", "path"]).or_else(|| {
        value
            .get("tool_input")
            .and_then(|tool_input| string_field(tool_input, &["file_path", "filePath", "path"]))
    });
    let roots = value.get("workspace_roots").and_then(|field| field.as_array());
    if let (Some(event_path), Some(roots)) = (event_path, roots) {
        let event_path = Path::new(event_path);
        if let Some(root) = roots
            .iter()
            .filter_map(|root| root.as_str())
            .filter(|root| event_path.starts_with(root))
            .max_by_key(|root| Path::new(root).components().count())
        {
            return root.to_string();
        }
    }
    hook_cwd(value)
}

fn is_vscode_tool(tool: &str) -> bool {
    matches!(
        tool,
        "run_in_terminal"
            | "runTerminalCommand"
            | "read_file"
            | "readFile"
            | "create_file"
            | "createFile"
            | "replace_string_in_file"
            | "replaceStringInFile"
            | "multi_replace_string_in_file"
            | "multiReplaceStringInFile"
            | "editFiles"
    )
}

fn normalize_tool_name(raw_tool: &str, tool_input: &serde_json::Value) -> Option<String> {
    let normalized = match raw_tool {
        "Shell" | "Bash" | "run_in_terminal" | "runTerminalCommand" | "run_terminal_command" =>
            "Bash",
        "Grep" | "grep_search" | "grepSearch" => "Grep",
        "Read" | "read_file" | "readFile" => "Read",
        "create_file" | "createFile" => "Write",
        "replace_string_in_file" | "replaceStringInFile" => "Edit",
        "multi_replace_string_in_file" | "multiReplaceStringInFile" | "editFiles" => "MultiEdit",
        "apply_patch" => "apply_patch",
        "Write" => {
            if tool_input.get("edits").is_some() || tool_input.get("replacements").is_some() {
                "MultiEdit"
            } else if tool_input.get("new_string").is_some()
                || tool_input.get("newString").is_some()
            {
                "Edit"
            } else {
                "Write"
            }
        },
        "Edit" | "MultiEdit" => raw_tool,
        _ => return None,
    };
    Some(normalized.to_string())
}

fn normalize_tool_input(tool_name: &str, value: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(command) = string_field(value, &["command", "commandLine"]) {
        out.insert("command".to_string(), command.into());
    }
    if let Some(pattern) = string_field(value, &["pattern", "query"]) {
        out.insert("pattern".to_string(), pattern.into());
    }
    if let Some(path) = string_field(value, &["file_path", "filePath", "path"]) {
        let key = if tool_name == "Grep" { "path" } else { "file_path" };
        out.insert(key.to_string(), path.into());
    }
    if let Some(content) = string_field(value, &["content"]) {
        out.insert("content".to_string(), content.into());
    }
    if let Some(new_string) = string_field(value, &["new_string", "newString", "replacement"]) {
        out.insert("new_string".to_string(), new_string.into());
    }
    if let Some(edits) = value.get("edits").or_else(|| value.get("replacements"))
        && let Some(edits) = edits.as_array()
    {
        let normalized = edits
            .iter()
            .filter_map(|edit| {
                let mut normalized = serde_json::Map::new();
                if let Some(path) = string_field(edit, &["file_path", "filePath", "path"]) {
                    normalized.insert("file_path".to_string(), path.into());
                }
                if let Some(new_string) =
                    string_field(edit, &["new_string", "newString", "replacement"])
                {
                    normalized.insert("new_string".to_string(), new_string.into());
                }
                (!normalized.is_empty()).then_some(serde_json::Value::Object(normalized))
            })
            .collect::<Vec<_>>();
        out.insert("edits".to_string(), normalized.into());
    }
    if let Some(files) = value.get("files").and_then(|field| field.as_array()) {
        let paths = files
            .iter()
            .filter_map(|file| {
                file.as_str().map(str::to_string).or_else(|| {
                    string_field(file, &["file_path", "filePath", "path"]).map(str::to_string)
                })
            })
            .collect::<Vec<_>>();
        out.insert("files".to_string(), serde_json::json!(paths));
    }
    serde_json::Value::Object(out)
}

fn string_field<'a>(value: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| value.get(name).and_then(|field| field.as_str()))
}

pub struct Search {
    pub pattern: String,
    pub search_path: Option<String>,
    // Set on every platform but read only by the unix listener path (it's sent in the socket
    // request); on Windows there is no listener, so it's intentionally unread there.
    #[cfg_attr(not(unix), allow(dead_code))]
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
    for (piped, segment) in split_top_level(command) {
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
        // A search command DOWNSTREAM of a pipe is filtering another tool's output
        // (`cargo test | grep …`, `gh … | rg …`), not a code search — skip it so augmentation
        // doesn't fire on incidental greps (#138). A grep that is the pipeline HEAD (`grep … |
        // head`) or a sequenced command (`cd x && rg …`) is a real search and still
        // matches.
        if piped {
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
/// Each returned segment carries a `piped` flag: true when it was preceded by a single `|` (i.e. it
/// consumes the previous command's output). `||`, `&&`, `;`, `&`, and the first segment are not
/// piped — they're independent commands. This lets the caller tell a real grep (pipeline head or
/// sequenced) from an incidental output filter (#138).
///
/// Quote characters are preserved verbatim into the segment so that [`shell_tokens`] can
/// strip them itself — a top-level separator inside quotes must not split, and a quoted
/// pattern (`rg "quoted pattern" src`) must survive intact for re-tokenization.
fn split_top_level(command: &str) -> Vec<(bool, String)> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    // Whether the segment currently being accumulated was preceded by a single `|`.
    let mut piped = false;
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
            (None, '|') => {
                // `|` pipes into the next segment; `|&` (bash shorthand for `2>&1 |`) also pipes
                // (stdout+stderr) so the next segment is still a downstream filter; `||` (logical
                // or) does NOT pipe — it's an independent command.
                let next_piped = match chars.peek() {
                    Some('|') => {
                        chars.next();
                        false
                    },
                    Some('&') => {
                        chars.next();
                        true
                    },
                    _ => true,
                };
                segments.push((piped, std::mem::take(&mut current)));
                piped = next_piped;
            },
            (None, ';') => {
                segments.push((piped, std::mem::take(&mut current)));
                piped = false;
            },
            (None, '&') => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                segments.push((piped, std::mem::take(&mut current)));
                piped = false;
            },
            (None, c) => current.push(c),
        }
    }
    segments.push((piped, current));
    segments
        .into_iter()
        .map(|(piped, s)| (piped, s.trim().to_string()))
        .filter(|(_, s)| !s.is_empty() && !s.starts_with("cd ") && *s != "cd")
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
            (None, c) if c.is_whitespace() =>
                if !current.is_empty() || quoted {
                    tokens.push(std::mem::take(&mut current));
                    quoted = false;
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

/// Entry point for `rag-rat agent-hook`. Every failure path prints nothing and returns `Ok(())`.
pub fn run(requested: AgentHookHarnessArg) -> anyhow::Result<()> {
    let _ = run_inner(requested); // swallow: silence is the contract
    Ok(())
}

fn run_inner(requested: AgentHookHarnessArg) -> anyhow::Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let Some(normalized) = normalize_hook(&raw, requested) else { return Ok(()) };
    let context = match normalized.dispatch {
        Dispatch::SessionStart => session_start(&normalized.input)?,
        Dispatch::PreToolUse => pretooluse(&normalized.input)?,
        Dispatch::PostToolUse => {
            posttooluse(&normalized.input)?;
            None
        },
        Dispatch::CursorPostToolUse => cursor_posttooluse(&normalized.input)?,
        Dispatch::Ignore => None,
    };
    if let Some(context) = context {
        print_context(normalized.harness, normalized.output_event, &context);
    }
    Ok(())
}

/// SessionStart path: inject a read-only repo orientation digest as plain stdout.
/// Every error path prints nothing and returns Ok — never block session start.
fn session_start(input: &HookInput) -> anyhow::Result<Option<String>> {
    // Allowlist: only fire for meaningful session triggers, not resume.
    match input.source.as_deref() {
        Some("startup") | Some("clear") | Some("compact") => {},
        _ => return Ok(None),
    }
    let Some(config) = find_governing_config(Path::new(&input.cwd)) else { return Ok(None) };
    // Do not open/create an absent database. Hooks are inert until the first index exists.
    if !config.database.is_file() {
        return Ok(None);
    }
    // Open read-only; compose the orientation; print the digest.
    // Any error (locked, corrupt, etc.) propagates via `?` to run_inner, which run() swallows
    // (`let _ = run_inner()`) — so the hook stays silent and never blocks session start. Do NOT
    // add error prints here: this branch's stdout is injected as model context.
    let conn = IndexConnection::open_read_only(&config.database)?;
    // Version skew (#484): a schema created by a newer rag-rat means this binary — and the MCP
    // server this session just started, which is at most as new — will refuse every open. Say so
    // actionably instead of composing a digest that half-works today and errors tomorrow. The
    // schema message already carries the remedy (and the non-Linux no-hot-upgrade caveat).
    let schema = rag_rat_db::schema::status(conn.connection())?;
    if schema.state == rag_rat_db::schema::SchemaState::Newer {
        return Ok(Some(newer_schema_notice(&schema.message)));
    }
    // Scope orientation to the session's worktree: `config.root` (anchored to the main worktree) is
    // the base index, `input.cwd` is where the session is — a linked worktree gets its branch
    // overlay (#219). find_config already anchored config.root to the main worktree, so the two
    // together resolve the right overlay even when the session is launched from a linked
    // checkout.
    let o = rag_rat_core::query::orientation::orientation(
        conn.connection(),
        &config.root,
        Path::new(&input.cwd),
        config.repo_id_override.as_deref(),
    )?;
    let (live, enabled) = watcher_state(&config);
    let mut context = format_digest(&o, live, enabled);
    if let Some(line) = version_check_line(&config) {
        context.push_str(&line);
    }
    Ok(Some(context))
}

/// The session-start notice replacing the digest when the index schema is NEWER than this binary
/// (#484). Pure so it's testable; `schema_message` is the core refusal text, which carries the
/// remedy and the platform caveat.
fn newer_schema_notice(schema_message: &str) -> String {
    format!(
        "⚠ rag-rat version skew: {schema_message}. Repo-intelligence MCP tools in this session \
         will error until then.\n"
    )
}

/// One digest line stating the running version vs the latest published on crates.io, with the
/// update command when behind. Reads the cached check only (no network — the MCP server refreshes
/// it out of band); `None` when version checking is disabled or no check has been cached yet, so a
/// fresh repo or an opted-out user sees nothing.
fn version_check_line(config: &Config) -> Option<String> {
    let status =
        rag_rat_core::version_check::cached_status(config.version_check.enabled, &config.database)?;
    version_line(&status)
}

/// Format the digest version line from a status (pure, so it's testable without a config/cache).
/// `None` when the latest version is unknown (no successful check cached yet) — stay quiet rather
/// than print a half-answer.
fn version_line(status: &rag_rat_core::version_check::VersionStatus) -> Option<String> {
    let latest = status.latest_version.as_deref()?;
    if status.update_available {
        Some(format!(
            "\n⚠ rag-rat update available: {} → {} — run `{}`\n",
            status.current_version, latest, status.update_command
        ))
    } else if latest == status.current_version {
        Some(format!("\nrag-rat {} (latest on crates.io)\n", status.current_version))
    } else {
        // Local build ahead of the published latest (dev / pre-release after a version bump) —
        // don't call it the crates.io latest.
        Some(format!("\nrag-rat {} (ahead of crates.io latest {latest})\n", status.current_version))
    }
}

/// PreToolUse path: the write-time clone check (#287) on the edit tools, read augmentation on
/// `Read` (#756), grep augmentation on a code search otherwise.
fn pretooluse(input: &HookInput) -> anyhow::Result<Option<String>> {
    if matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit" | "apply_patch") {
        return clone_check(input);
    }
    if input.tool_name == "Read" {
        return read_augment_hook(input);
    }
    let Some(search) = extract_search(input) else { return Ok(None) };
    let Some(config) = find_governing_config(Path::new(&input.cwd)) else { return Ok(None) };

    // Pass the session cwd through both paths so the grep-augmentation is scoped to the worktree
    // the session is in (a linked worktree gets its branch overlay), not just the base index
    // (#219).
    let context = ask_listener(&config, &input.session_id, &input.cwd, &search)
        .unwrap_or_else(|| fallback_compose(&config, &input.cwd, &search));
    Ok(context)
}

/// Read-augmentation path (#756): when the agent opens a file, inject the repo memories bound to it
/// (+ its directory) and the load-bearing symbols defined in it. Silent no-op unless the file is a
/// tracked, root-relative path with something to say. Prefers the warm listener (per-session
/// dedup); falls back to a direct read-only compose.
fn read_augment_hook(input: &HookInput) -> anyhow::Result<Option<String>> {
    let Some(config) = find_governing_config(Path::new(&input.cwd)) else { return Ok(None) };
    let Some(file_path) = input.tool_input.get("file_path").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let indexed_root = session_indexed_root(&config, &input.cwd);
    let Some(rel_path) = worktree_rel_path(file_path, &indexed_root) else { return Ok(None) };
    let context = ask_listener_read(&config, &input.session_id, &input.cwd, &rel_path)
        .unwrap_or_else(|| fallback_compose_read(&config, &input.cwd, &rel_path));
    Ok(context)
}

/// The absolute directory that indexed `files.path` are relative to, IN THE SESSION'S checkout.
/// `config.root` is main-anchored (`Config::load`, #219) and already folds in the config's position
/// under the git root plus `[index] root`.
///
/// - In the MAIN worktree (or when no main resolves) `config.root` already IS the session's indexed
///   root — return it, so `[index] root` and a git-root-nested config are both honored as-is.
/// - In a LINKED worktree, rebase `config.root` from the MAIN git-worktree root onto the SESSION
///   git-worktree root. BOTH anchors are git WORKDIR roots (git topology, NOT `rag-rat.toml`
///   directories), so the whole config-relative suffix — a nested config dir AND `[index] root` —
///   is preserved exactly once, and a branch-only-config linked worktree (whose main has no config)
///   still resolves (#756 review).
fn session_indexed_root(config: &Config, cwd: &str) -> PathBuf {
    let cwd = Path::new(cwd);
    match rag_rat_base::config::linked_worktree_main_root(cwd) {
        Some(main_git_root) => match rag_rat_base::config::worktree_root(cwd) {
            Some(session_git_root) => rebase_root(&config.root, &main_git_root, &session_git_root),
            None => config.root.clone(),
        },
        None => config.root.clone(),
    }
}

/// Rebase `config_root` from `main_top` onto `session_top`, preserving the whole suffix below
/// `main_top` (a git-root-nested config dir AND `[index] root`). Pure, so the linked-worktree ×
/// nested-config × subdir-root matrix is unit-tested without a filesystem. Returns `config_root`
/// unchanged when it isn't under `main_top` (an unexpected topology — never guess a wrong prefix).
fn rebase_root(config_root: &Path, main_top: &Path, session_top: &Path) -> PathBuf {
    match config_root.strip_prefix(main_top) {
        Ok(subdir) => session_top.join(subdir),
        Err(_) => config_root.to_path_buf(),
    }
}

/// The worktree-root-relative, SLASH-separated path for `file_path`, or `None` when it's OUTSIDE
/// `worktree_root` (nothing indexed to augment it with). Indexed bindings and symbols are keyed by
/// a forward-slash root-relative path on every OS, so join the stripped components with `/` rather
/// than `to_string_lossy` (which would leave Windows backslashes that never match) (#756 review).
fn worktree_rel_path(file_path: &str, worktree_root: &Path) -> Option<String> {
    let file = Path::new(file_path);
    // `worktree_root` comes back canonicalized from discovery while the hook's `file_path` is raw,
    // so canonicalize BOTH (when they exist) to survive a symlinked/`..`-laden checkout path;
    // fall back to a raw strip when either can't be resolved (keeps this unit-testable with
    // synthetic paths).
    let file_c = file.canonicalize();
    let root_c = worktree_root.canonicalize();
    let (f, r): (&Path, &Path) = match (file_c.as_deref(), root_c.as_deref()) {
        (Ok(f), Ok(r)) => (f, r),
        _ => (file, worktree_root),
    };
    let rel = f.strip_prefix(r).ok()?;
    Some(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
}

/// Emit an `additionalContext` block for a PreToolUse hook. PreToolUse contract: additionalContext
/// only — no `permissionDecision` (Codex rejects the "allow" value, and we only inject context,
/// never gate the tool). Plain stdout is debug-only.
fn print_context(harness: Harness, event: OutputEvent, context: &str) {
    if let Some(output) = format_context_output(harness, event, context) {
        print!("{output}");
        if harness != Harness::Native || event != OutputEvent::SessionStart {
            println!();
        }
    }
}

fn format_context_output(harness: Harness, event: OutputEvent, context: &str) -> Option<String> {
    let output = match (harness, event) {
        (Harness::Native, OutputEvent::SessionStart) => return Some(context.to_string()),
        (Harness::Vscode, OutputEvent::SessionStart) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }
        }),
        (Harness::Native | Harness::Vscode, OutputEvent::PreToolUse) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "additionalContext": context,
            }
        }),
        (Harness::Cursor, OutputEvent::SessionStart | OutputEvent::CursorPostToolUse) => {
            serde_json::json!({ "additional_context": context })
        },
        (Harness::Cursor, OutputEvent::CursorBeforeShell) => {
            serde_json::json!({ "agent_message": context })
        },
        (_, OutputEvent::None) | (Harness::Cursor, OutputEvent::PreToolUse) => return None,
        (
            Harness::Native | Harness::Vscode,
            OutputEvent::CursorBeforeShell | OutputEvent::CursorPostToolUse,
        ) => return None,
    };
    Some(output.to_string())
}

/// Cursor's generic postToolUse is the only documented context channel shared by reads and write
/// results. Reads reuse PreToolUse composition; edits run a clone check only when the normalized
/// payload contains actual replacement text. The dedicated afterFileEdit event owns reindexing.
fn cursor_posttooluse(input: &HookInput) -> anyhow::Result<Option<String>> {
    if matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit" | "apply_patch") {
        return clone_check(input);
    }
    pretooluse(input)
}

/// PostToolUse path (#661): after an edit tool completes, trigger a scoped reindex of the edited
/// path(s) so an agent-driven repo stays fresh without the FS watcher — precise, watch-free,
/// cross-OS. Watcher-aware and detached; exits 0 on every path so it never blocks or fails the
/// agent's tool call.
fn posttooluse(input: &HookInput) -> anyhow::Result<()> {
    let paths = extract_edited_paths(input);
    if paths.is_empty() {
        return Ok(());
    }
    // Governing discovery (not bare `find_config`): a linked-worktree edit with no branch-local
    // config must still resolve the main config so `reindex_paths` routes it through the overlay.
    let Some(config) = find_governing_config(Path::new(&input.cwd)) else { return Ok(()) };
    // The scoped `index --paths` mode requires an existing base index (#659/#427) — with no DB
    // there is nothing to reconcile into yet; the first `rag-rat index` builds it. (Do NOT
    // open/create it.)
    if !config.database.is_file() {
        return Ok(());
    }
    let to_reindex = paths_to_reindex(watcher_state(&config).0, &paths);
    if to_reindex.is_empty() {
        return Ok(());
    }
    // The hook does the job itself, DETACHED: a fresh `rag-rat` process is seconds of fixed
    // overhead, so it must not run inline. The child coalesces burst edits via the #660
    // single-flight and takes the write lock with a timeout (never blocking).
    spawn_detached_reindex(&input.cwd, &to_reindex);
    Ok(())
}

/// The edited absolute path(s) to reindex from a successful post-edit event. Relative paths are
/// resolved against the event cwd; all further root/scope validation remains owned by
/// `edit-reindex`. Codex does not register PostToolUse, so accepting an `apply_patch` here is safe
/// for harnesses that explicitly guarantee the post event fires after the write lands.
fn extract_edited_paths(input: &HookInput) -> Vec<PathBuf> {
    let paths = match input.tool_name.as_str() {
        "Write" | "Edit" | "MultiEdit" => {
            let mut paths = input
                .tool_input
                .get("file_path")
                .and_then(|value| value.as_str())
                .map(|path| vec![path.to_string()])
                .unwrap_or_default();
            if let Some(files) = input.tool_input.get("files").and_then(|value| value.as_array()) {
                paths.extend(files.iter().filter_map(|path| path.as_str().map(str::to_string)));
            }
            if let Some(edits) = input.tool_input.get("edits").and_then(|value| value.as_array()) {
                paths.extend(edits.iter().filter_map(|edit| {
                    string_field(edit, &["file_path", "filePath", "path"]).map(str::to_string)
                }));
            }
            paths
        },
        "apply_patch" => input
            .tool_input
            .get("command")
            .and_then(|value| value.as_str())
            .map(parse_v4a_paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    paths
        .into_iter()
        .map(|path| {
            let path = PathBuf::from(path);
            if path.is_absolute() { path } else { Path::new(&input.cwd).join(path) }
        })
        .collect()
}

/// Which of the edited paths this hook must reindex, given whether a watcher is live. No watcher ⇒
/// the hook covers everything. A live watcher covers the SOURCE edits (it gets the same inotify
/// event and schedules a debounced pass), so nothing needs the hook — EXCEPT manifests
/// (`Cargo.toml`): the watcher's event filter fires only for configured targets + `.gitignore`,
/// never a manifest, yet the scoped pass refreshes the package map for one, so the watcher would
/// silently miss it. Pure so the watcher-deferral decision is unit-tested without a live watcher.
fn paths_to_reindex(watcher_live: bool, paths: &[PathBuf]) -> Vec<PathBuf> {
    if !watcher_live {
        return paths.to_vec();
    }
    paths.iter().filter(|path| rag_rat_core::watch::is_manifest_path(path)).cloned().collect()
}

/// Spawn `rag-rat edit-reindex --cwd <cwd> --paths <paths…>` fully detached and return without
/// waiting, so the agent's synchronous tool call is never blocked. The child's stdio goes to null
/// so the harness's captured pipe gets EOF the moment this hook exits (otherwise the harness would
/// wait on the child's inherited stdout). Detaching so the child OUTLIVES the hook's process tree
/// is platform-specific: on unix `setsid` moves it into its own session; on Windows creation flags
/// detach it from the console and, where the launching Job Object permits, break it out of a
/// kill-on-close job. Best-effort — a spawn failure is swallowed (the next edit / periodic
/// reconcile catches up).
fn spawn_detached_reindex(cwd: &str, paths: &[PathBuf]) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command
        .arg("edit-reindex")
        .arg("--cwd")
        .arg(cwd)
        .arg("--paths")
        .args(paths)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: the pre_exec closure calls only `setsid`, which is async-signal-safe and touches
        // no inherited allocator/lock state — it just detaches the child into a new session.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        // Fire and forget: the setsid'd child is reparented to init when this hook exits.
        let _ = command.spawn();
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Nulled stdio + a dropped handle do NOT detach on Windows: the child stays attached to the
        // hook's console and, if the harness launched the hook inside a kill-on-close Job Object,
        // dies when that job closes. DETACHED_PROCESS + CREATE_NO_WINDOW drop the console;
        // CREATE_BREAKAWAY_FROM_JOB escapes the job — but CreateProcess REFUSES it when the job
        // forbids breakaway, so fall back to console-detached-but-in-job (most harness jobs are not
        // kill-on-close) rather than not spawning at all.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        if command.spawn().is_err() {
            command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
            let _ = command.spawn();
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command.spawn();
    }
}

/// The write-time clone-check size guard, factored out for testing. Skip the check ONLY when the
/// RAM fallback would run (`!indexed`) over more than [`MAX_CLONE_CHECK_FUNCTIONS`] functions. In
/// indexed mode the postings fast path is a bounded indexed lookup independent of corpus size, so
/// it never skips on size (#296 phase 4).
fn clone_check_skipped_for_size(indexed: bool, function_count: u64) -> bool {
    !indexed && function_count > MAX_CLONE_CHECK_FUNCTIONS
}

/// Write-time clone check (#287): fingerprint the just-written functions and warn if they duplicate
/// existing indexed code. Best-effort + READ-ONLY — every "not ready" path (no config, DB absent,
/// index owes a heal/migrate, index too large, no parseable functions) is a SILENT no-op, so it
/// never blocks or perceptibly delays a write.
fn clone_check(input: &HookInput) -> anyhow::Result<Option<String>> {
    let Some(config) = find_governing_config(Path::new(&input.cwd)) else { return Ok(None) };
    if !config.database.is_file() {
        return Ok(None); // index not built yet
    }
    // `try_open_config_read_only` returns None when the index still owes a heal/migrate (NOT
    // ready), so this is the no-op-when-not-ready guard — the same gate the MCP read tools use.
    let Some(mut db) = IndexDatabase::try_open_config_read_only(&config)? else { return Ok(None) };
    db.use_worktree_scope(&config.root, Some(Path::new(&input.cwd)))?;
    // The size guard bounds ONLY the RAM fallback. When a live postings generation is eligible the
    // check is a bounded indexed lookup (#296 phase 4), so run it regardless of corpus size; only
    // the fallback no-ops above the cap.
    let indexed = db.clone_check_indexed_generation().unwrap_or(None).is_some();
    if clone_check_skipped_for_size(indexed, db.clone_check_function_count().unwrap_or(u64::MAX)) {
        return Ok(None);
    }
    let inputs = extract_clone_inputs(input, &session_indexed_root(&config, &input.cwd));
    if inputs.is_empty() {
        return Ok(None);
    }
    let matches = db.clones_of_texts(&inputs, HOOK_NEAR_THRESHOLD)?;
    Ok(format_clone_warning(&matches))
}

/// Pull the (relative-path, text) inputs to clone-check from an edit tool call:
/// Write → the whole `content`; Edit → the `new_string`; MultiEdit → each edit's `new_string` (a
/// batch); `apply_patch` (Codex / Cursor) → the added (`+`) lines of each Add/Update-File section
/// of the V4A patch in `tool_input.command`. A fragment that isn't a complete function simply
/// yields no fingerprints downstream (a no-op).
fn extract_clone_inputs(input: &HookInput, root: &Path) -> Vec<CloneCheckInput> {
    if input.tool_name == "apply_patch" {
        return extract_apply_patch_inputs(&input.tool_input, root);
    }
    let ti = &input.tool_input;
    let default_path = ti.get("file_path").and_then(|value| value.as_str());
    match input.tool_name.as_str() {
        "Write" => default_path
            .zip(ti.get("content").and_then(|value| value.as_str()))
            .and_then(|(path, text)| clone_input(path, text, root))
            .into_iter()
            .collect(),
        "Edit" => default_path
            .zip(ti.get("new_string").and_then(|value| value.as_str()))
            .and_then(|(path, text)| clone_input(path, text, root))
            .into_iter()
            .collect(),
        "MultiEdit" => ti
            .get("edits")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|edit| {
                let path =
                    edit.get("file_path").and_then(|value| value.as_str()).or(default_path)?;
                let text = edit.get("new_string").and_then(|value| value.as_str())?;
                clone_input(path, text, root)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn clone_input(file_path: &str, text: &str, root: &Path) -> Option<CloneCheckInput> {
    let abs = Path::new(file_path);
    let language = Language::from_path(abs)?;
    // Indexed refs are root-relative, so relativize for parsing and self-file exclusion.
    let path = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
    Some(CloneCheckInput { text: text.to_string(), language, path })
}

/// Extract clone-check inputs from a Codex / Cursor `apply_patch` V4A envelope
/// (`tool_input.command`). Each Add/Update-File section contributes its added (`+`) lines as one
/// text; the file path picks the language. Non-code paths (no recognized language) are dropped.
fn extract_apply_patch_inputs(tool_input: &serde_json::Value, root: &Path) -> Vec<CloneCheckInput> {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    parse_v4a_added_lines(command)
        .into_iter()
        .filter_map(|(path, text)| {
            let abs = Path::new(&path);
            let language = Language::from_path(abs)?;
            // The indexed refs are root-relative, so relativize for the parse + self-file
            // exclusion.
            let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
            Some(CloneCheckInput { text, language, path: rel })
        })
        .collect()
}

/// Parse a V4A `apply_patch` envelope into `(file path, added-line text)` for each Add/Update-File
/// section. Only `+`-prefixed lines contribute; `*** Delete File:` and the `*** Begin|End Patch`
/// markers close the current section, and everything else inside a section (context, removed, `@@`
/// hunk anchors) is skipped. `*** Move to:` is ignored — the Update-File path picks the language.
fn parse_v4a_added_lines(patch: &str) -> Vec<(String, String)> {
    fn flush(path: &mut Option<String>, added: &mut String, out: &mut Vec<(String, String)>) {
        if let Some(p) = path.take()
            && !added.is_empty()
        {
            out.push((p, std::mem::take(added)));
        }
        added.clear();
    }

    let mut out: Vec<(String, String)> = Vec::new();
    let mut path: Option<String> = None;
    let mut added = String::new();
    for line in patch.lines() {
        if let Some(p) =
            line.strip_prefix("*** Add File: ").or_else(|| line.strip_prefix("*** Update File: "))
        {
            flush(&mut path, &mut added, &mut out);
            path = Some(p.trim().to_string());
        } else if line.starts_with("*** Delete File: ")
            || line.starts_with("*** Begin Patch")
            || line.starts_with("*** End Patch")
        {
            flush(&mut path, &mut added, &mut out);
        } else if path.is_some()
            && let Some(rest) = line.strip_prefix('+')
        {
            added.push_str(rest);
            added.push('\n');
        }
    }
    flush(&mut path, &mut added, &mut out);
    out
}

/// Extract every path affected by a V4A patch, including deletion-only sections that intentionally
/// have no added text and therefore cannot appear in [`parse_v4a_added_lines`].
fn parse_v4a_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "]
                .into_iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_string)
        })
        .collect()
}

/// Render clone-check findings as the `additionalContext` injected back to the agent, or `None`
/// when there are none (stay silent).
fn format_clone_warning(matches: &[TextCloneMatch]) -> Option<String> {
    if matches.is_empty() {
        return None;
    }
    let mut out = String::from(
        "▶ rag-rat clone check — code you're writing duplicates existing functions:\n",
    );
    for m in matches {
        let label = if m.kind == "exact" {
            "identical to".to_string()
        } else {
            format!("~{:.0}% similar to", m.similarity * 100.0)
        };
        let shown = m.clone_of.iter().take(MAX_CLONE_REFS).cloned().collect::<Vec<_>>().join(", ");
        let extra = m.clone_of.len().saturating_sub(MAX_CLONE_REFS);
        let more = if extra > 0 { format!(" (+{extra} more)") } else { String::new() };
        out.push_str(&format!(
            "  • `{}` (line {}) is {} {shown}{more}\n",
            m.name, m.start_line, label,
        ));
    }
    out.push_str(
        "Prefer reusing the existing function(s) over duplicating — impact_surface / \
         symbol_lookup to inspect them.\n",
    );
    Some(out)
}

/// Probe whether the per-worktree watcher election lock is currently held (i.e. a watcher is live).
///
/// Algorithm: try to acquire the election lock non-blocking.
/// - `Ok(None)` → lock is held by another process → watcher is live.
/// - `Ok(Some(_))` → we acquired it (no holder); release immediately → not live.
/// - `Err(_)` → treat as not live (conservative).
pub fn watcher_state(config: &Config) -> (bool /* live */, bool /* enabled */) {
    let enabled = config.watch.enabled && std::env::var_os("RAG_RAT_NO_WATCH").is_none();
    let base_dir =
        config.database.parent().map(Path::to_path_buf).unwrap_or_else(|| config.root.clone());
    let election_path = locks::election_lock_path(&base_dir, &config.root);
    // try_acquire: Ok(None) means the lock is held (watcher is live).
    let live = matches!(locks::FileLock::try_acquire(&election_path), Ok(None));
    (live, enabled)
}

// ─── Attribution header ───────────────────────────────────────────────────────

const ATTRIBUTION_HEADER: &str = "\
▶ rag-rat repo intelligence — injected by the rag-rat MCP server (prefer it over grep/cat)
  concept → semantic_search · callers/callees → find_callers/trace_callees
  before editing a symbol → impact_surface · exact symbol → symbol_lookup
  why/rationale → repo memories ride along; memory_search to dig
";

// ─── Digest formatting ────────────────────────────────────────────────────────

/// Strip a leading `crates/<crate>/src/` prefix from a repo-relative path.
///
/// Converts e.g. `crates/rag-rat-core/src/index/mod.rs` → `index/mod.rs`.
/// Paths that do not match the three-segment `crates/<anything>/src/` prefix are returned
/// unchanged.
pub fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.splitn(4, '/').collect();
    if parts.len() == 4 && parts[0] == "crates" && parts[2] == "src" {
        parts[3].to_string()
    } else {
        p.to_string()
    }
}

/// Render the full orientation digest as a plain-text string.
///
/// `live` = watcher election lock is currently held; `enabled` = watch is configured on.
pub fn format_digest(o: &Orientation, live: bool, enabled: bool) -> String {
    let mut out = String::with_capacity(2048);

    // Attribution + capability nudge.
    out.push_str(ATTRIBUTION_HEADER);
    out.push('\n');

    // Purpose line (root dir memory title) — omit entirely if absent.
    if let Some(ref title) = o.tree.root_memory_title {
        out.push_str(title);
        out.push('\n');
    }

    // LAYOUT — directory tree.
    out.push_str(&format!("LAYOUT  ({} files · ‹…› = directory memory)\n", o.total_files));
    for node in &o.tree.nodes {
        let indent = "  ".repeat(node.depth as usize);
        if let Some(ref title) = node.memory_title {
            out.push_str(&format!("{}{}  ‹{}›\n", indent, node.label, title));
        } else {
            out.push_str(&format!("{}{}\n", indent, node.label));
        }
    }
    if o.tree.truncated > 0 {
        out.push_str(&format!("  … (+{} more)\n", o.tree.truncated));
    }

    // Load-bearing files (paths shortened: crates/<crate>/src/X → X).
    if !o.load_bearing.is_empty() {
        let parts: Vec<String> = o
            .load_bearing
            .iter()
            .map(|(p, fi)| format!("{} (fan_in {})", short_path(p), fi))
            .collect();
        out.push_str(&format!("load-bearing: {}\n", parts.join(" · ")));
    }

    // Recent activity.
    {
        let mut line_parts: Vec<String> = Vec::new();
        if !o.recent_commits.is_empty() {
            line_parts.push(format!("recent: {}", o.recent_commits.join(" · ")));
        }
        if !o.hot_files.is_empty() {
            let short_hot: Vec<String> = o.hot_files.iter().map(|p| short_path(p)).collect();
            line_parts.push(format!("hot: {}", short_hot.join(", ")));
        }
        if !line_parts.is_empty() {
            out.push_str(&format!("{}\n", line_parts.join(" · ")));
        }
    }

    // Active non-dir memory titles — the list is already truncated to the display cap by
    // the query; the overflow note reflects the TRUE total, not the truncated list length.
    if !o.active_memory_titles.is_empty() {
        let mut mem_line = o.active_memory_titles.join(" · ");
        let extra = (o.active_memory_total as usize).saturating_sub(o.active_memory_titles.len());
        if extra > 0 {
            mem_line.push_str(&format!(" (+{extra} more)"));
        }
        out.push_str(&format!("memories: {mem_line}\n"));
    }

    // Watcher-aware health line.
    let fresh = o.head == o.indexed_head || o.head.is_empty() || o.indexed_head.is_empty();
    let health_status = match (live, enabled, fresh) {
        (true, _, true) => "index fresh (watcher live)".to_string(),
        (true, _, false) => "index syncing (watcher live)".to_string(),
        (false, true, false) => "index stale — start the rag-rat MCP server".to_string(),
        (false, false, false) => "watcher off; index stale — run 'rag-rat index'".to_string(),
        _ => "index fresh".to_string(),
    };
    let active = o.anchor.current + o.anchor.relocated;
    let mut health = format!("health: {} · memories {} active", health_status, active);
    if o.anchor.stale > 0 {
        health.push_str(&format!("/{} stale", o.anchor.stale));
    }
    if o.anchor.gone > 0 {
        health.push_str(&format!(" · {} gone → run 'rag-rat memory doctor'", o.anchor.gone));
    }
    if o.parser_failures > 0 {
        health.push_str(&format!(" · parser failures: {}", o.parser_failures));
    }
    out.push_str(&health);
    out.push('\n');

    out
}

/// Resolve the GOVERNING config for the hook's cwd and load it, falling back to the MAIN
/// worktree's config for a linked worktree with no branch-local `rag-rat.toml` (the documented
/// main-governed setup) — the same governing discovery the CLI's own entry uses. Bare
/// `nearest_config_at_or_above` stops at the linked worktree root and returns `None` there, which
/// would silently disable session, augmentation, clone-check, and reindex hooks. `Config::load`'s
/// seam then re-anchors root to main; each operation separately selects the active overlay.
/// `discover_config_path` returns a path even when none exists; `Config::load` then fails → `None`,
/// preserving the no-config no-op.
fn find_governing_config(start: &Path) -> Option<Config> {
    Config::load(rag_rat_base::config::discover_config_path(start)).ok()
}

/// Outer Option: did the listener answer at all (None ⇒ fall back). Inner Option: did it
/// have anything new to say.
fn ask_listener(
    config: &Config,
    session_id: &str,
    cwd: &str,
    search: &Search,
) -> Option<Option<String>> {
    #[cfg(unix)]
    {
        // `cwd` lets the listener scope the augmentation to the session's worktree overlay (#219);
        // an older listener without the field just ignores it (lenient deserialize) → base scope.
        let request = serde_json::json!({
            "v": 1, "kind": "grep_augment", "session_id": session_id,
            "cwd": cwd,
            "pattern": search.pattern, "search_path": search.search_path,
            "source": search.source,
        });
        // grep_augment predates the `kind` echo, so any v1 reply is authoritative — trust it and
        // don't consult the handled-kind marker (older listeners never send it).
        send_to_listener(config, &request).map(|reply| reply.context)
    }
    #[cfg(not(unix))]
    {
        let _ = (config, session_id, cwd, search);
        None
    }
}

/// Read-augmentation counterpart of [`ask_listener`] (#756): ask the warm listener for the digest
/// of a file being opened, so the per-session dedup applies. `path` is repo-root-relative.
fn ask_listener_read(
    config: &Config,
    session_id: &str,
    cwd: &str,
    path: &str,
) -> Option<Option<String>> {
    #[cfg(unix)]
    {
        let request = serde_json::json!({
            "v": 1, "kind": "read_augment", "session_id": session_id,
            "cwd": cwd, "path": path,
        });
        // Treat the reply as authoritative ONLY when the listener CONFIRMS it handled read_augment
        // (echoes the kind). An older listener that predates this feature returns a v1 null-context
        // reply with no handled kind — return outer `None` so the caller runs the direct fallback
        // instead of silently disabling read augmentation until the server restarts (#756 review).
        match send_to_listener(config, &request) {
            Some(reply) if reply.handled_kind.as_deref() == Some("read_augment") =>
                Some(reply.context),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (config, session_id, cwd, path);
        None
    }
}

/// A parsed listener reply: the injectable `context` (None ⇒ nothing new) and the request `kind`
/// the listener reported handling (None ⇒ an older listener that didn't understand the request).
#[cfg(unix)]
struct ListenerReply {
    context: Option<String>,
    handled_kind: Option<String>,
}

/// One request/response round-trip to the warm listener socket. `None` ⇒ the listener didn't answer
/// (no socket, timeout, protocol skew) so the caller falls back; `Some(reply)` carries the context
/// plus the handled-kind marker.
#[cfg(unix)]
fn send_to_listener(config: &Config, request: &serde_json::Value) -> Option<ListenerReply> {
    use std::io::{BufRead, BufReader, Write as _};
    use std::os::unix::net::UnixStream;
    let socket = socket_path(config);
    // SOCKET_BUDGET covers both read and write. Unix-domain connect() completes into the listener's
    // backlog immediately (no network round-trip), so no separate connect timeout is needed.
    let stream = UnixStream::connect(&socket).ok()?;
    stream.set_read_timeout(Some(SOCKET_BUDGET)).ok()?;
    stream.set_write_timeout(Some(SOCKET_BUDGET)).ok()?;
    let mut writer = stream.try_clone().ok()?;
    writeln!(writer, "{request}").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
    if reply.get("v")?.as_u64()? != 1 {
        return None;
    }
    Some(ListenerReply {
        context: reply.get("context").and_then(|c| c.as_str()).map(str::to_string),
        handled_kind: reply.get("kind").and_then(|k| k.as_str()).map(str::to_string),
    })
}

/// Single source of truth via `locks::hook_socket_path_for`; same computation as the MCP
/// listener's `socket_path_for`, guaranteed not to diverge.
// Only the unix listener path calls this; dead on Windows.
#[cfg(unix)]
fn socket_path(config: &Config) -> PathBuf {
    locks::hook_socket_path_for(config)
}

/// Stateless direct read (no dedupe — spec: fallback path). Any error ⇒ silence.
fn fallback_compose(config: &Config, cwd: &str, search: &Search) -> Option<String> {
    let conn = scoped_read_conn(config, cwd)?;
    grep_augment::compose(
        conn.connection(),
        &search.pattern,
        search.search_path.as_deref(),
        &grep_augment::DedupeFilter::default(),
        config.memory.surface,
    )
    .ok()
    .flatten()
    .map(|out| out.context)
}

/// Read-augmentation fallback (#756): the stateless direct compose for a file being opened, used
/// when the warm listener didn't answer. No session dedup on this path (spec: fallback is
/// stateless). `rel_path` is repo-root-relative. Any error ⇒ silence.
fn fallback_compose_read(config: &Config, cwd: &str, rel_path: &str) -> Option<String> {
    let conn = scoped_read_conn(config, cwd)?;
    read_augment::compose(
        conn.connection(),
        rel_path,
        &grep_augment::DedupeFilter::default(),
        config.memory.surface,
    )
    .ok()
    .flatten()
    .map(|out| out.context)
}

/// Open a read-only connection scoped to the session's worktree overlay, ready for a compose call.
/// Shared by the grep- and read-augment fallbacks. `compose` queries the `files` view, so without
/// the scope install it would read raw (unscoped) rows. `config.root` is the anchored main
/// worktree; `cwd` is the session dir (a linked worktree → its overlay, else base) (#219). The repo
/// dimension is resolved from this config (identity + override) so the scope binds the config's
/// repo, not the config-blind sole repo (a sibling in a consolidated DB); an unprovable repo →
/// empty scope, never a sibling's rows. `None` on any not-ready condition, so the caller stays
/// silent.
fn scoped_read_conn(config: &Config, cwd: &str) -> Option<IndexConnection> {
    let conn = IndexConnection::open_read_only(&config.database).ok()?;
    let repo_id = rag_rat_core::index::resolve_scope_repo_id(
        conn.connection(),
        &config.root,
        config.repo_id_override.as_deref(),
    )
    .ok()?
    .unwrap_or_default();
    rag_rat_core::index::install_worktree_scope_view(
        conn.connection(),
        &repo_id,
        &config.root,
        Path::new(cwd),
    )
    .ok()?;
    Some(conn)
}

#[cfg(test)]
mod tests;
