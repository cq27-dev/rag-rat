mod render;
mod run;
mod scan;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io};

use dialoguer::{Confirm, MultiSelect, Select};
use rag_rat_core::config::EmbeddingBackend;
use rag_rat_core::embedding_models::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, MODEL2VEC_MODEL_ID};
use rag_rat_core::index::ai::ReconcileOptions;
use rag_rat_core::index::ignore_rules::{IgnoreMatcher, is_virtualenv_dir};
use rag_rat_core::language::Language;
use rag_rat_core::{Config, IndexDatabase};
pub(crate) use render::*;
pub(crate) use run::*;
pub(crate) use scan::*;

use crate::{
    apply_embedding_runtime_env, git_paths, render_index_progress, render_reconcile_progress,
};

const DEFAULT_DATABASE: &str = ".rag-rat/index.sqlite";
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".rag-rat",
    ".direnv",
    ".next",
    ".turbo",
    ".venv",
    // Python virtualenv / dependency / cache trees — never project source. Skipping them at scan
    // time means their `.py` files never become dir candidates, so the "no default → promote the
    // largest" fallback can't select a `site-packages` tree (#167 review). NB: `virtualenv` is NOT
    // here — it's a real first-party package name; a virtualenv literally named `virtualenv/` is
    // caught by content (`pyvenv.cfg`) in `scan_dir` instead (#181).
    "venv",
    "site-packages",
    "__pycache__",
    ".tox",
    ".nox",
    "build",
    "dist",
    "node_modules",
    "target",
];

#[derive(Debug, Clone)]
struct InitOptions {
    yes: bool,
    dry_run: bool,
    force: bool,
    config_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct InitPlan {
    root_value: String,
    languages: Vec<Language>,
    bindings: BTreeMap<Language, Vec<PathBuf>>,
    backend: EmbeddingBackend,
    /// Whether to write `[oracle] auto_run = true` — the opt-in background refresh of
    /// compiler-grade (SCIP) importance ranking. Default false (matches `OracleConfig`'s
    /// default).
    oracle_auto_run: bool,
}

#[derive(Debug, Default)]
pub(crate) struct RepoScan {
    language_counts: BTreeMap<Language, usize>,
    dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
    direct_dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
    total_source_bytes: u64,
    /// The scan found a real Python virtualenv (a dir with a `pyvenv.cfg`) ANYWHERE the index
    /// would walk — not gitignored, not floored. The indexer floor can't cover a venv under an
    /// ambiguous name (`env`/`virtualenv`), so a `python = ["."]` walk WOULD index it; when
    /// one is present we must not auto-bind `.`. Gitignored / conventionally-floored venvs
    /// (`.venv`/`venv`) and first-party packages that merely share a venv-ish NAME (no
    /// `pyvenv.cfg`) are NOT recorded here — content detection, not the name, decides (#181
    /// review).
    has_python_virtualenv: bool,
    /// Full paths of ambiguous `.h` headers, held aside during the walk and assigned to a language
    /// only after the whole repo is seen: to **C++** if the repo has any C++ source
    /// (`.cpp`/`.cc`/…), else to **C**. Bare `Language::from_path` calls every `.h` C, which would
    /// bind a C++ library's header tree as `c` and parse it as C — see [`scan::assign_headers`].
    deferred_headers: Vec<PathBuf>,
}

impl InitOptions {
    fn from_args(args: &crate::cli::InitArgs, config_path: &str) -> Self {
        Self {
            yes: args.yes,
            dry_run: args.dry_run,
            force: args.force,
            config_path: PathBuf::from(config_path),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DirCandidate {
    path: PathBuf,
    count: usize,
    default: bool,
}

#[cfg(unix)]
struct TerminalResetGuard {
    fd: libc::c_int,
    handlers: Vec<(libc::c_int, libc::sighandler_t)>,
}

#[cfg(unix)]
impl TerminalResetGuard {
    fn install_if_interactive(interactive: bool) -> anyhow::Result<Option<Self>> {
        if !interactive {
            return Ok(None);
        }
        match Self::install() {
            Ok(guard) => Ok(Some(guard)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn install() -> io::Result<Self> {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        c_result(|| unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) })?;
        let termios = unsafe { termios.assume_init() };
        unsafe {
            std::ptr::addr_of_mut!(ORIGINAL_TERMIOS).write(std::mem::MaybeUninit::new(termios));
        }
        TERMINAL_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
        ORIGINAL_TERMIOS_SET.store(true, std::sync::atomic::Ordering::SeqCst);

        let handlers = install_signal_handlers()?;
        Ok(Self { fd: tty.into_raw_fd(), handlers })
    }
}

#[cfg(unix)]
impl Drop for TerminalResetGuard {
    fn drop(&mut self) {
        restore_terminal();
        for (signal, previous) in &self.handlers {
            unsafe {
                libc::signal(*signal, *previous);
            }
        }
        TERMINAL_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
        ORIGINAL_TERMIOS_SET.store(false, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(unix)]
static TERMINAL_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static ORIGINAL_TERMIOS_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(unix)]
static mut ORIGINAL_TERMIOS: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();

#[cfg(unix)]
fn install_signal_handlers() -> io::Result<Vec<(libc::c_int, libc::sighandler_t)>> {
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .map(|signal| {
            let previous = unsafe {
                libc::signal(signal, handle_terminal_signal as *const () as libc::sighandler_t)
            };
            if previous == libc::SIG_ERR {
                Err(io::Error::last_os_error())
            } else {
                Ok((signal, previous))
            }
        })
        .collect()
}

#[cfg(unix)]
extern "C" fn handle_terminal_signal(signal: libc::c_int) {
    restore_terminal();
    let reset = b"\x1b[0m\x1b[?25h\r\n";
    let fd = TERMINAL_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::write(fd, reset.as_ptr().cast(), reset.len());
        }
    }
    unsafe {
        libc::_exit(128 + signal);
    }
}

#[cfg(unix)]
fn restore_terminal() {
    if !ORIGINAL_TERMIOS_SET.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let fd = TERMINAL_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    unsafe {
        libc::tcsetattr(
            fd,
            libc::TCSANOW,
            std::ptr::addr_of!(ORIGINAL_TERMIOS).cast::<libc::termios>(),
        );
    }
}

#[cfg(unix)]
fn c_result<F: FnOnce() -> libc::c_int>(f: F) -> io::Result<()> {
    let status = f();
    if status == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(unix))]
struct TerminalResetGuard;

#[cfg(not(unix))]
impl TerminalResetGuard {
    fn install_if_interactive(_interactive: bool) -> anyhow::Result<Option<Self>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_config_uses_selected_language_bindings() {
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Rust, Language::TypeScript],
            bindings: BTreeMap::from([
                (Language::Rust, vec![PathBuf::from("crates/app/src")]),
                (Language::TypeScript, vec![PathBuf::from("web/src"), PathBuf::from("app/src")]),
            ]),
            backend: EmbeddingBackend::model2vec(),
            oracle_auto_run: false,
        };

        let text = render_config(&plan);

        assert!(text.contains("[index]"));
        assert!(text.contains("database = \".rag-rat/index.sqlite\""));
        assert!(text.contains("rust = [\"crates/app/src\"]"));
        assert!(text.contains("typescript = [\"web/src\", \"app/src\"]"));
        assert!(text.contains("[local_ai.embedding]"));
        // The selector is now the model_id (HF path), not an alias (#317).
        assert!(text.contains("model = \"minishlab/potion-retrieval-32M\""));
        // The oracle section is always rendered so the knob is discoverable; default is OFF.
        assert!(text.contains("[oracle]"));
        assert!(text.contains("auto_run = false"));
    }

    #[test]
    fn render_config_enables_oracle_auto_run_when_opted_in() {
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Rust],
            bindings: BTreeMap::from([(Language::Rust, vec![PathBuf::from("src")])]),
            backend: EmbeddingBackend::fast_embed(),
            oracle_auto_run: true,
        };
        assert!(render_config(&plan).contains("auto_run = true"));
    }

    #[test]
    fn render_config_emits_full_commented_surface_that_round_trips() {
        // The generated config documents the full surface (commented), still parses via
        // Config::load, and the example [[target]] / [watch] / [version_check] tables stay
        // COMMENTED — only the active bindings + model take effect.
        let root = std::env::temp_dir().join(format!("ragrat-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Cpp],
            bindings: BTreeMap::from([(Language::Cpp, vec![
                PathBuf::from("include"),
                PathBuf::from("src"),
            ])]),
            backend: EmbeddingBackend::fast_embed(),
            oracle_auto_run: false,
        };
        let text = render_config(&plan);
        assert!(text.contains("# [[target]]"), "documents the expanded target form");
        assert!(text.contains("# [watch]"));
        assert!(text.contains("# [version_check]"));
        assert!(text.contains("# [local_ai.embedding.runtime]"));
        assert!(text.contains("`.h`"), "explains the cpp .h-header binding");

        std::fs::write(root.join("rag-rat.toml"), &text).unwrap();
        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        // Exactly the one active cpp target — the example [[target]] stayed commented.
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].language, Language::Cpp);
        // Commented [watch] falls back to its default (enabled).
        assert!(config.watch.enabled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recommend_backend_scales_with_repo_size() {
        assert_eq!(recommend_backend(estimated_chunks(500_000)), EmbeddingBackend::fast_embed());
        assert_eq!(recommend_backend(estimated_chunks(50_000_000)), EmbeddingBackend::model2vec());
    }

    #[test]
    fn default_plan_selects_detected_src_dirs() {
        let scan = RepoScan {
            language_counts: BTreeMap::from([(Language::Rust, 2), (Language::Markdown, 1)]),
            dir_counts: BTreeMap::from([
                (
                    Language::Rust,
                    BTreeMap::from([(PathBuf::from("."), 2), (PathBuf::from("src"), 2)]),
                ),
                (
                    Language::Markdown,
                    BTreeMap::from([(PathBuf::from("."), 1), (PathBuf::from("docs"), 1)]),
                ),
            ]),
            direct_dir_counts: BTreeMap::new(),
            total_source_bytes: 0,
            has_python_virtualenv: false,
            deferred_headers: Vec::new(),
        };

        let plan = default_plan(".".to_string(), &scan);

        assert_eq!(plan.languages, vec![Language::Rust, Language::Markdown]);
        assert_eq!(plan.bindings[&Language::Rust], vec![PathBuf::from("src")]);
        assert_eq!(plan.bindings[&Language::Markdown], vec![
            PathBuf::from("."),
            PathBuf::from("docs")
        ]);
    }

    #[test]
    fn c_defaults_include_direct_source_feature_dirs() {
        let scan = RepoScan {
            language_counts: BTreeMap::from([(Language::C, 10)]),
            dir_counts: BTreeMap::from([(
                Language::C,
                BTreeMap::from([
                    (PathBuf::from("."), 10),
                    (PathBuf::from("drivers"), 1),
                    (PathBuf::from("drivers/entropy"), 1),
                    (PathBuf::from("samples"), 9),
                    (PathBuf::from("samples/simple_txrx"), 9),
                    (PathBuf::from("samples/simple_txrx/src"), 9),
                ]),
            )]),
            direct_dir_counts: BTreeMap::from([(
                Language::C,
                BTreeMap::from([
                    (PathBuf::from("drivers/entropy"), 1),
                    (PathBuf::from("samples/simple_txrx/src"), 1),
                ]),
            )]),
            total_source_bytes: 0,
            has_python_virtualenv: false,
            deferred_headers: Vec::new(),
        };

        let defaults = candidate_dirs(&scan, Language::C)
            .into_iter()
            .filter(|candidate| candidate.default)
            .map(|candidate| candidate.path)
            .collect::<Vec<_>>();

        assert!(defaults.contains(&PathBuf::from("drivers/entropy")));
        assert!(defaults.contains(&PathBuf::from("samples/simple_txrx/src")));
        assert!(!defaults.contains(&PathBuf::from("drivers")));
        assert!(!defaults.contains(&PathBuf::from(".")));
    }

    #[test]
    fn nested_config_uses_repo_root_relative_to_config_dir() {
        assert_eq!(config_root_value(Path::new("/repo"), Path::new("profiles/rag-rat.toml")), "..");
        assert_eq!(
            config_root_value(Path::new("/repo"), Path::new("profiles/dev/rag-rat.toml")),
            "../.."
        );
        assert_eq!(config_root_value(Path::new("/repo"), Path::new("rag-rat.toml")), ".");
    }
}
