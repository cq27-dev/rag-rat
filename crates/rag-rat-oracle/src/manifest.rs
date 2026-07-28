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
    /// Arguments the LIVE backends spawn the program with. Language servers disagree on how the
    /// stdio transport is selected — `rust-analyzer` speaks LSP on stdio by default, while
    /// `typescript-language-server` prints usage and exits without `--stdio` — so the argv belongs
    /// to the registry entry, not the client. Empty for batch tools, which build their whole
    /// invocation in [`ToolManifest::scip_command`].
    pub live_args: &'static [&'static str],
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
                live_args: &[],
                languages: &["rust"],
                install_hint: "rust-analyzer not found on PATH. Install it (e.g. `rustup \
                               component add rust-analyzer`) or pass a pre-built index with \
                               `--scip <path>`.",
            },
            OracleTool::ScipClang => ToolManifest {
                tool,
                program: "scip-clang",
                live_args: &[],
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
                live_args: &[],
                languages: &["python"],
                install_hint: "scip-python not found on PATH. Install it (e.g. `npm install -g \
                               @sourcegraph/scip-python`) AND install the project's dependencies \
                               (e.g. into a virtualenv) so imports resolve, or pass a pre-built \
                               index with `--scip <path>`.",
            },
            OracleTool::ScipTypescript => ToolManifest {
                tool,
                program: "scip-typescript",
                live_args: &[],
                languages: &["typescript"],
                install_hint: "scip-typescript not found on PATH. Install it (e.g. `npm install \
                               -g @sourcegraph/scip-typescript`) AND install the project's \
                               dependencies (`npm install`) so cross-package references resolve, \
                               or pass a pre-built index with `--scip <path>`.",
            },
            // scip-java is the SemanticDB-based JVM indexer; we drive it for KOTLIN (it indexes
            // through the project's Gradle build via the semanticdb-kotlinc compiler plugin —
            // scip-java's auto-indexer supports Kotlin for Gradle only, not Maven).
            OracleTool::ScipJava => ToolManifest {
                tool,
                program: "scip-java",
                live_args: &[],
                languages: &["kotlin"],
                install_hint: "scip-java not found on PATH. Install it (e.g. `cs install \
                               --contrib scip-java`, needs a JVM) — it indexes Kotlin through the \
                               project's Gradle build (Maven Kotlin is unsupported), so the build \
                               must succeed; or pass a pre-built index with `--scip <path>`.",
            },
            // The live rust-analyzer LSP client (#534): same binary as the batch Rust backend,
            // driven as a resident language server by the watcher (`[oracle.live]`), never as a
            // `.scip` producer — `batch_capable` gates it out of every batch driver.
            OracleTool::RaLsp => ToolManifest {
                tool,
                program: "rust-analyzer",
                live_args: &[],
                languages: &["rust"],
                install_hint: "rust-analyzer not found on PATH. Install it (e.g. `rustup \
                               component add rust-analyzer`) so the live oracle (`[oracle.live] \
                               enabled`) can spawn it as a language server.",
            },
            // The live typescript-language-server client (#536). `--stdio` is REQUIRED: without a
            // transport flag the program prints usage and exits, so the spawn would fail with an
            // opaque EOF instead of a session.
            OracleTool::TsLsp => ToolManifest {
                tool,
                program: "typescript-language-server",
                live_args: &["--stdio"],
                languages: &["typescript"],
                install_hint: "typescript-language-server not found on PATH. Install it (e.g. \
                               `npm install -g typescript-language-server typescript`) so the \
                               live oracle (`[oracle.live] enabled`) can spawn it as a language \
                               server.",
            },
            // The live clangd client (#536). `--background-index` is clangd's own default, PINNED
            // here because it is load-bearing: it is what resolves a call across translation
            // units, and with it off clangd answers with the header declaration instead. It also
            // makes clangd persist an index into `$CHECKOUT/.cache/clangd/` — no flag or
            // environment variable relocates that, so the write is accepted (and `.cache` is
            // floored out of indexing so it cannot feed the watcher back into itself).
            OracleTool::ClangdLsp => ToolManifest {
                tool,
                program: "clangd",
                live_args: &["--background-index"],
                languages: &["c", "cpp"],
                install_hint: "clangd not found on PATH. Install it (e.g. `apt install clangd`, \
                               or a release from github.com/clangd/clangd) so the live oracle \
                               (`[oracle.live] enabled`) can spawn it as a language server.",
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
        self.probe_with_cwd(None)
    }

    /// Probe from `cwd`. The live rust-analyzer backend uses this so rustup directory overrides in
    /// the indexed checkout select the same toolchain for `--version` and the later LSP process.
    pub fn probe_in(&self, cwd: &Path) -> ToolAvailability {
        self.probe_with_cwd(Some(cwd))
    }

    /// Probe from `root` AND fold in this tool's checkout prerequisite — "can this backend run
    /// here?", the question status surfaces should ask.
    ///
    /// [`Self::probe_in`] answers only "is the binary installed", which is the more actionable
    /// half when it is missing but a misleading answer when it is present and the checkout still
    /// cannot support it: `scip-clang` without a compile_commands.json, `scip-java` without a
    /// Gradle build, the live TypeScript client without a tsconfig project. Reporting those as
    /// `Available` says nothing is wrong about a backend that can never produce a verdict. An
    /// already-`Blocked` probe is returned untouched — an absent binary is the more actionable
    /// hint, and a prerequisite is moot until the tool exists.
    pub fn probe_runnable_in(&self, root: &Path) -> ToolAvailability {
        let availability = self.probe_in(root);
        if !availability.is_available() {
            // Already blocked, and the prerequisite is not evaluated at all — checking it would
            // only be discarded, and for the live TypeScript backend it is a whole-checkout
            // project search. `oracle status` with no `--tool` probes every backend, so an
            // eagerly-evaluated prerequisite would walk a large checkout once per absent tool.
            return availability;
        }
        match self.prerequisite_blocked(root) {
            Some(hint) => ToolAvailability::Blocked {
                tool: self.tool.as_db_str().to_string(),
                program: self.program.to_string(),
                hint,
            },
            None => availability,
        }
    }

    fn probe_with_cwd(&self, cwd: Option<&Path>) -> ToolAvailability {
        match detect_version_in(self.program, cwd) {
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
            // scip-python/typescript/java emit via an `index` subcommand; `index --help` exiting 0
            // is the analog of rust-analyzer's `scip --help` capability check.
            OracleTool::ScipPython | OracleTool::ScipTypescript | OracleTool::ScipJava =>
                Command::new(self.program)
                    .arg("index")
                    .arg("--help")
                    .output()
                    .is_ok_and(|output| output.status.success()),
            // The live clients drive their program as an LSP server (no `.scip` subcommand
            // needed); a successful `--version` (already detected by `probe`) is the capability
            // signal.
            OracleTool::RaLsp | OracleTool::TsLsp | OracleTool::ClangdLsp => true,
        }
    }

    /// A tool-specific prerequisite that must hold before a tool-driven run, beyond the binary
    /// being installed. `None` means ready. `scip-clang` needs a `compile_commands.json`
    /// compilation database at the checkout root; without it the run can't proceed, so it is
    /// reported as `Blocked` (install-hint UX, exit 0) rather than a subprocess error. The
    /// pre-built `--scip` path never reaches here.
    pub fn prerequisite_blocked(&self, root: &Path) -> Option<String> {
        self.prerequisite_blocked_with(root, None)
    }

    /// As [`Self::prerequisite_blocked`], but reusing a project layout the caller already
    /// resolved. A live spawn resolves one to build the server's argv, and resolving a second
    /// here would both double the checkout walk under the repository write lock and let the gate
    /// and the argv observe different layouts if a database changed between the two scans.
    pub fn prerequisite_blocked_with(
        &self,
        root: &Path,
        layout: Option<&super::backend::ProjectLayout>,
    ) -> Option<String> {
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
            // scip-java indexes THROUGH the build, so it needs a recognizable build at the root.
            // This backend advertises Kotlin only, and scip-java's automatic indexer supports
            // Kotlin for GRADLE only — Maven Kotlin (`kotlin-maven-plugin`) is unsupported upstream
            // (https://sourcegraph.github.io/scip-java/docs/getting-started.html#supported-build-tools).
            // So a Maven-only (`pom.xml`) Kotlin checkout must report Blocked, not run scip-java
            // and fail — accepting `pom.xml` would turn a clean block into a failed run
            // + background retries (Codex on #193). The JVM analog of scip-clang's
            // compile_commands.json gate.
            OracleTool::ScipJava => (!has_gradle_build(root)).then(|| {
                format!(
                    "scip-java requires a Gradle build at {} (build.gradle, build.gradle.kts, \
                     settings.gradle, or gradlew) — it indexes Kotlin through the Gradle build \
                     (Maven Kotlin is unsupported by scip-java's auto-indexer); or pass a \
                     pre-built index with `--scip <path>`.",
                    root.display()
                )
            }),
            // The live rust-analyzer client has no checkout prerequisite beyond the binary itself:
            // `experimental/serverStatus` reports quiescence for any checkout.
            OracleTool::RaLsp => None,
            // The live TS client needs a tsconfig project SOMEWHERE in the checkout, for a
            // different reason than the batch backend's root-level requirement (which is about
            // `--infer-tsconfig` writing a tsconfig into the source tree).
            //
            // typescript-language-server has no quiescence notification. The only warm-up signal
            // it emits is the work-done progress cycle bracketing a TSCONFIG PROJECT's load;
            // opening a file that belongs to no project creates an inferred project silently, with
            // no progress at all. So a checkout with no tsconfig anywhere can never report ready
            // and the backend would sit in `Warming` forever — correct (the readiness policy still
            // refuses to ask), but silent. Block with a reason instead.
            //
            // The config does NOT have to be at the root: a monorepo whose projects live at
            // `packages/*/tsconfig.json` warms fine, because the FIRST project load is what the
            // cycle reports. Requiring a root config would wrongly disable those checkouts.
            // The live clangd client needs the SAME compilation database the batch backend does,
            // for an additional reason: it is what clangd builds its cross-translation-unit index
            // from. Without one a call resolves only to its header declaration, and clangd emits
            // no project-load progress at all — so the backend could never report ready.
            // Both progress-signalled live backends gate on the SAME question — "is there a
            // project here whose load this server would report?" — so they share the check, and
            // each names its own marker in the hint. `checkout_can_signal_readiness` is the same
            // search the warm-up uses, so the gate and the warm-up cannot disagree.
            OracleTool::ClangdLsp | OracleTool::TsLsp => {
                let backend = super::backend::LiveBackend::for_tool(self.tool)?;
                let resolved;
                let layout = match layout {
                    Some(layout) => layout,
                    None => {
                        resolved = backend.resolve_layout(root);
                        &resolved
                    },
                };
                (!backend.checkout_can_signal_readiness(root, layout)).then(|| {
                    format!(
                        "the live {} oracle found no {} project under {} — {} only reports \
                         project-load progress for a real project, and that signal is what tells \
                         the oracle its answers are trustworthy. Add one to enable it.",
                        self.languages.join("/"),
                        backend.project_marker.map_or("project", |marker| marker.file),
                        root.display(),
                        self.program,
                    )
                })
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
            // scip-java indexes through the build (running it with the semanticdb-kotlinc plugin),
            // so cwd = root. `--build-tool Gradle` is PINNED: the prerequisite only proved Gradle
            // is present, but a checkout that ALSO carries a `pom.xml` (Maven→Gradle
            // migration, parent Maven descriptor) makes scip-java's auto-detection see
            // multiple build tools and abort with "Multiple build tools detected"
            // instead of indexing. Forcing Gradle (this backend's only supported Kotlin
            // path) skips that ambiguity (Codex on #193); the value is matched
            // case-insensitively upstream. No `--project-version` need (unlike
            // scip-python): scip-java emits `.` placeholders for the local project's
            // package/version regardless of the build's `group`/`version`, so monikers
            // are already commit-stable. `--output` is absolute.
            OracleTool::ScipJava => {
                let mut cmd = Command::new(self.program);
                cmd.current_dir(root)
                    .arg("index")
                    .arg("--build-tool")
                    .arg("Gradle")
                    .arg("--output")
                    .arg(output);
                cmd
            },
            // Unreachable: every batch driver gates live-only tools out BEFORE building a command
            // (`produce_scip_with_tool` returns `Blocked`, the auto-run loop and the wizard filter
            // on `batch_capable`). A live tool has no whole-checkout index invocation.
            tool @ (OracleTool::RaLsp | OracleTool::TsLsp | OracleTool::ClangdLsp) => unreachable!(
                "{} is a live oracle backend with no scip_command — the caller must gate on \
                 OracleTool::batch_capable()",
                tool.as_db_str()
            ),
        }
    }
}

/// Whether `root` has a Gradle build that scip-java can index Kotlin through. The sentinel set
/// mirrors scip-java's own `GradleBuildTool.usedInCurrentDirectory` EXACTLY (v0.12.3:
/// `settings.gradle`, `gradlew`, `build.gradle`, `build.gradle.kts`) so this gate agrees with what
/// the tool will actually detect (Codex on #193): note scip-java does NOT recognize
/// `settings.gradle.kts`, and DOES recognize the `gradlew` wrapper — accepting the former or
/// omitting the latter would let a checkout pass the gate then fail with "no Gradle tool", or block
/// a wrapper-only root scip-java could index. Maven is deliberately excluded: scip-java's automatic
/// indexer supports Kotlin only for Gradle, and this backend advertises Kotlin only — a Maven
/// (`pom.xml`) Kotlin checkout should report Blocked, not run.
fn has_gradle_build(root: &Path) -> bool {
    ["settings.gradle", "gradlew", "build.gradle", "build.gradle.kts"]
        .iter()
        .any(|name| root.join(name).exists())
}

/// Run `<program> --version` and return the first non-empty trimmed stdout line, or `None` when the
/// program is absent / not executable / exits non-zero. The version string is opaque (recorded as
/// `tool_version` for content-addressed staleness — a different version invalidates prior
/// verdicts).
fn detect_version_in(program: &str, cwd: Option<&Path>) -> Option<String> {
    let mut command = Command::new(program);
    command.arg("--version");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().ok()?;
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
    fn live_backends_declare_their_stdio_argv_and_batch_tools_declare_none() {
        // A live backend is spawned as `program live_args…`; batch tools build their whole
        // invocation in `scip_command`, so a stray `live_args` there would be silently ignored
        // and mislead the next reader.
        let ts = ToolManifest::for_tool(OracleTool::TsLsp);
        assert_eq!(ts.program, "typescript-language-server");
        assert_eq!(
            ts.live_args,
            ["--stdio"],
            "without a transport flag the program prints usage and exits, so the spawn fails with \
             an opaque EOF instead of a session"
        );
        // rust-analyzer speaks LSP on stdio with no flag — the two backends genuinely differ.
        assert!(ToolManifest::for_tool(OracleTool::RaLsp).live_args.is_empty());
        for tool in OracleTool::ALL.iter().filter(|tool| tool.batch_capable()) {
            assert!(
                ToolManifest::for_tool(*tool).live_args.is_empty(),
                "{} is batch-only and must declare no live argv",
                tool.as_db_str()
            );
        }
    }

    #[test]
    fn a_runnable_probe_reports_an_unmet_prerequisite_instead_of_available() {
        // "installed" is not "can run here". A tool reported Available while its checkout
        // prerequisite is unmet says nothing is wrong about a backend that can never produce a
        // verdict — the worst of the three possible answers.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("probe-runnable");
        // Stand in for an installed binary whose checkout prerequisite is unmet: `cargo` reliably
        // versions, scip-clang's capability check is a no-op (it IS the emitter), and its gate
        // wants a compile_commands.json.
        let installed_but_unprepared = ToolManifest {
            tool: OracleTool::ScipClang,
            program: "cargo",
            live_args: &[],
            languages: &["c"],
            install_hint: "install hint",
        };
        assert!(
            installed_but_unprepared.probe_in(&dir).is_available(),
            "the stand-in must be installed, or this tests the wrong branch",
        );
        match installed_but_unprepared.probe_runnable_in(&dir) {
            ToolAvailability::Blocked { hint, tool, program } => {
                assert_eq!(tool, "scip-clang");
                assert_eq!(program, "cargo");
                assert!(
                    hint.contains("compile_commands.json"),
                    "the hint must name the fix, not repeat the install hint: {hint}",
                );
            },
            other => panic!("an unmet prerequisite must not read as {other:?}"),
        }
        // Satisfy the prerequisite and the same probe reports the tool as runnable.
        std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
        assert!(installed_but_unprepared.probe_runnable_in(&dir).is_available());
    }

    #[test]
    fn a_runnable_probe_keeps_the_install_hint_when_the_binary_is_absent() {
        // An absent binary is the more actionable half, and a prerequisite is moot until the tool
        // exists — so the missing-tool hint must survive, not be replaced by a prerequisite one.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("probe-runnable-absent");
        let absent = ToolManifest {
            tool: OracleTool::ScipClang,
            program: "rag-rat-no-such-tool-xyzzy",
            live_args: &[],
            languages: &["c"],
            install_hint: "install scip-clang",
        };
        match absent.probe_runnable_in(&dir) {
            ToolAvailability::Blocked { hint, .. } => assert_eq!(hint, "install scip-clang"),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn the_live_clangd_backend_pins_background_indexing_and_needs_a_compdb() {
        // `--background-index` is clangd's own default, pinned because it is load-bearing: it is
        // what resolves a call across translation units. With it off clangd answers with the
        // header declaration instead, and emits no project-load progress at all.
        let manifest = ToolManifest::for_tool(OracleTool::ClangdLsp);
        assert_eq!(manifest.program, "clangd");
        assert_eq!(manifest.live_args, ["--background-index"]);
        assert_eq!(manifest.languages, ["c", "cpp"], "one server, both dialects");

        // The compilation database is what clangd builds that index from — the same file the
        // batch scip-clang backend requires, for its own reasons.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-lsp-prereq");
        let blocked = manifest.prerequisite_blocked(&dir).expect("no compdb ⇒ Blocked");
        assert!(blocked.contains("compile_commands.json"), "{blocked}");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
        // A syntactically valid but EMPTY database describes no project: measured, clangd emits
        // no readiness cycle for one, so accepting it would report the backend runnable while it
        // could only ever sit in `Warming`.
        std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
        assert!(
            manifest.prerequisite_blocked(&dir).is_some(),
            "an empty compilation database is not a warmable project",
        );
        std::fs::write(
            dir.join("compile_commands.json"),
            r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#,
        )
        .unwrap();
        assert!(manifest.prerequisite_blocked(&dir).is_none());
    }

    #[test]
    fn the_live_typescript_backend_requires_a_warmable_tsconfig_project() {
        // Not the batch backend's source-tree reason: typescript-language-server only reports its
        // project-load progress for a real tsconfig project. With none anywhere it emits no signal
        // ever, so the backend could only sit in `Warming` — correct, but silent. Block with a
        // reason instead. The gate asks the same question the warm-up does ("is there a document
        // whose open would signal readiness?"), so the two cannot disagree.
        let manifest = ToolManifest::for_tool(OracleTool::TsLsp);
        let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-prereq");
        let blocked = manifest.prerequisite_blocked(&dir).expect("no project ⇒ Blocked");
        assert!(blocked.contains("tsconfig.json"), "{blocked}");
        // A config need not sit at the ROOT, and it must have a file to open: the progress cycle
        // reports a PROJECT load, so an empty project is nothing to warm on.
        std::fs::create_dir_all(dir.join("packages/app")).unwrap();
        std::fs::write(dir.join("packages/app/tsconfig.json"), "{}").unwrap();
        assert!(
            manifest.prerequisite_blocked(&dir).is_some(),
            "a project with no TypeScript file cannot warm the server",
        );
        std::fs::write(dir.join("packages/app/main.ts"), "export function x() {}\n").unwrap();
        assert!(
            manifest.prerequisite_blocked(&dir).is_none(),
            "a nested project with a file satisfies the gate",
        );
        // The Rust live backend has no such gate: its signal works for any checkout.
        assert!(ToolManifest::for_tool(OracleTool::RaLsp).prerequisite_blocked(&dir).is_none());
    }
}
