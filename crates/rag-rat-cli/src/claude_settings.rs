//! Claude Code settings.json management for the grep-augment PreToolUse hook
//! (`rag-rat hooks install|uninstall|status --claude [--global]`).
//!
//! Edits are additive and marker-aware: our entries are recognized by `HOOK_COMMAND`;
//! everything else in the file is preserved byte-for-byte at the JSON level (read → modify
//! → pretty-print 2-space, matching how Claude Code writes the file).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const HOOK_COMMAND: &str = "rag-rat claude-hook";
const MATCHERS: &[&str] = &["Grep", "Bash"];

fn our_entry(matcher: &str) -> Value {
    json!({
        "matcher": matcher,
        "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": 10}]
    })
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(|h| h["command"] == HOOK_COMMAND))
}

/// The `hooks.PreToolUse` array, the single place both install and uninstall navigate to.
/// With `create`, missing `hooks`/`PreToolUse` containers are inserted (install path); without
/// it, a missing or malformed path yields `None` (uninstall/read path). Returns `None` rather
/// than panicking whenever `hooks` or `PreToolUse` exists but is the wrong JSON type — a user's
/// hand-edited settings.json must never crash `rag-rat hooks`.
fn pretooluse_array_mut(settings: &mut Value, create: bool) -> Option<&mut Vec<Value>> {
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
            hooks.as_object_mut().unwrap().entry("PreToolUse").or_insert_with(|| json!([]));
        }
    }
    settings.get_mut("hooks").and_then(|h| h.get_mut("PreToolUse")).and_then(Value::as_array_mut)
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
            entries.push(our_entry(matcher));
            changed = true;
        }
    }
    changed
}

/// Remove our entries; prune `PreToolUse`/`hooks` containers that end up empty.
pub fn remove_hook_entries(settings: &mut Value) -> bool {
    let Some(entries) = pretooluse_array_mut(settings, false) else {
        return false;
    };
    let before = entries.len();
    entries.retain(|e| !is_ours(e));
    let changed = entries.len() != before;
    if entries.is_empty() {
        // Both unwraps are safe: we only reach here because get_mut("hooks") succeeded
        // (meaning "hooks" key exists and is an object) and "PreToolUse" was present.
        settings["hooks"].as_object_mut().unwrap().remove("PreToolUse");
    }
    if settings["hooks"].as_object().is_some_and(serde_json::Map::is_empty) {
        settings.as_object_mut().unwrap().remove("hooks");
    }
    changed
}

/// (grep_installed, bash_installed) for `hooks status --claude`.
pub fn hook_status(settings: &Value) -> (bool, bool) {
    let installed = |matcher: &str| {
        settings["hooks"]["PreToolUse"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|e| e["matcher"] == matcher && is_ours(e)))
    };
    (installed("Grep"), installed("Bash"))
}

/// Project `.claude/settings.json` or, with `--global`, `~/.claude/settings.json`.
pub fn settings_path(repo_root: &Path, global: bool) -> anyhow::Result<PathBuf> {
    if global {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(settings)?))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(hook_status(&settings), (false, false));
        merge_hook_entries(&mut settings);
        assert_eq!(hook_status(&settings), (true, true));
    }
}
