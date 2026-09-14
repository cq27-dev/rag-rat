//! Mirror runner tests, grouped by concern. The scripted provider client and the shared
//! fixtures live here; each sibling file holds the tests for one part of the walk.

use std::cell::RefCell;
use std::collections::VecDeque;

use rag_rat_db::schema;

use super::*;

mod attested;
mod backfill;
mod comment_streams;
mod continuation;
mod item_threads;
mod pruning;

struct ScriptClient {
    comment_streams: &'static [&'static str],
    attested: RefCell<VecDeque<Option<AttestedClosersPage>>>,
    attested_pause: std::cell::Cell<bool>,
    attested_hard_error: std::cell::Cell<bool>,
    pages: RefCell<VecDeque<anyhow::Result<ItemsPage>>>,
    probes: RefCell<VecDeque<FreshnessResult>>,
    item_comments: RefCell<VecDeque<anyhow::Result<Vec<PapertrailComment>>>>,
    item_comment_pages: RefCell<VecDeque<anyhow::Result<CommentsPage>>>,
    repo_comments: RefCell<VecDeque<anyhow::Result<CommentsPage>>>,
    item_page_requests: RefCell<Vec<PageCursor>>,
    item_comment_requests: RefCell<Vec<String>>,
    item_comment_page_requests: RefCell<Vec<PageCursor>>,
    repo_comment_requests: RefCell<Vec<PageCursor>>,
}

impl ScriptClient {
    fn new(pages: Vec<anyhow::Result<ItemsPage>>) -> Self {
        Self {
            comment_streams: &["default"],
            attested: RefCell::new(VecDeque::new()),
            attested_pause: std::cell::Cell::new(false),
            attested_hard_error: std::cell::Cell::new(false),
            pages: RefCell::new(pages.into()),
            probes: RefCell::new(VecDeque::new()),
            item_comments: RefCell::new(VecDeque::new()),
            item_comment_pages: RefCell::new(VecDeque::new()),
            repo_comments: RefCell::new(VecDeque::new()),
            item_page_requests: RefCell::new(Vec::new()),
            item_comment_requests: RefCell::new(Vec::new()),
            item_comment_page_requests: RefCell::new(Vec::new()),
            repo_comment_requests: RefCell::new(Vec::new()),
        }
    }

    fn with_probe(self, probe: FreshnessResult) -> Self {
        self.probes.borrow_mut().push_back(probe);
        self
    }

    fn with_comment_streams(mut self, streams: &'static [&'static str]) -> Self {
        self.comment_streams = streams;
        self
    }

    fn with_item_comments(self, comments: Vec<PapertrailComment>) -> Self {
        self.item_comments.borrow_mut().push_back(Ok(comments));
        self
    }

    fn with_item_comment_results(
        self,
        results: Vec<anyhow::Result<Vec<PapertrailComment>>>,
    ) -> Self {
        self.item_comments.borrow_mut().extend(results);
        self
    }

    fn with_item_comment_pages(self, pages: Vec<anyhow::Result<CommentsPage>>) -> Self {
        self.item_comment_pages.borrow_mut().extend(pages);
        self
    }

    fn with_repo_comments(self, comments: Vec<PapertrailComment>) -> Self {
        self.repo_comments.borrow_mut().push_back(Ok(CommentsPage {
            comments,
            next: None,
            frontier: None,
        }));
        self
    }

    fn with_repo_comment_pages(self, pages: Vec<anyhow::Result<CommentsPage>>) -> Self {
        self.repo_comments.borrow_mut().extend(pages);
        self
    }
}

impl PapertrailClient for ScriptClient {
    fn comment_streams(&self) -> &'static [&'static str] {
        self.comment_streams
    }

    async fn item(
        &self,
        _project: &str,
        _kind: ItemKind,
        _key: &str,
    ) -> anyhow::Result<PapertrailItem> {
        anyhow::bail!("unused")
    }

    async fn item_comments(
        &self,
        _project: &str,
        _kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>> {
        self.item_comment_requests.borrow_mut().push(key.to_string());
        self.item_comments.borrow_mut().pop_front().unwrap_or(Ok(Vec::new()))
    }

    async fn item_comments_page(
        &self,
        _project: &str,
        _kind: ItemKind,
        key: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        self.item_comment_requests.borrow_mut().push(key.to_string());
        self.item_comment_page_requests.borrow_mut().push(cursor.clone());
        if let Some(page) = self.item_comment_pages.borrow_mut().pop_front() {
            return page;
        }
        Ok(CommentsPage {
            comments: self.item_comments.borrow_mut().pop_front().unwrap_or(Ok(Vec::new()))?,
            next: None,
            frontier: None,
        })
    }

    async fn items_page(&self, _project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage> {
        self.item_page_requests.borrow_mut().push(cursor.clone());
        self.pages.borrow_mut().pop_front().expect("scripted item page")
    }

    async fn comments_page(
        &self,
        _project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        self.repo_comment_requests.borrow_mut().push(cursor.clone());
        self.repo_comments.borrow_mut().pop_front().unwrap_or(Ok(CommentsPage {
            comments: Vec::new(),
            next: None,
            frontier: None,
        }))
    }

    async fn freshness_probe(
        &self,
        _project: &str,
        probe: &FreshnessProbe,
    ) -> anyhow::Result<FreshnessResult> {
        Ok(self.probes.borrow_mut().pop_front().unwrap_or(FreshnessResult {
            latest: None,
            etag: probe.etag.clone(),
            not_modified: true,
        }))
    }

    async fn attested_closers_page(
        &self,
        _project: &str,
        _cursor: Option<&str>,
        _since: Option<&str>,
    ) -> anyhow::Result<Option<AttestedClosersPage>> {
        if self.attested_pause.get() {
            return Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 999_000,
                reason: PauseReason::RetryAfter,
            }));
        }
        if self.attested_hard_error.get() {
            anyhow::bail!("attested walk boom");
        }
        Ok(self.attested.borrow_mut().pop_front().unwrap_or(None))
    }
}

fn binding(tags: &[&str]) -> ResolvedTracker {
    ResolvedTracker {
        provider: Tracker::Github,
        project: "o/r".to_string(),
        base_url: None,
        auth: None,
        authentication: TrackerAuthentication::AuthMissing,
        tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
    }
}

fn item(key: &str, updated_at: &str, title: &str, tags: &[&str]) -> PapertrailItem {
    PapertrailItem {
        project: "o/r".to_string(),
        item_kind: ItemKind::Issue,
        item_key: key.to_string(),
        url: format!("https://github.com/o/r/issues/{key}"),
        state: "open".to_string(),
        title: title.to_string(),
        body: String::new(),
        author: None,
        created_at: None,
        updated_at: Some(updated_at.to_string()),
        merged_at: None,
        closed_at: None,
        resolution: None,
        merge_commit_sha: None,
        author_kind: None,
        author_association: None,
        tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
    }
}

fn page(items: Vec<PapertrailItem>) -> anyhow::Result<ItemsPage> {
    Ok(ItemsPage { items, next: None, backfill_boundary: None })
}

fn comment(key: &str, id: &str, updated_at: &str) -> PapertrailComment {
    PapertrailComment {
        project: "o/r".to_string(),
        item_kind: ItemKind::Issue,
        item_key: key.to_string(),
        comment_id: id.to_string(),
        url: None,
        body: "comment".to_string(),
        author: None,
        author_kind: None,
        author_association: None,
        created_at: Some(updated_at.to_string()),
        updated_at: Some(updated_at.to_string()),
        review_state: None,
        anchor_path: None,
    }
}

fn empty_report(binding: &ResolvedTracker) -> MirrorBindingReport {
    MirrorBindingReport {
        tracker: binding.provider,
        project: binding.project.clone(),
        stored_items: 0,
        stored_comments: 0,
        pruned_items: 0,
        attested_edges: 0,
        attested_writes: 0,
        attested_error: None,
        paused_until_ms: None,
        pause_reason: None,
        completed_full_walk: false,
        probe_not_modified: false,
    }
}

/// Cache a CLOSED in-scope issue so the attested-edge cached-closed gate admits its edges.
fn cache_closed_issue(conn: &Connection, key: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO papertrail_items(tracker, project, item_kind, item_key, url, \
         state, title, body, synced_at_ms, repo_id, state_normalized) VALUES ('github', 'o/r', \
         'issue', ?1, 'u', 'closed', 't', 'b', 1, (SELECT COALESCE((SELECT repo_id FROM repos \
         LIMIT 1), '__unassigned__')), 'closed')",
        [key],
    )
    .unwrap();
}

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::test_hooks()).unwrap();
    conn
}

fn keys(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT item_key FROM papertrail_items ORDER BY item_key").unwrap();
    stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
}
