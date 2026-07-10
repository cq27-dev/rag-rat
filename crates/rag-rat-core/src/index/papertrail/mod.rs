mod api;
mod evidence;
mod parse;
mod store;
mod sync;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub is_pull_request: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubComment {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubPullRequest {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubReview {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: Option<String>,
    pub state: String,
    pub body: String,
    pub author: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubReviewComment {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub path: Option<String>,
    pub html_url: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

pub trait GitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<GitHubIssue>;
    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubComment>>;
    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<GitHubPullRequest>>;
    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReview>>;
    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>>;
}

pub struct GhCliGitHubClient;

impl GitHubClient for GhCliGitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<GitHubIssue> {
        let value = gh_api_json(&format!("repos/{owner}/{repo}/issues/{number}"))?;
        Ok(issue_from_value(owner, repo, &value))
    }

    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let values = gh_api_paginated(&format!("repos/{owner}/{repo}/issues/{number}/comments"))?;
        Ok(values.iter().map(|value| comment_from_value(owner, repo, number, value)).collect())
    }

    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<GitHubPullRequest>> {
        match gh_api_json(&format!("repos/{owner}/{repo}/pulls/{number}")) {
            Ok(value) => Ok(Some(pull_from_value(owner, repo, number, &value))),
            Err(_) => Ok(None),
        }
    }

    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReview>> {
        let values = gh_api_paginated(&format!("repos/{owner}/{repo}/pulls/{number}/reviews"))?;
        Ok(values.iter().map(|value| review_from_value(owner, repo, number, value)).collect())
    }

    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>> {
        let values = gh_api_paginated(&format!("repos/{owner}/{repo}/pulls/{number}/comments"))?;
        Ok(values
            .iter()
            .map(|value| review_comment_from_value(owner, repo, number, value))
            .collect())
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
