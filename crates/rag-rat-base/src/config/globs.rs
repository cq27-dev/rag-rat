//! The glob matcher behind a target's `include` / `exclude` patterns.
//!
//! **Real globs, via [`globset`].** This used to be a hand-written cascade over three shapes — a
//! `**/*.ext` suffix, a `dir/**` subtree, and a literal — whose last line fell through to
//! `path.contains(pattern.trim_matches('*'))`. Substring containment is not glob matching: `*.rs`
//! became `contains(".rs")` and claimed `notes.rs.bak` and `src/lib.rs.orig`, and `*` (which trims
//! to the empty string, contained by every path) claimed the whole tree. Those are wrong answers
//! about which files are indexed, and they fail silently. [`globset`] is `ignore`'s own matching
//! layer — already compiled on every build, and the matcher ripgrep uses for this exact job — so
//! `**`, `*`, `?`, `[…]` and `{…}` now mean what a glob says they mean.
//!
//! Three options are pinned explicitly rather than taken from the defaults, because each default is
//! wrong for a repo-relative config pattern:
//!
//! 1. **`literal_separator(true)`.** `globset` defaults it to *false*, where `*` and `?` match `/`
//!    as well — so `src/*.rs` would claim `src/a/b/deep.rs`. A config pattern names path
//!    components, so a wildcard must stop at a separator; only `**` crosses one.
//! 2. **`backslash_escape(true)`.** `globset` defaults this to `!is_separator('\\')` — *true* on
//!    Unix, *false* on Windows. Left unpinned, one `rag-rat.toml` would claim different files on
//!    different platforms. Pinned on, `\` escapes the next character everywhere, so a pattern
//!    reaches a Unix file whose NAME contains a backslash by spelling it `\\`.
//! 3. **The match runs the compiled glob's regex over the rendered BYTES**, never through
//!    [`globset`]'s `Candidate`. The caller already holds the `/`-separated rendering `files.path`
//!    is stored with ([`crate::paths::path_string`]); `Candidate` re-runs platform-specific
//!    separator handling over that already-canonical string (on Windows it rewrites `\` to `/`,
//!    turning a Unix file NAMED `drafts\secret.md` into a child of `drafts/`), so one config would
//!    again claim different files on different platforms. [`Glob::regex`] is `globset`'s documented
//!    escape hatch for exactly this: the emitted pattern is built for the [`regex`] crate's bytes
//!    API.
//!
//! One shape is normalized before compiling rather than pinned as an option: a trailing `/` names a
//! subtree, so `src/` compiles as `src/**` (see [`compile`]).
//!
//! [`Glob::regex`]: globset::Glob::regex

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use globset::GlobBuilder;
use regex::bytes::{Regex, RegexBuilder};

/// Compiled matchers keyed by the pattern text, so a pattern is turned into a regex once per
/// process instead of once per file.
///
/// [`super::ResolvedTarget::globs_claim`] is the per-file gate of the indexing walk and is also
/// asked once per path (across every target) by the incremental resolver, so pattern compilation
/// must not sit on that path. The pattern set is bounded by the config file, so the map needs no
/// eviction. A pattern that fails to compile is memoized as [`None`] — the compile error is
/// reported once, not once per file.
static COMPILED: LazyLock<RwLock<HashMap<String, Option<Regex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Whether one config glob claims `path`, a `/`-separated repo-relative rendering.
///
/// A pattern that is not a legal glob (an unclosed `[`, a dangling `\`) claims NOTHING and is
/// reported once. Config load does not reject such a pattern today, so the matcher has to answer
/// something; claiming nothing keeps a typo from silently widening an `include`.
pub(crate) fn pattern_claims(path: &str, pattern: &str) -> bool {
    let cached = COMPILED.read().ok().and_then(|compiled| {
        compiled
            .get(pattern)
            .map(|matcher| matcher.as_ref().is_some_and(|re| re.is_match(path.as_bytes())))
    });
    if let Some(claims) = cached {
        return claims;
    }
    let matcher = compile(pattern);
    let claims = matcher.as_ref().is_some_and(|re| re.is_match(path.as_bytes()));
    if let Ok(mut compiled) = COMPILED.write() {
        compiled.entry(pattern.to_string()).or_insert(matcher);
    }
    claims
}

/// Compile one pattern, or [`None`] (plus a warning) when it is not a legal glob.
///
/// A trailing `/` names a ROOT-ANCHORED SUBTREE, so `src/` compiles as `src/**`. As a bare glob it
/// would name a single path ending in a separator and claim nothing at all — and a target whose
/// `include` silently claims nothing indexes zero files, a worse and quieter failure than the
/// over-claiming this replacement removes. This is rag-rat's own target dialect: close to but not
/// identical to gitignore's trailing slash, which matches unanchored (a `src/` there would also
/// match `a/src/`). Here `src/` is the ROOT `src/`, matching what the old substring fallback got
/// *approximately* right — it matched `src/` by containment, but also claimed `a/src/lib.rs`.
/// Normalizing to `src/**` keeps the intent and adds the anchor the fallback lacked.
fn compile(pattern: &str) -> Option<Regex> {
    let subtree = pattern.strip_suffix('/').map(|dir| format!("{dir}/**"));
    let pattern = subtree.as_deref().unwrap_or(pattern);
    let glob =
        match GlobBuilder::new(pattern).literal_separator(true).backslash_escape(true).build() {
            Ok(glob) => glob,
            Err(error) => {
                tracing::warn!(
                    pattern,
                    %error,
                    "target include/exclude pattern is not a valid glob; it claims no files",
                );
                return None;
            },
        };
    // `dot_matches_new_line` is a BUILDER flag in globset (not part of the emitted pattern), so it
    // must be re-applied here or a file whose NAME contains a newline escapes every `**`.
    match RegexBuilder::new(glob.regex()).dot_matches_new_line(true).build() {
        Ok(re) => Some(re),
        Err(error) => {
            // The emitted pattern is always valid, but a huge (still legal) brace alternation can
            // exceed the regex crate's compiled-size limit; claim nothing rather than panic.
            tracing::warn!(pattern, %error, "glob regex did not compile; it claims no files");
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_does_not_cross_a_separator() {
        // `literal_separator` is OFF by default in globset, which would make `*` match `/`.
        assert!(pattern_claims("src/lib.rs", "src/*.rs"));
        assert!(!pattern_claims("src/a/deep.rs", "src/*.rs"));
        assert!(pattern_claims("src/a/deep.rs", "src/**/*.rs"));
    }

    #[test]
    fn a_backslash_escape_is_pinned_on_every_platform() {
        // globset's default here is platform-dependent (`!is_separator('\\')`), so one config would
        // otherwise claim different files on Unix and Windows. Pinned ON: `\\` is one literal `\`.
        assert!(pattern_claims("drafts\\secret.md", "drafts\\\\secret.md"));
        assert!(!pattern_claims("drafts/secret.md", "drafts\\\\secret.md"));
    }

    #[test]
    fn a_newline_in_a_name_does_not_escape_a_subtree_glob() {
        // `**` compiles to `.*`; without `dot_matches_new_line` re-applied at regex build, a
        // newline-bearing name would slip past every subtree exclude.
        assert!(pattern_claims("src/a\nb.rs", "src/**"));
        assert!(pattern_claims("src/a\nb.rs", "src/"));
    }

    #[test]
    fn a_trailing_slash_names_the_subtree_and_anchors_it() {
        assert!(pattern_claims("src/lib.rs", "src/"));
        assert!(pattern_claims("src/a/b/deep.rs", "src/"));
        // The anchor the substring fallback lacked: `src/` is the ROOT `src/`, not any `src/`.
        assert!(!pattern_claims("a/src/lib.rs", "src/"));
        assert!(!pattern_claims("srcx/lib.rs", "src/"));
        // …and the directory itself is not one of its own children.
        assert!(!pattern_claims("src", "src/"));
    }

    #[test]
    fn an_illegal_glob_claims_nothing_instead_of_everything() {
        // A dangling escape is a compile error; the matcher must not fall back to "matches all".
        assert!(!pattern_claims("src/lib.rs", "src/lib.rs\\"));
        // The memoized failure answers the same way on the second call.
        assert!(!pattern_claims("src/lib.rs", "src/lib.rs\\"));
    }

    #[test]
    fn a_compiled_pattern_is_reused_across_calls() {
        // Second call must come off the memo and agree with the first — the property the per-file
        // hot path depends on.
        let pattern = "**/*.reused-in-a-test";
        assert!(pattern_claims("a/b.reused-in-a-test", pattern));
        assert!(pattern_claims("a/b.reused-in-a-test", pattern));
        assert!(
            COMPILED.read().expect("cache lock").contains_key(pattern),
            "the pattern must be compiled once and memoized",
        );
    }
}
