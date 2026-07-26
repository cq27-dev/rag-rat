//! Tracker bindings resolved for one checkout: the `[[tracker]]` config list (or, with no
//! config, a binding auto-detected from the git `origin` remote) turned into concrete
//! (provider, project) pairs the sync and ref-grammar layers consume. Resolution reads only
//! local git state — the old `gh repo view` network shell-out is gone.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use rag_rat_base::config::{Tracker, TrackerAuth, TrackerConfig};
use sha2::{Digest, Sha256};

use super::TrackerAuthentication;

/// One tracker binding with its `project` resolved to a concrete value — the runtime shape
/// behind [`super::PapertrailContext`]. The context carries a LIST of these so the mirror sync
/// can drive every binding concurrently, each under its own rate governor; a binding whose
/// project cannot be resolved (no usable git remote) is dropped at resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTracker {
    pub provider: Tracker,
    /// Concrete project path/key: `owner/repo`, a full GitLab namespace path (subgroups kept),
    /// `workspace/repo`, or a Jira project key.
    pub project: String,
    /// Self-hosted base URL (no trailing slash). `None` = the provider's cloud host.
    pub base_url: Option<String>,
    pub auth: Option<TrackerAuth>,
    /// Authentication capability snapshotted when the binding is resolved. Environment sources
    /// are checked for token presence; commands are treated as configured without executing shell
    /// code on ordinary index opens. Transport construction resolves and fails fast at sync time.
    pub authentication: TrackerAuthentication,
    /// Configured tags as written (trimmed, non-empty); matching and fingerprinting normalize
    /// on demand via [`normalized_tags`].
    pub tags: Vec<String>,
}

impl ResolvedTracker {
    /// Whether an item with these label-like facets is tracked: OR across the configured tags,
    /// case-insensitive; an EMPTY tag list tracks everything. Callers must not apply the filter
    /// to item kinds with no tag facet (e.g. Bitbucket pull requests) — those are always
    /// tracked, per the config contract, so this is never called for them.
    pub fn tracks<'a>(&self, facets: impl IntoIterator<Item = &'a str>) -> bool {
        if self.tags.is_empty() {
            return true;
        }
        let tags = normalized_tags(&self.tags);
        facets.into_iter().any(|facet| tags.binary_search(&normalize_tag(facet)).is_ok())
    }

    /// Stable fingerprint of the normalized tag list for the sync cursor's `filter_fingerprint`:
    /// hex sha256 over the NUL-joined normalized tags. The EMPTY list (track all) is the empty
    /// string, so "no filter" stays legible in the cursor row. Case/order/duplicate changes that
    /// leave the tag SET intact keep the fingerprint — only a real widening/narrowing changes it
    /// (which is what resets or prunes the backfill descent).
    pub fn filter_fingerprint(&self) -> String {
        let normalized = normalized_tags(&self.tags);
        if normalized.is_empty() {
            return String::new();
        }
        let mut hasher = Sha256::new();
        for tag in &normalized {
            hasher.update(tag.as_bytes());
            // NUL-terminate each tag so list boundaries stay unambiguous (a tag cannot contain
            // NUL after TOML parsing + trimming, unlike `\n` or `,`).
            hasher.update([0u8]);
        }
        let mut out = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// Case-insensitive tag normalization: trim + Unicode-lowercase (labels are user text, not
/// ASCII identifiers).
fn normalize_tag(tag: &str) -> String {
    tag.trim().to_lowercase()
}

/// The normalized tag list: lowercased, deduplicated, sorted — the canonical form behind both
/// [`ResolvedTracker::tracks`] and [`ResolvedTracker::filter_fingerprint`].
pub fn normalized_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|tag| normalize_tag(tag))
        .filter(|tag| !tag.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Resolve the configured bindings (or auto-detect one) against the checkout at `root`. With an
/// explicit `[[tracker]]` list, each code-host binding missing `project` derives it from its
/// configured git remote; a binding that cannot resolve is DROPPED — the same silent posture as
/// the old `gh repo view` failure, because this runs on every index open and an unresolvable
/// binding is a no-op there, not an error. With NO configured binding, the `origin` remote picks
/// the provider by host and the project from the URL path; Jira is never auto-detected.
pub fn resolve_trackers(bindings: &[TrackerConfig], root: &Path) -> Vec<ResolvedTracker> {
    if bindings.is_empty() {
        return auto_detect_tracker(root).into_iter().collect();
    }
    bindings.iter().filter_map(|binding| resolve_binding(binding, root)).collect()
}

fn resolve_binding(binding: &TrackerConfig, root: &Path) -> Option<ResolvedTracker> {
    let (project, derived_base_url) = match &binding.project {
        Some(project) => (project.clone(), None),
        // Config validation already demands an explicit Jira project; belt-and-braces here so a
        // hand-built binding can never derive a Jira "project" from a git remote.
        None if !binding.provider.is_code_host() => return None,
        None => {
            let url = git_remote_url(root, &binding.remote)?;
            let parts = parse_git_remote_url(&url)?;
            // An explicit provider may identify an otherwise unknown self-hosted forge, but a
            // host that names a DIFFERENT known provider cannot lend its project identity.
            if detect_provider(&parts.host).is_some_and(|provider| provider != binding.provider) {
                return None;
            }
            let project = remote_url_project(&parts, binding.provider)?;
            let base_url = remote_base_url(&parts, binding.provider);
            (project, base_url)
        },
    };
    Some(ResolvedTracker {
        provider: binding.provider,
        project,
        base_url: binding.base_url.clone().or(derived_base_url),
        auth: binding.auth.clone(),
        authentication: super::transport::authentication(binding.auth.as_ref()),
        tags: binding.tags.clone(),
    })
}

/// Zero-config detection from the `origin` remote URL — the same detection `resolve_trackers`
/// applies when no `[[tracker]]` binding is configured. Exposed so callers (the `rag-rat init`
/// wizard) can show what auto-detection would bind and gate tracker-dependent features on it.
pub fn auto_detect_tracker(root: &Path) -> Option<ResolvedTracker> {
    detect_tracker_for_remote(root, "origin")
}

/// Detect the tracker a specific git remote resolves to — the same host/path derivation
/// auto-detection applies to `origin`, for any named remote. Returns `None` when the remote is
/// missing or its host is not a recognized code host. Exposed so the `rag-rat init` wizard can
/// verify a configured non-`origin` remote instead of trusting it.
pub fn detect_tracker_for_remote(root: &Path, remote: &str) -> Option<ResolvedTracker> {
    detect_tracker_from_remote_url(&git_remote_url(root, remote)?)
}

/// Detect (provider, project, base_url) from one git remote URL: `github.com` → GitHub, a
/// `gitlab.`-containing host (cloud or self-hosted) → GitLab, `bitbucket.org` → Bitbucket.
/// Self-hosted GitHub/Bitbucket — and Jira, always — need an explicit `[[tracker]]` binding.
pub fn detect_tracker_from_remote_url(url: &str) -> Option<ResolvedTracker> {
    let parts = parse_git_remote_url(url)?;
    let provider = detect_provider(&parts.host)?;
    let project = remote_url_project(&parts, provider)?;
    let base_url = remote_base_url(&parts, provider);
    Some(ResolvedTracker {
        provider,
        project,
        base_url,
        auth: None,
        authentication: TrackerAuthentication::AuthMissing,
        tags: Vec::new(),
    })
}

/// A self-hosted remote's authority is part of provider identity: both URL parsing and transport
/// must use it. Cloud remotes keep `None` so the provider default remains canonical.
fn remote_base_url(parts: &GitRemoteParts, provider: Tracker) -> Option<String> {
    (Some(parts.host.as_str()) != cloud_host(provider))
        .then(|| parts.base_url.clone().unwrap_or_else(|| format!("https://{}", parts.host)))
}

pub(crate) fn detect_provider(host: &str) -> Option<Tracker> {
    if host == "github.com" {
        Some(Tracker::Github)
    } else if host.contains("gitlab.") {
        // Substring, not equality: self-hosted GitLab conventionally lives at a
        // `gitlab.<company>.<tld>` host, and `gitlab.com` itself matches too.
        Some(Tracker::Gitlab)
    } else if host == "bitbucket.org" {
        Some(Tracker::Bitbucket)
    } else {
        None
    }
}

/// The provider's cloud host, `None` for Jira (every Jira instance is "self-hosted" from the
/// binding's point of view — site URLs are per-tenant).
fn cloud_host(provider: Tracker) -> Option<&'static str> {
    match provider {
        Tracker::Github => Some("github.com"),
        Tracker::Gitlab => Some("gitlab.com"),
        Tracker::Bitbucket => Some("bitbucket.org"),
        Tracker::Jira => None,
    }
}

/// Project path for `provider` from parsed remote parts. GitHub and Bitbucket Cloud projects are
/// exactly `owner/repo`; Bitbucket Data Center clone URLs add a `/scm/` prefix. GitLab keeps the
/// full (possibly subgroup-nested) namespace path.
pub(crate) fn remote_url_project(parts: &GitRemoteParts, provider: Tracker) -> Option<String> {
    match provider {
        Tracker::Github => (parts.segments.len() == 2).then(|| parts.segments.join("/")),
        Tracker::Bitbucket => match parts.segments.as_slice() {
            [scm, project, repo] if scm.eq_ignore_ascii_case("scm") =>
                Some(format!("{project}/{repo}")),
            [workspace, repo] => Some(format!("{workspace}/{repo}")),
            _ => None,
        },
        Tracker::Gitlab => (parts.segments.len() >= 2).then(|| parts.segments.join("/")),
        Tracker::Jira => None,
    }
}

/// Host + project path parsed out of a git remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitRemoteParts {
    /// Lowercased host, userinfo and port stripped.
    pub host: String,
    pub base_url: Option<String>,
    /// Path segments with the `.git` suffix stripped (e.g. `["group", "sub", "repo"]`).
    pub segments: Vec<String>,
}

/// Parse a git remote URL: the scheme forms (`https://host/path`, `http://`, `ssh://git@host
/// [:port]/path`, `git://host/path`) and the scp-like form (`git@host:path`). `None` for local
/// paths, unsupported schemes, or an empty/degenerate path.
pub(crate) fn parse_git_remote_url(url: &str) -> Option<GitRemoteParts> {
    let url = url.trim();
    let (host_part, path, scheme) = if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "ssh" | "git") {
            return None;
        }
        let (host, path) = rest.split_once('/')?;
        (host, path, Some(scheme))
    } else {
        // scp-like `git@host:path`. A local path (`/srv/git/repo`, `../repo`) has no `:`; a
        // slash BEFORE the first `:` means the colon is inside a path, not a host separator.
        let (host_part, path) = url.split_once(':')?;
        if host_part.contains('/') || path.starts_with("//") {
            return None;
        }
        (host_part, path, None)
    };
    // Remote URLs may embed transport credentials. They are neither provider identity nor safe
    // configuration: strip them once, then use only this sanitized authority for BOTH host
    // detection and a derived HTTP(S) base URL.
    let sanitized_authority = host_part.rsplit_once('@').map_or(host_part, |(_, host)| host);
    // Strip a `:port` (the scheme forms carry it on the host part). Bracketed IPv6 literals are
    // not code-host names — rejected below with every other non-hostname shape.
    let host = sanitized_authority
        .split_once(':')
        .map_or(sanitized_authority, |(host, _)| host)
        .to_ascii_lowercase();
    if host.is_empty() || host.contains(['[', ']']) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let segments = path.split('/').map(str::to_string).collect::<Vec<_>>();
    if segments.iter().any(String::is_empty) {
        return None;
    }
    let base_url = scheme
        .filter(|scheme| matches!(scheme.as_str(), "http" | "https"))
        .map(|scheme| format!("{scheme}://{}", sanitized_authority.to_ascii_lowercase()));
    Some(GitRemoteParts { host, base_url, segments })
}

/// The configured URL of `remote` for the repo containing `root` (`remote.<name>.url`), read
/// from git config via gix — no subprocess, works offline, and resolves the SHARED repo config
/// from any linked worktree.
fn git_remote_url(root: &Path, remote: &str) -> Option<String> {
    let repo = rag_rat_base::repo_discover::discover_repo(root).ok()?;
    let key = format!("remote.{remote}.url");
    let url = repo.config_snapshot().string(key.as_str())?.to_string();
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

#[cfg(test)]
mod tests {

    use super::*;

    fn github(project: &str) -> Option<ResolvedTracker> {
        Some(ResolvedTracker {
            provider: Tracker::Github,
            project: project.to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        })
    }

    #[test]
    fn resolved_binding_defers_token_commands_but_detects_missing_env() {
        let binding = |command: &str| TrackerConfig {
            provider: Tracker::Gitlab,
            project: Some("group/repo".to_string()),
            remote: "origin".to_string(),
            base_url: None,
            auth: Some(TrackerAuth::TokenCommand(command.to_string())),
            tags: Vec::new(),
        };
        let root = Path::new(".");
        let configured = resolve_trackers(&[binding("exit 3")], root);
        assert_eq!(configured[0].authentication, TrackerAuthentication::AuthConfigured);
        let mut missing_binding = binding("echo unused");
        missing_binding.auth = Some(TrackerAuth::Env("RAG_RAT_TEST_UNSET_TOKEN_VAR".to_string()));
        let missing = resolve_trackers(&[missing_binding], root);
        assert_eq!(missing[0].authentication, TrackerAuthentication::AuthMissing);
    }

    #[test]
    fn auto_detect_covers_the_cloud_hosts_over_ssh_and_https() {
        // github.com — scp-like ssh + https, `.git` suffix optional.
        assert_eq!(
            detect_tracker_from_remote_url("git@github.com:owner/repo.git"),
            github("owner/repo")
        );
        assert_eq!(
            detect_tracker_from_remote_url("https://github.com/owner/repo"),
            github("owner/repo")
        );
        assert_eq!(
            detect_tracker_from_remote_url("https://github.com/owner/repo.git"),
            github("owner/repo")
        );
        assert_eq!(
            detect_tracker_from_remote_url("ssh://git@github.com/owner/repo.git"),
            github("owner/repo")
        );

        // gitlab.com keeps multi-level subgroup paths.
        let gitlab = detect_tracker_from_remote_url("git@gitlab.com:group/sub/repo.git").unwrap();
        assert_eq!(gitlab.provider, Tracker::Gitlab);
        assert_eq!(gitlab.project, "group/sub/repo");
        assert_eq!(gitlab.base_url, None);

        let bitbucket =
            detect_tracker_from_remote_url("https://user@bitbucket.org/workspace/repo.git")
                .unwrap();
        assert_eq!(bitbucket.provider, Tracker::Bitbucket);
        assert_eq!(bitbucket.project, "workspace/repo");
        assert_eq!(bitbucket.base_url, None);
    }

    #[test]
    fn auto_detect_resolves_self_hosted_gitlab_by_host_substring() {
        // Self-hosted GitLab: provider by the `gitlab.` substring, base_url from the host, an
        // ssh port stripped.
        let tracker =
            detect_tracker_from_remote_url("ssh://git@gitlab.example.com:2222/group/sub/repo.git")
                .unwrap();
        assert_eq!(tracker.provider, Tracker::Gitlab);
        assert_eq!(tracker.project, "group/sub/repo");
        assert_eq!(tracker.base_url.as_deref(), Some("https://gitlab.example.com"));

        let https = detect_tracker_from_remote_url("https://gitlab.example.com/team/app").unwrap();
        assert_eq!(https.base_url.as_deref(), Some("https://gitlab.example.com"));

        let http_port =
            detect_tracker_from_remote_url("http://gitlab.example.com:8080/team/app.git").unwrap();
        assert_eq!(http_port.base_url.as_deref(), Some("http://gitlab.example.com:8080"));

        let credentialed = detect_tracker_from_remote_url(
            "https://oauth2:TOKEN@gitlab.example.com:8443/team/app.git",
        )
        .unwrap();
        assert_eq!(credentialed.project, "team/app");
        assert_eq!(credentialed.base_url.as_deref(), Some("https://gitlab.example.com:8443"));
    }

    #[test]
    fn auto_detect_rejects_unknown_hosts_local_paths_and_degenerate_urls() {
        // Unknown hosts (incl. self-hosted GitHub Enterprise — explicit config only), local
        // paths, unsupported schemes, and non-project path depths all stay undetected.
        for url in [
            "git@codeberg.org:owner/repo.git",
            "git@github.example.com:owner/repo.git", // GHE is NOT auto-detected
            "/srv/git/repo.git",
            "../sibling/repo",
            "file:///srv/git/repo.git",
            "https://github.com/owner", // too shallow for owner/repo
            "https://github.com/owner/repo/extra", // too deep for owner/repo
            "https://github.com//repo.git", // empty segment
            "https://gitlab.com/solo",  // gitlab needs namespace/project
        ] {
            assert_eq!(detect_tracker_from_remote_url(url), None, "{url}");
        }
    }

    #[test]
    fn parse_git_remote_url_normalizes_host_and_path() {
        let parts =
            parse_git_remote_url("SSH://Git@GitLab.Example.COM:2222/Group/Repo.git").unwrap();
        assert_eq!(parts.host, "gitlab.example.com");
        assert_eq!(parts.segments, ["Group", "Repo"]); // path case is meaningful; host case is not
        assert_eq!(parse_git_remote_url(""), None);
        assert_eq!(parse_git_remote_url("git@github.com:"), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        rag_rat_base::test_git::run(dir, args);
    }

    fn temp_git_repo() -> rag_rat_base::test_scratch::ScratchDir {
        let root = rag_rat_base::test_scratch::ScratchDir::new("trackers");
        git(&root, &["init", "-q"]);
        root
    }

    #[test]
    fn resolve_trackers_auto_detects_from_the_origin_remote_when_unconfigured() {
        let root = temp_git_repo();
        git(&root, &["remote", "add", "origin", "git@github.com:owner/repo.git"]);
        let trackers = resolve_trackers(&[], &root);
        assert_eq!(trackers, vec![github("owner/repo").unwrap()]);
    }

    #[test]
    fn resolve_trackers_is_empty_without_a_recognizable_remote() {
        let root = temp_git_repo();
        assert_eq!(resolve_trackers(&[], &root), Vec::new(), "no remote at all");
        git(&root, &["remote", "add", "origin", "git@codeberg.org:owner/repo.git"]);
        assert_eq!(resolve_trackers(&[], &root), Vec::new(), "unknown host");
    }

    #[test]
    fn resolve_trackers_derives_a_projectless_binding_from_its_named_remote() {
        let root = temp_git_repo();
        git(&root, &["remote", "add", "origin", "git@github.com:owner/repo.git"]);
        git(&root, &["remote", "add", "upstream", "git@gitlab.example.com:group/sub/repo.git"]);
        git(&root, &[
            "remote",
            "add",
            "bitbucket",
            "https://bitbucket.example.com/scm/PROJ/repo.git",
        ]);
        let bindings = vec![
            TrackerConfig {
                provider: Tracker::Gitlab,
                project: None,
                remote: "upstream".to_string(),
                base_url: None,
                auth: None,
                tags: vec!["bug".to_string()],
            },
            TrackerConfig {
                provider: Tracker::Bitbucket,
                project: None,
                remote: "bitbucket".to_string(),
                base_url: None,
                auth: None,
                tags: Vec::new(),
            },
            // A recognized host owned by another provider cannot supply this binding's identity.
            TrackerConfig {
                provider: Tracker::Gitlab,
                project: None,
                remote: "origin".to_string(),
                base_url: None,
                auth: None,
                tags: Vec::new(),
            },
            // Explicit project: no remote lookup, `remote` is irrelevant.
            TrackerConfig {
                provider: Tracker::Jira,
                project: Some("PROJ".to_string()),
                remote: "origin".to_string(),
                base_url: Some("https://example.atlassian.net".to_string()),
                auth: None,
                tags: Vec::new(),
            },
            // Unresolvable code-host binding (remote absent): dropped, not an error.
            TrackerConfig {
                provider: Tracker::Bitbucket,
                project: None,
                remote: "missing".to_string(),
                base_url: None,
                auth: None,
                tags: Vec::new(),
            },
        ];
        let trackers = resolve_trackers(&bindings, &root);
        assert_eq!(trackers.len(), 3);
        assert_eq!(
            (trackers[0].provider, trackers[0].project.as_str(), trackers[0].base_url.as_deref(),),
            (Tracker::Gitlab, "group/sub/repo", Some("https://gitlab.example.com"))
        );
        assert_eq!(
            (trackers[1].provider, trackers[1].project.as_str(), trackers[1].base_url.as_deref(),),
            (Tracker::Bitbucket, "PROJ/repo", Some("https://bitbucket.example.com"))
        );
        assert_eq!((trackers[2].provider, trackers[2].project.as_str()), (Tracker::Jira, "PROJ"));
    }

    fn tagged(tags: &[&str]) -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn tag_filter_is_or_matched_and_case_insensitive_with_empty_meaning_all() {
        let all = tagged(&[]);
        assert!(all.tracks(["anything"]) && all.tracks([]));

        let filtered = tagged(&["Bug", "perf"]);
        assert!(filtered.tracks(["BUG"]), "case-insensitive both ways");
        assert!(filtered.tracks(["docs", "Perf"]), "OR across the list");
        assert!(!filtered.tracks(["docs"]));
        assert!(!filtered.tracks([]), "a label-bearing kind with no labels is filtered out");
    }

    #[test]
    fn filter_fingerprint_is_stable_under_case_order_and_duplicates() {
        let canonical = tagged(&["bug", "perf"]).filter_fingerprint();
        assert_eq!(canonical.len(), 64);
        assert_eq!(tagged(&["Perf", "BUG", "bug"]).filter_fingerprint(), canonical);
        assert_ne!(tagged(&["bug"]).filter_fingerprint(), canonical, "narrowing changes it");
        assert_ne!(
            tagged(&["bug", "perf", "docs"]).filter_fingerprint(),
            canonical,
            "widening changes it"
        );
        assert_eq!(tagged(&[]).filter_fingerprint(), "", "track-all is the empty sentinel");
        // The join is boundary-unambiguous: ["ab","c"] and ["a","bc"] must differ.
        assert_ne!(
            tagged(&["ab", "c"]).filter_fingerprint(),
            tagged(&["a", "bc"]).filter_fingerprint()
        );
    }
}
