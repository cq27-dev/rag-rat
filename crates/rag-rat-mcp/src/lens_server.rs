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

    let db = rag_rat_core::IndexDatabase::open_config(&config)?;
    db.materialize_lens_coupling()?;
    drop(db);
    let listener = bind_first_free(ports).await?;
    let ownership_token = ownership_token()?;
    let repo_id = rag_rat_base::repo_identity::resolve_repo_identity(
        &config.root,
        config.repo_id_override.as_deref(),
    )
    .ok()
    .map(|identity| identity.repo_id);
    let indexed_root = indexed_root_relative(&config, &workspace_root)?;
    let discovery = LensDiscovery::new(
        listener.local_addr()?,
        repo_id,
        indexed_root,
        path_case_insensitive(&workspace_root),
        ownership_token.clone(),
    );
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
    let options = ServeOptions {
        workspace_root: Some(workspace_root),
        indexed_root: discovery.indexed_root.clone(),
        case_insensitive_paths: discovery.case_insensitive_paths,
        auth_token: Some(ownership_token),
        allowed_origins: origins,
        control: control.clone(),
        ..ServeOptions::default()
    };
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
    let StandaloneServeOptions { auth_token, allowed_origins, advertise_url } = options;
    // The caller acquires the election lock before any side effects (index heal, watcher) so a
    // contended worktree fails fast.
    let _election_lock = election_lock;
    let db = rag_rat_core::IndexDatabase::open_config(&config)?;
    db.materialize_lens_coupling()?;
    drop(db);
    let listener = TcpListener::bind(address).await?;
    let repo_id = rag_rat_base::repo_identity::resolve_repo_identity(
        &config.root,
        config.repo_id_override.as_deref(),
    )
    .ok()
    .map(|identity| identity.repo_id);
    let indexed_root = indexed_root_relative(&config, &workspace_root)?;
    let mut discovery = LensDiscovery::new(
        listener.local_addr()?,
        repo_id,
        indexed_root,
        path_case_insensitive(&workspace_root),
        auth_token.clone(),
    );
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
    let options = ServeOptions {
        workspace_root: Some(workspace_root),
        indexed_root: discovery.indexed_root.clone(),
        case_insensitive_paths: discovery.case_insensitive_paths,
        auth_token: Some(auth_token),
        allowed_origins,
        control: control.clone(),
        ..ServeOptions::default()
    };
    http::serve(listener, config, options, shutdown).await?;
    Ok(())
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
    let mut last_error = None;
    for port in ports {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|error| {
            last_error.map(anyhow::Error::from).unwrap_or_else(|| anyhow::Error::from(error))
        })
        .context("binding a loopback lens port")
}

pub fn ownership_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generating lens ownership token: {error}"))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
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
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rag-rat-lens-server-test-{}-{sequence}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn discovery_serializes_and_publishes_atomically() {
        let root = temp_dir();
        let path = root.join("sockets/lens.json");
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            Some("repo-1".into()),
            "crate".into(),
            true,
            "owner-1".into(),
        );
        let guard = DiscoveryGuard::publish(path.clone(), &discovery).unwrap();

        let decoded: LensDiscovery = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded, discovery);
        assert_eq!(decoded.schema, DISCOVERY_SCHEMA);
        assert_eq!(decoded.version, DISCOVERY_VERSION);
        assert_eq!(decoded.indexed_root, "crate");
        assert!(decoded.case_insensitive_paths);
        assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), "/.gitignore\nsockets/\n");
        // The file holds a bearer token for a loopback service every local account can reach, so
        // neither platform may leave it at whatever the checkout's directory happened to grant.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        #[cfg(windows)]
        for secured in [path.as_path(), path.parent().unwrap()] {
            let dacl = windows_acl::applied_dacl(secured).unwrap();
            assert_eq!(
                dacl.ace_count,
                1,
                "{} must grant access to exactly one trustee",
                secured.display()
            );
            assert!(
                dacl.protected,
                "{} must block the parent directory's inherited grants",
                secured.display()
            );
        }
        let residue: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != path)
            .collect();
        assert!(residue.is_empty(), "atomic publish left temp files behind: {residue:?}");

        drop(guard);
        assert!(!path.exists(), "owned discovery should be removed on cleanup");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_ignore_rule_wins_over_existing_negations() {
        let root = temp_dir();
        fs::write(root.join(".gitignore"), "sockets/\n!sockets/\n!sockets/lens.json\n").unwrap();
        let path = root.join("sockets/lens.json");
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "owner-1".into(),
        );
        let guard = DiscoveryGuard::publish(path, &discovery).unwrap();

        assert!(
            fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .ends_with("/.gitignore\nsockets/\n")
        );
        drop(guard);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_does_not_dirty_a_clean_repository() {
        let root = temp_dir();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let path = root.join(".rag-rat/sockets/lens.json");
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "owner-1".into(),
        );

        let guard = DiscoveryGuard::publish(path, &discovery).unwrap();
        drop(guard);

        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty(), "discovery dirtied the repository");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn environment_origins_are_canonicalized_like_cli_origins() {
        assert_eq!(
            canonical_lens_origin("HTTPS://Lens.Example:443/").unwrap(),
            "https://lens.example"
        );
        assert!(canonical_lens_origin("https://lens.example/path").is_err());
    }

    #[test]
    fn the_origin_allowlist_canonicalizes_every_entry_or_rejects_the_list() {
        assert_eq!(
            parse_lens_origins("HTTPS://Lens.Example:443/ , https://vscode.dev").unwrap(),
            vec!["https://lens.example".to_string(), "https://vscode.dev".to_string()],
            "entries are trimmed and canonicalized independently"
        );
        assert_eq!(parse_lens_origins("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_lens_origins(" , ,, ").unwrap(),
            Vec::<String>::new(),
            "separator noise is not an origin"
        );
        // Fail the whole list, not just the bad entry: silently dropping one would narrow the
        // allowlist at startup and present as an unexplained CORS failure much later.
        assert!(parse_lens_origins("https://lens.example,https://lens.example/path").is_err());
        assert!(parse_lens_origins("https://lens.example,not a url").is_err());
    }

    #[test]
    fn discovery_refuses_to_modify_a_tracked_runtime_ignore_file() {
        let root = temp_dir();
        let path = root.join(".rag-rat/sockets/lens.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let gitignore = root.join(".rag-rat/.gitignore");
        fs::write(&gitignore, "existing-rule\n").unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["add", ".rag-rat/.gitignore"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "owner-1".into(),
        );

        let error = match DiscoveryGuard::publish(path, &discovery) {
            Ok(_) => panic!("tracked runtime ignore file was modified"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("tracked lens ignore file"), "{error}");
        assert_eq!(fs::read_to_string(gitignore).unwrap(), "existing-rule\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_refuses_to_overwrite_a_tracked_credential_path() {
        let root = temp_dir();
        let path = root.join(".rag-rat/sockets/lens.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "tracked placeholder\n").unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["add", ".rag-rat/sockets/lens.json"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "owner-1".into(),
        );

        let error = match DiscoveryGuard::publish(path.clone(), &discovery) {
            Ok(_) => panic!("tracked discovery path was overwritten"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("tracked lens discovery file"), "{error}");
        assert_eq!(fs::read_to_string(path).unwrap(), "tracked placeholder\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_refuses_a_case_aliased_tracked_credential_path() {
        let root = temp_dir();
        let tracked_path = track_case_aliased_credential(&root);
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            // A case-insensitive filesystem — casefolded Linux and network mounts included, not
            // only Windows and macOS — resolves this tracked spelling onto the lowercase path.
            true,
            "owner-1".into(),
        );

        let error =
            match DiscoveryGuard::publish(root.join(".rag-rat/sockets/lens.json"), &discovery) {
                Ok(_) => panic!("case-aliased tracked discovery path was overwritten"),
                Err(error) => error.to_string(),
            };
        assert!(error.contains("tracked lens discovery file"), "{error}");
        assert_eq!(fs::read_to_string(tracked_path).unwrap(), "tracked placeholder\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn case_sensitive_serving_publishes_beside_a_differently_cased_tracked_path() {
        let root = temp_dir();
        let tracked_path = track_case_aliased_credential(&root);
        if path_case_insensitive(&root) {
            // The alias is literally the same file here, so there is nothing to publish beside.
            let _ = fs::remove_dir_all(root);
            return;
        }
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "owner-1".into(),
        );

        let path = root.join(".rag-rat/sockets/lens.json");
        let guard = DiscoveryGuard::publish(path.clone(), &discovery)
            .expect("an unrelated tracked spelling must not block a case-sensitive worktree");
        assert!(path.exists());
        assert_eq!(fs::read_to_string(tracked_path).unwrap(), "tracked placeholder\n");
        drop(guard);
        let _ = fs::remove_dir_all(root);
    }

    /// Track `.rag-rat/sockets/Lens.json` in a fresh repository and return its path.
    fn track_case_aliased_credential(root: &Path) -> PathBuf {
        let tracked_path = root.join(".rag-rat/sockets/Lens.json");
        fs::create_dir_all(tracked_path.parent().unwrap()).unwrap();
        fs::write(&tracked_path, "tracked placeholder\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["add", ".rag-rat/sockets/Lens.json"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        tracked_path
    }

    #[cfg(unix)]
    #[test]
    fn discovery_refuses_a_symlinked_runtime_directory() {
        use std::os::unix::fs::symlink;

        let root = temp_dir();
        let outside = temp_dir();
        symlink(&outside, root.join("sockets")).unwrap();
        let path = root.join("sockets/lens.json");
        let discovery = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "owner-1".into(),
        );

        let error = match DiscoveryGuard::publish(path, &discovery) {
            Ok(_) => panic!("symlinked runtime directory was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("symlinked lens runtime directory"), "{error}");
        assert!(!outside.join("lens.json").exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn subdirectory_index_root_is_relative_to_the_active_worktree() {
        let root = temp_dir();
        fs::create_dir_all(root.join("crate/src")).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let config = test_config(root.join("crate"));

        assert_eq!(workspace_root(&config), root);
        assert_eq!(indexed_root_relative(&config, &root).unwrap(), "crate");

        let main_root = temp_dir();
        let mut config = test_config(main_root.join("crate"));
        config.source_root_reanchored_from = Some(root.join("crate"));

        assert_eq!(workspace_root(&config), root);
        assert_eq!(indexed_root_relative(&config, &root).unwrap(), "crate");
        let _ = fs::remove_dir_all(main_root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_case_sensitivity_follows_the_actual_filesystem() {
        let root = temp_dir();
        let numeric_root = root.join("123");
        fs::create_dir(&numeric_root).unwrap();
        let probe = numeric_root.join("CaseProbe");
        fs::create_dir(&probe).unwrap();
        let alias = numeric_root.join("caseProbe");
        let expected = rag_rat_base::paths::canonicalize(alias)
            .ok()
            .zip(rag_rat_base::paths::canonicalize(&probe).ok())
            .is_some_and(|(alias, probe)| alias == probe);

        assert_eq!(path_case_insensitive(&probe), expected);
        assert_eq!(path_case_insensitive(&numeric_root), expected);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_only_removes_discovery_owned_by_the_guard() {
        let root = temp_dir();
        let path = root.join("lens.json");
        let predecessor = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18120),
            None,
            String::new(),
            false,
            "predecessor".into(),
        );
        let guard = DiscoveryGuard::publish(path.clone(), &predecessor).unwrap();
        let successor = LensDiscovery::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18121),
            None,
            String::new(),
            false,
            "successor".into(),
        );
        let mut successor_bytes = serde_json::to_vec(&successor).unwrap();
        successor_bytes.push(b'\n');
        write_atomic(&path, &successor_bytes).unwrap();

        drop(guard);
        let remaining: LensDiscovery = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(remaining, successor, "predecessor cleanup deleted successor discovery");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn second_contender_loses_nonblocking_election() {
        let root = temp_dir();
        let path = root.join("lens.lock");
        let first = FileLock::try_acquire(&path).unwrap().expect("first contender should win");
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(FileLock::try_acquire(&path).unwrap().is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn port_scan_falls_back_after_a_busy_candidate() {
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        let available = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let available_port = available.local_addr().unwrap().port();
        drop(available);

        let selected = bind_first_free([busy_port, available_port]).await.unwrap();
        assert_eq!(selected.local_addr().unwrap().port(), available_port);
    }

    #[tokio::test]
    async fn active_task_publishes_and_cleans_up_on_abort() {
        let root = temp_dir();
        let mut config = test_config(root.clone());
        config.allow_empty = true;
        drop(rag_rat_core::IndexDatabase::rebuild(&config).unwrap());
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'git_coupling_stamp'", []).unwrap();
        drop(conn);
        let available = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = available.local_addr().unwrap().port();
        drop(available);

        let control = ServeControl::default();
        let task = tokio::spawn(run_on_ports(config.clone(), [port], control));
        let path = lens_discovery_path(&root);
        for _ in 0..100 {
            if path.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(path.is_file(), "active lens task did not publish discovery");
        let discovery: LensDiscovery = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(discovery.port, port);
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        assert!(
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM repo_meta WHERE key = 'git_coupling_stamp')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap(),
            "Lens startup must materialize stale coupling before serving read-only requests"
        );
        drop(conn);

        task.abort();
        let _ = task.await;
        assert!(!path.exists(), "aborting the active task must clean up its discovery");
        assert!(
            FileLock::try_acquire(&rag_rat_base::locks::lens_server_lock_path_for(&config, &root))
                .unwrap()
                .is_some(),
            "aborting the active task must release its election lock"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Standalone serving publishes the discovery file ONLY for a loopback bind. The file carries
    /// the bearer token into a directory inside the workspace, and a non-loopback serve is the
    /// hosted shape: its address is one the extension refuses to dial anyway, so writing the
    /// credential there would be pure exposure. Both branches serve either way.
    #[tokio::test]
    async fn standalone_serving_publishes_discovery_only_for_a_loopback_bind() {
        for (bind_ip, publishes) in
            [(IpAddr::V4(Ipv4Addr::LOCALHOST), true), (IpAddr::V4(Ipv4Addr::UNSPECIFIED), false)]
        {
            let root = temp_dir();
            let mut config = test_config(root.clone());
            config.allow_empty = true;
            drop(rag_rat_core::IndexDatabase::rebuild(&config).unwrap());
            // Claim a port, then release it so `serve_standalone` binds that exact one — its
            // chosen address is not otherwise observable from here.
            let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = probe.local_addr().unwrap().port();
            drop(probe);

            let election = FileLock::try_acquire(&root.join("election.lock"))
                .unwrap()
                .expect("an uncontended election lock");
            let (stop, wait_for_stop) = tokio::sync::oneshot::channel::<()>();
            let served = tokio::spawn(serve_standalone(
                config.clone(),
                root.clone(),
                SocketAddr::new(bind_ip, port),
                StandaloneServeOptions {
                    auth_token: "standalone-token".to_string(),
                    allowed_origins: Vec::new(),
                    advertise_url: None,
                },
                election,
                async move {
                    let _ = wait_for_stop.await;
                    Ok(())
                },
            ));

            // Publication happens before the listener is served, so a port that answers means the
            // decision has already been made either way.
            let mut serving = false;
            for _ in 0..200 {
                if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.is_ok() {
                    serving = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(serving, "standalone serve never accepted on {bind_ip}:{port}");

            let discovery_path = lens_discovery_path(&root);
            assert_eq!(
                discovery_path.is_file(),
                publishes,
                "{bind_ip} bind: discovery file presence must follow loopback-ness"
            );
            if publishes {
                let published: LensDiscovery =
                    serde_json::from_slice(&fs::read(&discovery_path).unwrap()).unwrap();
                assert_eq!(published.port, port);
                assert_eq!(published.ownership_token, "standalone-token");
                assert!(published.url.contains(&port.to_string()));
            }

            let _ = stop.send(());
            tokio::time::timeout(std::time::Duration::from_secs(10), served)
                .await
                .expect("standalone serve must return once its shutdown future resolves")
                .expect("the serve task must not panic")
                .expect("a clean shutdown is not an error");
            let _ = fs::remove_dir_all(root);
        }
    }

    /// `--advertise-url` publishes discovery on a non-loopback bind with the ADVERTISED
    /// address — the container-split shape: the listener binds the docker-network IP,
    /// and the extension dials the URL it is told to.
    #[tokio::test]
    async fn standalone_serving_publishes_the_advertise_url_when_given() {
        let root = temp_dir();
        let mut config = test_config(root.clone());
        config.allow_empty = true;
        drop(rag_rat_core::IndexDatabase::rebuild(&config).unwrap());
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let election = FileLock::try_acquire(&root.join("election.lock"))
            .unwrap()
            .expect("an uncontended election lock");
        let (stop, wait_for_stop) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(serve_standalone(
            config.clone(),
            root.clone(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            StandaloneServeOptions {
                auth_token: "standalone-token".to_string(),
                allowed_origins: Vec::new(),
                advertise_url: Some("http://lens.internal:18120".to_string()),
            },
            election,
            async move {
                let _ = wait_for_stop.await;
                Ok(())
            },
        ));

        let discovery_path = lens_discovery_path(&root);
        for _ in 0..200 {
            if discovery_path.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let published: LensDiscovery = serde_json::from_slice(
            &fs::read(&discovery_path).expect("an advertise-url serve must publish discovery"),
        )
        .unwrap();
        assert_eq!(published.url, "http://lens.internal:18120");
        assert_eq!(published.host, "lens.internal");
        assert_eq!(published.port, 18120);
        assert_eq!(published.ownership_token, "standalone-token");

        let _ = stop.send(());
        tokio::time::timeout(std::time::Duration::from_secs(10), served)
            .await
            .expect("standalone serve must return once its shutdown future resolves")
            .expect("the serve task must not panic")
            .expect("a clean shutdown is not an error");
        let _ = fs::remove_dir_all(root);
    }

    fn test_config(root: PathBuf) -> Config {
        Config {
            database: root.join(".rag-rat/index.sqlite"),
            root,
            targets: Vec::new(),
            llm: Default::default(),
            watch: Default::default(),
            log: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            trackers: Vec::new(),
            papertrail: Default::default(),
            sync: Default::default(),
            repo_id_override: Some("test-repo".into()),
            database_key_pinned: true,
            source_root_reanchored_from: None,
            allow_empty: false,
        }
    }
}
