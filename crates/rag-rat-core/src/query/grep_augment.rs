//! Payload composition for the Claude Code grep-augmentation PreToolUse hook.
//!
//! Shared by the `rag-rat mcp` socket listener (with per-session dedupe) and the hook
//! client's direct read-only fallback (stateless). Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`. Never loads the embedding
//! model — symbol/FTS lanes only.

/// Strip regex syntax from a grep pattern, leaving plain query text. Metacharacters become
/// spaces (so alternation/group contents survive as separate words); runs of whitespace
/// collapse; result is trimmed.
pub fn normalize_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Drop regex escapes entirely: \s/\b/\w and \\, \., \-, etc. all become a
                // space — punctuation and class shorthands are not query signal.
                if chars.peek().is_some() {
                    chars.next(); // consume the escaped char
                }
                out.push(' ');
            },
            '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                out.push(' ');
            },
            _ => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A normalized pattern that looks like one code identifier (optionally `::`/`.`-qualified):
/// the symbol-lane trigger. Multi-word or short patterns return `None`.
pub fn identifier_candidate(normalized: &str) -> Option<&str> {
    if normalized.len() < 3 || normalized.contains(' ') {
        return None;
    }
    let mut chars = normalized.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.')).then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_regex_metacharacters_and_anchors() {
        assert_eq!(normalize_pattern(r"^fn\s+watcher_main\b"), "fn watcher_main");
        assert_eq!(
            normalize_pattern(r"Watcher::spawn(_with_fleet)?"),
            "Watcher::spawn _with_fleet"
        );
        assert_eq!(normalize_pattern("plain words"), "plain words");
        assert_eq!(normalize_pattern(r".*[]()|+?^$\\"), "");
    }

    #[test]
    fn identifier_candidate_accepts_identifier_shapes_only() {
        assert_eq!(identifier_candidate("watcher_main"), Some("watcher_main"));
        assert_eq!(identifier_candidate("Watcher::spawn"), Some("Watcher::spawn"));
        assert_eq!(identifier_candidate("foo.bar"), Some("foo.bar"));
        assert_eq!(identifier_candidate("fn watcher_main"), None); // two words
        assert_eq!(identifier_candidate("ab"), None); // too short
        assert_eq!(identifier_candidate("1abc"), None); // leading digit
        assert_eq!(identifier_candidate(""), None);
    }
}
