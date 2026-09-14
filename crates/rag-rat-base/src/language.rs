use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// A source language rag-rat indexes. [`Language::as_db_str`] is the persisted token (the
/// `files.language` / `symbols.language` / `parser_failures.language` column value and the config
/// spelling); the variant name, lowercased, IS that token.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[strum(serialize_all = "lowercase")]
pub enum Language {
    Rust,
    TypeScript,
    Kotlin,
    C,
    Cpp,
    Python,
    Swift,
    Go,
    Markdown,
}

/// Per-language static metadata beyond the name: the alternate spellings config accepts and the
/// extensions each detection mode claims.
#[derive(Debug, Clone, Copy)]
struct LanguageSpec {
    aliases: &'static [&'static str],
    simple_extensions: &'static [&'static str],
    target_extensions: &'static [&'static str],
}

impl Language {
    /// Every language, in declaration order.
    pub fn all() -> &'static [Self] {
        <Self as strum::VariantArray>::VARIANTS
    }

    /// The persisted token for this language. Stable wire string — never rename a variant without
    /// a migration.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// The exact inverse of [`Self::as_db_str`]; `None` for any other text, aliases included (they
    /// are config spellings, never stored).
    pub fn from_db_str(token: &str) -> Option<Self> {
        Self::all().iter().copied().find(|language| language.as_db_str() == token)
    }

    /// Extensions used for **bare** language detection ([`Self::from_path`]) — the unambiguous
    /// default for a file seen with no explicit target binding. `.h` lives on C here (the safe
    /// default for the ambiguous C/C++ header); an explicit `cpp` binding upgrades it via
    /// [`Self::target_extensions`].
    pub fn simple_extensions(self) -> &'static [&'static str] {
        self.spec().simple_extensions
    }

    /// Extensions an **explicit** target/binding of this language claims for indexing. Identical to
    /// [`Self::simple_extensions`] except a `cpp` target also claims the ambiguous `.h` header:
    /// bare detection resolves `.h` to C (the safe default), but binding a directory as `cpp` is
    /// the signal to index its `.h` headers as C++ (otherwise a C++ library whose API lives in
    /// `.h` files — most of them — gets no header symbols, so cross-file calls resolve to
    /// nothing).
    pub fn target_extensions(self) -> &'static [&'static str] {
        self.spec().target_extensions
    }

    /// Whether an explicit target of this language claims a file with this extension (see
    /// [`Self::target_extensions`]).
    pub fn claims_extension(self, ext: &str) -> bool {
        self.target_extensions().contains(&ext)
    }

    /// Whether this language claims an **ambiguous** extension — one another language owns by
    /// default — as a deliberate upgrade (currently only C++ claiming `.h`, which bare
    /// detection gives to C). Indexing precedence sorts such targets FIRST so the explicit
    /// upgrade wins the shared file: a `.h` matched by both a `c` and a `cpp` binding indexes
    /// as C++ (the deliberate intent), not C (the alphabetical-order accident). A `.c` is
    /// claimed only by C, so this never steals it.
    pub fn upgrades_ambiguous_extension(self) -> bool {
        self.target_extensions().iter().any(|ext| !self.simple_extensions().contains(ext))
    }

    /// Whether an explicit target of this language claims this path, by its extension. `false` for
    /// an extensionless path or one whose extension this language doesn't claim.
    pub fn claims_path(self, path: &std::path::Path) -> bool {
        path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| self.claims_extension(ext))
    }

    /// The default `include` globs for a simple binding of this language — one `**/*.<ext>` per
    /// [`Self::target_extensions`]. The single source of truth for rendering a target's filters
    /// ([`crate::config`]) and validating a corpus checkout against its bindings, so the two never
    /// drift.
    pub fn default_include_globs(self) -> Vec<String> {
        self.target_extensions().iter().map(|ext| format!("**/*.{ext}")).collect()
    }

    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        Self::all().iter().copied().find(|language| language.simple_extensions().contains(&ext))
    }

    fn spec(self) -> &'static LanguageSpec {
        match self {
            Self::Rust => &LanguageSpec {
                aliases: &["rs"],
                simple_extensions: &["rs"],
                target_extensions: &["rs"],
            },
            Self::TypeScript => &LanguageSpec {
                aliases: &["ts", "tsx"],
                simple_extensions: &["ts", "tsx"],
                target_extensions: &["ts", "tsx"],
            },
            Self::Kotlin => &LanguageSpec {
                aliases: &["kt"],
                simple_extensions: &["kt", "kts"],
                target_extensions: &["kt", "kts"],
            },
            Self::C => &LanguageSpec {
                aliases: &[],
                simple_extensions: &["c", "h"],
                target_extensions: &["c", "h"],
            },
            Self::Cpp => &LanguageSpec {
                aliases: &["c++", "cc", "cxx"],
                simple_extensions: &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"],
                target_extensions: &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "h"],
            },
            Self::Python => &LanguageSpec {
                aliases: &["py"],
                simple_extensions: &["py", "pyi"],
                target_extensions: &["py", "pyi"],
            },
            Self::Swift => &LanguageSpec {
                aliases: &[],
                simple_extensions: &["swift"],
                target_extensions: &["swift"],
            },
            Self::Go => &LanguageSpec {
                aliases: &["golang"],
                simple_extensions: &["go"],
                target_extensions: &["go"],
            },
            Self::Markdown => &LanguageSpec {
                aliases: &["md"],
                simple_extensions: &["md", "markdown"],
                target_extensions: &["md", "markdown"],
            },
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_db_str())
    }
}

impl FromStr for Language {
    type Err = LanguageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Config input: trimmed and case-insensitive, and the aliases are accepted too.
        let normalized = value.trim().to_ascii_lowercase();
        Self::all()
            .iter()
            .copied()
            .find(|language| {
                language.as_db_str() == normalized
                    || language.spec().aliases.contains(&normalized.as_str())
            })
            .ok_or(LanguageError::Unknown(normalized))
    }
}

#[derive(Debug, Error)]
pub enum LanguageError {
    #[error("unknown language `{0}`")]
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::str::FromStr;

    use super::Language;

    /// The persisted tokens, pinned byte-for-byte: they are column values in every index.
    #[test]
    fn db_tokens_are_pinned_and_round_trip() {
        let expected = [
            (Language::Rust, "rust"),
            (Language::TypeScript, "typescript"),
            (Language::Kotlin, "kotlin"),
            (Language::C, "c"),
            (Language::Cpp, "cpp"),
            (Language::Python, "python"),
            (Language::Swift, "swift"),
            (Language::Go, "go"),
            (Language::Markdown, "markdown"),
        ];
        assert_eq!(expected.map(|(language, _)| language).as_slice(), Language::all());
        for (language, token) in expected {
            assert_eq!(language.as_db_str(), token);
            assert_eq!(language.to_string(), token);
            assert_eq!(Language::from_db_str(token), Some(language));
        }
        assert_eq!(Language::from_db_str("Rust"), None, "the DB side is exact");
        assert_eq!(Language::from_db_str("rs"), None, "aliases are config spellings only");
    }

    #[test]
    fn language_registry_is_unique_and_parses_every_name_and_alias() {
        let mut names = HashSet::new();
        for &language in Language::all() {
            let spec = language.spec();
            let name = language.as_db_str();
            assert!(names.insert(name), "duplicate canonical language name: {name}");
            assert_eq!(Language::from_str(name).unwrap(), language);
            assert_eq!(
                Language::from_str(&format!(" {} ", name.to_uppercase())).unwrap(),
                language
            );
            for alias in spec.aliases {
                assert!(names.insert(alias), "duplicate language name or alias: {alias}");
                assert_eq!(Language::from_str(alias).unwrap(), language);
            }
            assert!(
                spec.simple_extensions.iter().all(|ext| spec.target_extensions.contains(ext)),
                "target extensions must include every simple extension for {name}"
            );
        }
        assert_eq!(
            Language::from_str(" Nope ").unwrap_err().to_string(),
            "unknown language `nope`"
        );
    }

    #[test]
    fn bare_detection_resolves_h_to_c_not_cpp() {
        // `.h` is ambiguous; bare detection picks the safe default (C), never C++.
        assert_eq!(Language::from_path(Path::new("a/b.h")), Some(Language::C));
        assert_eq!(Language::from_path(Path::new("a/b.cpp")), Some(Language::Cpp));
        assert_eq!(Language::from_path(Path::new("a/b.rs")), Some(Language::Rust));
        assert_eq!(Language::from_path(Path::new("a/b.swift")), Some(Language::Swift));
        assert_eq!(Language::from_path(Path::new("a/README")), None);
    }

    #[test]
    fn cpp_target_claims_h_headers_but_c_target_keeps_them_too() {
        // An explicit `cpp` binding claims `.h` (so a C++ library's `.h` API gets indexed)...
        assert!(Language::Cpp.claims_extension("h"));
        assert!(Language::Cpp.claims_extension("cpp"));
        assert!(Language::Cpp.claims_path(Path::new("include/fmt/format.h")));
        // ...while `.c` still belongs to C, not C++.
        assert!(!Language::Cpp.claims_extension("c"));
        // C continues to claim both `.c` and `.h`.
        assert!(Language::C.claims_extension("c"));
        assert!(Language::C.claims_extension("h"));
        // Other languages are unchanged (no `.h` creep).
        assert!(!Language::Rust.claims_extension("h"));
        assert!(Language::Rust.claims_extension("rs"));
        assert!(!Language::Cpp.claims_path(Path::new("README")));
    }

    #[test]
    fn default_include_globs_track_target_extensions() {
        assert_eq!(Language::Rust.default_include_globs(), vec!["**/*.rs"]);
        // cpp globs include `**/*.h` (the header-resolution fix) alongside the cpp source globs.
        let cpp = Language::Cpp.default_include_globs();
        assert!(cpp.contains(&"**/*.h".to_string()), "cpp globs must include .h: {cpp:?}");
        assert!(cpp.contains(&"**/*.cpp".to_string()));
    }
}
