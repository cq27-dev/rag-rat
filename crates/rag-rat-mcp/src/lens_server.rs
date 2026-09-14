//! Elected Lens HTTP server lifecycle for active MCP and standalone serving.

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::RangeInclusive;
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use rag_rat_base::config::Config;
use rag_rat_base::locks::FileLock;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::http::{self, ServeControl, ServeOptions};

const DISCOVERY_SCHEMA: &str = "rag-rat-lens-discovery";
const DISCOVERY_VERSION: u32 = 1;
const LENS_PORTS: RangeInclusive<u16> = 18120..=18129;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct LensDiscovery {
    schema: String,
    version: u32,
    url: String,
    host: String,
    port: u16,
    pid: u32,
    repo_id: Option<String>,
    indexed_root: String,
    case_insensitive_paths: bool,
    ownership_token: String,
}

impl LensDiscovery {
    fn new(
        address: SocketAddr,
        repo_id: Option<String>,
        indexed_root: String,
        case_insensitive_paths: bool,
        ownership_token: String,
    ) -> Self {
        let host = address.ip().to_string();
        let port = address.port();
        Self {
            schema: DISCOVERY_SCHEMA.to_string(),
            version: DISCOVERY_VERSION,
            url: format!("http://{address}"),
            host,
            port,
            pid: std::process::id(),
            repo_id,
            indexed_root,
            case_insensitive_paths,
            ownership_token,
        }
    }
}

struct DiscoveryGuard {
    path: PathBuf,
    ownership_token: String,
}

impl DiscoveryGuard {
    fn publish(path: PathBuf, discovery: &LensDiscovery) -> anyhow::Result<Self> {
        let case_insensitive = discovery.case_insensitive_paths;
        refuse_tracked_discovery(&path, case_insensitive)?;
        prepare_discovery_parent(&path)?;
        // The discovery file carries a bearer credential. Establish the ignore rule before the
        // credential exists so a first-run `git add -A` cannot stage it.
        ignore_sockets_dir(&path, case_insensitive)?;
        let mut contents = serde_json::to_vec_pretty(discovery)?;
        contents.push(b'\n');
        write_atomic(&path, &contents)?;
        Ok(Self { path, ownership_token: discovery.ownership_token.clone() })
    }
}

fn refuse_tracked_discovery(discovery_path: &Path, case_insensitive: bool) -> anyhow::Result<()> {
    if tracked_runtime_path(discovery_path, case_insensitive)? {
        anyhow::bail!(
            "refusing to overwrite tracked lens discovery file {}",
            discovery_path.display()
        );
    }
    Ok(())
}

/// Report whether a lens runtime path is tracked in the repository index. `case_insensitive` is
/// the serving filesystem's own answer, probed at startup: casefolded Linux directories and
/// network mounts collapse `.RAG-RAT/lens.json` onto the lowercase runtime path exactly like
/// Windows and macOS do, so the alias lookup cannot be selected by build target. It stays off for
/// case-sensitive volumes, where a differently-cased tracked file is an unrelated file and
/// refusing over it would keep Lens from starting.
fn tracked_runtime_path(path: &Path, case_insensitive: bool) -> anyhow::Result<bool> {
    let Some(rag_rat_dir) = path
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".rag-rat"))
    else {
        return Ok(false);
    };
    let workspace_root = rag_rat_dir.parent().context("lens runtime directory has no workspace")?;
    let repo = match rag_rat_base::repo_discover::discover_repo(workspace_root) {
        Ok(repo) => repo,
        Err(_error) if !workspace_root.join(".git").exists() => return Ok(false),
        Err(error) =>
            return Err(anyhow::Error::msg(error.to_string()))
                .with_context(|| format!("opening Git repository at {}", workspace_root.display())),
    };
    let workdir = repo.workdir().context("lens discovery requires a non-bare Git worktree")?;
    let relative = path.strip_prefix(workdir).with_context(|| {
        format!(
            "lens runtime path {} is outside Git worktree {}",
            path.display(),
            workdir.display()
        )
    })?;
    let relative = gix::path::to_unix_separators_on_windows(gix::path::into_bstr(relative));
    let index = repo.index_or_empty()?;
    if index.entry_by_path(relative.as_ref()).is_some() {
        return Ok(true);
    }
    if !case_insensitive {
        return Ok(false);
    }
    let lookup = index.prepare_icase_backing();
    Ok(index.entry_by_path_icase(relative.as_ref(), true, &lookup).is_some())
}

fn prepare_discovery_parent(discovery_path: &Path) -> anyhow::Result<()> {
    let sockets_dir = discovery_path.parent().context("lens discovery path has no parent")?;
    let rag_rat_dir = sockets_dir.parent().context("lens sockets path has no parent")?;
    ensure_real_directory(rag_rat_dir)?;
    ensure_real_directory(sockets_dir)?;
    restrict_directory_to_owner(sockets_dir)?;
    Ok(())
}

/// Keep the directory that holds the discovery credential readable only by the account running the
/// server.
///
/// A checkout can live anywhere — a shared drive, `C:\projects`, a directory whose ACL a previous
/// tool widened — and the file inside carries the bearer token for a loopback service every local
/// account can reach. Inheriting the parent's permissions is therefore not good enough on either
/// platform; both branches replace them outright.
fn restrict_directory_to_owner(directory: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    windows_acl::restrict_to_current_user(directory, windows_acl::Inheritance::ToChildren)?;
    #[cfg(not(any(unix, windows)))]
    let _ = directory;
    Ok(())
}

fn ensure_real_directory(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("refusing symlinked lens runtime directory {}", path.display())
        },
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!("lens runtime path is not a directory: {}", path.display())
        },
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound =>
            fs::create_dir(path).with_context(|| format!("creating {}", path.display())),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn ignore_sockets_dir(discovery_path: &Path, case_insensitive: bool) -> anyhow::Result<()> {
    let Some(rag_rat_dir) = discovery_path.parent().and_then(Path::parent) else {
        return Ok(());
    };
    let gitignore = rag_rat_dir.join(".gitignore");
    if fs::symlink_metadata(&gitignore).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        anyhow::bail!("refusing symlinked lens ignore file {}", gitignore.display());
    }
    let existing = match fs::read_to_string(&gitignore) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", gitignore.display()));
        },
    };
    // Keep the protective rules last: later negations must not make either the generated ignore
    // file or the credential stageable.
    let last_rules = existing
        .lines()
        .map(str::trim)
        .rev()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .take(2)
        .collect::<Vec<_>>();
    let sockets_last = last_rules
        .first()
        .is_some_and(|line| *line == "sockets/" || *line == "/sockets/" || *line == "sockets");
    let ignore_file_before_it =
        last_rules.get(1).is_some_and(|line| *line == ".gitignore" || *line == "/.gitignore");
    if sockets_last && ignore_file_before_it {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("/.gitignore\nsockets/\n");
    if tracked_runtime_path(&gitignore, case_insensitive)? {
        anyhow::bail!("refusing to modify tracked lens ignore file {}", gitignore.display());
    }
    fs::write(&gitignore, updated)?;
    Ok(())
}

impl Drop for DiscoveryGuard {
    fn drop(&mut self) {
        let owned = fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LensDiscovery>(&bytes).ok())
            .is_some_and(|discovery| discovery.ownership_token == self.ownership_token);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub struct LensServerHandle {
    control: ServeControl,
    _task: JoinHandle<()>,
}

impl Drop for LensServerHandle {
    fn drop(&mut self) {
        self.control.stop();
    }
}

pub(crate) fn spawn(config: Config) -> Option<LensServerHandle> {
    if std::env::var_os("RAG_RAT_NO_LENS").is_some() {
        return None;
    }
    let control = ServeControl::default();
    let task_control = control.clone();
    let task = tokio::spawn(async move {
        // A persistent startup failure (uncreatable locks dir, busy port range) must not warn
        // once per second forever: back off exponentially up to a minute and reset on success.
        let mut retry_after_ms = 1_000_u64;
        loop {
            if task_control.is_stopping() {
                return;
            }
            match run(config.clone(), task_control.clone()).await {
                Ok(()) => retry_after_ms = 1_000,
                Err(error) => {
                    tracing::warn!(
                        target: "rag_rat_mcp::lens_server",
                        %error,
                        retry_after_ms,
                        "lens HTTP server unavailable; retrying while MCP continues"
                    );
                    tokio::select! {
                        () = task_control.stopped() => return,
                        () = tokio::time::sleep(std::time::Duration::from_millis(retry_after_ms)) => {},
                    }
                    retry_after_ms = (retry_after_ms * 2).min(60_000);
                },
            }
        }
    });
    Some(LensServerHandle { control, _task: task })
}

async fn run(config: Config, control: ServeControl) -> anyhow::Result<()> {
    run_on_ports(config, LENS_PORTS, control).await
}

async fn run_on_ports(
    config: Config,
    ports: impl IntoIterator<Item = u16>,
    control: ServeControl,
) -> anyhow::Result<()> {
    let workspace_root = workspace_root(&config);
    let lock_path = rag_rat_base::locks::lens_server_lock_path_for(&config, &workspace_root);
    let mut election_logged = false;
    let _election_lock = loop {
        if let Some(lock) = FileLock::try_acquire(&lock_path)? {
            break lock;
        }
        if !election_logged {
            election_logged = true;
            tracing::info!(
                target: "rag_rat_mcp::lens_server",
                "another process owns the lens election for this worktree; standing by"
            );
        }
        tokio::select! {
            () = control.stopped() => return Ok(()),
            () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
        }
    };

    rag_rat_core::IndexDatabase::open_config(&config)?.materialize_lens_coupling()?;
    let listener = bind_first_free(ports).await?;
    let discovery = lens_discovery(&config, &workspace_root, &listener, ownership_token()?)?;
    let discovery_path = lens_discovery_path(&workspace_root);
    let _discovery = DiscoveryGuard::publish(discovery_path.clone(), &discovery)
        .with_context(|| format!("publishing lens discovery at {}", discovery_path.display()))?;

    tracing::info!(
        target: "rag_rat_mcp::lens_server",
        url = %discovery.url,
        discovery = %discovery_path.display(),
        "lens HTTP server listening"
    );
    let origins = origins_from_env()?;
    let options = lens_serve_options(workspace_root, &discovery, origins, control.clone());
    let shutdown_control = control.clone();
    http::serve(listener, config, options, async move {
        shutdown_control.stopped().await;
        Ok(())
    })
    .await
    .context("serving lens HTTP API")
}

/// Options for [`serve_standalone`] beyond the bind address: the bearer credential, the
/// CORS allowlist, and the optional advertised discovery URL.
#[derive(Debug, Default)]
pub struct StandaloneServeOptions {
    pub auth_token: String,
    pub allowed_origins: Vec<String>,
    /// Publish discovery advertising this URL instead of the bind address (the
    /// container-split shape: the extension dials this, not the bind IP).
    pub advertise_url: Option<String>,
}

pub async fn serve_standalone(
    config: Config,
    workspace_root: PathBuf,
    address: SocketAddr,
    options: StandaloneServeOptions,
    election_lock: FileLock,
    shutdown: impl Future<Output = std::io::Result<()>> + Send + 'static,
) -> anyhow::Result<()> {
    serve_standalone_with_binder(
        config,
        workspace_root,
        address,
        options,
        election_lock,
        shutdown,
        TcpListener::bind,
    )
    .await
}

async fn serve_standalone_with_binder<Bind, BindFuture>(
    config: Config,
    workspace_root: PathBuf,
    address: SocketAddr,
    options: StandaloneServeOptions,
    election_lock: FileLock,
    shutdown: impl Future<Output = std::io::Result<()>> + Send + 'static,
    bind: Bind,
) -> anyhow::Result<()>
where
    Bind: FnOnce(SocketAddr) -> BindFuture,
    BindFuture: Future<Output = std::io::Result<TcpListener>>,
{
    let StandaloneServeOptions { auth_token, allowed_origins, advertise_url } = options;
    // The caller acquires the election lock before any side effects (index heal, watcher) so a
    // contended worktree fails fast.
    let _election_lock = election_lock;
    rag_rat_core::IndexDatabase::open_config(&config)?.materialize_lens_coupling()?;
    let listener = bind(address).await?;
    let mut discovery = lens_discovery(&config, &workspace_root, &listener, auth_token)?;
    if let Some(advertise) = advertise_url.as_deref() {
        // The advertised URL replaces the bind address in the published discovery: the
        // extension dials it, so it must parse and carry a usable host + port (a
        // sibling-container bind like 0.0.0.0 or 172.x is exactly the case this exists
        // for). The bind address still decides where the listener lives.
        let parsed = url::Url::parse(advertise)
            .with_context(|| format!("invalid --advertise-url `{advertise}`"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("--advertise-url `{advertise}` has no host"))?
            .to_string();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("--advertise-url `{advertise}` has no port"))?;
        discovery.url = format!("{}://{host}:{port}", parsed.scheme());
        discovery.host = host;
        discovery.port = port;
    }
    // Discovery publishes on loopback (the extension shares the host) or when an
    // explicit --advertise-url names a reachable address. A bare non-loopback bind
    // publishes nothing: the file would carry the hosted bearer token into a workspace
    // file while advertising an address the extension refuses to dial.
    let _discovery = if listener.local_addr()?.ip().is_loopback() || advertise_url.is_some() {
        if !listener.local_addr()?.ip().is_loopback() {
            eprintln!(
                "non-loopback serve is plain HTTP — terminate TLS in a trusted reverse proxy"
            );
        }
        Some(DiscoveryGuard::publish(lens_discovery_path(&workspace_root), &discovery)?)
    } else {
        eprintln!(
            "non-loopback serve without --advertise-url: no workspace discovery file published"
        );
        None
    };
    eprintln!("rag-rat serve listening on {}", discovery.url);
    let control = ServeControl::default();
    let options = lens_serve_options(workspace_root, &discovery, allowed_origins, control.clone());
    http::serve(listener, config, options, shutdown).await?;
    Ok(())
}

/// The discovery record for a lens server bound to `listener`, shared by both serve paths so the
/// published record cannot drift between them. The repo identity is best effort: a checkout
/// without one publishes `null`.
fn lens_discovery(
    config: &Config,
    workspace_root: &Path,
    listener: &TcpListener,
    ownership_token: String,
) -> anyhow::Result<LensDiscovery> {
    let repo_id = rag_rat_base::repo_identity::resolve_repo_identity(
        &config.root,
        config.repo_id_override.as_deref(),
    )
    .ok()
    .map(|identity| identity.repo_id);
    let indexed_root = indexed_root_relative(config, workspace_root)?;
    Ok(LensDiscovery::new(
        listener.local_addr()?,
        repo_id,
        indexed_root,
        path_case_insensitive(workspace_root),
        ownership_token,
    ))
}

/// The HTTP options both serve paths hand to [`http::serve`]. The bearer token the server enforces
/// is the one `discovery` publishes, by construction.
fn lens_serve_options(
    workspace_root: PathBuf,
    discovery: &LensDiscovery,
    allowed_origins: Vec<String>,
    control: ServeControl,
) -> ServeOptions {
    ServeOptions {
        workspace_root: Some(workspace_root),
        indexed_root: discovery.indexed_root.clone(),
        case_insensitive_paths: discovery.case_insensitive_paths,
        auth_token: Some(discovery.ownership_token.clone()),
        allowed_origins,
        control,
        ..ServeOptions::default()
    }
}

fn lens_runtime_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".rag-rat").join("sockets")
}

fn lens_discovery_path(workspace_root: &Path) -> PathBuf {
    lens_runtime_dir(workspace_root).join("lens.json")
}

fn indexed_root_relative(config: &Config, workspace_root: &Path) -> anyhow::Result<String> {
    let active_root = config
        .source_root_reanchored_from
        .as_deref()
        .filter(|root| root.starts_with(workspace_root));
    let relative = active_root
        .and_then(|root| root.strip_prefix(workspace_root).ok())
        .or_else(|| {
            let repo = rag_rat_base::repo_discover::discover_repo(&config.root).ok()?;
            config.root.strip_prefix(repo.workdir()?).ok()
        })
        .or_else(|| config.root.strip_prefix(workspace_root).ok())
        .with_context(|| {
            format!(
                "indexed root {} is not within worktree {}",
                config.root.display(),
                workspace_root.display()
            )
        })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) =>
                parts.push(part.to_str().with_context(|| {
                    format!("indexed root is not UTF-8: {}", relative.display())
                })?),
            Component::CurDir => {},
            _ => anyhow::bail!("indexed root is not worktree-relative: {}", relative.display()),
        }
    }
    Ok(parts.join("/"))
}

fn path_case_insensitive(path: &Path) -> bool {
    let Ok(canonical) = rag_rat_base::paths::canonicalize(path) else { return false };
    if path_has_case_alias(&canonical) {
        return true;
    }
    let Ok(entries) = canonical.read_dir() else { return false };
    entries.filter_map(Result::ok).any(|entry| path_has_case_alias(&entry.path()))
}

fn path_has_case_alias(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
    let mut toggled = name.as_bytes().to_vec();
    let Some(index) = toggled.iter().position(u8::is_ascii_alphabetic) else { return false };
    toggled[index] = if toggled[index].is_ascii_lowercase() {
        toggled[index].to_ascii_uppercase()
    } else {
        toggled[index].to_ascii_lowercase()
    };
    let Ok(toggled) = String::from_utf8(toggled) else { return false };
    let Ok(canonical) = rag_rat_base::paths::canonicalize(path) else { return false };
    path.parent()
        .and_then(|parent| rag_rat_base::paths::canonicalize(parent.join(toggled)).ok())
        .is_some_and(|alias| alias == canonical)
}

async fn bind_first_free(ports: impl IntoIterator<Item = u16>) -> anyhow::Result<TcpListener> {
    bind_first_free_with(ports, TcpListener::bind).await.context("binding a loopback lens port")
}

async fn bind_first_free_with<T, Bind, BindFuture>(
    ports: impl IntoIterator<Item = u16>,
    mut bind: Bind,
) -> Result<T, std::io::Error>
where
    Bind: FnMut(SocketAddr) -> BindFuture,
    BindFuture: Future<Output = Result<T, std::io::Error>>,
{
    let mut last_error = None;
    for port in ports {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        match bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|error| last_error.unwrap_or(error))
}

pub fn ownership_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generating lens ownership token: {error}"))?;
    Ok(rag_rat_base::hash::hex_lower(&bytes))
}

/// The worktree where discovery, election keys, and source scoping anchor: the reanchored linked
/// worktree when config resolution recorded one; else the LAUNCHING linked worktree when the
/// process cwd is a sibling of `config.root`'s repo (a config-less branch worktree nested under
/// the main checkout — the election must never collide with main's); else the git worktree TOP
/// (so a subdir-rooted `[index] root` still publishes where the extension probes); else the
/// config root in non-git trees.
pub fn workspace_root(config: &Config) -> PathBuf {
    if let Some(reanchored) = config.source_root_reanchored_from.clone() {
        return rag_rat_base::repo_discover::discover_repo(&reanchored)
            .ok()
            .and_then(|repo| repo.workdir().map(Path::to_path_buf))
            .unwrap_or(reanchored);
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(linked) = validated_linked_worktree(&config.root, &cwd)
    {
        return linked;
    }
    rag_rat_base::repo_discover::discover_repo(&config.root)
        .ok()
        .and_then(|repo| repo.workdir().map(Path::to_path_buf))
        .unwrap_or_else(|| config.root.clone())
}

/// `candidate` is a LINKED worktree (its per-worktree git dir differs from the common dir) of the
/// same repository as `root`. Mirrors `git_context::validated_sibling_worktree` — a main worktree,
/// a foreign repo, or an unreadable path returns `None` so the caller falls back to base scope
/// rather than serving the wrong repo.
fn validated_linked_worktree(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let repo = rag_rat_base::repo_discover::discover_repo(candidate).ok()?;
    let git_dir = rag_rat_base::paths::canonicalize(repo.git_dir()).ok()?;
    let common_dir = rag_rat_base::paths::canonicalize(repo.common_dir()).ok()?;
    if git_dir == common_dir {
        return None;
    }
    let root_common = rag_rat_base::paths::canonicalize(
        rag_rat_base::repo_discover::discover_repo(root).ok()?.common_dir(),
    )
    .ok()?;
    if common_dir != root_common {
        return None;
    }
    repo.workdir().map(Path::to_path_buf)
}

fn origins_from_env() -> anyhow::Result<Vec<String>> {
    match std::env::var("RAG_RAT_LENS_ORIGINS") {
        Ok(value) => parse_lens_origins(&value),
        Err(_) => Ok(Vec::new()),
    }
}

/// Parse the comma-separated browser-origin allowlist. Split out from the environment read so the
/// rules that decide which origins may reach the API are testable without mutating process-global
/// state — one entry that fails to canonicalize rejects the whole list rather than being dropped,
/// which is what keeps a typo from silently narrowing the allowlist instead of failing startup.
fn parse_lens_origins(value: &str) -> anyhow::Result<Vec<String>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(canonical_lens_origin)
        .collect()
}

/// Canonicalize one browser Origin (`scheme://host[:port]`, no path/query/fragment/credentials).
/// Shared by the server-side `RAG_RAT_LENS_ORIGINS` env allowlist and the CLI's
/// `--allow-origin` clap parser so both surfaces accept exactly the same spellings.
pub fn canonical_lens_origin(raw: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(raw)
        .with_context(|| format!("invalid origin in RAG_RAT_LENS_ORIGINS: `{raw}`"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        anyhow::bail!("invalid origin in RAG_RAT_LENS_ORIGINS: `{raw}`");
    }
    Ok(parsed.origin().ascii_serialization())
}

/// Owner-only file permissions on Windows, where there is no `chmod`.
///
/// Unix keeps the discovery credential private with mode bits. Windows has only the ACL, and a new
/// file inherits its parent's — so on a shared drive, a widened `C:\projects`, or any directory a
/// previous tool opened up, the bearer token for a loopback service every local account can reach
/// would be readable by all of them. This replaces the inherited permissions with a *protected*
/// DACL naming exactly one trustee: the account running the server.
#[cfg(windows)]
mod windows_acl {
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    use anyhow::Context as _;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree, WIN32_ERROR,
    };
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, GetTokenInformation, NO_INHERITANCE,
        PROTECTED_DACL_SECURITY_INFORMATION, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Whether the granted access also seeds what the directory's children inherit.
    #[derive(Clone, Copy)]
    pub(super) enum Inheritance {
        /// For the credential file itself: the ACE governs this object and nothing else.
        None,
        /// For the directory: files and subdirectories created inside start owner-only too.
        ToChildren,
    }

    impl Inheritance {
        fn flags(self) -> ACE_FLAGS {
            match self {
                Self::None => NO_INHERITANCE,
                Self::ToChildren => SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            }
        }
    }

    /// A borrowed process token, closed on every exit path.
    struct ProcessToken(HANDLE);

    impl Drop for ProcessToken {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a successful `OpenProcessToken` and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// An ACL allocated by `SetEntriesInAclW`, which the caller must release with `LocalFree`.
    struct LocalAcl(*mut ACL);

    impl Drop for LocalAcl {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `SetEntriesInAclW` allocates with `LocalAlloc`, so this is the matching
                // free, performed once.
                unsafe { LocalFree(self.0.cast()) };
            }
        }
    }

    pub(super) fn restrict_to_current_user(
        path: &Path,
        inheritance: Inheritance,
    ) -> anyhow::Result<()> {
        let token = open_process_token(path)?;
        // `TOKEN_USER` carries its SID inline, so its size is only known after asking.
        let user = token_user(&token, path)?;
        // SAFETY: `user` holds a well-formed `TOKEN_USER` written by `GetTokenInformation`, so its
        // prefix is a valid `TOKEN_USER`. Reading it out copies the struct; the `Sid` it carries
        // still points into `user`, which outlives every use below.
        let sid = unsafe { user.as_ptr().cast::<TOKEN_USER>().read_unaligned().User.Sid };

        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: inheritance.flags(),
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast(),
            },
        };

        // A null `oldacl` builds the ACL from this one entry alone rather than merging it into
        // whatever the object already carries — the point is to drop the inherited grants, not to
        // add ours alongside them.
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: one entry is described by `access`, whose SID stays alive in `user` for the
        // duration of the call; `acl` receives a fresh `LocalAlloc` allocation on success.
        let status = unsafe { SetEntriesInAclW(1, &access, std::ptr::null(), &mut acl) };
        let acl = LocalAcl(acl);
        check(status, path, "building the owner-only permissions for")?;

        let wide = wide_path(path);
        // `PROTECTED_DACL_SECURITY_INFORMATION` is the load-bearing flag: without it the parent's
        // inheritable grants are re-applied on top of ours and the file stays readable.
        // SAFETY: `wide` is NUL-terminated and outlives the call; `acl.0` is the ACL just built;
        // the owner, group, and SACL pointers are null because only the DACL is being replaced.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl.0,
                std::ptr::null(),
            )
        };
        check(status, path, "applying owner-only permissions to")
    }

    fn open_process_token(path: &Path) -> anyhow::Result<ProcessToken> {
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle from `GetCurrentProcess` needs no cleanup, and
        // `OpenProcessToken` writes a real handle through `handle` when it reports success.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) };
        if opened == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("reading this process's identity to secure {}", path.display())
            });
        }
        Ok(ProcessToken(handle))
    }

    fn token_user(token: &ProcessToken, path: &Path) -> anyhow::Result<Vec<u8>> {
        let mut needed = 0u32;
        // SAFETY: a null buffer of length zero is the documented size probe; it fails with
        // `ERROR_INSUFFICIENT_BUFFER` and writes the required length through `needed`.
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        let mut buffer = vec![0u8; needed.max(1) as usize];
        // SAFETY: `buffer` is at least `needed` bytes, which is what the probe above asked for.
        let read = unsafe {
            GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed)
        };
        if read == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("reading this process's identity to secure {}", path.display())
            });
        }
        Ok(buffer)
    }

    /// Fail closed: a credential we could not lock down must not be published. Callers surface
    /// this as a startup error rather than serving a token any local account can read.
    fn check(status: WIN32_ERROR, path: &Path, doing: &str) -> anyhow::Result<()> {
        if status == ERROR_SUCCESS {
            return Ok(());
        }
        Err(std::io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("{doing} {}", path.display()))
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// What the object's DACL actually says, for tests: a call that reports success is not
    /// evidence that the inherited grants are gone.
    #[cfg(test)]
    pub(super) struct AppliedDacl {
        /// How many trustees the object grants access to. Owner-only means exactly one.
        pub(super) ace_count: u32,
        /// Whether inheritance from the parent directory is blocked. Without this the parent's
        /// grants are re-applied on top and the credential stays readable.
        pub(super) protected: bool,
    }

    #[cfg(test)]
    pub(super) fn applied_dacl(path: &Path) -> anyhow::Result<AppliedDacl> {
        use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
        use windows_sys::Win32::Security::{
            ACL_SIZE_INFORMATION, AclSizeInformation, GetAclInformation,
            GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        };

        let wide = wide_path(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; the owner/group/SACL outputs are null because only the
        // DACL was asked for. On success `descriptor` owns the returned `dacl`.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        let descriptor = LocalAcl(descriptor.cast());
        check(status, path, "reading the applied permissions of")?;

        let mut sizes = ACL_SIZE_INFORMATION::default();
        // SAFETY: `dacl` points inside the live `descriptor`, and `sizes` is the layout
        // `AclSizeInformation` writes.
        let read = unsafe {
            GetAclInformation(
                dacl,
                std::ptr::from_mut(&mut sizes).cast(),
                u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>())?,
                AclSizeInformation,
            )
        };
        anyhow::ensure!(read != 0, "reading the ACE count of {}", path.display());

        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: `descriptor.0` is the live descriptor returned above.
        let read = unsafe {
            GetSecurityDescriptorControl(descriptor.0.cast(), &mut control, &mut revision)
        };
        anyhow::ensure!(read != 0, "reading the descriptor control of {}", path.display());

        Ok(AppliedDacl { ace_count: sizes.AceCount, protected: control & SE_DACL_PROTECTED != 0 })
    }
}

static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Atomically publish `contents` at `path` via a fully-synced sibling temp file + rename. The
/// temp name must carry NO secret (the bearer token lives only in the file's contents): the
/// parent dirs are world-listable under a default umask and crash residue must not leak it.
fn write_atomic(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
        // The discovery dir holds a bearer credential file; keep it owner-only like the file.
        restrict_directory_to_owner(parent)?;
    }
    let dir = parent.unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!(".{file_name}.{}.{sequence}.tmp", std::process::id()));

    let write_result = (|| -> anyhow::Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        // Lock the credential down BEFORE its bytes exist. On Unix the open mode already did it;
        // on Windows the fresh file carries whatever it inherited, so replace that outright rather
        // than trusting the directory ACE set above to have been inherited.
        #[cfg(windows)]
        windows_acl::restrict_to_current_user(&temp, windows_acl::Inheritance::None)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    #[cfg(windows)]
    if path.exists() {
        // `std::fs::rename` cannot replace an existing destination on Windows. The election lock
        // proves there is no live cooperating owner, so remove only the crash-stale artifact;
        // publication itself remains a rename of the fully-synced sibling temp file.
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
