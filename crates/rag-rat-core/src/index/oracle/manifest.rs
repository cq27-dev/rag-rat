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
            OracleTool::ScipClang => ToolManifest {
                tool,
                program: "scip-clang",
                languages: &["c", "cpp"],
                install_hint: "scip-clang not found on PATH. Install it from \
                               github.com/sourcegraph/scip-clang and generate a \
                               compile_commands.json for the checkout (e.g. `bear -- make`, CMake \
                               `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`, or the kernel's \
                               scripts/clang-tools/gen_compile_commands.py), or pass a pre-built \
                               index with `--scip <path>`.",
            },
            OracleTool::ScipPython => ToolManifest {
                tool,
                program: "scip-python",
                languages: &["python"],
                install_hint: "scip-python not found on PATH. Install it (e.g. `npm install -g \
                               @sourcegraph/scip-python`) AND install the project's dependencies \
                               (e.g. into a virtualenv) so imports resolve, or pass a pre-built \
                               index with `--scip <path>`.",
            },
            OracleTool::ScipTypescript => ToolManifest {
                tool,
                program: "scip-typescript",
                languages: &["typescript"],
                install_hint: "scip-typescript not found on PATH. Install it (e.g. `npm install \
                               -g @sourcegraph/scip-typescript`) AND install the project's \
                               dependencies (`npm install`) so cross-package references resolve, \
                               or pass a pre-built index with `--scip <path>`.",
            },
        }
    }

    /// Probe whether the tool can run by detecting its version AND confirming it can actually
    /// produce SCIP (the [`Self::can_emit_scip`] capability check, which is tool-specific). Never
    /// errors: an absent program, an unrunnable one, OR a versioned binary that can't emit SCIP
    /// all yield [`ToolAvailability::Blocked`] with the install hint, so the `oracle run` /
    /// `oracle status` UX degrades gracefully (exit 0) instead of failing — or worse, invoking a
    /// SCIP-less binary and reporting a confusing subprocess error.
    ///
    /// Checking only `--version` (#82 P3) would mark a stripped/wrong `rust-analyzer` build with no
    /// `scip` subcommand as `Available`, then fail the actual run. `Blocked` must mean "can't
    /// produce SCIP."
    pub fn probe(&self) -> ToolAvailability {
        match detect_version(self.program) {
            Some(version) if self.can_emit_scip() => ToolAvailability::Available {
                tool: self.tool.as_db_str().to_string(),
                program: self.program.to_string(),
                version,
            },
            _ => ToolAvailability::Blocked {
                tool: self.tool.as_db_str().to_string(),
                program: self.program.to_string(),
                hint: self.install_hint.to_string(),
            },
        }
    }

    /// The cheap capability check that a versioned binary can actually emit a SCIP index — its
    /// shape is tool-specific. `rust-analyzer` emits via a `scip` subcommand (`scip --help` must
    /// exit 0; a stripped build without it is `Blocked`, #82 P3). `scip-clang` IS the SCIP
    /// emitter — it has no subcommand — so a successful `--version` (already detected) is the
    /// capability signal and this is a no-op `true`.
    fn can_emit_scip(&self) -> bool {
        match self.tool {
            OracleTool::RustAnalyzer => Command::new(self.program)
                .arg("scip")
                .arg("--help")
                .output()
                .is_ok_and(|output| output.status.success()),
            OracleTool::ScipClang => true,
            // scip-python emits via an `index` subcommand; `index --help` exiting 0 is the analog
            // of rust-analyzer's `scip --help` capability check.
            OracleTool::ScipPython | OracleTool::ScipTypescript => Command::new(self.program)
                .arg("index")
                .arg("--help")
                .output()
                .is_ok_and(|output| output.status.success()),
        }
    }

    /// A tool-specific prerequisite that must hold before a tool-driven run, beyond the binary
    /// being installed. `None` means ready. `scip-clang` needs a `compile_commands.json`
    /// compilation database at the checkout root; without it the run can't proceed, so it is
    /// reported as `Blocked` (install-hint UX, exit 0) rather than a subprocess error. The
    /// pre-built `--scip` path never reaches here.
    pub fn prerequisite_blocked(&self, root: &Path) -> Option<String> {
        match self.tool {
            // scip-python's "deps must be installed" prerequisite has no single sentinel file to
            // check (it's whatever the corpus `prepare` venv installs); a failed environment shows
            // up as a near-zero moniker count the report health gate catches, so there's nothing to
            // block on here.
            OracleTool::RustAnalyzer | OracleTool::ScipPython => None,
            // scip-typescript needs a `tsconfig.json` at the root: with `--infer-tsconfig` it would
            // otherwise WRITE one into the checkout (confirmed against v0.4.0), violating the
            // read-only-on-source contract — so we don't pass that flag and instead require a real
            // tsconfig (the TS analog of scip-clang's compile_commands.json). Cross-package deps
            // (`node_modules`) are the corpus `prepare` step's job; a missing one is NOT reliably
            // caught by the moniker-count gate (scip-typescript mints local monikers from
            // package.json regardless of node_modules) — only external resolution drops. A
            // dedicated external-resolution health signal is tracked in #185.
            OracleTool::ScipTypescript => (!root.join("tsconfig.json").exists()).then(|| {
                format!(
                    "scip-typescript requires a tsconfig.json at {} — add one to the project \
                     (most TypeScript projects ship one), or pass a pre-built index with `--scip \
                     <path>`.",
                    root.display()
                )
            }),
            OracleTool::ScipClang => (!root.join("compile_commands.json").exists()).then(|| {
                format!(
                    "scip-clang requires a compile_commands.json at {} — generate one (e.g. `bear \
                     -- make`, CMake `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`, or the kernel's \
                     scripts/clang-tools/gen_compile_commands.py), or pass a pre-built index with \
                     `--scip <path>`.",
                    root.display()
                )
            }),
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
            // scip-clang consumes the compilation database, not a source root, and emits the index
            // directly (no subcommand). Run with cwd = root so the compdb's relative paths
            // resolve, and point it at `root/compile_commands.json` (prerequisite-checked).
            OracleTool::ScipClang => {
                let mut cmd = Command::new(self.program);
                cmd.current_dir(root)
                    .arg(format!("--compdb-path={}", root.join("compile_commands.json").display()))
                    .arg(format!("--index-output-path={}", output.display()));
                cmd
            },
            // scip-python indexes a working directory (not a source root arg) via its `index`
            // subcommand. `--cwd <root>` is where it resolves the project + its installed deps;
            // `--project-name` (the root's dir name) becomes the package component of in-corpus
            // monikers, so a non-empty name is what lets `count_symbols_with_moniker` see them.
            // `--project-version _` is PINNED (Codex on #176): scip-python otherwise defaults the
            // version to the checkout's git revision, which is embedded in every SCIP symbol
            // string, so every commit would churn all Python monikers — breaking
            // moniker-anchored memory relocation (which resolves by exact moniker per
            // tool). A constant version keeps a symbol's moniker stable across commits
            // (and sidesteps scip-python's crash on a non-git checkout, where the
            // git-rev default is undefined). `--output` is absolute, so it's unaffected
            // by `--cwd`.
            OracleTool::ScipPython => {
                let project_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("project");
                let mut cmd = Command::new(self.program);
                cmd.arg("index")
                    .arg("--project-name")
                    .arg(project_name)
                    .arg("--project-version")
                    .arg("_")
                    .arg("--cwd")
                    .arg(root)
                    .arg("--output")
                    .arg(output);
                cmd
            },
            // scip-typescript indexes the working dir via its `index` subcommand (like
            // scip-python), reading the project's `tsconfig.json` (prerequisite-checked — we
            // deliberately do NOT pass `--infer-tsconfig`, which WRITES a tsconfig into the source
            // tree, breaking read-only-on-source). No `--project-name` / `--project-version`:
            // package name + version come from `package.json`. That version is embedded in every
            // local moniker and has no CLI override, so it's normalized downstream at
            // moniker-write time (`scip::stabilize_moniker_version`), not here. `--output` is
            // absolute, unaffected by `--cwd`.
            OracleTool::ScipTypescript => {
                let mut cmd = Command::new(self.program);
                cmd.arg("index").arg("--cwd").arg(root).arg("--output").arg(output);
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
        // non-empty program + hint, so `oracle run`/`status` can always describe it. (The `match`
        // in `for_tool` is the exhaustiveness guard a new variant trips.)
        for &tool in OracleTool::ALL {
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
    fn probe_requires_the_scip_subcommand_not_just_a_version() {
        // `cargo` is on PATH and reports a `--version`, but has no `scip` subcommand — so the
        // capability check fails and the tool is Blocked, not Available (#82 P3: `Blocked` must
        // mean "can't produce SCIP," not merely "absent"). We borrow `cargo` as a stand-in
        // for a binary that exists + versions but can't emit SCIP.
        let manifest = ToolManifest {
            tool: OracleTool::RustAnalyzer,
            program: "cargo",
            languages: &["rust"],
            install_hint: "hint",
        };
        assert!(detect_version("cargo").is_some(), "cargo reports a version");
        assert!(!manifest.can_emit_scip(), "cargo has no `scip` subcommand");
        assert!(
            !manifest.probe().is_available(),
            "a versioned binary lacking `scip` must probe Blocked, not Available"
        );
    }

    #[test]
    fn scip_clang_consumes_a_compdb_not_a_root() {
        // scip-clang's invocation differs from rust-analyzer's: --compdb-path / --index-output-path
        // and cwd = root (so the compdb's relative paths resolve), no `scip` subcommand.
        let manifest = ToolManifest::for_tool(OracleTool::ScipClang);
        assert_eq!(manifest.program, "scip-clang");
        let cmd = manifest.scip_command(Path::new("/repo"), Path::new("/tmp/out.scip"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec![
            "--compdb-path=/repo/compile_commands.json",
            "--index-output-path=/tmp/out.scip"
        ]);
        // No compile_commands.json under a bogus root → prerequisite Blocked (not a run error).
        assert!(
            manifest.prerequisite_blocked(Path::new("/no/such/repo/xyzzy")).is_some(),
            "missing compile_commands.json must report a prerequisite block"
        );
        // rust-analyzer has no such prerequisite.
        assert!(
            ToolManifest::for_tool(OracleTool::RustAnalyzer)
                .prerequisite_blocked(Path::new("/repo"))
                .is_none()
        );
    }

    #[test]
    fn scip_command_targets_output_path() {
        let manifest = ToolManifest::for_tool(OracleTool::RustAnalyzer);
        let cmd = manifest.scip_command(Path::new("/repo"), Path::new("/tmp/out.scip"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "rust-analyzer");
        assert_eq!(args, vec!["scip", "/repo", "--output", "/tmp/out.scip"]);
    }

    #[test]
    fn scip_python_indexes_a_cwd_with_a_project_name() {
        // scip-python's invocation: `scip-python index --project-name <root-basename> --cwd <root>
        // --output <abs>`. The project name (the root's dir name) is what gives in-corpus symbols
        // a non-empty moniker package, and `--cwd` is where it resolves the installed deps. No
        // compile_commands.json prerequisite (the venv install is the corpus `prepare` step's job).
        let manifest = ToolManifest::for_tool(OracleTool::ScipPython);
        assert_eq!(manifest.program, "scip-python");
        assert_eq!(manifest.languages, &["python"]);
        let cmd = manifest.scip_command(Path::new("/work/requests"), Path::new("/tmp/out.scip"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec![
            "index",
            "--project-name",
            "requests",
            // Pinned constant version (Codex #176): keeps monikers stable across commits.
            "--project-version",
            "_",
            "--cwd",
            "/work/requests",
            "--output",
            "/tmp/out.scip",
        ]);
        assert!(manifest.prerequisite_blocked(Path::new("/no/such/repo/xyzzy")).is_none());
    }

    #[test]
    fn scip_typescript_indexes_a_cwd() {
        // scip-typescript's invocation: `scip-typescript index --cwd <root> --output <abs>`. No
        // `--project-name` / `--project-version` (package.json supplies both; the version is
        // normalized downstream at moniker-write time). NO `--infer-tsconfig` — that flag writes a
        // tsconfig into the source tree, so a missing tsconfig is a prerequisite Block instead.
        let manifest = ToolManifest::for_tool(OracleTool::ScipTypescript);
        assert_eq!(manifest.program, "scip-typescript");
        assert_eq!(manifest.languages, &["typescript"]);
        let cmd = manifest.scip_command(Path::new("/work/ky"), Path::new("/tmp/out.scip"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["index", "--cwd", "/work/ky", "--output", "/tmp/out.scip"]);
        assert!(!args.iter().any(|a| a == "--infer-tsconfig"), "must not dirty the source tree");
    }

    #[test]
    fn scip_typescript_requires_a_tsconfig() {
        // A checkout without a tsconfig.json is Blocked (install-hint UX), not silently run with
        // `--infer-tsconfig` writing one into the tree — the read-only-on-source contract.
        let manifest = ToolManifest::for_tool(OracleTool::ScipTypescript);
        assert!(manifest.prerequisite_blocked(Path::new("/no/such/repo/xyzzy")).is_some());
        let dir = std::env::temp_dir().join("rag_rat_ts_prereq_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        assert!(manifest.prerequisite_blocked(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
