//! The oracle tool registry: per-tool id, version detection, invocation command, prerequisites,
//! and languages. Phase 2 (#69) ships only the `rust-analyzer scip` backend; the registry is the
//! seam later language backends (#71 TS, #72 Kotlin, #74 live LSP) extend with one entry each
//! rather than new protocol code.
//!
//! A missing or unrunnable tool is NOT an error: [`ToolManifest::probe`] returns
//! [`ToolAvailability::Blocked`] with an install hint, the same UX as a missing embedding model.
//! The `oracle run` command turns a `Blocked` probe into a printed hint + exit 0, never a non-zero
//! exit.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use super::OracleTool;

/// The static registry entry for one oracle backend.
#[derive(Debug, Clone, Copy)]
pub struct ToolManifest {
    /// The persisted tool id (`OracleTool::as_db_str`).
    pub tool: OracleTool,
    /// The executable to invoke (looked up on `PATH`).
    pub program: &'static str,
    /// Languages this backend resolves, for status/diagnostics.
    pub languages: &'static [&'static str],
    /// A one-line install hint surfaced when the tool is absent (the `Blocked` UX).
    pub install_hint: &'static str,
}

/// Whether a registered tool can run, with the data needed to either invoke it or explain why not.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolAvailability {
    /// The tool is on `PATH` and reported a version; `oracle run` can invoke it.
    Available { tool: String, program: String, version: String },
    /// The tool is missing or unrunnable. `oracle run` prints `hint` and exits 0 (no error),
    /// mirroring the missing-embedding-model degradation.
    Blocked { tool: String, program: String, hint: String },
}

impl ToolAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

impl ToolManifest {
    /// The manifest entry for a tool. Every [`OracleTool`] variant has exactly one.
    pub fn for_tool(tool: OracleTool) -> ToolManifest {
        match tool {
            OracleTool::RustAnalyzer => ToolManifest {
                tool,
                program: "rust-analyzer",
                languages: &["rust"],
                install_hint: "rust-analyzer not found on PATH. Install it (e.g. `rustup \
                               component add rust-analyzer`) or pass a pre-built index with \
                               `--scip <path>`.",
            },
        }
    }

    /// Probe whether the tool can run by detecting its version. Never errors: an absent or
    /// unrunnable program yields [`ToolAvailability::Blocked`] with the install hint, so the
    /// `oracle run` / `oracle status` UX can degrade gracefully (exit 0) instead of failing.
    pub fn probe(&self) -> ToolAvailability {
        match detect_version(self.program) {
            Some(version) => ToolAvailability::Available {
                tool: self.tool.as_db_str().to_string(),
                program: self.program.to_string(),
                version,
            },
            None => ToolAvailability::Blocked {
                tool: self.tool.as_db_str().to_string(),
                program: self.program.to_string(),
                hint: self.install_hint.to_string(),
            },
        }
    }

    /// Build the command that produces a `.scip` index at `output` for the checkout rooted at
    /// `root`. `rust-analyzer scip <root> --output <path>` writes the SCIP index to a deterministic
    /// path so the caller (a temp file) can consume it.
    pub fn scip_command(&self, root: &Path, output: &Path) -> Command {
        match self.tool {
            OracleTool::RustAnalyzer => {
                let mut cmd = Command::new(self.program);
                cmd.arg("scip").arg(root).arg("--output").arg(output);
                cmd
            },
        }
    }
}

/// Run `<program> --version` and return the first non-empty trimmed stdout line, or `None` when the
/// program is absent / not executable / exits non-zero. The version string is opaque (recorded as
/// `tool_version` for content-addressed staleness — a different version invalidates prior
/// verdicts).
fn detect_version(program: &str) -> Option<String> {
    let output = Command::new(program).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_manifest_entry() {
        // Exhaustive over the OracleTool registry: each variant must resolve to a manifest with a
        // non-empty program + hint, so `oracle run`/`status` can always describe it. (One variant
        // today; the `match` is the exhaustiveness guard a new variant trips.)
        let all_tools: &[OracleTool] = &[OracleTool::RustAnalyzer];
        for &tool in all_tools {
            let manifest = ToolManifest::for_tool(tool);
            assert_eq!(manifest.tool, tool);
            assert!(!manifest.program.is_empty());
            assert!(!manifest.install_hint.is_empty());
            assert!(!manifest.languages.is_empty());
        }
    }

    #[test]
    fn absent_program_probes_blocked_with_hint() {
        // A program that cannot exist on PATH yields Blocked (never an error), carrying the hint —
        // the missing-tool UX the `oracle run` command turns into exit 0.
        let manifest = ToolManifest {
            tool: OracleTool::RustAnalyzer,
            program: "rag-rat-no-such-tool-xyzzy",
            languages: &["rust"],
            install_hint: "install hint here",
        };
        let availability = manifest.probe();
        assert!(!availability.is_available());
        match availability {
            ToolAvailability::Blocked { hint, program, .. } => {
                assert_eq!(program, "rag-rat-no-such-tool-xyzzy");
                assert_eq!(hint, "install hint here");
            },
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn detect_version_reads_a_known_program() {
        // `cargo --version` is reliably present in the test environment (we're building with it).
        // Proves the version-detection path returns a non-empty line for a real program.
        let version = detect_version("cargo");
        assert!(version.is_some_and(|v| v.starts_with("cargo")));
    }

    #[test]
    fn scip_command_targets_output_path() {
        let manifest = ToolManifest::for_tool(OracleTool::RustAnalyzer);
        let cmd = manifest.scip_command(Path::new("/repo"), Path::new("/tmp/out.scip"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "rust-analyzer");
        assert_eq!(args, vec!["scip", "/repo", "--output", "/tmp/out.scip"]);
    }
}
