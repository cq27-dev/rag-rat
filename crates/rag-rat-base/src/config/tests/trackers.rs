use super::*;

#[test]
fn tracker_and_papertrail_config_parse_with_defaults() {
    let dir = scratch("cfg-parse");
    std::fs::write(
        dir.join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"github\"\nproject = \"owner/repo\"\n",
    )
    .unwrap();
    let cfg = Config::load(dir.join("rag-rat.toml")).unwrap();
    assert_eq!(cfg.trackers.len(), 1);
    assert_eq!(cfg.trackers[0].provider, config::Tracker::Github);
    assert_eq!(cfg.trackers[0].project.as_deref(), Some("owner/repo"));
    assert_eq!(cfg.trackers[0].remote, "origin");
}

#[test]
fn jira_tracker_requires_an_explicit_project() {
    let dir = scratch("cfg-parse");
    std::fs::write(dir.join("rag-rat.toml"), "[[tracker]]\nprovider = \"jira\"\n").unwrap();
    assert!(matches!(
        Config::load(dir.join("rag-rat.toml")),
        Err(ConfigError::JiraTrackerRequiresProject)
    ));
}

#[test]
fn tracker_auth_requires_exactly_one_source() {
    let dir = scratch("cfg-parse");
    std::fs::write(
        dir.join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"gitlab\"\nauth = { env = \"TOKEN\", token_command = \"glab \
         auth token\" }\n",
    )
    .unwrap();
    assert!(matches!(
        Config::load(dir.join("rag-rat.toml")),
        Err(ConfigError::TrackerAuthExactlyOne)
    ));
}

#[test]
fn tracker_auth_accepts_each_supported_source() {
    for (auth, expected) in [
        ("env = \"TOKEN\"", TrackerAuth::Env("TOKEN".to_string())),
        (
            "token_command = \"gh auth token\"",
            TrackerAuth::TokenCommand("gh auth token".to_string()),
        ),
    ] {
        let dir = scratch("cfg-parse");
        std::fs::write(
            dir.join("rag-rat.toml"),
            format!(
                "[[tracker]]\nprovider = \"github\"\nproject = \"org/repo\"\nauth = {{ {auth} }}\n"
            ),
        )
        .unwrap();
        let config = Config::load(dir.join("rag-rat.toml")).unwrap();
        assert_eq!(config.trackers[0].auth.as_ref(), Some(&expected));
    }
}

#[test]
fn tracker_provider_is_required_and_closed() {
    for (body, expected) in [
        ("[[tracker]]\nproject = \"org/repo\"\n", "requires a `provider`"),
        ("[[tracker]]\nprovider = \"forgejo\"\n", "must be one of"),
    ] {
        let dir = scratch("cfg-parse");
        std::fs::write(dir.join("rag-rat.toml"), body).unwrap();
        let error = Config::load(dir.join("rag-rat.toml")).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn tracker_projects_are_validated_by_provider() {
    for (provider, project) in [
        ("github", "org/team/repo"),
        ("bitbucket", "workspace/repo/extra"),
        ("gitlab", "repo"),
        ("github", "org/repo?query=1"),
        ("github", "org/%2e%2e"),
        ("gitlab", "group/../repo"),
        ("jira", "Proj"),
        ("jira", "A"),
    ] {
        let dir = scratch("cfg-parse");
        std::fs::write(
            dir.join("rag-rat.toml"),
            format!("[[tracker]]\nprovider = \"{provider}\"\nproject = \"{project}\"\n"),
        )
        .unwrap();
        assert!(matches!(
            Config::load(dir.join("rag-rat.toml")),
            Err(ConfigError::InvalidTrackerProject { .. })
        ));
    }

    for (provider, project) in
        [("github", "org/repo"), ("bitbucket", "workspace/repo"), ("gitlab", "group/sub/repo")]
    {
        let dir = scratch("cfg-parse");
        std::fs::write(
            dir.join("rag-rat.toml"),
            format!("[[tracker]]\nprovider = \"{provider}\"\nproject = \"{project}\"\n"),
        )
        .unwrap();
        Config::load(dir.join("rag-rat.toml")).unwrap();
    }
}

#[test]
fn tracker_base_url_requires_a_nonempty_authority() {
    for base_url in [
        "https://",
        "https:///gitlab",
        "ftp://gitlab.example.com",
        "gitlab.example.com",
        "https://:8443",
        "https://gitlab example.com",
        "https://gitlab.example.com/path",
        "https://gitlab.example.com?query=1",
        "https://gitlab.example.com#fragment",
    ] {
        let dir = scratch("cfg-parse");
        std::fs::write(
            dir.join("rag-rat.toml"),
            format!("[[tracker]]\nprovider = \"gitlab\"\nbase_url = \"{base_url}\"\n"),
        )
        .unwrap();
        assert!(matches!(
            Config::load(dir.join("rag-rat.toml")),
            Err(ConfigError::TrackerBaseUrlNotHttp(_))
        ));
    }
}

#[test]
fn tracker_base_url_rejects_credentials_and_normalizes_trailing_slashes() {
    let dir = scratch("cfg-parse");
    std::fs::write(
        dir.join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"gitlab\"\nbase_url = \"https://user:token@gitlab.example.com\"\n",
    )
    .unwrap();
    assert!(matches!(
        Config::load(dir.join("rag-rat.toml")),
        Err(ConfigError::TrackerBaseUrlHasCredentials)
    ));

    std::fs::write(
        dir.join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"gitlab\"\nbase_url = \"http://gitlab.example.com:8080/\"\n",
    )
    .unwrap();
    let config = Config::load(dir.join("rag-rat.toml")).unwrap();
    assert_eq!(config.trackers[0].base_url.as_deref(), Some("http://gitlab.example.com:8080"));
}

#[test]
fn papertrail_scheduling_intervals_are_parsed() {
    let dir = scratch("cfg-parse");
    std::fs::write(dir.join("rag-rat.toml"), "[papertrail]\nprobe_interval_secs = 60\n").unwrap();
    let config = Config::load(dir.join("rag-rat.toml")).unwrap();
    assert_eq!(config.papertrail.probe_interval_secs, 60);
    assert_eq!(config.papertrail.sync_min_interval_secs, 900);
    assert_eq!(config.papertrail.full_sync_interval_secs, 86_400);
}

#[test]
fn papertrail_wake_cadences_reject_zero_but_the_attempt_gate_may_be_zero() {
    let dir = scratch("cfg-parse");
    let path = dir.join("rag-rat.toml");
    // A zero wake cadence would silently disable automatic sync (the watcher's deadline is the
    // minimum of the two), so both are rejected at load.
    for key in ["probe_interval_secs", "full_sync_interval_secs"] {
        std::fs::write(&path, format!("[papertrail]\n{key} = 0\n")).unwrap();
        assert!(
            matches!(Config::load(&path), Err(ConfigError::PapertrailIntervalZero(rejected)) if rejected == key),
            "`{key} = 0` must be rejected"
        );
    }
    // The minimum attempt interval only gates retries; zero disables nothing and stays legal.
    std::fs::write(&path, "[papertrail]\nsync_min_interval_secs = 0\n").unwrap();
    assert_eq!(Config::load(&path).unwrap().papertrail.sync_min_interval_secs, 0);
}

#[test]
fn papertrail_rate_limit_reserve_is_parsed_and_validated() {
    let dir = scratch("cfg-parse");
    let path = dir.join("rag-rat.toml");
    std::fs::write(&path, "[papertrail]\nrate_limit_reserve = 0.2\n").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.papertrail.rate_limit_reserve, 0.2);

    std::fs::write(&path, "[papertrail]\nrate_limit_reserve = 0.0\n").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.papertrail.rate_limit_reserve, 0.0, "zero disables the reserved slice");

    for invalid in ["-0.1", "1.0", "nan"] {
        std::fs::write(&path, format!("[papertrail]\nrate_limit_reserve = {invalid}\n")).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::PapertrailRateLimitReserveOutOfRange(_))
        ));
    }
}
