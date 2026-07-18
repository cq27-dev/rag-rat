use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::{
    self as config, Config, ConfigError, LlmConfig, LogConfig, MemoryConfig, PapertrailConfig,
    RawConfig, RawTarget, ResolvedTarget, TargetKind, TrackerConfig,
};
use crate::language::Language;

impl Config {
    /// The deduplicated set of target directories (relative to [`Config::root`]) across all
    /// targets, in stable order. Used to scope `.gitignore` nested-discovery to the indexed trees
    /// (see `IgnoreMatcher::compile`) instead of recursing the whole
    /// root into unindexed siblings.
    pub fn target_directories(&self) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        let mut dirs = Vec::new();
        for target in &self.targets {
            for dir in &target.directories {
                if seen.insert(dir.clone()) {
                    dirs.push(dir.clone());
                }
            }
        }
        dirs
    }

    /// A copy of this (BASE) config whose `targets` are re-resolved from a LINKED worktree's own
    /// `rag-rat.toml` (at `<linked_worktree_root>/rag-rat.toml`), so an overlay refresh indexes
    /// that branch with its OWN target set — not the sweeping process's. The shared `root`
    /// (anchored to main), `database`, and the rest are kept: the overlay still resolves
    /// against the one base index, but the delta + ignore filtering see the branch's targets.
    /// Returns the config unchanged when the linked worktree has no readable/valid
    /// `rag-rat.toml`, or when its targets don't validate against the linked checkout — so a
    /// malformed branch config degrades to base targets rather than dropping the worktree. The
    /// branch targets are root-relative, so they apply to the shared `root` directly (#219
    /// review).
    ///
    /// Used by `refresh_worktree_overlays`: the main watcher/maintenance process refreshes every
    /// linked worktree, but a worktree whose branch ADDS a target (e.g. `extra/`) would otherwise
    /// be filtered against the sweeper's targets — pruning overlay rows a branch-launched hook
    /// indexed.
    pub fn for_linked_worktree_overlay(&self, linked_path: &Path) -> Self {
        let linked_targets = (|| {
            // `linked_path` may be the checkout root, a subdir of it, or the git dir (a hook); the
            // branch `rag-rat.toml` lives at the WORKDIR top, so resolve the workdir first.
            let workdir = crate::repo_discover::discover_repo(linked_path)
                .ok()
                .and_then(|repo| repo.workdir().map(Path::to_path_buf))
                .unwrap_or_else(|| linked_path.to_path_buf());
            let text = fs::read_to_string(workdir.join("rag-rat.toml")).ok()?;
            let raw: RawConfig = toml::from_str(&text).ok()?;
            // Targets are relative to the config's `[index].root` (a subdir layout puts the toml at
            // the worktree top with `root = "<subdir>"`); resolve + validate them there so the
            // stored, root-relative directories match the base config's spelling exactly.
            let target_root = workdir.join(raw.index.root.as_deref().unwrap_or("."));
            resolve_targets(&target_root, raw.target_bindings, raw.target).ok()
        })();
        match linked_targets {
            Some(targets) => Self { targets, ..self.clone() },
            None => self.clone(),
        }
    }

    /// Load a config, resolving WHICH `rag-rat.toml` GOVERNS at one seam: in a linked git
    /// worktree, the MAIN worktree's config file is authoritative for the WHOLE config — identity,
    /// database location, targets, models, everything. A branch-local `rag-rat.toml` (an older
    /// branch, a divergent checkout) cannot fork any of it; when its content differs from main's
    /// it is ignored with a one-line warning naming the ignored file. This subsumes the historical
    /// per-key anchoring (root #218/#219, targets #219, `repo_id` #413, `database` A7) — the
    /// question "which config governs this repo" is answered once, so new keys are main-anchored
    /// by default and cannot re-open the split-brain class.
    ///
    /// DECLARED WORKTREE-LOCAL ALLOW-LIST (the only keys that legitimately vary per worktree):
    ///  * `target_bindings` / `[[target]]` — FOR THE OVERLAY INDEX ONLY, read by
    ///    [`Config::for_linked_worktree_overlay`] from the linked checkout's own file, because a
    ///    branch may add/remove source dirs and its overlay must index its own file set (#219). The
    ///    BASE config's targets remain main-anchored here.
    ///
    /// Everything else, present and future, resolves from the governing (main) config.
    ///
    /// EDGE POSTURES:
    ///  * Main worktree resolvable but CONFIG-LESS → the local config governs, with a warning (the
    ///    repo's config belongs in main; until it exists there, the branch copy is all we have —
    ///    best-effort, mirroring the old per-key fallbacks).
    ///  * Main config exists but FAILS to parse/read → the error PROPAGATES: loading from any
    ///    worktree must behave like loading from main, errors included (silently falling back to
    ///    the branch config would fork the repo exactly when main is briefly broken).
    ///  * No resolvable main (bare-repo hubs, pruned main, custom GIT_DIR, non-git roots) →
    ///    `main_worktree_root` is `None`, the local config governs unchanged — there is no
    ///    designated main to defer to.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)?;
        // Parse the LOCAL file but DO NOT fail yet: when a main config governs, the branch-local
        // file's contents are irrelevant by design — a parse/validation failure there must fold
        // into the divergence warning, never block every command from the linked checkout (Codex
        // batch 8, finding 2). Wherever the local config GOVERNS, the error is fatal as always.
        // The `[local_ai]` / `[dream]` rejections are part of local validity — see the
        // `RawConfig` presence-capture fields.
        let local_parse: Result<RawConfig, ConfigError> =
            toml::from_str::<RawConfig>(&text).map_err(ConfigError::from).and_then(validate_raw);
        let local_config_dir = path.parent().unwrap_or_else(|| Path::new("."));
        // The topology subject must be a discoverable directory: a RELATIVE config path like
        // `rag-rat.toml` has the EMPTY path as its parent (`Path::parent` yields `Some("")`, not
        // `None`), which git discovery cannot open — it means the process cwd.
        let local_checkout =
            if local_config_dir.as_os_str().is_empty() { Path::new(".") } else { local_config_dir };

        // Best-effort resolution of what the LOCAL checkout's own `[index] root` names, taken
        // BEFORE the governing seam below picks a winner — used only to detect + report a re-anchor
        // to the caller (#427), never to decide anything (the seam is the sole source of truth for
        // governance). `None` on any parse/resolution failure; that is not a second error path,
        // just a diagnostic that stays silent when it cannot be computed.
        let local_root_named: Option<PathBuf> = local_parse.as_ref().ok().and_then(|local_raw| {
            config::normalize_existing_dir(
                &local_config_dir
                    .join(local_raw.index.root.clone().unwrap_or_else(|| ".".to_string())),
            )
            .ok()
        });

        // THE GOVERNING SEAM (see the doc comment). Linked-ness comes from git TOPOLOGY — the
        // checkout holding the config file vs the repo's designated main worktree
        // ([`linked_worktree_main_root`]) — and governance is UNCONDITIONAL on that predicate.
        // It must never hang off a root-anchoring proxy: a branch-only `[index] root` makes
        // `anchor_root_to_main_worktree` return the local root unchanged, and an equality trigger
        // would then let the branch config govern database/identity/models — the exact
        // split-brain the seam exists to prevent (Codex batch 8, finding 3). Anchoring outcomes
        // affect ROOT resolution only, never who governs.
        let (mut raw, config_dir, root, target_validation_root) =
            match config::linked_worktree_main_root(local_checkout) {
                Some(main_top) => match governing_main_config(&main_top)? {
                    Some((main_raw, main_config_dir)) => {
                        let divergent = match &local_parse {
                            Ok(local_raw) => *local_raw != main_raw,
                            Err(_) => true,
                        };
                        if divergent {
                            let ignored =
                                path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                            let invalid_note =
                                if local_parse.is_err() { " (also invalid)" } else { "" };
                            eprintln!(
                                "rag-rat: ignoring branch config{invalid_note} {} — in a linked \
                                 worktree the main worktree's config governs ({}); edit that file \
                                 instead",
                                ignored.display(),
                                main_config_dir.join("rag-rat.toml").display(),
                            );
                        }
                        // Re-derive root from MAIN's own config, exactly as loading it directly
                        // would (its root is already the main worktree — anchoring is identity).
                        let main_root =
                            config::normalize_existing_dir(&main_config_dir.join(
                                main_raw.index.root.clone().unwrap_or_else(|| ".".to_string()),
                            ))?;
                        (main_raw, main_config_dir, main_root.clone(), main_root)
                    },
                    None => {
                        // Config-less main: the LOCAL config governs (best-effort, loudly), so
                        // its validity is fatal exactly as in a non-linked checkout.
                        let local_raw = local_parse?;
                        eprintln!(
                            "rag-rat: the main worktree has no rag-rat.toml; using {} until one \
                             exists there (the repo's config belongs in the main worktree)",
                            path.display(),
                        );
                        // Root stays anchored so the shared index still keys off the main
                        // checkout; targets validate against the local checkout where they
                        // exist (#219).
                        let local_root = config::normalize_existing_dir(&local_config_dir.join(
                            local_raw.index.root.clone().unwrap_or_else(|| ".".to_string()),
                        ))?;
                        let anchored_root = anchor_root_to_main_worktree(&local_root);
                        (local_raw, local_config_dir.to_path_buf(), anchored_root, local_root)
                    },
                },
                None => {
                    // The config's own checkout is the main worktree (or there is no designated
                    // main): the local config governs and its validity is fatal. Root anchoring
                    // still applies for the exotic `[index] root` pointing into a linked
                    // checkout (#218/#219) — an anchoring concern, not a governance one.
                    let local_raw = local_parse?;
                    let local_root = config::normalize_existing_dir(
                        &local_config_dir
                            .join(local_raw.index.root.clone().unwrap_or_else(|| ".".to_string())),
                    )?;
                    let anchored_root = anchor_root_to_main_worktree(&local_root);
                    (local_raw, local_config_dir.to_path_buf(), anchored_root, local_root)
                },
            };

        // #427: `root` may have ended up different from what the LOCAL checkout's own `[index]
        // root` names — either because a linked worktree's config was overridden wholesale by
        // MAIN's (the common case, above), or because `anchor_root_to_main_worktree` rebased an
        // exotic branch-only root (#218/#219). Either way, report the pre-anchor value so `index`
        // can warn the operator they're indexing the main checkout, not the worktree they named.
        let source_root_reanchored_from = local_root_named.filter(|named| *named != root);

        // The database path (A7 default flip): an explicit `database` key is honored as-is — the
        // deprecated per-repo deployment, that repo stays un-consolidated and never syncs. ABSENT,
        // the default is the CONSOLIDATED GLOBAL store, EXCEPT a pre-existing legacy
        // `.rag-rat/index.sqlite` is kept (with a deprecation nudge toward `rag-rat consolidate`).
        // Relative explicit paths (and the legacy path) resolve against the MAIN worktree TOP —
        // NOT `root`, which may be a subdirectory — so every worktree of a repo AND any
        // `root="<subdir>"` config land on the SAME index.
        let db_base = config::main_worktree_root(&root).unwrap_or_else(|| root.clone());
        let repo_id_override =
            raw.index.repo_id.take().map(|id| id.trim().to_string()).filter(|id| !id.is_empty());
        let governing_database_key = raw.index.database.take();
        let database_key_pinned = governing_database_key.is_some();
        let database = match governing_database_key {
            Some(db) if Path::new(&db).is_absolute() => PathBuf::from(db),
            Some(db) => db_base.join(db),
            // The keyless default probes the repo IDENTITY (root + the governing `[index]
            // repo_id` pin): only an identity-BEARING root may land in the shared global store —
            // see `default_database_with_disposition`.
            None => config::resolve_default_database(&db_base, &root, repo_id_override.as_deref()),
        };
        // The identity gate's SECOND entrance (Codex batch 8, finding 5): an explicit pin AT the
        // consolidated global store bypasses the keyless identity gate above, and an
        // identity-less root (non-git, unborn HEAD) opening the shared store would fall through
        // to adoption's sole-repo fallback — scoping this project onto whichever SIBLING repo
        // sorts first. Refuse at resolution with the remedy. (The structural backstop for every
        // other shared-path pin shape lives in `adopt_repo_from_config`: an identity-less open
        // never sole-picks on a multi-repo database.)
        if database_key_pinned
            && Some(database.as_path()) == crate::data_dir::global_database_path().as_deref()
            && !crate::repo_identity::identity_is_resolvable(&root, repo_id_override.as_deref())
        {
            return Err(ConfigError::GlobalPinWithoutIdentity);
        }
        // Targets resolve from the GOVERNING config; validation runs against the checkout that
        // config describes (main when main governs; the local checkout on the config-less-main
        // fallback, tolerating branch-only dirs — #219). `ResolvedTarget.directories` are
        // root-relative, so the stored targets are checkout-independent either way. A linked
        // branch's own target set is NOT lost: the overlay refresh reads the branch config via
        // `for_linked_worktree_overlay` and indexes the branch with it (#219).
        let targets = resolve_targets(&target_validation_root, raw.target_bindings, raw.target)?;
        let mut llm = LlmConfig::try_from(raw.llm)?;
        // Resolve a RELATIVE cookbook recipe PATH against the GOVERNING config dir, not the
        // process CWD (R6): the recipe is handed to `node`/`npx`, which resolve it against
        // wherever reconcile/the watcher runs — ENOENT from a subdir or a daemon.
        if let Some(remote) = llm.embedding.remote.as_mut()
            && let Some(cookbook) = remote.cookbook.as_ref()
            && let Some(resolved) = config::resolve_relative_cookbook_path(cookbook, &config_dir)
        {
            remote.cookbook = Some(resolved);
        }
        // Same relative-cookbook resolution for the dream remote — its recipe is handed to
        // `node`/`npx` too, so a relative path must resolve against the config dir, not the process
        // CWD. `remote` is not optional for dream (a local-Ollama connect default), so only the
        // ephemeral case has a cookbook to rewrite.
        if let Some(cookbook) = llm.dream.remote.cookbook.as_ref()
            && let Some(resolved) = config::resolve_relative_cookbook_path(cookbook, &config_dir)
        {
            llm.dream.remote.cookbook = Some(resolved);
        }
        // Same relative-cookbook resolution for the distill remote (#704) — it rides the same
        // `RemoteDreamConfig` and hands its recipe to `node`/`npx` too.
        if let Some(cookbook) = llm.distill.remote.cookbook.as_ref()
            && let Some(resolved) = config::resolve_relative_cookbook_path(cookbook, &config_dir)
        {
            llm.distill.remote.cookbook = Some(resolved);
        }
        let watch = raw.watch.into();
        let version_check = raw.version_check.into();
        let oracle = raw.oracle.into();
        let search = raw.search.into();
        let memory = MemoryConfig::try_from(raw.memory)?;
        let trackers =
            raw.tracker.into_iter().map(TrackerConfig::try_from).collect::<Result<Vec<_>, _>>()?;
        let papertrail =
            raw.papertrail.map(PapertrailConfig::try_from).transpose()?.unwrap_or_default();
        let mut log = LogConfig::try_from(raw.log)?;
        // Finalize `dir`: empty (unset) → sibling of the db (`<db_parent>/logs`); a set value is
        // resolved relative to the GOVERNING config dir (absolute honored).
        log.dir = if log.dir.as_os_str().is_empty() {
            database.parent().map(|p| p.join("logs")).unwrap_or_else(|| PathBuf::from("logs"))
        } else if log.dir.is_absolute() {
            log.dir.clone()
        } else {
            config_dir.join(&log.dir)
        };

        Ok(Self {
            root,
            database,
            targets,
            llm,
            watch,
            version_check,
            oracle,
            search,
            memory,
            trackers,
            papertrail,
            log,
            repo_id_override,
            database_key_pinned,
            source_root_reanchored_from,
            allow_empty: false,
        })
    }
}

/// The MAIN worktree's parsed config, when one exists — the governing side of `Config::load`'s
/// seam. `main_top` is the already-derived main worktree top ([`linked_worktree_main_root`]).
/// `Ok(None)` = main has NO `rag-rat.toml` (the local-governs fallback); a main config that
/// exists but cannot be read or parsed PROPAGATES its error (loading from a linked worktree must
/// behave like loading from main, errors included). Returns the main worktree TOP as the
/// governing config dir.
fn governing_main_config(main_top: &Path) -> Result<Option<(RawConfig, PathBuf)>, ConfigError> {
    let main_top = main_top.to_path_buf();
    let main_config_path = main_top.join("rag-rat.toml");
    let text = match fs::read_to_string(&main_config_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let raw = validate_raw(toml::from_str(&text)?)?;
    Ok(Some((raw, main_top)))
}

/// Presence-captured retired/reserved tables are invalid regardless of which checkout's config
/// wins the governing seam. Keep this validation single-sourced so loading the same config from
/// main and from a linked worktree cannot disagree.
fn validate_raw(raw: RawConfig) -> Result<RawConfig, ConfigError> {
    if raw.local_ai.is_some() {
        return Err(ConfigError::LocalAiTableRenamed);
    }
    if raw.dream.is_some() {
        return Err(ConfigError::DreamTableMoved);
    }
    Ok(raw)
}

pub(crate) fn resolve_targets(
    root: &Path,
    simple: BTreeMap<String, Vec<String>>,
    expanded: Vec<RawTarget>,
) -> Result<Vec<ResolvedTarget>, ConfigError> {
    let mut names = BTreeSet::new();
    let mut targets = Vec::new();

    for (language_name, directories) in simple {
        let language = Language::from_str(&language_name)?;
        let kind =
            if language == Language::Markdown { TargetKind::Docs } else { TargetKind::Source };
        let name = language.as_str().to_string();
        push_target(root, &mut names, &mut targets, ResolvedTarget {
            include: language.default_include_globs(),
            exclude: Vec::new(),
            name,
            language,
            directories: directories.into_iter().map(PathBuf::from).collect(),
            kind,
        })?;
    }

    for target in expanded {
        let language = Language::from_str(&target.language)?;
        let kind = target
            .kind
            .as_deref()
            .map(TargetKind::from_str)
            .transpose()?
            .unwrap_or(TargetKind::Source);
        push_target(root, &mut names, &mut targets, ResolvedTarget {
            name: target.name,
            language,
            directories: target.directories.into_iter().map(PathBuf::from).collect(),
            include: target.include.unwrap_or_else(|| language.default_include_globs()),
            exclude: target.exclude.unwrap_or_default(),
            kind,
        })?;
    }

    Ok(targets)
}

fn push_target(
    root: &Path,
    names: &mut BTreeSet<String>,
    targets: &mut Vec<ResolvedTarget>,
    target: ResolvedTarget,
) -> Result<(), ConfigError> {
    if !names.insert(target.name.clone()) {
        return Err(ConfigError::DuplicateTarget(target.name));
    }
    for directory in &target.directories {
        let full_path = root.join(directory);
        if !full_path.is_dir() {
            return Err(ConfigError::MissingDirectory(directory.clone()));
        }
    }
    targets.push(target);
    Ok(())
}

/// Re-anchor a **linked** worktree's `root` to the equivalent path under the **main** worktree, so
/// every worktree of a repo resolves to one root + one shared index — while PRESERVING any
/// subdirectory the config root points at (a `root="<subdir>"` rebases to `<main>/<subdir>`, not
/// the repo top). The main worktree (and non-git dirs) resolve to themselves. Collapsing a subdir
/// root to the repo top changed the indexed file set and could fail config load when a target dir
/// exists only under the subdir (#219 review).
pub(crate) fn anchor_root_to_main_worktree(root: &Path) -> PathBuf {
    let Ok(repo) = crate::repo_discover::discover_repo(root) else {
        return root.to_path_buf();
    };
    let (Some(workdir), Some(main_root)) = (repo.workdir(), config::main_worktree_root(root))
    else {
        return root.to_path_buf();
    };
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    if main_root == workdir {
        return root.to_path_buf(); // already the main worktree — keep the configured (sub)root
    }
    // Linked worktree: rebase root's in-worktree subpath under the main worktree top. `root` is
    // canonicalized by `normalize_existing_dir`, so it strips cleanly against the canonical
    // workdir; `root == workdir` (a `root="."` config) yields an empty suffix → the main
    // worktree top.
    let anchored = match root.strip_prefix(&workdir) {
        Ok(rel) => main_root.join(rel),
        Err(_) => main_root,
    };
    // The anchored path must EXIST in the main checkout. When the linked branch sets `[index].root`
    // to a directory that lives only on the branch (not in main), `main_root.join(rel)` points at a
    // missing path, which would break the later `discover_repo` / database-base / base-indexing
    // calls that read `Config.root`. Keep the linked checkout's (validated, existing) root in that
    // case — the overlay still serves the branch; the base just can't anchor there (#219 review).
    if anchored.is_dir() { anchored } else { root.to_path_buf() }
}
