use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    TypeScript,
    Kotlin,
    C,
    Cpp,
    Python,
    Markdown,
}

impl Language {
    pub const ALL: [Self; 7] = [
        Self::Rust,
        Self::TypeScript,
        Self::Kotlin,
        Self::C,
        Self::Cpp,
        Self::Python,
        Self::Markdown,
    ];

    pub fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Kotlin => "kotlin",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Python => "python",
            Self::Markdown => "markdown",
        }
    }

    /// Extensions used for **bare** language detection ([`Self::from_path`]) — the unambiguous
    /// default for a file seen with no explicit target binding. `.h` lives on C here (the safe
    /// default for the ambiguous C/C++ header); an explicit `cpp` binding upgrades it via
    /// [`Self::target_extensions`].
    pub fn simple_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::TypeScript => &["ts", "tsx"],
            Self::Kotlin => &["kt", "kts"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"],
            Self::Python => &["py", "pyi"],
            Self::Markdown => &["md", "markdown"],
        }
    }

    /// Extensions an **explicit** target/binding of this language claims for indexing. Identical to
    /// [`Self::simple_extensions`] except a `cpp` target also claims the ambiguous `.h` header:
    /// bare detection resolves `.h` to C (the safe default), but binding a directory as `cpp` is
    /// the signal to index its `.h` headers as C++ (otherwise a C++ library whose API lives in
    /// `.h` files — most of them — gets no header symbols, so cross-file calls resolve to
    /// nothing).
    pub fn target_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Cpp => &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "h"],
            _ => self.simple_extensions(),
        }
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

    pub fn supports_embeddings(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::TypeScript
                | Self::Kotlin
                | Self::C
                | Self::Cpp
                | Self::Python
                | Self::Markdown
        )
    }

    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        Self::all().iter().copied().find(|language| language.simple_extensions().contains(&ext))
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
        match value.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Ok(Self::Rust),
            "typescript" | "ts" | "tsx" => Ok(Self::TypeScript),
            "kotlin" | "kt" => Ok(Self::Kotlin),
            "c" => Ok(Self::C),
            "cpp" | "c++" | "cc" | "cxx" => Ok(Self::Cpp),
            "python" | "py" => Ok(Self::Python),
            "markdown" | "md" => Ok(Self::Markdown),
            other => Err(LanguageError::Unknown(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum LanguageError {
    #[error("unknown language `{0}`")]
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::Language;

    #[test]
    fn bare_detection_resolves_h_to_c_not_cpp() {
        // `.h` is ambiguous; bare detection picks the safe default (C), never C++.
        assert_eq!(Language::from_path(Path::new("a/b.h")), Some(Language::C));
        assert_eq!(Language::from_path(Path::new("a/b.cpp")), Some(Language::Cpp));
        assert_eq!(Language::from_path(Path::new("a/b.rs")), Some(Language::Rust));
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
