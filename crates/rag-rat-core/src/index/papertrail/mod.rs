mod api;
pub mod autosync;
mod evidence;
mod github;
mod gitlab;
mod mirror;
mod parse;
mod schedule;
mod store;
mod sync;
mod trackers;
pub(crate) mod transport;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

pub(crate) use api::*;
pub(crate) use evidence::*;
pub(crate) use github::*;
pub(crate) use gitlab::*;
pub use mirror::MirrorBindingReport;
pub(crate) use mirror::{MirrorContinuation, load_mirror_continuation, mirror_binding};
pub(crate) use parse::*;
pub use parse::{TrackerParsedRef, parse_tracker_refs};
use rusqlite::{Connection, OptionalExtension, params};
pub use schedule::AutosyncRequest;
pub(crate) use schedule::*;
use serde::{Deserialize, Serialize};
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
    /// Production discovery, rationale lookup, and manual mirror sync consume every binding.
    pub trackers: Vec<ResolvedTracker>,
    pub(crate) transport_options: transport::TransportOptions,
    pub(crate) schedule: crate::config::PapertrailConfig,
}

impl PapertrailContext {
    /// Resolve from the repo config: `[[tracker]]` bindings when present, otherwise a binding
    /// auto-detected from the git `origin` remote. Call ONLY at the real-usage boundary
    /// (open_config), never inside the library internals or tests.
    pub(crate) fn resolve(config: &crate::config::Config) -> Self {
        let mut trackers = resolve_trackers(&config.trackers, &config.root);
        // GitHub's provider-native fallback is environment auth. Materialize the ENV-VAR NAME
        // into the binding (never its value) so status and transport use the same per-binding
        // capability instead of a process-global `gh_available` bit.
        apply_implicit_github_auth(&mut trackers, |name| std::env::var(name).ok());
        Self {
            trackers,
            transport_options: transport::TransportOptions::from(&config.papertrail),
            schedule: config.papertrail.clone(),
        }
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
        Self {
            trackers,
            transport_options: transport::TransportOptions::default(),
            schedule: crate::config::PapertrailConfig::default(),
        }
    }

    /// `owner/repo` qualifying a manually requested legacy GitHub reference. Production
    /// discovery and rationale parsing use all bindings through [`parse_tracker_refs`].
    #[cfg(test)]
    fn default_repo(&self) -> Option<&str> {
        self.trackers
            .iter()
            .find(|tracker| tracker.provider == Tracker::Github)
            .map(|tracker| tracker.project.as_str())
    }
}

fn apply_implicit_github_auth(
    trackers: &mut [ResolvedTracker],
    env: impl Fn(&str) -> Option<String>,
) {
    for tracker in trackers {
        if tracker.provider == Tracker::Github
            && tracker.base_url.is_none()
            && tracker.auth.is_none()
            && let Some(name) = ["GH_TOKEN", "GITHUB_TOKEN"]
                .into_iter()
                .find(|name| env(name).is_some_and(|value| !value.trim().is_empty()))
        {
            tracker.auth = Some(crate::config::TrackerAuth::Env(name.to_string()));
            tracker.authentication = TrackerAuthentication::AuthConfigured;
        }
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
    pub bindings: Vec<PapertrailBindingStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PapertrailBindingStatus {
    pub tracker: Tracker,
    pub project: String,
    pub last_attempt_ms: Option<i64>,
    pub last_successful_probe_ms: Option<i64>,
    pub last_successful_mirror_ms: Option<i64>,
    pub last_full_walk_ms: Option<i64>,
    pub retry_not_before_ms: Option<i64>,
    pub full_walk_in_progress: bool,
    pub error_class: Option<PapertrailErrorClass>,
    pub error_detail: Option<String>,
    pub overdue: bool,
    pub failed: bool,
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
    Native,
    ProviderClientPending,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailSyncReport {
    pub offline: bool,
    pub discovered_refs: usize,
    pub skipped_refs: usize,
    pub failed_refs: usize,
    pub synced_items: usize,
    /// One resumable outcome per dispatched binding, including rate-governed pauses.
    pub bindings: Vec<MirrorBindingReport>,
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

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct PapertrailSyncProgress {
    pub current: usize,
    pub total: usize,
    pub project: String,
    pub item_key: String,
    pub action: PapertrailSyncAction,
    pub message: Option<String>,
}

#[cfg(test)]
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

/// Whether a provider's item keys are unique across kinds. GitHub's shared issue/PR numbering
/// is the exception (a key names at most one item, and its repo comment feed cannot always name
/// the kind); namespaced providers (GitLab iids, Bitbucket, Jira) always name the kind on their
/// comments, so a comment there must NEVER attach across namespaces.
pub(crate) fn shared_item_numbering(tracker: Tracker) -> bool {
    matches!(tracker, Tracker::Github)
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
    /// Provider facets used by the binding's client-side OR filter (GitHub labels today).
    pub tags: Vec<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageCursor {
    /// Provider-defined logical stream within an operation (for example GitHub issue comments
    /// versus review comments). Each stream owns independent durable progress.
    pub stream: Option<String>,
    /// Fetch entries updated at-or-after this provider-format timestamp.
    pub updated_since: Option<String>,
    /// Strict upper boundary for newest-first historical descent.
    pub updated_before: Option<String>,
    /// Provider-opaque continuation token, when the provider paginates by token.
    pub page_token: Option<String>,
    /// Provider-opaque state needed to interpret a continuation (for example a Search tie
    /// boundary and the number of results already observed). The mirror persists but never
    /// interprets this value.
    pub provider_state: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FreshnessProbe {
    pub updated_since: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FreshnessResult {
    pub latest: Option<String>,
    pub etag: Option<String>,
    pub not_modified: bool,
}

#[derive(Debug, Clone)]
pub struct ItemsPage {
    pub items: Vec<PapertrailItem>,
    pub next: Option<PageCursor>,
    /// Provider-confirmed strict boundary below every item consumed in this logical backfill
    /// page. Compound provider streams may need this because the last physical response does not
    /// necessarily contain the oldest item observed across the whole page.
    pub backfill_boundary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommentsPage {
    pub comments: Vec<PapertrailComment>,
    pub next: Option<PageCursor>,
    /// Provider-confirmed watermark for THIS page beyond the returned comments — set when the
    /// provider's feed can contain entries that map to no comment (GitLab events on commit or
    /// snippet notes). Without it, a first page of only-skipped entries returns no timestamps,
    /// the stream's frontier cannot advance, and every scheduled sync replays the same pages.
    pub frontier: Option<String>,
}

/// Provider outcomes that affect mirror control flow rather than representing a retryable
/// transport or payload failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PapertrailClientError {
    #[error("tracker item not found")]
    ItemNotFound,
}

/// Reject a non-2xx provider response, mapping 404 to the typed not-found outcome the mirror
/// understands. `provider` labels the error ("GitHub", "GitLab").
pub(crate) fn ensure_provider_success(
    provider: &str,
    status: u16,
    body: &str,
) -> anyhow::Result<()> {
    if status == 404 {
        return Err(PapertrailClientError::ItemNotFound.into());
    }
    anyhow::ensure!((200..300).contains(&status), "{provider} HTTP {status}: {body}");
    Ok(())
}

/// The `rel="next"` target from an RFC-8288 `Link` header — the pagination continuation both
/// GitHub and GitLab emit.
pub(crate) fn next_link(headers: &reqwest::header::HeaderMap) -> anyhow::Result<Option<String>> {
    let Some(link) = headers.get(reqwest::header::LINK) else { return Ok(None) };
    let link = link.to_str()?;
    Ok(link.split(',').find_map(|part| {
        let (url, rel) = part.trim().split_once(';')?;
        rel.trim().eq(r#"rel="next""#).then(|| url.trim().trim_matches(['<', '>']).to_string())
    }))
}

/// Provider payload rows must carry a positive numeric identity before they are trusted.
pub(crate) fn validate_positive_id(
    value: &serde_json::Value,
    field: &str,
    resource: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        value[field].as_u64().is_some_and(|id| id > 0),
        "{resource} has no valid {field}"
    );
    Ok(())
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
    /// Stable logical comment streams for one item. Providers with several independently paged
    /// resources expose each one here so the mirror can checkpoint after every returned page.
    fn item_comment_streams(&self, _kind: ItemKind) -> &'static [&'static str] {
        &["default"]
    }

    /// One page from exactly one [`Self::item_comment_streams`] lane. The default adapts legacy
    /// clients that return a whole thread in one call; native paginated clients override it.
    async fn item_comments_page(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        anyhow::ensure!(cursor.page_token.is_none(), "legacy item comments cannot resume a page");
        Ok(CommentsPage {
            comments: self.item_comments(project, kind, key).await?,
            next: None,
            frontier: None,
        })
    }
    /// Complete provider-specific item fields before the mirror starts the item's durable thread.
    /// The mirror invokes this one item at a time and checkpoints each completed item, so a
    /// transport pause cannot force an entire provider page to be enriched again.
    async fn enrich_item(&self, _item: &mut PapertrailItem) -> anyhow::Result<()> {
        Ok(())
    }
    /// A page of items updated since the cursor, newest first (mirror backfill/delta).
    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage>;
    /// Stable logical comment streams whose cursors must advance independently.
    fn comment_streams(&self) -> &'static [&'static str] {
        &["default"]
    }

    /// A page of comments from exactly one [`Self::comment_streams`] lane. Implementations must not
    /// advance into another stream through `next`; the mirror persists each stream
    /// independently.
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
        probe: &FreshnessProbe,
    ) -> anyhow::Result<FreshnessResult>;
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

#[cfg(test)]
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

    #[test]
    fn implicit_cloud_token_never_crosses_an_enterprise_binding() {
        let tracker = |base_url: Option<&str>| ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: base_url.map(str::to_string),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        };
        let mut trackers = vec![tracker(None), tracker(Some("https://github.example.com"))];
        apply_implicit_github_auth(&mut trackers, |name| {
            (name == "GH_TOKEN").then(|| "cloud-secret".to_string())
        });

        assert!(matches!(
            trackers[0].auth,
            Some(crate::config::TrackerAuth::Env(ref name)) if name == "GH_TOKEN"
        ));
        assert_eq!(trackers[0].authentication, TrackerAuthentication::AuthConfigured);
        assert!(trackers[1].auth.is_none());
        assert_eq!(trackers[1].authentication, TrackerAuthentication::AuthMissing);
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
