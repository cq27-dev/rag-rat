mod api;
mod evidence;
mod parse;
mod store;
mod sync;
mod trackers;
// Shared HTTP substrate for the native provider clients (#589): reqwest transport, per-
// (provider, host, token) rate governor, env/token_command auth. `dead_code`/`unused_imports`
// are allowed until the first native client consumes it (#591) — drop the allow there.
#[allow(dead_code, unused_imports)]
pub(crate) mod transport;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub(crate) use api::*;
pub(crate) use evidence::*;
pub(crate) use parse::*;
pub use parse::{TrackerParsedRef, parse_tracker_refs};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub(crate) use store::*;
pub(crate) use sync::*;
pub(crate) use trackers::resolve_trackers;
pub use trackers::{ResolvedTracker, detect_tracker_from_remote_url, normalized_tags};

pub use crate::config::Tracker;
use crate::index::now_ms;

/// Resolved tracker context, injected into the sync/query paths instead of being resolved inside
/// the library. Resolution runs ONLY at the real-usage boundary (`IndexDatabase::open_config`)
/// and never in tests, which pass an explicit context (#60): it reads local git state (the
/// remote URL). Authentication is carried per binding; there is no process-global `gh`
/// capability bit.
#[derive(Debug, Clone, Default)]
pub struct PapertrailContext {
    /// Resolved tracker bindings, in config order (or the single auto-detected code host).
    /// Production discovery and rationale lookup consume every binding. Live network sync remains
    /// GitHub-only until the provider-client PRs build on [`transport`].
    pub trackers: Vec<ResolvedTracker>,
}

impl PapertrailContext {
    /// Resolve from the repo config: `[[tracker]]` bindings when present, otherwise a binding
    /// auto-detected from the git `origin` remote. Call ONLY at the real-usage boundary
    /// (open_config), never inside the library internals or tests.
    pub(crate) fn resolve(config: &crate::config::Config) -> Self {
        Self { trackers: resolve_trackers(&config.trackers, &config.root) }
    }

    /// An explicit GitHub-repo context that never touches git or `gh` — for tests and non-gh
    /// callers.
    pub(crate) fn new(default_repo: Option<&str>) -> Self {
        let trackers = default_repo
            .map(|project| ResolvedTracker {
                provider: Tracker::Github,
                project: project.to_string(),
                base_url: None,
                auth: None,
                authentication: TrackerAuthentication::AuthMissing,
                tags: Vec::new(),
            })
            .into_iter()
            .collect();
        Self { trackers }
    }

    /// `owner/repo` qualifying a manually requested legacy GitHub reference. Production
    /// discovery and rationale parsing use all bindings through [`parse_tracker_refs`].
    fn default_repo(&self) -> Option<&str> {
        self.trackers
            .iter()
            .find(|tracker| tracker.provider == Tracker::Github)
            .map(|tracker| tracker.project.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailStatus {
    pub refs: u64,
    pub issues: u64,
    pub change_requests: u64,
    pub comments: u64,
    pub last_sync_ms: Option<i64>,
    pub capabilities: Vec<TrackerCapability>,
}

/// Per-binding auth and live-sync capability. Environment auth reflects token presence; command
/// auth reflects a configured source without running shell code on read paths. The transport
/// resolves either source when a network client is built.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TrackerCapability {
    pub tracker: Tracker,
    pub project: String,
    pub authentication: TrackerAuthentication,
    pub synchronization: TrackerSynchronization,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerAuthentication {
    AuthConfigured,
    AuthMissing,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerSynchronization {
    LegacyGithubCli,
    LegacyGithubCliMissing,
    ProviderClientPending,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailSyncReport {
    pub offline: bool,
    pub discovered_refs: usize,
    pub skipped_refs: usize,
    pub failed_refs: usize,
    pub synced_items: usize,
    pub errors: Vec<PapertrailSyncError>,
    pub status: PapertrailStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailSyncError {
    pub tracker: Tracker,
    pub project: String,
    pub item_key: String,
    pub status: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct PapertrailSyncProgress {
    pub current: usize,
    pub total: usize,
    pub project: String,
    pub item_key: String,
    pub action: PapertrailSyncAction,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PapertrailSyncAction {
    Syncing,
    Skipped,
    Synced,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailRef {
    pub tracker: Tracker,
    pub project: String,
    pub item_key: String,
    pub item_kind: Option<ItemKind>,
    pub ref_kind: String,
    pub source_kind: String,
    pub source_path: Option<String>,
    pub source_commit: Option<String>,
    pub source_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailEvidence {
    pub tracker: String,
    pub project: String,
    /// Kind of the ITEM this row belongs to: `issue` | `change_request`.
    pub item_kind: String,
    pub item_key: String,
    /// Which mirror row matched: the item's own text (`item`) or one of its comments (`comment`).
    pub doc_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_id: Option<String>,
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub classification: String,
    pub evidence_kind: &'static str,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Papertrail {
    pub current_source: Option<CurrentSourceEvidence>,
    pub evidence: Vec<PapertrailEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback_evidence: Vec<PapertrailEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentSourceEvidence {
    pub chunk_id: Option<i64>,
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub symbol: Option<String>,
}

/// Provider-neutral item kind. GitHub's shared issue/PR numbering is the exception, not the
/// rule — GitLab issues and merge requests live in separate iid namespaces and Jira has no
/// change requests at all — so the kind is part of an item's identity, never inferred.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    ChangeRequest,
}

impl ItemKind {
    /// The exact persisted token (`papertrail_items.item_kind` and friends).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown item kind token `{value}`"))
    }
}

/// One tracker item (issue or change request), normalized across providers. `item_key` is a
/// string because provider keys are not uniformly numeric (Jira: `PROJ-123`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PapertrailItem {
    /// Provider project path (`owner/repo` on GitHub).
    pub project: String,
    pub item_kind: ItemKind,
    pub item_key: String,
    pub url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Change requests only; `None` for issues and unmerged change requests.
    pub merged_at: Option<String>,
}

/// One comment on a tracker item, unified across the provider review models: a plain thread
/// comment carries neither optional; a review event carries `review_state` (e.g. `APPROVED`);
/// a file-anchored comment carries `anchor_path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PapertrailComment {
    pub project: String,
    /// Kind of the parent item — part of the parent identity, because namespaced providers
    /// (GitLab) can have an issue and a change request sharing the same key.
    pub item_kind: ItemKind,
    pub item_key: String,
    /// Provider comment id, normalized into a source-qualified namespace when one provider has
    /// multiple comment resources (for example `comment:9`, `review:9`, `review_comment:9`).
    pub comment_id: String,
    pub url: Option<String>,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// `Some` marks a review event (approval / changes requested / review summary).
    pub review_state: Option<String>,
    /// `Some` marks a file-anchored comment; the value is the repo-relative path.
    pub anchor_path: Option<String>,
}

/// Position in a provider's `updated_at`-ordered item/comment stream. The mirror sync keeps one
/// persisted cursor per (tracker, project); a page fetch returns the cursor to continue from.
#[derive(Debug, Clone, Default)]
pub struct PageCursor {
    /// Fetch entries updated at-or-after this provider-format timestamp.
    pub updated_since: Option<String>,
    /// Provider-opaque continuation token, when the provider paginates by token.
    pub page_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ItemsPage {
    pub items: Vec<PapertrailItem>,
    pub next: Option<PageCursor>,
}

#[derive(Debug, Clone)]
pub struct CommentsPage {
    pub comments: Vec<PapertrailComment>,
    pub next: Option<PageCursor>,
}

/// Advance decision for a freshness probe: the newest `updated_at` the provider reported vs the
/// cursor's position. `Some(latest)` when the tracker moved past the cursor, `None` when nothing
/// did (provider timestamps compare lexicographically — ISO-8601 UTC).
pub(crate) fn probe_advance(latest: Option<String>, since: Option<&str>) -> Option<String> {
    match (latest, since) {
        (Some(latest), Some(since)) if latest.as_str() <= since => None,
        (latest, _) => latest,
    }
}

/// Provider client behind the papertrail sync. Implementations exist per tracker (GitHub today;
/// GitLab / Bitbucket / Jira to follow) and own URL building, pagination, and payload mapping.
///
/// The futures are driven exclusively through [`block_on`] on a private current-thread runtime —
/// nothing here is spawned — so the omitted `Send` bounds of `async fn` in a public trait are
/// deliberate, not an oversight.
#[allow(async_fn_in_trait)]
pub trait PapertrailClient {
    /// One item. `kind` is part of the identity: namespaced providers (GitLab) MUST treat it as
    /// binding — an issue and a change request can share a key — while a shared-numbering
    /// provider (GitHub) treats it as advisory and returns the item with its kind resolved.
    async fn item(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<PapertrailItem>;
    /// Every comment on one item, unified (thread comments, review events, file-anchored).
    /// `kind` identifies the parent exactly as in [`Self::item`] — pass the RESOLVED kind.
    async fn item_comments(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>>;
    /// A page of items updated since the cursor, newest first (mirror backfill/delta).
    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage>;
    /// A page of comments updated since the cursor (mirror delta).
    async fn comments_page(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage>;
    /// Cheap "anything changed?" check: `Some(latest updated_at)` when the tracker has items
    /// newer than the cursor, `None` when nothing moved.
    async fn freshness_probe(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<Option<String>>;
}

/// Drive a papertrail future to completion from the synchronous call paths (CLI, maintenance).
/// Uses a private lazily-built current-thread runtime. Callable from inside a multi-thread tokio
/// runtime too (the worker is demoted via `block_in_place`, matching how the pre-async code
/// simply blocked). A caller on a CURRENT-THREAD ambient runtime has no sound way to block the
/// thread — and the future cannot be offloaded, it borrows the `!Sync` connection — so that case
/// returns an error instead of panicking; such callers need an async entry point.
pub(crate) fn block_on<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("papertrail runtime")
    });
    match tokio::runtime::Handle::try_current() {
        Err(_) => runtime.block_on(future),
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            tokio::task::block_in_place(|| runtime.block_on(future)),
        Ok(_) => anyhow::bail!(
            "papertrail sync cannot block inside a current-thread tokio runtime; drive the async \
             sync entry points on that runtime instead"
        ),
    }
}

/// The GitHub provider, currently backed by the `gh` CLI (`gh api`); replaced by a native HTTP
/// client when the mirror sync lands. The subprocess calls block inside the async methods, which
/// is acceptable on the private [`block_on`] runtime — nothing else shares it.
pub struct GitHubClient;

impl PapertrailClient for GitHubClient {
    async fn item(
        &self,
        project: &str,
        _kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<PapertrailItem> {
        // GitHub numbers issues and PRs in one namespace, so the requested kind is advisory:
        // the issues endpoint resolves either, and the returned item carries the real kind.
        let value = gh_api_json(&format!("repos/{project}/issues/{key}"))?;
        let mut item = item_from_issue_value(project, &value);
        if item.item_kind == ItemKind::ChangeRequest {
            enrich_item_from_pull_value(
                &mut item,
                &gh_api_json(&format!("repos/{project}/pulls/{key}"))?,
            );
        }
        Ok(item)
    }

    async fn item_comments(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>> {
        let mut comments = Vec::new();
        for value in gh_api_paginated(&format!("repos/{project}/issues/{key}/comments"))? {
            comments.push(comment_from_value(project, kind, key, &value));
        }
        // Review endpoints exist only for change requests; the caller passes the RESOLVED kind,
        // so any failure here (rate limit, auth, network) is a real failure and must propagate —
        // swallowing it would cache an incomplete comment list and mark the ref synced forever.
        if kind == ItemKind::ChangeRequest {
            for value in gh_api_paginated(&format!("repos/{project}/pulls/{key}/reviews"))? {
                comments.push(review_to_comment_from_value(project, kind, key, &value));
            }
            for value in gh_api_paginated(&format!("repos/{project}/pulls/{key}/comments"))? {
                comments.push(review_comment_to_comment_from_value(project, kind, key, &value));
            }
        }
        Ok(comments)
    }

    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage> {
        // `gh api --paginate` drains every page in one call, so the returned cursor is always
        // terminal. The native client replaces this with true per-page fetches.
        let mut path =
            format!("repos/{project}/issues?state=all&sort=updated&direction=desc&per_page=100");
        if let Some(since) = cursor.updated_since.as_deref() {
            path.push_str(&format!("&since={since}"));
        }
        let items = gh_api_paginated(&path)?
            .iter()
            .map(|value| item_from_issue_value(project, value))
            .collect();
        Ok(ItemsPage { items, next: None })
    }

    async fn comments_page(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        // Repo-wide comment streams; review *events* have no repo-wide updated-since endpoint,
        // so the mirror delta re-walks changed items for those (native-client concern).
        let since = cursor
            .updated_since
            .as_deref()
            .map(|since| format!("?since={since}"))
            .unwrap_or_default();
        let mut comments = Vec::new();
        for value in gh_api_paginated(&format!("repos/{project}/issues/comments{since}"))? {
            if let Some(comment) = repo_comment_from_value(project, &value, false) {
                comments.push(comment);
            }
        }
        for value in gh_api_paginated(&format!("repos/{project}/pulls/comments{since}"))? {
            if let Some(comment) = repo_comment_from_value(project, &value, true) {
                comments.push(comment);
            }
        }
        Ok(CommentsPage { comments, next: None })
    }

    async fn freshness_probe(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<Option<String>> {
        let value = gh_api_json(&format!(
            "repos/{project}/issues?state=all&sort=updated&direction=desc&per_page=1"
        ))?;
        let latest = value
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["updated_at"].as_str())
            .map(str::to_string);
        Ok(probe_advance(latest, cursor.updated_since.as_deref()))
    }
}
#[derive(Default)]
pub(crate) struct SyncRefsReport {
    synced_items: usize,
    skipped_refs: usize,
    failed_refs: usize,
    errors: Vec<PapertrailSyncError>,
}

pub(crate) struct FtsRow<'a> {
    tracker: &'a str,
    project: &'a str,
    item_kind: &'a str,
    item_key: &'a str,
    /// `item` for the item's own title/body row, `comment` for a comment row.
    doc_kind: &'a str,
    /// The provider comment id for a `comment` row; empty string on `item` rows.
    comment_id: &'a str,
    url: &'a str,
    title: &'a str,
    body: &'a str,
    /// The active repo that owns this papertrail row (V041). Stamped into
    /// `papertrail_fts.repo_id` (UNINDEXED) so a MATCH in a consolidated DB filters to one repo.
    repo_id: &'a str,
}

/// A tracker reference parsed out of source/commit/branch text. The grammar is GitHub's today
/// (`#N`, `owner/repo#N`, `GH-N`, issue/PR URLs), so the parsed key is numeric; `project` is the
/// `owner/repo` path.
#[derive(Debug, Clone)]
pub(crate) struct ParsedRef {
    project: String,
    number: i64,
    kind: String,
}

impl ParsedRef {
    /// Lift a parsed GitHub-grammar ref into a storable [`PapertrailRef`] with its discovery
    /// provenance. The grammar is GitHub-only today, so the tracker is fixed here.
    pub(crate) fn into_ref(
        self,
        source_kind: &str,
        source_path: Option<String>,
        source_commit: Option<String>,
        source_text: String,
    ) -> PapertrailRef {
        PapertrailRef {
            tracker: Tracker::Github,
            project: self.project,
            item_key: self.number.to_string(),
            item_kind: None,
            ref_kind: self.kind,
            source_kind: source_kind.to_string(),
            source_path,
            source_commit,
            source_text,
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    // The persisted token sets are CLOSED and exact: `from_db_str` must round-trip every
    // `as_db_str` output and reject everything else (no trimming, no case folding) — a papertrail
    // row read back with a drifted token is a bug to surface, not to coerce.
    #[test]
    fn tracker_tokens_are_exact_and_closed() {
        for (tracker, token) in [
            (Tracker::Github, "github"),
            (Tracker::Gitlab, "gitlab"),
            (Tracker::Bitbucket, "bitbucket"),
            (Tracker::Jira, "jira"),
        ] {
            assert_eq!(tracker.as_db_str(), token);
            assert_eq!(Tracker::from_db_str(token).unwrap(), tracker);
        }
        for rejected in ["GitHub", "GITHUB", " github", "github ", "sourcehut", ""] {
            assert!(Tracker::from_db_str(rejected).is_err(), "must reject `{rejected}`");
        }
    }

    #[test]
    fn item_kind_tokens_are_exact_and_closed() {
        assert_eq!(ItemKind::Issue.as_db_str(), "issue");
        assert_eq!(ItemKind::ChangeRequest.as_db_str(), "change_request");
        assert_eq!(ItemKind::from_db_str("issue").unwrap(), ItemKind::Issue);
        assert_eq!(ItemKind::from_db_str("change_request").unwrap(), ItemKind::ChangeRequest);
        for rejected in ["Issue", "pull", "change-request", " issue", ""] {
            assert!(ItemKind::from_db_str(rejected).is_err(), "must reject `{rejected}`");
        }
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[test]
    fn probe_advance_only_reports_movement_past_the_cursor() {
        assert_eq!(probe_advance(None, None), None);
        assert_eq!(probe_advance(None, Some("2026-01-01T00:00:00Z")), None);
        assert_eq!(
            probe_advance(Some("2026-01-02T00:00:00Z".into()), None).as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        assert_eq!(
            probe_advance(Some("2026-01-02T00:00:00Z".into()), Some("2026-01-01T00:00:00Z"))
                .as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        // At-or-before the cursor is quiet — equality must not loop the delta lane forever.
        assert_eq!(
            probe_advance(Some("2026-01-01T00:00:00Z".into()), Some("2026-01-01T00:00:00Z")),
            None
        );
        assert_eq!(
            probe_advance(Some("2025-12-31T00:00:00Z".into()), Some("2026-01-01T00:00:00Z")),
            None
        );
    }

    // The bridge must behave like the pre-async code (block) wherever blocking is sound, and
    // refuse gracefully where it is not — never panic.
    #[test]
    fn block_on_bridges_from_a_multi_thread_runtime_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let value = runtime
            .block_on(async {
                tokio::task::spawn(async { block_on(async { anyhow::Ok(7) }) }).await.unwrap()
            })
            .unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn block_on_refuses_gracefully_on_a_current_thread_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err =
            runtime.block_on(async { block_on(async { anyhow::Ok(7) }) }).unwrap_err().to_string();
        assert!(err.contains("current-thread"), "{err}");
    }
}
