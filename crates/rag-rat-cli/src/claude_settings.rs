//! Claude Code settings.json management for the grep-augment PreToolUse hook and the
//! SessionStart orientation hook (`rag-rat hooks install|uninstall|status --claude [--global]`).
//!
//! Edits are additive and marker-aware: our entries are recognized by `HOOK_COMMAND`;
//! everything else in the file is preserved byte-for-byte at the JSON level (read → modify
//! → pretty-print 2-space, matching how Claude Code writes the file).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const HOOK_COMMAND: &str = "rag-rat claude-hook";
const MATCHERS: &[&str] = &["Grep", "Bash"];
const SESSION_START_MATCHER: &str = "startup|clear|compact";
const SESSION_START_TIMEOUT: u64 = 5;

/// Named status returned by [`hook_status`].
pub struct HookStatus {
    pub pretooluse: bool,
    pub session_start: bool,
}

fn our_pretooluse_entry(matcher: &str) -> Value {
    json!({
        "matcher": matcher,
        "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": 10}]
    })
}

fn our_session_start_entry() -> Value {
    json!({
        "matcher": SESSION_START_MATCHER,
        "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": SESSION_START_TIMEOUT}]
    })
}

/// True if this hook-array entry contains our command in any of its hooks.
fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(|h| h["command"] == HOOK_COMMAND))
}

// ---------------------------------------------------------------------------
// Event-array helpers (parameterised by event name)
// ---------------------------------------------------------------------------

/// Return a mutable reference to the array at `settings.hooks.<event_name>`.
///
/// When `create` is true, missing `hooks`/`<event_name>` containers are inserted (install
/// path). Without it, a missing or malformed path yields `None` (uninstall/read path).
/// Returns `None` rather than panicking whenever `hooks` or the event array exists but is
/// the wrong JSON type — a user's hand-edited settings.json must never crash `rag-rat hooks`.
fn event_array_mut<'a>(
    settings: &'a mut Value,
    event_name: &str,
    create: bool,
) -> Option<&'a mut Vec<Value>> {
    if create {
        if !settings.is_object() {
            *settings = json!({});
        }
        let hooks = settings
            .as_object_mut()
            .unwrap() // safe: just ensured object above
            .entry("hooks")
            .or_insert_with(|| json!({}));
        if hooks.is_object() {
            hooks.as_object_mut().unwrap().entry(event_name).or_insert_with(|| json!([]));
        }
    }
    settings.get_mut("hooks").and_then(|h| h.get_mut(event_name)).and_then(Value::as_array_mut)
}

/// Prune `hooks.<event_name>` and the `hooks` container itself when they become empty.
fn prune_empty_event(settings: &mut Value, event_name: &str) {
    // Check if the event array is empty; if so, remove it.
    let event_empty = settings["hooks"][event_name].as_array().is_some_and(|a| a.is_empty());
    if event_empty && let Some(hooks_obj) = settings.get_mut("hooks").and_then(Value::as_object_mut)
    {
        hooks_obj.remove(event_name);
    }
    // If `hooks` itself is now an empty object, remove it too.
    if settings["hooks"].as_object().is_some_and(serde_json::Map::is_empty)
        && let Some(root) = settings.as_object_mut()
    {
        root.remove("hooks");
    }
}

// ---------------------------------------------------------------------------
// PreToolUse (per-matcher semantics — Grep + Bash each get their own entry)
// ---------------------------------------------------------------------------

/// The `hooks.PreToolUse` array. Kept for backwards-compat surface; delegates to
/// [`event_array_mut`].
fn pretooluse_array_mut(settings: &mut Value, create: bool) -> Option<&mut Vec<Value>> {
    event_array_mut(settings, "PreToolUse", create)
}

/// Add missing Grep/Bash entries. Returns true when the document changed.
///
/// Gracefully handles malformed pre-existing settings: if `hooks` is not an object or
/// `hooks.PreToolUse` is not an array, leaves the document untouched and returns false.
pub fn merge_hook_entries(settings: &mut Value) -> bool {
    let Some(entries) = pretooluse_array_mut(settings, true) else { return false };
    let mut changed = false;
    for matcher in MATCHERS {
        let present = entries.iter().any(|e| e["matcher"] == *matcher && is_ours(e));
        if !present {
            entries.push(our_pretooluse_entry(matcher));
            changed = true;
        }
    }
    // SessionStart: single is-ours entry; replace if matcher/timeout drifted.
    changed | merge_session_start(settings)
}

/// Ensure exactly one is-ours SessionStart entry with the canonical matcher+timeout.
/// Replaces a drifted entry; no-ops when already correct. Returns true if changed.
fn merge_session_start(settings: &mut Value) -> bool {
    let Some(entries) = event_array_mut(settings, "SessionStart", true) else { return false };

    // Find any existing is-ours entry.
    let ours_pos = entries.iter().position(is_ours);

    match ours_pos {
        Some(pos) => {
            let entry = &entries[pos];
            // Check whether matcher and timeout already match.
            let matcher_ok = entry["matcher"] == SESSION_START_MATCHER;
            let timeout_ok = entry["hooks"][0]["timeout"] == SESSION_START_TIMEOUT;
            if matcher_ok && timeout_ok {
                false // already correct, no-op
            } else {
                entries[pos] = our_session_start_entry();
                true
            }
        },
        None => {
            entries.push(our_session_start_entry());
            true
        },
    }
}

/// Remove our entries from both PreToolUse and SessionStart; prune empty containers.
pub fn remove_hook_entries(settings: &mut Value) -> bool {
    let mut changed = false;

    // --- PreToolUse ---
    if let Some(entries) = pretooluse_array_mut(settings, false) {
        let before = entries.len();
        entries.retain(|e| !is_ours(e));
        if entries.len() != before {
            changed = true;
        }
    }
    prune_empty_event(settings, "PreToolUse");

    // --- SessionStart ---
    if let Some(entries) = event_array_mut(settings, "SessionStart", false) {
        let before = entries.len();
        entries.retain(|e| !is_ours(e));
        if entries.len() != before {
            changed = true;
        }
    }
    prune_empty_event(settings, "SessionStart");

    changed
}

/// Named status for `hooks status --claude`.
pub fn hook_status(settings: &Value) -> HookStatus {
    let installed_in = |event: &str, matcher: &str| {
        settings["hooks"][event]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|e| e["matcher"] == matcher && is_ours(e)))
    };
    let session_start_installed = settings["hooks"]["SessionStart"]
        .as_array()
        .is_some_and(|entries| entries.iter().any(is_ours));
    HookStatus {
        pretooluse: installed_in("PreToolUse", "Grep") && installed_in("PreToolUse", "Bash"),
        session_start: session_start_installed,
    }
}

/// Project `.claude/settings.json` or, with `--global`, `~/.claude/settings.json`.
pub fn settings_path(repo_root: &Path, global: bool) -> anyhow::Result<PathBuf> {
    if global {
        // Resolve the user home portably: $HOME on unix, %USERPROFILE% on stock Windows
        // (where HOME is usually unset).
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| anyhow::anyhow!("neither HOME nor USERPROFILE is set"))?;
        Ok(PathBuf::from(home).join(".claude/settings.json"))
    } else {
        Ok(repo_root.join(".claude/settings.json"))
    }
}

pub fn read_settings(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn write_settings(path: &Path, settings: &Value) -> anyhow::Result<()> {
    // settings.json is user-authored and not regenerable — write it atomically so an
    // interrupted write can't truncate it (#52). write_atomic creates the parent dir.
    let body = format!("{}\n", serde_json::to_string_pretty(settings)?);
    crate::write_atomic(path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PreToolUse (existing semantics preserved)
    // -----------------------------------------------------------------------

    #[test]
    fn install_into_empty_settings_creates_both_matchers() {
        let mut settings = serde_json::json!({});
        let changed = merge_hook_entries(&mut settings);
        assert!(changed);
        let entries = settings["hooks"]["PreToolUse"].as_array().unwrap();
        let matchers: Vec<&str> = entries.iter().map(|e| e["matcher"].as_str().unwrap()).collect();
        assert!(matchers.contains(&"Grep") && matchers.contains(&"Bash"));
        for entry in entries {
            let hook = &entry["hooks"][0];
            assert_eq!(hook["command"], HOOK_COMMAND);
            assert_eq!(hook["timeout"], 10);
        }
    }

    #[test]
    fn install_is_idempotent_and_preserves_foreign_entries() {
        let mut settings = serde_json::json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"PreToolUse": [
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "other-tool"}]}
            ]}
        });
        assert!(merge_hook_entries(&mut settings));
        assert!(!merge_hook_entries(&mut settings), "second install is a no-op");
        let entries = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "foreign Edit entry preserved alongside Grep+Bash");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
    }

    #[test]
    fn uninstall_removes_only_ours_and_prunes_empty_containers() {
        let mut settings = serde_json::json!({});
        merge_hook_entries(&mut settings);
        assert!(remove_hook_entries(&mut settings));
        assert!(settings.get("hooks").is_none(), "empty containers pruned");

        let mut mixed = serde_json::json!({
            "hooks": {"PreToolUse": [
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "other-tool"}]}
            ]}
        });
        merge_hook_entries(&mut mixed);
        remove_hook_entries(&mut mixed);
        let entries = mixed["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["matcher"], "Edit");
    }

    #[test]
    fn status_reports_per_matcher_presence() {
        let mut settings = serde_json::json!({});
        let s = hook_status(&settings);
        assert!(!s.pretooluse && !s.session_start);
        merge_hook_entries(&mut settings);
        let s = hook_status(&settings);
        assert!(s.pretooluse && s.session_start);
    }

    // -----------------------------------------------------------------------
    // SessionStart — new behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn install_writes_session_start_entry() {
        let mut settings = serde_json::json!({});
        merge_hook_entries(&mut settings);

        let ss = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1, "exactly one SessionStart entry");
        assert_eq!(ss[0]["matcher"], SESSION_START_MATCHER);
        let hook = &ss[0]["hooks"][0];
        assert_eq!(hook["command"], HOOK_COMMAND);
        assert_eq!(hook["timeout"], SESSION_START_TIMEOUT);
    }

    #[test]
    fn install_session_start_is_idempotent() {
        let mut settings = serde_json::json!({});
        merge_hook_entries(&mut settings);
        let changed = merge_hook_entries(&mut settings);
        assert!(!changed, "re-install must be a no-op");

        let ss = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1, "no duplicate SessionStart entry");
    }

    #[test]
    fn install_replaces_drifted_session_start_matcher() {
        // Pre-existing is-ours SessionStart entry with a different matcher.
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "matcher": "startup",
                        "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": 5}]
                    }
                ]
            }
        });
        let changed = merge_hook_entries(&mut settings);
        assert!(changed, "drifted matcher must trigger a change");

        let ss = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1, "old drifted entry replaced, not duplicated");
        assert_eq!(ss[0]["matcher"], SESSION_START_MATCHER);
    }

    #[test]
    fn uninstall_removes_both_events_and_prunes() {
        let mut settings = serde_json::json!({});
        merge_hook_entries(&mut settings);
        let changed = remove_hook_entries(&mut settings);
        assert!(changed);
        assert!(settings.get("hooks").is_none(), "hooks container pruned when empty");
    }

    #[test]
    fn uninstall_preserves_foreign_session_start_entry() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "matcher": "startup",
                        "hooks": [{"type": "command", "command": "some-other-tool", "timeout": 5}]
                    }
                ]
            }
        });
        merge_hook_entries(&mut settings);
        remove_hook_entries(&mut settings);

        // Foreign SessionStart entry must survive.
        let ss = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1, "foreign SessionStart entry preserved");
        assert_eq!(ss[0]["hooks"][0]["command"], "some-other-tool");
    }

    #[test]
    fn status_reflects_both_events_after_install_and_uninstall() {
        let mut settings = serde_json::json!({});
        merge_hook_entries(&mut settings);
        let s = hook_status(&settings);
        assert!(s.pretooluse, "pretooluse installed");
        assert!(s.session_start, "session_start installed");

        remove_hook_entries(&mut settings);
        let s = hook_status(&settings);
        assert!(!s.pretooluse, "pretooluse uninstalled");
        assert!(!s.session_start, "session_start uninstalled");
    }

    #[test]
    fn garbage_hooks_value_does_not_panic() {
        // `hooks` is a string instead of an object — must never panic.
        let mut settings = serde_json::json!({"hooks": "not-an-object"});
        // All three operations must survive without panicking.
        let _ = merge_hook_entries(&mut settings);
        let _ = remove_hook_entries(&mut settings);
        let _ = hook_status(&settings);
    }
}
