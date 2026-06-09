//! Unix-socket listener serving the Claude Code grep-augmentation PreToolUse hook.
//!
//! One listener per worktree (socket election lock); newline-delimited JSON, one request per
//! connection; per-session dedupe in memory. Read-only on the index by construction. Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// One grep-augment query from a hook client. Unknown fields are ignored (forward compat);
/// unknown `v`/`kind` get a null-context reply rather than an error.
#[derive(Debug, Deserialize)]
pub struct HookRequest {
    pub v: u32,
    pub kind: String,
    pub session_id: String,
    pub pattern: String,
    #[serde(default)]
    pub search_path: Option<String>,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct HookResponse {
    pub v: u32,
    pub context: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_and_tolerates_unknown_fields() {
        let json = r#"{"v":1,"kind":"grep_augment","session_id":"s1","pattern":"foo",
                       "search_path":null,"source":"grep_tool","future_field":true}"#;
        let req: HookRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.v, 1);
        assert_eq!(req.kind, "grep_augment");
        assert_eq!(req.pattern, "foo");
        assert!(req.search_path.is_none());
    }

    #[test]
    fn response_serializes_null_context_explicitly() {
        let resp = HookResponse { v: 1, context: None };
        assert_eq!(serde_json::to_string(&resp).unwrap(), r#"{"v":1,"context":null}"#);
    }
}
