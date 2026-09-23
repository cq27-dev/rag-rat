use super::*;

#[test]
fn watch_config_defaults_on_and_parses_overrides() {
    let default: WatchConfig = RawWatch::default().into();
    assert!(default.enabled, "watcher is on by default");
    assert_eq!(default.debounce_ms, 400);
    assert_eq!(default.max_latency_ms, 2500);
    assert_eq!(default.periodic_sweep_secs, 300);
    assert_eq!(default.pass_cooldown_secs, 60);
    assert_eq!(default.overlay_quiet_secs, 300);

    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [watch]
            enabled = false
            debounce_ms = 750
            max_latency_ms = 4000
            periodic_sweep_secs = 0
            pass_cooldown_secs = 5
            overlay_quiet_secs = 0
            "#,
    )
    .unwrap();
    let watch: WatchConfig = raw.watch.into();
    assert_eq!(watch, WatchConfig {
        enabled: false,
        debounce_ms: 750,
        max_latency_ms: 4000,
        periodic_sweep_secs: 0,
        pass_cooldown_secs: 5,
        overlay_quiet_secs: 0,
    });
}

#[test]
fn version_check_defaults_on_and_parses_opt_out() {
    let default: VersionCheckConfig = RawVersionCheck::default().into();
    assert!(default.enabled, "version check is opted in by default");

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[version_check]\nenabled = false\n").unwrap();
    let version_check: VersionCheckConfig = raw.version_check.into();
    assert!(!version_check.enabled, "[version_check] enabled = false opts out");
}

#[test]
fn search_defaults_off_and_parses_opt_in() {
    let default: SearchConfig = RawSearch::default().into();
    assert!(!default.graded_git_rerank, "graded git rerank is OFF by default");

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[search]\ngraded_git_rerank = true\n").unwrap();
    let search: SearchConfig = raw.search.into();
    assert!(search.graded_git_rerank, "[search] graded_git_rerank = true opts in");
}

#[test]
fn sync_relay_defaults_to_the_shipped_relay_and_parses_an_override() {
    let default: SyncConfig = RawSync::default().into();
    assert_eq!(
        default.relay_url, DEFAULT_SYNC_RELAY,
        "an absent [sync] block uses the shipped default relay"
    );

    let raw: RawConfig = toml::from_str(
        "[index]\nroot = \".\"\n\n[sync]\nrelay_url = \"https://relay.example.test\"\n",
    )
    .unwrap();
    let sync: SyncConfig = raw.sync.into();
    assert_eq!(
        sync.relay_url, "https://relay.example.test",
        "[sync] relay_url overrides the default"
    );

    // A blank value falls back to the default rather than binding an empty relay URL.
    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[sync]\nrelay_url = \"   \"\n").unwrap();
    let sync: SyncConfig = raw.sync.into();
    assert_eq!(sync.relay_url, DEFAULT_SYNC_RELAY, "a blank relay_url falls back to the default");

    // An unknown key under [sync] is rejected, not silently dropped.
    assert!(
        toml::from_str::<RawConfig>("[index]\nroot = \".\"\n\n[sync]\nrelay = \"x\"\n").is_err(),
        "[sync] rejects unknown keys (deny_unknown_fields)"
    );
}

#[test]
fn sync_server_peers_default_empty_and_dedupe_while_push_interval_defaults() {
    let default: SyncConfig = RawSync::default().into();
    assert!(default.server_peers.is_empty(), "device-side sync is off until server_peers is set");
    assert_eq!(default.push_interval_secs, 300, "the default device-sync cadence is 300s");

    let raw: RawConfig = toml::from_str(
        "[index]\nroot = \".\"\n\n[sync]\nserver_peers = [\" node-a \", \"node-b\", \"node-a\", \
         \"  \"]\npush_interval_secs = 60\n",
    )
    .unwrap();
    let sync: SyncConfig = raw.sync.into();
    assert_eq!(
        sync.server_peers,
        vec!["node-a".to_string(), "node-b".to_string()],
        "server_peers are trimmed, de-duplicated, and blanks dropped, order preserved"
    );
    assert_eq!(sync.push_interval_secs, 60, "[sync] push_interval_secs overrides the default");
}

#[test]
fn sync_discovery_is_on_by_default_and_can_be_switched_off_entirely() {
    let default: SyncConfig = RawSync::default().into();
    assert!(
        default.discovery,
        "peers are discovered by default; pinning every host is the opt-out"
    );

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[sync]\ndiscovery = false\n").unwrap();
    let sync: SyncConfig = raw.sync.into();
    assert!(!sync.discovery, "[sync] discovery = false stops all contact with the service");
    assert!(
        !sync.discoverable,
        "and leaves nothing to advertise to — `discoverable` has no effect without it"
    );
}

#[test]
fn sync_discoverable_defaults_off_and_the_service_node_defaults_to_the_shipped_one() {
    let default: SyncConfig = RawSync::default().into();
    assert!(
        !default.discoverable,
        "publishing this device to the discovery service is opt-in; fetching is not gated on it"
    );
    assert_eq!(
        default.discovery_node_id, DEFAULT_DISCOVERY_NODE,
        "an absent [sync] block uses the shipped discovery service node id"
    );

    let raw: RawConfig = toml::from_str(
        "[index]\nroot = \".\"\n\n[sync]\ndiscoverable = true\ndiscovery_node_id = \" abc123 \"\n",
    )
    .unwrap();
    let sync: SyncConfig = raw.sync.into();
    assert!(sync.discoverable, "[sync] discoverable = true opts this device into publishing");
    assert_eq!(sync.discovery_node_id, "abc123", "discovery_node_id is trimmed");

    // A blank node id falls back to the shipped default: an empty one would disable discovery in a
    // way indistinguishable from discovery simply not working.
    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[sync]\ndiscovery_node_id = \"   \"\n").unwrap();
    let sync: SyncConfig = raw.sync.into();
    assert_eq!(
        sync.discovery_node_id, DEFAULT_DISCOVERY_NODE,
        "a blank discovery_node_id falls back to the default"
    );
}

#[test]
fn memory_surface_defaults_summary_and_parses_full_and_rejects_unknown() {
    let default: MemoryConfig = RawMemory::default().try_into().unwrap();
    assert_eq!(
        default.surface,
        MemorySurface::Summary,
        "the memory surface is `summary` by default (bodies deferred to `memory show`)"
    );

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \"full\"\n").unwrap();
    let memory: MemoryConfig = raw.memory.try_into().unwrap();
    assert_eq!(memory.surface, MemorySurface::Full, "surface = \"full\" opts back to whole bodies");

    // Trimmed and case-insensitive, and a round-trip through `as_db_str`.
    let upper: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \" SUMMARY \"\n").unwrap();
    let memory: MemoryConfig = upper.memory.try_into().unwrap();
    assert_eq!(memory.surface, MemorySurface::Summary);
    assert_eq!("FULL".parse(), Ok(MemorySurface::Full));
    assert_eq!(MemorySurface::Summary.as_db_str(), "summary");
    assert_eq!(MemorySurface::Full.as_db_str(), "full");

    let bad: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \"digest\"\n").unwrap();
    assert!(matches!(
        MemoryConfig::try_from(bad.memory),
        Err(ConfigError::UnknownMemorySurface(_))
    ));
}

#[test]
fn oracle_defaults_off_and_parses_overrides() {
    let default: OracleConfig = RawOracle::default().into();
    assert!(!default.auto_run, "background oracle is OFF by default");
    assert_eq!(default.auto_run_quiet_period_secs, 900);
    assert_eq!(default.auto_run_min_interval_secs, 21_600);
    assert!(!default.live.enabled, "live oracle is OFF by default (#534)");
    assert_eq!(default.live.idle_shutdown_secs, 900);
    assert_eq!(default.live.max_requests_per_pass, 200);

    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [oracle]
            auto_run = true
            auto_run_quiet_period_secs = 60
            auto_run_min_interval_secs = 3600

            [oracle.live]
            enabled = true
            idle_shutdown_secs = 120
            max_requests_per_pass = 50
            "#,
    )
    .unwrap();
    let oracle: OracleConfig = raw.oracle.into();
    assert_eq!(oracle, OracleConfig {
        auto_run: true,
        auto_run_quiet_period_secs: 60,
        auto_run_min_interval_secs: 3600,
        live: OracleLiveConfig {
            enabled: true,
            idle_shutdown_secs: 120,
            max_requests_per_pass: 50,
            max_checkouts: 1,
        },
    });

    // `[oracle.live]` stands alone: it neither implies nor requires `[oracle] auto_run` (#534).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [oracle.live]
            enabled = true
            "#,
    )
    .unwrap();
    let oracle: OracleConfig = raw.oracle.into();
    assert!(oracle.live.enabled);
    assert!(!oracle.auto_run, "live must not imply the batch auto-run");
}

#[test]
fn oracle_live_rejects_a_zero_request_budget() {
    let dir = scratch("cfg-parse");
    let path = dir.join("rag-rat.toml");
    std::fs::write(&path, "[oracle.live]\nenabled = true\nmax_requests_per_pass = 0\n").unwrap();
    assert!(matches!(Config::load(path), Err(ConfigError::OracleLiveRequestBudgetZero)));
}

#[test]
fn oracle_live_serves_one_checkout_by_default_and_takes_an_override() {
    // A resident language server per (backend x checkout) multiplies by the worktree fleet, so the
    // shipped default serves only the checkout being edited. The knob is how an operator pays for
    // more (#1010).
    let default = OracleLiveConfig::default();
    assert_eq!(default.max_checkouts, 1, "the shipped default serves one checkout");

    let raw: RawConfig =
        toml::from_str("[oracle.live]\nenabled = true\nmax_checkouts = 4\n").unwrap();
    let oracle: OracleConfig = raw.oracle.into();
    assert_eq!(oracle.live.max_checkouts, 4, "the override is honoured");
}

#[test]
fn oracle_live_rejects_a_zero_checkout_cap() {
    // Zero would admit no checkout at all, so every checkout's live work would sit in a backlog
    // that nothing ever drains — silently, since the stage never fails a pass.
    let dir = scratch("cfg-parse");
    let path = dir.join("rag-rat.toml");
    std::fs::write(&path, "[oracle.live]\nenabled = true\nmax_checkouts = 0\n").unwrap();
    assert!(matches!(Config::load(path), Err(ConfigError::OracleLiveCheckoutCapZero)));
}

#[test]
fn log_config_defaults_off() {
    let raw: RawConfig = toml::from_str("").unwrap();
    let log: LogConfig = raw.log.try_into().unwrap();
    assert!(!log.enabled);
    assert_eq!(log.level, LogLevel::Info);
    assert_eq!(log.format, LogFormat::Text);
    assert_eq!(log.retention_days, 7);
    assert_eq!(log.max_files, 200);
}

#[test]
fn log_config_parses_and_rejects_unknown_level_and_format() {
    let raw: RawConfig = toml::from_str(
        "[log]\nenabled=true\nlevel=\"debug\"\nformat=\"json\"\nfilter=\"\
         rag_rat_core::index::ai=trace\"\nmax_files=10",
    )
    .unwrap();
    let log: LogConfig = raw.log.try_into().unwrap();
    assert!(log.enabled);
    assert_eq!(log.level, LogLevel::Debug);
    assert_eq!(log.format, LogFormat::Json);
    assert_eq!(log.filter.as_deref(), Some("rag_rat_core::index::ai=trace"));
    assert_eq!(log.max_files, 10);

    let bad_level: RawConfig = toml::from_str("[log]\nlevel=\"loud\"").unwrap();
    assert!(matches!(LogConfig::try_from(bad_level.log), Err(ConfigError::UnknownLogLevel(_))));
    let bad_fmt: RawConfig = toml::from_str("[log]\nformat=\"xml\"").unwrap();
    assert!(matches!(LogConfig::try_from(bad_fmt.log), Err(ConfigError::UnknownLogFormat(_))));
}

#[test]
fn log_dir_defaults_to_db_sibling_and_custom_is_config_relative() {
    let dir = scratch("cfg-parse");
    std::fs::write(dir.join("rag-rat.toml"), "[log]\nenabled=true\n").unwrap();
    let cfg = Config::load(dir.join("rag-rat.toml")).unwrap();
    assert_eq!(cfg.log.dir, cfg.database.parent().unwrap().join("logs"));
}

/// The `[log]` tokens are config spellings an operator writes by hand: pinned, and accepted
/// trimmed in any case.
#[test]
fn log_tokens_are_pinned_and_case_insensitive() {
    assert_eq!(LogLevel::Warn.as_filter_str(), "warn");
    assert_eq!(LogFormat::Text.as_db_str(), "text");
    assert_eq!(LogFormat::Json.as_db_str(), "json");
    let raw: RawConfig = toml::from_str("[log]\nlevel = \" TRACE \"\nformat = \"Json\"\n").unwrap();
    let log: LogConfig = raw.log.try_into().unwrap();
    assert_eq!(log.level, LogLevel::Trace);
    assert_eq!(log.format, LogFormat::Json);
}

#[test]
fn mcp_toolsets_parse_dedupe_and_reject_unknown_names() {
    let parse = |text: &str| {
        let raw: RawConfig = toml::from_str(text).unwrap();
        McpConfig::try_from(raw.mcp)
    };
    assert_eq!(parse("[index]\nroot = \".\"\n").unwrap(), McpConfig::default());
    assert_eq!(
        parse("[index]\nroot = \".\"\n[mcp]\ntoolsets = [\"graph\", \"admin\", \"admin\"]\n")
            .unwrap()
            .toolsets,
        vec![McpToolset::Admin, McpToolset::Graph]
    );
    let err = parse("[index]\nroot = \".\"\n[mcp]\ntoolsets = [\"everything\"]\n").unwrap_err();
    assert!(err.to_string().contains("everything"), "{err}");
}
