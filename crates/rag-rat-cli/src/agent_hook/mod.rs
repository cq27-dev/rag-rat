//! `rag-rat agent-hook`: the coding-agent hook client (PreToolUse + PostToolUse + SessionStart).
//! Harness-neutral — Claude Code, Codex, and Cursor all use the same `hook_event_name` /
//! `tool_name` / `tool_input` input and the same `hookSpecificOutput.additionalContext` output, so
//! one entrypoint serves all.
//!
//! Reads the hook JSON from stdin and branches on `hook_event_name`:
//! - `"SessionStart"`: injects a read-only repo orientation digest into the model context.
//! - `"PostToolUse"`: after an edit tool completes, triggers a scoped reindex of the edited path(s)
//!   in a DETACHED child (`edit_reindex`), watcher-aware — see [`posttooluse`]. Scoped to Claude
//!   Code, whose PostToolUse carries the edited `file_path` (Codex's `apply_patch` PostToolUse
//!   fires before the write lands, and Cursor's payload is unpinned, so neither is wired — #661).
//! - PreToolUse (anything else, or absent): grep-augmentation on `Grep`/`Bash` (asks the elected
//!   listener or falls back to a direct read-only query), and the write-time clone check on the
//!   edit tools — `Write`/`Edit`/`MultiEdit` (Claude) and `apply_patch` (Codex/Cursor, whose V4A
//!   diff is parsed for added lines). Prints `additionalContext` JSON.
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
use rag_rat_core::query::grep_augment;
use rag_rat_core::query::orientation::Orientation;
use rag_rat_db::storage::IndexConnection;
use serde::Deserialize;

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

/// Entry point for `rag-rat agent-hook`. Every failure path prints nothing and returns
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
        Some("PostToolUse") => posttooluse(&input),
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
    // Version skew (#484): a schema created by a newer rag-rat means this binary — and the MCP
    // server this session just started, which is at most as new — will refuse every open. Say so
    // actionably instead of composing a digest that half-works today and errors tomorrow. The
    // schema message already carries the remedy (and the non-Linux no-hot-upgrade caveat).
    let schema = rag_rat_db::schema::status(conn.connection())?;
    if schema.state == rag_rat_db::schema::SchemaState::Newer {
        print!("{}", newer_schema_notice(&schema.message));
        return Ok(());
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
    print!("{}", format_digest(&o, live, enabled));
    if let Some(line) = version_check_line(&config) {
        print!("{line}");
    }
    Ok(())
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

/// PreToolUse path: the write-time clone check (#287) on the edit tools, grep augmentation
/// otherwise.
fn pretooluse(input: &HookInput) -> anyhow::Result<()> {
    if matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit" | "apply_patch") {
        return clone_check(input);
    }
    let Some(search) = extract_search(input) else { return Ok(()) };
    let Some(config) = find_config(Path::new(&input.cwd)) else { return Ok(()) };

    // Pass the session cwd through both paths so the grep-augmentation is scoped to the worktree
    // the session is in (a linked worktree gets its branch overlay), not just the base index
    // (#219).
    let context = ask_listener(&config, &input.session_id, &input.cwd, &search)
        .unwrap_or_else(|| fallback_compose(&config, &input.cwd, &search));
    if let Some(context) = context {
        // PreToolUse contract: additionalContext only — no `permissionDecision` (Codex rejects the
        // "allow" value, and we only inject context, never gate the tool). Plain stdout is
        // debug-only.
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": context,
                }
            })
        );
    }
    Ok(())
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
    let Some(config) = find_config(Path::new(&input.cwd)) else { return Ok(()) };
    // The scoped `index --paths` mode requires an existing base index (#659/#427) — with no DB
    // there is nothing to reconcile into yet; the first `rag-rat index` builds it. (Do NOT
    // open/create it.)
    if !config.database.is_file() {
        return Ok(());
    }
    // Watcher-live ⇒ no-op: a live MCP watcher for this worktree receives the SAME inotify event
    // and schedules a debounced pass, so reindexing here would just double the work. Mirrors
    // the maintenance watcher-state deferral — and is what lets this trigger skip socket
    // delegation entirely (the only listener that would matter is already handling the edit).
    if watcher_state(&config).0 {
        return Ok(());
    }
    // No listener ⇒ the hook does the job itself, but DETACHED: a fresh `rag-rat` process is
    // seconds of fixed overhead, so it must not run inline. The child coalesces burst edits via
    // the #660 single-flight and takes the write lock with a timeout (never blocking).
    spawn_detached_reindex(&input.cwd, &paths);
    Ok(())
}

/// The edited absolute path(s) to reindex, from a PostToolUse edit-tool payload. `Write` / `Edit` /
/// `MultiEdit` (Claude Code) all carry a single `tool_input.file_path` (MultiEdit batches edits to
/// ONE file). Any other tool — including `apply_patch`, whose Codex PostToolUse fires before the
/// write lands — yields nothing, so the trigger stays scoped to the harness that delivers a usable
/// post-edit event (#661).
fn extract_edited_paths(input: &HookInput) -> Vec<PathBuf> {
    match input.tool_name.as_str() {
        "Write" | "Edit" | "MultiEdit" => input
            .tool_input
            .get("file_path")
            .and_then(|value| value.as_str())
            .map(|path| vec![PathBuf::from(path)])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
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
fn clone_check(input: &HookInput) -> anyhow::Result<()> {
    let Some(config) = find_config(Path::new(&input.cwd)) else { return Ok(()) };
    if !config.database.is_file() {
        return Ok(()); // index not built yet
    }
    // `try_open_config_read_only` returns None when the index still owes a heal/migrate (NOT
    // ready), so this is the no-op-when-not-ready guard — the same gate the MCP read tools use.
    let Some(db) = IndexDatabase::try_open_config_read_only(&config)? else { return Ok(()) };
    // The size guard bounds ONLY the RAM fallback. When a live postings generation is eligible the
    // check is a bounded indexed lookup (#296 phase 4), so run it regardless of corpus size; only
    // the fallback no-ops above the cap.
    let indexed = db.clone_check_indexed_generation().unwrap_or(None).is_some();
    if clone_check_skipped_for_size(indexed, db.clone_check_function_count().unwrap_or(u64::MAX)) {
        return Ok(());
    }
    let inputs = extract_clone_inputs(input, &config.root);
    if inputs.is_empty() {
        return Ok(());
    }
    let matches = db.clones_of_texts(&inputs, HOOK_NEAR_THRESHOLD)?;
    if let Some(context) = format_clone_warning(&matches) {
        // PreToolUse contract: additionalContext only — a warning, not a block (no
        // permissionDecision).
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": context,
                }
            })
        );
    }
    Ok(())
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
    let Some(file_path) = ti.get("file_path").and_then(|v| v.as_str()) else { return Vec::new() };
    let abs = Path::new(file_path);
    let Some(language) = Language::from_path(abs) else { return Vec::new() };
    // The indexed refs are root-relative, so relativize for the parse + the self-file exclusion.
    let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
    let texts: Vec<String> = match input.tool_name.as_str() {
        "Write" => ti
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        "Edit" => ti
            .get("new_string")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        "MultiEdit" => ti
            .get("edits")
            .and_then(|v| v.as_array())
            .map(|edits| {
                edits
                    .iter()
                    .filter_map(|e| {
                        e.get("new_string").and_then(|v| v.as_str()).map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    texts.into_iter().map(|text| CloneCheckInput { text, language, path: rel.clone() }).collect()
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

/// Walk up from the hook's cwd to the nearest rag-rat.toml and load it. `None` ⇒ not a rag-rat repo
/// ⇒ silent no-op (what makes `--global` install safe). Shares the upward-walk primitive with
/// `Config::load`'s discovery seam ([`rag_rat_base::config::nearest_config_at_or_above`]).
fn find_config(start: &Path) -> Option<Config> {
    rag_rat_base::config::nearest_config_at_or_above(start)
        .and_then(|path| Config::load(&path).ok())
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
        use std::io::{BufRead, BufReader, Write as _};
        use std::os::unix::net::UnixStream;
        let socket = socket_path(config);
        // SOCKET_BUDGET covers both read and write. Unix-domain connect() completes into
        // the listener's backlog immediately (no network round-trip), so no separate connect
        // timeout is needed.
        let stream = UnixStream::connect(&socket).ok()?;
        stream.set_read_timeout(Some(SOCKET_BUDGET)).ok()?;
        stream.set_write_timeout(Some(SOCKET_BUDGET)).ok()?;
        // `cwd` lets the listener scope the augmentation to the session's worktree overlay (#219);
        // an older listener without the field just ignores it (lenient deserialize) → base scope.
        let request = serde_json::json!({
            "v": 1, "kind": "grep_augment", "session_id": session_id,
            "cwd": cwd,
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
        let _ = (config, session_id, cwd, search);
        None
    }
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
    let conn = IndexConnection::open_read_only(&config.database).ok()?;
    // Scope to the session's worktree overlay before composing — `compose` queries the `files`
    // view, so without this it would read raw (unscoped) rows. config.root is the anchored main
    // worktree; cwd is the session dir (a linked worktree → its overlay, else base) (#219). Resolve
    // the repo dimension from this config (identity + override) so the scope binds the config's
    // repo, not the config-blind sole repo (a sibling in a consolidated DB); an unprovable repo →
    // empty scope, never a sibling's rows.
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

#[cfg(test)]
mod tests;
