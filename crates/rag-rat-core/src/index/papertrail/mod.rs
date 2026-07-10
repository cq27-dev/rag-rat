mod api;
mod evidence;
mod parse;
mod store;
mod sync;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

pub(crate) use api::*;
pub(crate) use evidence::*;
pub(crate) use parse::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub(crate) use store::*;
pub(crate) use sync::*;

use crate::index::now_ms;

/// Resolved GitHub repo context, injected into the sync/query paths instead of being resolved
/// from the local `gh` CLI inside the library. `gh` is network-bound, non-deterministic under
/// parallelism, and unauthenticated in CI — so it is resolved ONLY at the real-usage boundary
/// (`IndexDatabase::open_config`) and never in tests, which pass an explicit context (#60).
#[derive(Debug, Clone, Default)]
pub struct PapertrailContext {
    /// `owner/repo` used to qualify bare `#N` refs. `None` leaves bare refs unresolved.
    pub default_repo: Option<String>,
    /// Whether the `gh` CLI is available (reported as a capability in status).
    pub gh_available: bool,
}

impl PapertrailContext {
    /// Resolve from the local `gh` CLI. Call ONLY at the real-usage boundary (open_config),
    /// never inside the library internals or tests.
    pub(crate) fn from_gh() -> Self {
        Self { default_repo: default_repo(), gh_available: gh_available() }
    }

    /// An explicit context that never touches `gh` — for tests and non-gh callers.
    pub(crate) fn new(default_repo: Option<&str>, gh_available: bool) -> Self {
        Self { default_repo: default_repo.map(str::to_string), gh_available }
    }

    fn default_repo(&self) -> Option<&str> {
        self.default_repo.as_deref()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailStatus {
    pub refs: u64,
    pub issues: u64,
    pub comments: u64,
    pub pulls: u64,
    pub reviews: u64,
    pub review_comments: u64,
    pub last_sync_ms: Option<i64>,
    pub capability: String,
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
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub status: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct PapertrailSyncProgress {
    pub current: usize,
    pub total: usize,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub action: PapertrailSyncAction,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PapertrailSyncAction {
    Syncing,
    Skipped,
    Synced,
    Failed,
    RebuildingFts,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailRef {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub ref_kind: String,
    pub source_kind: String,
    pub source_path: Option<String>,
    pub source_commit: Option<String>,
    pub source_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailEvidence {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub item_kind: String,
    pub item_id: String,
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
    pub github_evidence: Vec<PapertrailEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback_github_evidence: Vec<PapertrailEvidence>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    ChangeRequest,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::ChangeRequest => "change_request",
        }
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
    pub item_key: String,
    /// Provider comment id, stringly — provider id spaces are not uniformly numeric.
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

/// Provider client behind the papertrail sync. Implementations exist per tracker (GitHub today;
/// GitLab / Bitbucket / Jira to follow) and own URL building, pagination, and payload mapping.
///
/// The futures are driven exclusively through [`block_on`] on a private current-thread runtime —
/// nothing here is spawned — so the omitted `Send` bounds of `async fn` in a public trait are
/// deliberate, not an oversight.
#[allow(async_fn_in_trait)]
pub trait PapertrailClient {
    /// One item with its kind resolved.
    async fn item(&self, project: &str, key: &str) -> anyhow::Result<PapertrailItem>;
    /// Every comment on one item, unified (thread comments, review events, file-anchored).
    async fn item_comments(
        &self,
        project: &str,
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
/// Uses a private lazily-built current-thread runtime; MUST NOT be called from within an async
/// context (the MCP server never reaches sync paths that hold a client).
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("papertrail runtime")
        })
        .block_on(future)
}

/// The GitHub provider, currently backed by the `gh` CLI (`gh api`); replaced by a native HTTP
/// client when the mirror sync lands. The subprocess calls block inside the async methods, which
/// is acceptable on the private [`block_on`] runtime — nothing else shares it.
pub struct GitHubClient;

impl PapertrailClient for GitHubClient {
    async fn item(&self, project: &str, key: &str) -> anyhow::Result<PapertrailItem> {
        let value = gh_api_json(&format!("repos/{project}/issues/{key}"))?;
        let mut item = item_from_issue_value(project, &value);
        if item.item_kind == ItemKind::ChangeRequest
            && let Ok(pull) = gh_api_json(&format!("repos/{project}/pulls/{key}"))
        {
            enrich_item_from_pull_value(&mut item, &pull);
        }
        Ok(item)
    }

    async fn item_comments(
        &self,
        project: &str,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>> {
        let mut comments = Vec::new();
        for value in gh_api_paginated(&format!("repos/{project}/issues/{key}/comments"))? {
            comments.push(comment_from_value(project, key, &value));
        }
        // The pulls endpoints 404 for plain issues — an error here means "not a change
        // request", not a failure (mirrors the old optional `pull()` probe).
        if let Ok(values) = gh_api_paginated(&format!("repos/{project}/pulls/{key}/reviews")) {
            for value in &values {
                comments.push(review_to_comment_from_value(project, key, value));
            }
        }
        if let Ok(values) = gh_api_paginated(&format!("repos/{project}/pulls/{key}/comments")) {
            for value in &values {
                comments.push(review_comment_to_comment_from_value(project, key, value));
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
        Ok(match (latest, cursor.updated_since.as_deref()) {
            (Some(latest), Some(since)) if latest.as_str() <= since => None,
            (latest, _) => latest,
        })
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
    owner: &'a str,
    repo: &'a str,
    number: i64,
    kind: &'a str,
    item_id: &'a str,
    url: &'a str,
    title: &'a str,
    body: &'a str,
    /// The active repo that owns this papertrail row (V041). Stamped into `github_fts.repo_id`
    /// (UNINDEXED) so a MATCH in a consolidated DB filters to one repo.
    repo_id: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedRef {
    owner: String,
    repo: String,
    number: i64,
    kind: String,
}
