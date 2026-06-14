//! `rag-rat claude-hook`: the Claude Code hook client (PreToolUse + SessionStart).
//!
//! Reads the hook JSON from stdin and branches on `hook_event_name`:
//! - `"SessionStart"`: injects a read-only repo orientation digest into the model context.
//! - anything else (or absent): the PreToolUse grep-augmentation path — asks the elected listener
//!   or falls back to a direct read-only query, prints `additionalContext` JSON.
//!
//! Exit 0 on every path — the hook must never block a tool call or session start.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rag_rat_core::config::Config;
use rag_rat_core::locks;
use rag_rat_core::query::grep_augment;
use rag_rat_core::query::orientation::Orientation;
use rag_rat_core::storage::IndexConnection;
use serde::Deserialize;

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

/// Entry point for `rag-rat claude-hook`. Every failure path prints nothing and returns
/// Ok(()) — the hook must never block a grep (spec: error posture).
pub fn run() -> anyhow::Result<()> {
    let _ = run_inner(); // swallow: silence is the contract
    Ok(())
}

fn run_inner() -> anyhow::Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    // Tolerant parse: missing fields use Default so both PreToolUse and SessionStart succeed.
    let input: HookInput = serde_json::from_str(&raw).unwrap_or_default();
    match input.hook_event_name.as_deref() {
        Some("SessionStart") => session_start(&input),
        _ => pretooluse(&input),
    }
}

/// SessionStart path: inject a read-only repo orientation digest as plain stdout.
/// Every error path prints nothing and returns Ok — never block session start.
fn session_start(input: &HookInput) -> anyhow::Result<()> {
    // Allowlist: only fire for meaningful session triggers, not resume.
    match input.source.as_deref() {
        Some("startup") | Some("clear") | Some("compact") => {},
        _ => return Ok(()),
    }
    let Some(config) = find_config(Path::new(&input.cwd)) else { return Ok(()) };
    // If the DB file does not exist, print a minimal nudge and exit — do NOT open/create it.
    if !config.database.is_file() {
        print!("{}", db_absent_notice());
        return Ok(());
    }
    // Open read-only; compose the orientation; print the digest.
    // Any error (locked, corrupt, etc.) propagates via `?` to run_inner, which run() swallows
    // (`let _ = run_inner()`) — so the hook stays silent and never blocks session start. Do NOT
    // add error prints here: this branch's stdout is injected as model context.
    let conn = IndexConnection::open_read_only(&config.database)?;
    let root = Path::new(&input.cwd);
    let o = rag_rat_core::query::orientation::orientation(conn.connection(), root)?;
    let (live, enabled) = watcher_state(&config);
    print!("{}", format_digest(&o, live, enabled));
    if let Some(line) = version_check_line(&config) {
        print!("{line}");
    }
    Ok(())
}

/// One digest line stating the running version vs the latest published on crates.io, with the
/// update command when behind. Reads the cached check only (no network — the MCP server refreshes
/// it out of band); `None` when version checking is disabled or no check has been cached yet, so a
/// fresh repo or an opted-out user sees nothing.
fn version_check_line(config: &Config) -> Option<String> {
    use rag_rat_core::version_check;
    let status = version_check::cached_status(config.version_check.enabled, &config.database)?;
    let latest = status.latest_version?;
    if status.update_available {
        Some(format!(
            "\n⚠ rag-rat update available: {} → {} — run `{}`\n",
            status.current_version, latest, status.update_command
        ))
    } else {
        Some(format!("\nrag-rat {} (latest on crates.io)\n", status.current_version))
    }
}

/// PreToolUse path (grep augmentation).
fn pretooluse(input: &HookInput) -> anyhow::Result<()> {
    let Some(search) = extract_search(input) else { return Ok(()) };
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

/// One-liner shown when the DB file is absent (no config directory walk — `find_config` already
/// succeeded, so we know we're in a rag-rat repo; the DB just hasn't been built yet).
fn db_absent_notice() -> String {
    format!("{}\nindex not built — run 'rag-rat index'\n", ATTRIBUTION_HEADER.trim_end())
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
        use std::io::{BufRead, BufReader, Write as _};
        use std::os::unix::net::UnixStream;
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
    use rag_rat_core::index::AnchorHealth;
    use rag_rat_core::query::orientation::Orientation;
    use rag_rat_core::query::tree::{DirTree, TreeNode};

    use super::*;

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
        assert!(
            s.contains("main.rs (fan_in 42)"),
            "crates path not shortened for main.rs; got:\n{s}"
        );
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
            anchor,
            0,
        );
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
}
