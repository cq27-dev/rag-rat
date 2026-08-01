use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
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

#[derive(Debug, Clone, Copy)]
struct LanguageSpec {
    language: Language,
    name: &'static str,
    aliases: &'static [&'static str],
    simple_extensions: &'static [&'static str],
    target_extensions: &'static [&'static str],
}

const LANGUAGE_SPECS: [LanguageSpec; 9] = [
    LanguageSpec {
        language: Language::Rust,
        name: "rust",
        aliases: &["rs"],
        simple_extensions: &["rs"],
        target_extensions: &["rs"],
    },
    LanguageSpec {
        language: Language::TypeScript,
        name: "typescript",
        aliases: &["ts", "tsx"],
        simple_extensions: &["ts", "tsx"],
        target_extensions: &["ts", "tsx"],
    },
    LanguageSpec {
        language: Language::Kotlin,
        name: "kotlin",
        aliases: &["kt"],
        simple_extensions: &["kt", "kts"],
        target_extensions: &["kt", "kts"],
    },
    LanguageSpec {
        language: Language::C,
        name: "c",
        aliases: &[],
        simple_extensions: &["c", "h"],
        target_extensions: &["c", "h"],
    },
    LanguageSpec {
        language: Language::Cpp,
        name: "cpp",
        aliases: &["c++", "cc", "cxx"],
        simple_extensions: &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"],
        target_extensions: &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "h"],
    },
    LanguageSpec {
        language: Language::Python,
        name: "python",
        aliases: &["py"],
        simple_extensions: &["py", "pyi"],
        target_extensions: &["py", "pyi"],
    },
    LanguageSpec {
        language: Language::Swift,
        name: "swift",
        aliases: &[],
        simple_extensions: &["swift"],
        target_extensions: &["swift"],
    },
    LanguageSpec {
        language: Language::Go,
        name: "go",
        aliases: &["golang"],
        simple_extensions: &["go"],
        target_extensions: &["go"],
    },
    LanguageSpec {
        language: Language::Markdown,
        name: "markdown",
        aliases: &["md"],
        simple_extensions: &["md", "markdown"],
        target_extensions: &["md", "markdown"],
    },
];

impl Language {
    pub const ALL: [Self; 9] = [
        Self::Rust,
        Self::TypeScript,
        Self::Kotlin,
        Self::C,
        Self::Cpp,
        Self::Python,
        Self::Swift,
        Self::Go,
        Self::Markdown,
    ];

    pub fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub fn as_str(self) -> &'static str {
        self.spec().name
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
        &LANGUAGE_SPECS[self as usize]
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Language {
    type Err = LanguageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        LANGUAGE_SPECS
            .iter()
            .find(|spec| spec.name == normalized || spec.aliases.contains(&normalized.as_str()))
            .map(|spec| spec.language)
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

    use super::{LANGUAGE_SPECS, Language};

    #[test]
    fn language_registry_is_complete_unique_and_round_trips_every_name() {
        assert_eq!(
            LANGUAGE_SPECS.iter().map(|spec| spec.language).collect::<Vec<_>>(),
            Language::all()
        );
        let mut names = HashSet::new();
        for spec in LANGUAGE_SPECS {
            assert_eq!(
                spec.language.as_str(),
                spec.name,
                "Language discriminant/spec order drifted"
            );
            assert!(names.insert(spec.name), "duplicate canonical language name: {}", spec.name);
            assert_eq!(Language::from_str(spec.name).unwrap(), spec.language);
            for alias in spec.aliases {
                assert!(names.insert(alias), "duplicate language name or alias: {alias}");
                assert_eq!(Language::from_str(alias).unwrap(), spec.language);
            }
            assert!(
                spec.simple_extensions.iter().all(|ext| spec.target_extensions.contains(ext)),
                "target extensions must include every simple extension for {}",
                spec.name
            );
        }
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
