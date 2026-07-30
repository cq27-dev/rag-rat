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

use rag_rat_base::language::Language;
use serde::Serialize;

use super::OracleTool;
use super::backend::CheckoutScope;

/// The static registry entry for one oracle backend.
#[derive(Debug, Clone, Copy)]
pub struct ToolManifest {
    /// The persisted tool id (`OracleTool::as_db_str`).
    pub tool: OracleTool,
    /// The executable to invoke (looked up on `PATH`).
    pub program: &'static str,
    /// Languages this backend resolves, for status/diagnostics and for gating the auto-run loop.
    ///
    /// The SAME encoding `LiveBackend::languages` and `ResolvedTarget.language` use. It was once a
    /// list of lowercase registry tokens, which meant every consumer round-tripped through
    /// `Language::as_str()` and a test had to reconcile the two spellings.
    pub languages: &'static [Language],
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
                languages: &[Language::Rust],
                install_hint: "rust-analyzer not found on PATH. Install it (e.g. `rustup \
                               component add rust-analyzer`) or pass a pre-built index with \
                               `--scip <path>`.",
            },
            OracleTool::ScipClang => ToolManifest {
                tool,
                program: "scip-clang",
                languages: &[Language::C, Language::Cpp],
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
                languages: &[Language::Python],
                install_hint: "scip-python not found on PATH. Install it (e.g. `npm install -g \
                               @sourcegraph/scip-python`) AND install the project's dependencies \
                               (e.g. into a virtualenv) so imports resolve, or pass a pre-built \
                               index with `--scip <path>`.",
            },
            OracleTool::ScipTypescript => ToolManifest {
                tool,
                program: "scip-typescript",
                languages: &[Language::TypeScript],
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
                languages: &[Language::Kotlin],
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
                languages: &[Language::Rust],
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
                languages: &[Language::TypeScript],
                install_hint: "typescript-language-server not found on PATH. Install it (e.g. \
                               `npm install -g typescript-language-server typescript`) so the \
                               live oracle (`[oracle.live] enabled`) can spawn it as a language \
                               server.",
            },
            // The live clangd client (#536). `--background-index` is clangd's own default, PINNED
            // here because it is load-bearing: it is what resolves a call across translation
            // units, and with it off clangd answers with the header declaration instead. It also
            // makes clangd persist an index into `$CHECKOUT/.cache/clangd/` — no flag or
            // environment variable relocates that, so the write is accepted, and that one path
            // (`.cache/clangd`, not `.cache` as a whole) is floored out of the discovery walk so a
            // large, entirely machine-written index tree is never indexed as first-party code.
            OracleTool::ClangdLsp => ToolManifest {
                tool,
                program: "clangd",
                languages: &[Language::C, Language::Cpp],
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
    pub fn probe_runnable_in(&self, checkout: &CheckoutScope<'_>) -> ToolAvailability {
        let availability = self.probe_in(checkout.root());
        if !availability.is_available() {
            // Already blocked, and the prerequisite is not evaluated at all — checking it would
            // only be discarded, and for the live TypeScript backend it is a whole-checkout
            // project search. `oracle status` with no `--tool` probes every backend, so an
            // eagerly-evaluated prerequisite would walk a large checkout once per absent tool.
            return availability;
        }
        match self.prerequisite_blocked(checkout) {
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
    /// Whether a versioned binary can actually emit a SCIP index — declared per backend as
    /// [`crate::backend::spec::ScipCapability`]. A tool with no batch declaration is never asked to
    /// emit one, so it trivially can.
    fn can_emit_scip(&self) -> bool {
        crate::backend::BatchSpec::for_tool(self.tool)
            .is_none_or(|spec| spec.capability.holds_for(self.program))
    }

    /// A tool-specific prerequisite that must hold before a tool-driven run, beyond the binary
    /// being installed. `None` means ready. `scip-clang` needs a `compile_commands.json`
    /// compilation database at the checkout root; without it the run can't proceed, so it is
    /// reported as `Blocked` (install-hint UX, exit 0) rather than a subprocess error. The
    /// pre-built `--scip` path never reaches here.
    pub fn prerequisite_blocked(&self, checkout: &CheckoutScope<'_>) -> Option<String> {
        self.prerequisite_blocked_with(checkout, None)
    }

    /// As [`Self::prerequisite_blocked`], but reusing a project layout the caller already
    /// resolved. A live spawn resolves one to build the server's argv, and resolving a second
    /// here would both double the checkout walk under the repository write lock and let the gate
    /// and the argv observe different layouts if a database changed between the two scans.
    pub fn prerequisite_blocked_with(
        &self,
        checkout: &CheckoutScope<'_>,
        layout: Option<&super::backend::ProjectLayout>,
    ) -> Option<String> {
        let root = checkout.root();
        match self.tool {
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
            // project here whose load this server would report?" — so they share the check.
            // The HINT is per backend: the shared sentence states the gate, and the marker's own
            // detail names the measured symptom and the command that fixes it, which differ per
            // server. `checkout_can_signal_readiness` is the same search the warm-up uses, so the
            // gate and the warm-up cannot disagree.
            OracleTool::ClangdLsp | OracleTool::TsLsp => {
                // `None` from this function means READY, so a missing registry entry must NOT
                // short-circuit with `?`: a progress-signalled tool with no `LiveBackend` would
                // then report the checkout ready and the driver would spawn a server it has no
                // argv, readiness policy, or language set for. Absent means blocked, and says so.
                let Some(backend) = super::backend::LiveBackend::for_tool(self.tool) else {
                    return Some(format!(
                        "{} is registered as a live backend but has no `LiveBackend` entry, so \
                         the oracle has no argv, readiness policy, or language set for it. This \
                         is a registry gap, not a checkout problem.",
                        self.tool.as_db_str(),
                    ));
                };
                let resolved;
                let layout = match layout {
                    Some(layout) => layout,
                    None => {
                        resolved = backend.resolve_layout(checkout);
                        &resolved
                    },
                };
                // A progress-signalled backend that declared no marker could never signal either,
                // so it still blocks — with the generic wording, since there is no file to name.
                // Every name that would satisfy the marker, so an operator is not sent to create
                // the one spelling the backend happened to list first when another would do.
                //
                // A marker declaring NO name falls back to the generic wording, like a backend
                // with no marker at all: joining an empty list renders "found no  project under",
                // which names nothing while looking like it does. Every other reader of an empty
                // declaration already fails closed; this is the one that has to say so in prose.
                let (marker, detail) = backend.project_model.map_or(
                    (hint_marker_names(&[]), "Add one to enable it."),
                    |model| {
                        let names = model.files();
                        let detail = if names.is_empty() {
                            "Add one to enable it."
                        } else {
                            model.hint_detail
                        };
                        (hint_marker_names(names), detail)
                    },
                );
                if backend.checkout_can_signal_readiness(checkout, layout) {
                    return None;
                }
                // THIS is where a checkout whose database governs nothing it indexes is reported.
                //
                // Such a checkout blocks here — a database that describes no indexed file counts
                // for neither trust level, so no document can warm the session — which means the
                // block happens BEFORE any session exists. A message routed through the pass report
                // could therefore never reach the case it was written for. The generic wording is
                // also actively wrong here: it tells an operator who has a compilation database
                // that none was found, and sends them to `bear -- make`.
                if layout.has_database_governing_nothing_indexed() {
                    return Some(format!(
                        "the live {} oracle found a {} under {}, but it names no file this \
                         checkout indexes — so it is not used. Forcing it on first-party sources \
                         would analyse them under another project's defines and include paths, \
                         which resolves calls to the wrong definition. Regenerate the database so \
                         it covers the indexed sources, or bind the tree it does describe in \
                         `[target_bindings]` if that tree is meant to be indexed.",
                        backend.display_name,
                        marker,
                        root.display(),
                    ));
                }
                Some(format!(
                    "the live {} oracle found no {} project under {} — {} only reports \
                     project-load progress for a real project, and that signal is what tells the \
                     oracle its answers are trustworthy. {}",
                    backend.display_name,
                    marker,
                    root.display(),
                    self.program,
                    detail,
                ))
            },
            // Every other tool's prerequisite is the root-level marker question.
            _ => self.batch_prerequisite_blocked(root),
        }
    }

    /// The BATCH prerequisite: a marker file at the checkout root.
    ///
    /// Answerable from the root alone — no corpus, no checkout ceiling — which is what lets
    /// [`crate::produce_scip_with_tool`] ask it without building a [`CheckoutScope`] for a question
    /// that has nothing to do with one. The live backends' prerequisite is a question about the
    /// whole checkout instead; see [`Self::prerequisite_blocked_with`].
    pub fn batch_prerequisite_blocked(&self, root: &Path) -> Option<String> {
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
            // The progress-signalled live backends gate on "is there a project ANYWHERE in this
            // checkout whose load the server would report", which no root-level file test can
            // answer. That gate lives in `prerequisite_blocked_with`, which has the scope it needs.
            OracleTool::ClangdLsp | OracleTool::TsLsp => None,
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

/// How a project marker's names read in the prerequisite hint: every spelling that would satisfy
/// it, joined, so an operator is not sent to create the one the backend happened to list first
/// when another would do.
///
/// An empty list renders as the generic word rather than as nothing. Joining it would produce
/// "found no  project under", which names no file while looking like it does — and every other
/// reader of an empty declaration already fails closed.
pub(crate) fn hint_marker_names(files: &[&str]) -> String {
    if files.is_empty() { "project".to_string() } else { files.join(" or ") }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            languages: &[Language::C],
            install_hint: "install hint",
        };
        assert!(
            installed_but_unprepared.probe_in(&dir).is_available(),
            "the stand-in must be installed, or this tests the wrong branch",
        );
        match installed_but_unprepared
            .probe_runnable_in(&crate::test_support::every_path_scope(&dir))
        {
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
        assert!(
            installed_but_unprepared
                .probe_runnable_in(&crate::test_support::every_path_scope(&dir))
                .is_available()
        );
    }

    #[test]
    fn a_runnable_probe_keeps_the_install_hint_when_the_binary_is_absent() {
        // An absent binary is the more actionable half, and a prerequisite is moot until the tool
        // exists — so the missing-tool hint must survive, not be replaced by a prerequisite one.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("probe-runnable-absent");
        let absent = ToolManifest {
            tool: OracleTool::ScipClang,
            program: "rag-rat-no-such-tool-xyzzy",
            languages: &[Language::C],
            install_hint: "install scip-clang",
        };
        match absent.probe_runnable_in(&crate::test_support::every_path_scope(&dir)) {
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
        assert_eq!(manifest.languages, [Language::C, Language::Cpp], "one server, both dialects");

        // The compilation database is what clangd builds that index from — the same file the
        // batch scip-clang backend requires, for its own reasons.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-lsp-prereq");
        let blocked = manifest
            .prerequisite_blocked(&crate::test_support::every_path_scope(&dir))
            .expect("no compdb ⇒ Blocked");
        // The two progress-signalled backends share the gate but not the hint: this one names
        // clangd's marker, clangd's measured symptom, and the command that produces a database.
        // Nothing here may read as the TypeScript backend's advice.
        assert!(blocked.contains("the live C/C++ oracle"), "{blocked}");
        assert!(blocked.contains("compile_commands.json"), "{blocked}");
        assert!(blocked.contains("header declaration"), "{blocked}");
        assert!(blocked.contains("bear -- make"), "{blocked}");
        assert!(!blocked.contains("tsconfig.json"), "{blocked}");
        assert!(!blocked.contains("import statement"), "{blocked}");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
        // A syntactically valid but EMPTY database describes no project: measured, clangd emits
        // no readiness cycle for one, so accepting it would report the backend runnable while it
        // could only ever sit in `Warming`.
        std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
        assert!(
            manifest.prerequisite_blocked(&crate::test_support::every_path_scope(&dir)).is_some(),
            "an empty compilation database is not a warmable project",
        );
        std::fs::write(
            dir.join("compile_commands.json"),
            r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#,
        )
        .unwrap();
        assert!(
            manifest.prerequisite_blocked(&crate::test_support::every_path_scope(&dir)).is_none()
        );
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
        let blocked = manifest
            .prerequisite_blocked(&crate::test_support::every_path_scope(&dir))
            .expect("no project ⇒ Blocked");
        // Its own marker, its own measured symptom, its own nudge — and none of clangd's, which
        // shares the gate and would otherwise be indistinguishable here.
        assert!(blocked.contains("the live TypeScript oracle"), "{blocked}");
        assert!(blocked.contains("tsconfig.json"), "{blocked}");
        assert!(blocked.contains("import statement instead of the definition"), "{blocked}");
        assert!(blocked.contains("most TypeScript projects ship one"), "{blocked}");
        assert!(!blocked.contains("compile_commands.json"), "{blocked}");
        assert!(!blocked.contains("header declaration"), "{blocked}");
        // A config need not sit at the ROOT, and it must have a file to open: the progress cycle
        // reports a PROJECT load, so an empty project is nothing to warm on.
        std::fs::create_dir_all(dir.join("packages/app")).unwrap();
        std::fs::write(dir.join("packages/app/tsconfig.json"), "{}").unwrap();
        assert!(
            manifest.prerequisite_blocked(&crate::test_support::every_path_scope(&dir)).is_some(),
            "a project with no TypeScript file cannot warm the server",
        );
        std::fs::write(dir.join("packages/app/main.ts"), "export function x() {}\n").unwrap();
        assert!(
            manifest.prerequisite_blocked(&crate::test_support::every_path_scope(&dir)).is_none(),
            "a nested project with a file satisfies the gate",
        );
        // The Rust live backend has no such gate: its signal works for any checkout.
        assert!(
            ToolManifest::for_tool(OracleTool::RaLsp)
                .prerequisite_blocked(&crate::test_support::every_path_scope(&dir))
                .is_none()
        );
    }
}
