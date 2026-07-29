//! What a BATCH backend declares: how to invoke it, how to prove it can emit SCIP, and the two
//! reader facts its `.scip` output implies.
//!
//! The live half of the registry is [`super::registry::LiveBackend`]. Keeping the two kinds'
//! declarations in separate records is what makes their impossible states unrepresentable. A batch
//! backend has no stdio argv; a live backend has no whole-checkout invocation. While both lived on
//! one struct, each carried the other's questions as a lie: `live_args: &[]` on five batch entries,
//! an `unreachable!` arm in `scip_command` for the three live tools, and vacuous "exists only for
//! exhaustiveness" arms in the capability check and the position-encoding default.
//!
//! `BatchSpec::for_tool` returning `None` IS the statement "this tool is not a batch backend", and
//! it is the one place that fact is declared — [`crate::OracleTool::batch_capable`] reads it.

use std::path::Path;
use std::process::Command;

use crate::OracleTool;

/// Everything a batch driver needs from a backend, and nothing a live driver would ask for.
pub(crate) struct BatchSpec {
    /// Builds the whole-checkout invocation that writes a `.scip` to the output path.
    ///
    /// A function per backend rather than a `match` in the driver: the invocations have nothing in
    /// common beyond the program name — a subcommand here, a cwd there, flags that are pinned for
    /// reasons documented at each one — and a `match` over them is what needed an `unreachable!`
    /// arm for the tools that have no invocation at all.
    pub(crate) command: fn(program: &str, root: &Path, output: &Path) -> Command,
    /// How to prove a versioned binary can actually emit SCIP.
    pub(crate) capability: ScipCapability,
    /// Whether a NON-ZERO exit reflects source diagnostics rather than an indexing failure — i.e.
    /// whether the tool can exit non-zero while still having written a complete, valid index. Only
    /// such tools get the diagnostic-exit tolerance; for every other backend a non-zero exit is a
    /// genuine failure and bails, so a crashed or killed indexer is never read as success.
    pub(crate) exit_code_reflects_diagnostics: bool,
    /// The encoding to assume when this tool's `.scip` leaves `position_encoding` unset.
    ///
    /// scip-typescript and scip-java (JVM/semanticdb) both emit UTF-16 columns with the field
    /// unset — empirically confirmed: a token after an astral character lands at the UTF-16 count.
    /// Reading them as the UTF-32 fallback mis-converts past astral characters.
    pub(crate) assumed_position_encoding: ::scip::types::PositionEncoding,
}

/// How to prove a versioned binary can emit a SCIP index. Distinct from "is it installed", which
/// the version probe already answered: a stripped build can be present and unable to index.
pub(crate) enum ScipCapability {
    /// The binary IS the emitter — it has no indexing subcommand, so a successful `--version`
    /// (already detected) is the capability signal.
    VersionSuffices,
    /// An indexing subcommand must exist: `<program> <subcommand> --help` exiting 0 is the check.
    SubcommandHelpSucceeds(&'static str),
}

impl ScipCapability {
    pub(crate) fn holds_for(&self, program: &str) -> bool {
        match self {
            Self::VersionSuffices => true,
            Self::SubcommandHelpSucceeds(subcommand) => Command::new(program)
                .arg(subcommand)
                .arg("--help")
                .output()
                .is_ok_and(|output| output.status.success()),
        }
    }
}

impl BatchSpec {
    /// The batch declaration for `tool`, or `None` when it is not a batch backend at all.
    pub(crate) fn for_tool(tool: OracleTool) -> Option<Self> {
        use ::scip::types::PositionEncoding::{
            UTF16CodeUnitOffsetFromLineStart as Utf16, UnspecifiedPositionEncoding as Unspecified,
        };
        let spec = match tool {
            OracleTool::RustAnalyzer => Self {
                command: rust_analyzer_command,
                capability: ScipCapability::SubcommandHelpSucceeds("scip"),
                exit_code_reflects_diagnostics: false,
                assumed_position_encoding: Unspecified,
            },
            OracleTool::ScipClang => Self {
                command: scip_clang_command,
                capability: ScipCapability::VersionSuffices,
                exit_code_reflects_diagnostics: false,
                assumed_position_encoding: Unspecified,
            },
            OracleTool::ScipPython => Self {
                command: scip_python_command,
                capability: ScipCapability::SubcommandHelpSucceeds("index"),
                // scip-python exits non-zero on unresolved imports while still writing a usable
                // index, which is the ordinary state of a checkout whose deps are partly installed.
                exit_code_reflects_diagnostics: true,
                assumed_position_encoding: Unspecified,
            },
            OracleTool::ScipTypescript => Self {
                command: scip_typescript_command,
                capability: ScipCapability::SubcommandHelpSucceeds("index"),
                exit_code_reflects_diagnostics: false,
                assumed_position_encoding: Utf16,
            },
            OracleTool::ScipJava => Self {
                command: scip_java_command,
                capability: ScipCapability::SubcommandHelpSucceeds("index"),
                exit_code_reflects_diagnostics: false,
                assumed_position_encoding: Utf16,
            },
            // The live backends drive their program as an LSP server and never produce a `.scip`.
            // `None` is not a gap to paper over with defaults — it is the declaration.
            OracleTool::RaLsp | OracleTool::TsLsp | OracleTool::ClangdLsp => return None,
        };
        Some(spec)
    }
}

/// `rust-analyzer scip <root> --output <path>` writes the index to a deterministic path so the
/// caller (a temp file) can consume it.
fn rust_analyzer_command(program: &str, root: &Path, output: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.arg("scip").arg(root).arg("--output").arg(output);
    cmd
}

/// scip-clang consumes the compilation database, not a source root, and emits the index directly
/// (no subcommand). cwd = root so the compdb's relative paths resolve, pointed at
/// `root/compile_commands.json` (prerequisite-checked).
fn scip_clang_command(program: &str, root: &Path, output: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(root)
        .arg(format!("--compdb-path={}", root.join("compile_commands.json").display()))
        .arg(format!("--index-output-path={}", output.display()));
    cmd
}

/// scip-python indexes a working directory (not a source-root argument) via its `index` subcommand.
/// `--cwd <root>` is where it resolves the project and its installed deps; `--project-name` (the
/// root's directory name) becomes the package component of in-corpus monikers, so a non-empty name
/// is what lets `count_symbols_with_moniker` see them.
///
/// `--project-version _` is PINNED: scip-python otherwise defaults the version to the checkout's
/// git revision, which is embedded in every SCIP symbol string, so every commit would churn all
/// Python monikers — breaking moniker-anchored memory relocation, which resolves by exact moniker
/// per tool. A constant version keeps a symbol's moniker stable across commits, and sidesteps
/// scip-python's crash on a non-git checkout where the git-rev default is undefined. `--output` is
/// absolute, so it is unaffected by `--cwd`.
fn scip_python_command(program: &str, root: &Path, output: &Path) -> Command {
    let project_name = root.file_name().and_then(|name| name.to_str()).unwrap_or("project");
    let mut cmd = Command::new(program);
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
}

/// scip-typescript indexes the working dir via its `index` subcommand, reading the project's
/// `tsconfig.json` (prerequisite-checked — `--infer-tsconfig` is deliberately NOT passed, because
/// it WRITES a tsconfig into the source tree, breaking read-only-on-source).
///
/// No `--project-name` / `--project-version`: the package name and version come from
/// `package.json`. That version is embedded in every local moniker and has no CLI override, so it
/// is normalized downstream at moniker-write time (`scip::stabilize_moniker_version`), not here.
/// `--output` is absolute, unaffected by `--cwd`.
fn scip_typescript_command(program: &str, root: &Path, output: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.arg("index").arg("--cwd").arg(root).arg("--output").arg(output);
    cmd
}

/// scip-java indexes THROUGH the build (running it with the semanticdb-kotlinc plugin), so cwd =
/// root. `--build-tool Gradle` is PINNED: the prerequisite only proved Gradle is present, but a
/// checkout that ALSO carries a `pom.xml` (a Maven→Gradle migration, a parent Maven descriptor)
/// makes scip-java's auto-detection see multiple build tools and abort with "Multiple build tools
/// detected" instead of indexing. Forcing Gradle — this backend's only supported Kotlin path —
/// skips that ambiguity; the value is matched case-insensitively upstream.
///
/// No `--project-version` need, unlike scip-python: scip-java emits `.` placeholders for the local
/// project's package and version regardless of the build's `group`/`version`, so monikers are
/// already commit-stable. `--output` is absolute.
fn scip_java_command(program: &str, root: &Path, output: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(root)
        .arg("index")
        .arg("--build-tool")
        .arg("Gradle")
        .arg("--output")
        .arg(output);
    cmd
}
