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
        fs::read_to_string(root.join(".gitignore")).unwrap().ends_with("/.gitignore\nsockets/\n")
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
    assert_eq!(canonical_lens_origin("HTTPS://Lens.Example:443/").unwrap(), "https://lens.example");
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

    let error = match DiscoveryGuard::publish(root.join(".rag-rat/sockets/lens.json"), &discovery) {
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
async fn port_scan_continues_after_a_busy_candidate() {
    let mut attempted_ports = Vec::new();
    let selected = bind_first_free_with([18120, 18121], |address| {
        attempted_ports.push(address.port());
        std::future::ready(if address.port() == 18120 {
            Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "deterministically occupied candidate",
            ))
        } else {
            Ok(address.port())
        })
    })
    .await
    .unwrap();

    assert_eq!(selected, 18121, "the scan must select the next configured candidate");
    assert_eq!(attempted_ports, [18120, 18121], "the scan must not jump to port zero");
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
    let control = ServeControl::default();
    let task = tokio::spawn(run_on_ports(config.clone(), [0], control));
    let path = lens_discovery_path(&root);
    for _ in 0..100 {
        if path.is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(path.is_file(), "active lens task did not publish discovery");
    let discovery: LensDiscovery = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_ne!(discovery.port, 0, "discovery must publish the kernel-selected port");
    let published_listener =
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, discovery.port)).await;
    assert!(published_listener.is_ok(), "discovery must name the active listener");
    drop(published_listener);
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
    for (bind_ip, connect_ip, publishes) in [
        (IpAddr::V4(Ipv4Addr::UNSPECIFIED), IpAddr::V4(Ipv4Addr::LOCALHOST), false),
        (IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V4(Ipv4Addr::LOCALHOST), true),
        (
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            false,
        ),
        (
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            true,
        ),
    ] {
        let root = temp_dir();
        let mut config = test_config(root.clone());
        config.allow_empty = true;
        drop(rag_rat_core::IndexDatabase::rebuild(&config).unwrap());
        let election = FileLock::try_acquire(&root.join("election.lock"))
            .unwrap()
            .expect("an uncontended election lock");
        let listener = TcpListener::bind(SocketAddr::new(bind_ip, 0)).await.unwrap();
        let bound_address = listener.local_addr().unwrap();
        let (started, wait_for_start) = tokio::sync::oneshot::channel::<()>();
        let (stop, wait_for_stop) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(serve_standalone_with_binder(
            config.clone(),
            root.clone(),
            bound_address,
            StandaloneServeOptions {
                auth_token: "standalone-token".to_string(),
                allowed_origins: Vec::new(),
                advertise_url: None,
            },
            election,
            async move {
                let _ = started.send(());
                let _ = wait_for_stop.await;
                Ok(())
            },
            move |address| {
                assert_eq!(address, bound_address);
                std::future::ready(Ok(listener))
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(10), wait_for_start)
            .await
            .expect("standalone serve must reach its shutdown future")
            .expect("standalone serve must not exit before serving");

        let accepted =
            tokio::net::TcpStream::connect(SocketAddr::new(connect_ip, bound_address.port())).await;
        assert!(
            accepted.is_ok(),
            "{bind_ip} bind must accept connections through {connect_ip}:{}",
            bound_address.port()
        );
        drop(accepted);

        let discovery_path = lens_discovery_path(&root);
        assert_eq!(
            discovery_path.is_file(),
            publishes,
            "{bind_ip} bind: discovery file presence must follow loopback-ness"
        );
        if publishes {
            let published: LensDiscovery =
                serde_json::from_slice(&fs::read(&discovery_path).unwrap()).unwrap();
            assert_ne!(published.port, 0, "discovery must publish the kernel-selected port");
            assert_eq!(published.ownership_token, "standalone-token");
            assert!(published.url.contains(&published.port.to_string()));
            let published_listener =
                tokio::net::TcpStream::connect(SocketAddr::new(connect_ip, published.port)).await;
            assert!(published_listener.is_ok(), "discovery must name the active listener");
            drop(published_listener);
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
    let election = FileLock::try_acquire(&root.join("election.lock"))
        .unwrap()
        .expect("an uncontended election lock");
    let (started, wait_for_start) = tokio::sync::oneshot::channel::<()>();
    let (stop, wait_for_stop) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(serve_standalone(
        config.clone(),
        root.clone(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        StandaloneServeOptions {
            auth_token: "standalone-token".to_string(),
            allowed_origins: Vec::new(),
            advertise_url: Some("http://lens.internal:18120".to_string()),
        },
        election,
        async move {
            let _ = started.send(());
            let _ = wait_for_stop.await;
            Ok(())
        },
    ));

    tokio::time::timeout(std::time::Duration::from_secs(10), wait_for_start)
        .await
        .expect("advertise-url serve must reach its shutdown future")
        .expect("advertise-url serve must not exit before serving");
    let discovery_path = lens_discovery_path(&root);
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
        mcp: Default::default(),
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
