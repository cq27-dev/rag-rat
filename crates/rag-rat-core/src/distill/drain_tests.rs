use std::collections::VecDeque;
use std::sync::Mutex;

use rag_rat_db::schema::migrations;
use rag_rat_llm::chat::{ChatModel, GuidedJson};
use rusqlite::Connection;

use super::{PromptBudget, compute_model_input_hash, drain, load_prepared_jobs, pending_count};
use crate::distill::prompts;

struct ScriptedModel {
    replies: Mutex<VecDeque<anyhow::Result<String>>>,
    before_first: Option<Box<dyn Fn() + Send + Sync>>,
    calls: Mutex<usize>,
}

impl ScriptedModel {
    fn new(replies: Vec<anyhow::Result<String>>) -> Self {
        Self { replies: Mutex::new(replies.into()), before_first: None, calls: Mutex::new(0) }
    }

    fn before_first(mut self, action: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_first = Some(Box::new(action));
        self
    }
}

impl ChatModel for ScriptedModel {
    fn complete_guided(
        &self,
        _prompt: &str,
        _guided: Option<GuidedJson<'_>>,
    ) -> anyhow::Result<String> {
        let mut calls = self.calls.lock().unwrap();
        if *calls == 0
            && let Some(action) = &self.before_first
        {
            action();
        }
        *calls += 1;
        self.replies.lock().unwrap().pop_front().unwrap()
    }

    fn model_id(&self) -> &str {
        "scripted"
    }
}

fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    migrations::apply_distill_record_store(&conn).unwrap();
    migrations::apply_distill_anchor_selection(&conn).unwrap();
    migrations::apply_distill_safe_input_snapshot(&conn).unwrap();
    migrations::apply_distill_enriched_context(&conn).unwrap();
    migrations::apply_distill_evidence_source_part(&conn).unwrap();
    // The evidence insert now writes the per-thread ordinal (V111), so the evidence table must
    // be in its rebuilt shape in these partial-schema fixtures.
    migrations::apply_syncable_distill_evidence(&conn).unwrap();
    // `persist_success` advances the papertrail Lens lane, which needs a registered repo and
    // the `repo_meta` sink (V108 dropped the row triggers). This partial fixture
    // predates the full schema, so create both and register the repo the seeds use.
    conn.execute_batch(
        "CREATE TABLE repos(
                 repo_id TEXT NOT NULL PRIMARY KEY,
                 display_name TEXT,
                 registered_at_ms INTEGER NOT NULL
             ) STRICT;
             INSERT INTO repos(repo_id, display_name, registered_at_ms)
                 VALUES ('repo', 'repo', 0);
             CREATE TABLE repo_meta(
                 repo_id TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT,
                 PRIMARY KEY(repo_id, key)
             ) STRICT;",
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE git_commits(
                 hash TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL,
                 repo_id TEXT NOT NULL, PRIMARY KEY(repo_id, hash)
             ) STRICT;",
    )
    .unwrap();
    conn
}

fn seed(conn: &Connection, key: &str, enqueued_at_ms: i64) {
    conn.execute(
        "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  fix_edge_source, thread_shape, distilled_at_ms, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, ?2, 2, 'provider', 'investigation', 1,
                     'repo')",
        rusqlite::params![key, format!("sha256:input-{key}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_queue
                 (tracker, project, item_kind, item_key, enqueued_at_ms, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, ?2, 'repo')",
        rusqlite::params![key, enqueued_at_ms],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, author, author_association, created_at_ms, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 0, 'primary', NULL, 'issue', ?1, 'item',
                     'title', ?1, 'A title', 'owner', 'OWNER', 10, 'repo'),
                    ('github', 'org/repo', 'issue', ?1, 1, 'primary', NULL, 'issue', ?1, 'item',
                     'body', ?1, 'Cause and decision landed.', 'owner', 'OWNER', 10, 'repo')",
        [key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_units
                 (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
                  byte_end, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 0, 1, 0, 26, 'repo')",
        [key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, file_path,
                  name, resolved, candidate_ordinal, selected, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 'symbol', 'sym_1', 'src/lib.rs', 'run', 1,
                     0, 0, 'repo')",
        [key],
    )
    .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO git_commits(hash, subject, body, repo_id)
             VALUES ('abc', 'Fix it', 'Detailed fix.', 'repo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 'abc', 'repo')",
        [key],
    )
    .unwrap();
}

fn valid_reply() -> String {
    serde_json::json!({
        "root_issue": "The operation failed.",
        "root_cause_units": [0],
        "root_cause": "A stale decision caused the failure.",
        "root_cause_class": "stale decision",
        "decision_units": [0],
        "decision": {
            "chosen": "Use the current decision.",
            "rejected": [{"alternative": "Keep stale state", "reason": "It fails."}]
        },
        "outcome_units": [0],
        "anchor_indices": [0],
        "outcome": {"status": "landed", "summary": "The fix landed."}
    })
    .to_string()
}

#[test]
fn evidence_records_the_part_each_citation_came_from() {
    // #801: an item's title and body share the same source_id (the item key). A citation to the
    // title and one to the body must be distinguishable in the persisted evidence — the only
    // discriminator is source_part.
    let conn = fixture();
    let lock_db = tempfile::NamedTempFile::new().unwrap();
    seed(&conn, "7", 20);
    // seed() gives unit 0 → the body source (ordinal 1). Add unit 1 → the title source
    // (ordinal 0), so the model can cite both parts of the same item.
    conn.execute(
        "INSERT INTO papertrail_distill_units
                 (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
                  byte_end, repo_id)
             VALUES ('github', 'org/repo', 'issue', '7', 1, 0, 0, 7, 'repo')",
        [],
    )
    .unwrap();
    // root_cause cites the TITLE unit (1); decision and outcome cite the BODY unit (0).
    let reply = serde_json::json!({
        "root_issue": "The operation failed.",
        "root_cause_units": [1],
        "root_cause": "A stale decision caused the failure.",
        "root_cause_class": "stale decision",
        "decision_units": [0],
        "decision": {"chosen": "Use the current decision.", "rejected": []},
        "outcome_units": [0],
        "anchor_indices": [0],
        "outcome": {"status": "landed", "summary": "The fix landed."}
    })
    .to_string();

    let report =
        drain(&conn, lock_db.path(), "repo", &ScriptedModel::new(vec![Ok(reply)]), 10, 99).unwrap();
    assert_eq!((report.threads, report.succeeded), (1, 1));

    let rows: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT field, source_part, source_id FROM papertrail_distill_evidence
                 ORDER BY field",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("decision".to_string(), "body".to_string(), "7".to_string()),
            ("outcome".to_string(), "body".to_string(), "7".to_string()),
            ("root_cause".to_string(), "title".to_string(), "7".to_string()),
        ],
        "the title citation and the body citations share a source_id but differ by source_part",
    );
}

#[test]
fn successful_drain_persists_the_complete_model_transition() {
    let conn = fixture();
    let lock_db = tempfile::NamedTempFile::new().unwrap();
    seed(&conn, "2", 20);
    let report =
        drain(&conn, lock_db.path(), "repo", &ScriptedModel::new(vec![Ok(valid_reply())]), 10, 99)
            .unwrap();
    assert_eq!((report.threads, report.succeeded, report.failed, report.stale), (1, 1, 0, 0));
    let row: (String, String, i64, i64, i64, i64, String) = conn
        .query_row(
            "SELECT root_cause, outcome_status_model, quotes_materialized,
                        decision_provenance_verified, outcome_claim_verified, prompt_version,
                        model_input_hash
                 FROM papertrail_distill WHERE repo_id = 'repo' AND item_key = '2'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, "A stale decision caused the failure.");
    assert_eq!(row.1, "landed");
    assert_eq!((row.2, row.3, row.4, row.5), (3, 1, 1, i64::from(prompts::PROMPT_VERSION)));
    assert!(row.6.starts_with("sha256:"));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM papertrail_distill_queue", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM papertrail_distill_evidence", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row("SELECT quote FROM papertrail_distill_evidence LIMIT 1", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "Cause and decision landed."
    );
    // Every citation here is the body unit, so the persisted evidence records its part (#801).
    assert_eq!(
        conn.query_row("SELECT DISTINCT source_part FROM papertrail_distill_evidence", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "body",
    );
    assert_eq!(
        conn.query_row("SELECT selected FROM papertrail_distill_anchors", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT threads, rung_guided, rung_serde, failed
                 FROM papertrail_distill_runs WHERE repo_id = 'repo'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap(),
        (1, 1, 1, 0)
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM papertrail_distill_record_commits", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        1,
        "the drain never rewrites mechanical fixing commits"
    );
}

#[test]
fn pending_count_includes_only_prepared_work_for_the_requested_repo() {
    let conn = fixture();
    assert_eq!(pending_count(&conn, "repo").unwrap(), 0);
    seed(&conn, "2", 20);
    assert_eq!(pending_count(&conn, "repo").unwrap(), 1);
    assert_eq!(pending_count(&conn, "other").unwrap(), 0);

    conn.execute("DELETE FROM papertrail_distill_sources WHERE repo_id = 'repo'", []).unwrap();
    assert_eq!(pending_count(&conn, "repo").unwrap(), 0);
}

#[test]
fn failed_ladder_increments_attempt_and_bounds_diagnostics() {
    let conn = fixture();
    let lock_db = tempfile::NamedTempFile::new().unwrap();
    seed(&conn, "2", 20);
    let huge = "x".repeat(70_000);
    let report = drain(
        &conn,
        lock_db.path(),
        "repo",
        &ScriptedModel::new(vec![Ok("not json".into()), Ok(huge)]),
        10,
        99,
    )
    .unwrap();
    assert_eq!((report.failed, report.stale), (1, 0));
    let (attempts, error, raw): (i64, String, String) = conn
        .query_row(
            "SELECT attempts, last_error, raw_reply FROM papertrail_distill_queue",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(error.chars().count() <= 2_000);
    assert_eq!(raw.chars().count(), 64_000);
}

#[test]
fn stale_success_does_not_delete_or_poison_new_work() {
    let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let conn = Connection::open(&path).unwrap();
    migrations::apply_distill_record_store(&conn).unwrap();
    migrations::apply_distill_anchor_selection(&conn).unwrap();
    migrations::apply_distill_safe_input_snapshot(&conn).unwrap();
    migrations::apply_distill_enriched_context(&conn).unwrap();
    migrations::apply_distill_evidence_source_part(&conn).unwrap();
    // The evidence insert now writes the per-thread ordinal (V111), so the evidence table must
    // be in its rebuilt shape in these partial-schema fixtures.
    migrations::apply_syncable_distill_evidence(&conn).unwrap();
    conn.execute_batch(
        "CREATE TABLE git_commits(
                 hash TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL,
                 repo_id TEXT NOT NULL, PRIMARY KEY(repo_id, hash)
             ) STRICT; PRAGMA journal_mode = WAL;",
    )
    .unwrap();
    seed(&conn, "2", 20);
    let path_for_model = path.to_path_buf();
    let model = ScriptedModel::new(vec![Ok(valid_reply())]).before_first(move || {
        let other = Connection::open(&path_for_model).unwrap();
        other
            .execute(
                "UPDATE papertrail_distill SET distill_input_hash = 'sha256:new-input'
                     WHERE repo_id = 'repo' AND item_key = '2'",
                [],
            )
            .unwrap();
    });
    let report = drain(&conn, path.as_ref(), "repo", &model, 10, 99).unwrap();
    assert_eq!((report.succeeded, report.stale), (0, 1));
    let (root_cause, attempts): (Option<String>, i64) = conn
        .query_row(
            "SELECT d.root_cause, q.attempts FROM papertrail_distill d
                 JOIN papertrail_distill_queue q USING(repo_id, tracker, project, item_kind, \
             item_key)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((root_cause, attempts), (None, 0));
}

#[test]
fn loading_is_ordered_limited_and_hashes_exact_visible_input() {
    let conn = fixture();
    seed(&conn, "later", 20);
    seed(&conn, "first", 10);
    // Insert partner rows out of row-id order. The persisted partner ordinal, not insertion
    // order, defines the render sequence.
    conn.execute_batch(
        "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github', 'org/repo', 'issue', 'first', 4, 'partner', 1,
                     'change_request', 'later-partner', 'item', 'title', 'later-partner',
                     'Later partner', 'repo'),
                    ('github', 'org/repo', 'issue', 'first', 5, 'partner', 1,
                     'change_request', 'later-partner', 'item', 'body', 'later-partner',
                     'Later partner body', 'repo'),
                    ('github', 'org/repo', 'issue', 'first', 2, 'partner', 0,
                     'change_request', 'first-partner', 'item', 'title', 'first-partner',
                     'First partner', 'repo'),
                    ('github', 'org/repo', 'issue', 'first', 3, 'partner', 0,
                     'change_request', 'first-partner', 'item', 'body', 'first-partner',
                     'First partner body', 'repo');",
    )
    .unwrap();
    let budget = PromptBudget::default();
    let jobs = load_prepared_jobs(&conn, "repo", 1, &budget).unwrap();
    assert_eq!(jobs[0].key.item_key, "first");
    assert!(jobs[0].rendered_prompt.contains("Detailed fix."));
    assert!(jobs[0].rendered_prompt.contains("[A0]"));
    // Every coalesced partner renders, lowest partner_ordinal first (#800).
    let first_pos = jobs[0].rendered_prompt.find("First partner").expect("first partner");
    let later_pos = jobs[0].rendered_prompt.find("Later partner").expect("later partner");
    assert!(first_pos < later_pos, "partners render in durable ordinal order");
    assert_eq!(
        jobs[0].model_input_hash,
        compute_model_input_hash(&jobs[0].rendered_prompt, &jobs[0].schema).unwrap()
    );
    let again = load_prepared_jobs(&conn, "repo", 1, &budget).unwrap();
    assert_eq!(jobs[0].model_input_hash, again[0].model_input_hash);
    conn.execute(
        "UPDATE git_commits SET body = 'Changed model-visible body' WHERE repo_id = 'repo'",
        [],
    )
    .unwrap();
    let changed = load_prepared_jobs(&conn, "repo", 1, &budget).unwrap();
    assert_ne!(jobs[0].model_input_hash, changed[0].model_input_hash);
}

#[test]
fn enriched_context_renders_from_the_frozen_snapshots() {
    let conn = fixture();
    seed(&conn, "first", 10);
    conn.execute(
        "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github', 'org/repo', 'issue', 'first', 'abc123', 'src/widget.rs',
                     'diff --git a/src/widget.rs b/src/widget.rs\n--- a/src/widget.rs\n+++ \
         b/src/widget.rs\n@@ -1 +1 @@\n-old\n+new\n',
                     'repo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES ('github', 'org/repo', 'issue', 'first', 0, 'github', 'org/repo', 'issue',
                     '9', 'reference', 'Related refactor', 'We reworked the render path.', 'repo')",
        [],
    )
    .unwrap();

    let jobs = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default()).unwrap();
    let prompt = &jobs[0].rendered_prompt;
    assert!(prompt.contains("DIFF:"), "the diff block renders: {prompt}");
    assert!(prompt.contains("+new"), "hunk content renders: {prompt}");
    assert!(prompt.contains("REFERENCED ITEMS:"), "{prompt}");
    assert!(
        prompt.contains("[issue] #9 (reference): Related refactor — We reworked the render path."),
        "{prompt}"
    );
}

#[test]
fn fix_diff_rows_concatenate_in_commit_path_order_with_newline_separators() {
    // `load_fix_diff` re-sorts the persisted per-file patches by (commit_sha, path) and joins
    // them, inserting a separating newline after any row that does not already end in one.
    let conn = fixture();
    seed(&conn, "first", 10);
    // Inserted out of (commit_sha, path) order; the SECOND row (commit 'bbb') has no trailing
    // newline, exercising the normalization branch.
    conn.execute(
        "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','org/repo','issue','first','bbb','src/z.rs','PATCH-Z-no-newline',
                     'repo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','org/repo','issue','first','aaa','src/a.rs','PATCH-A' || char(10),
                     'repo')",
        [],
    )
    .unwrap();

    let jobs = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default()).unwrap();
    let prompt = &jobs[0].rendered_prompt;
    let a_pos = prompt.find("PATCH-A").expect("patch A renders");
    let z_pos = prompt.find("PATCH-Z-no-newline").expect("patch Z renders");
    assert!(a_pos < z_pos, "rows render in (commit_sha, path) order: aaa before bbb: {prompt}");
}

#[test]
fn a_null_xref_target_kind_renders_an_empty_kind_label() {
    // `load_xrefs` tolerates a NULL `target_item_kind` (defaulting it to an empty label) rather
    // than dropping the row — the drain never fabricates a kind the snapshot did not resolve.
    let conn = fixture();
    seed(&conn, "first", 10);
    conn.execute(
        "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES \
         ('github','org/repo','issue','first',0,'github','org/repo',NULL,'9','reference',
                     'Kindless target','','repo')",
        [],
    )
    .unwrap();

    let jobs = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default()).unwrap();
    let prompt = &jobs[0].rendered_prompt;
    assert!(prompt.contains("REFERENCED ITEMS:"), "{prompt}");
    assert!(
        prompt.contains("[] #9 (reference): Kindless target"),
        "a NULL target kind renders as an empty kind label: {prompt}",
    );
}

#[test]
fn an_unknown_source_token_fails_the_load_instead_of_dropping_out_of_a_filter() {
    // The column's CHECK admits only the enum's tokens, so an unknown one means the constraint
    // was bypassed. Read as a bare string it would render as an item source and slip past
    // every `comment` check; hydrated as a closed enum it fails the load.
    let conn = fixture();
    seed(&conn, "first", 10);
    conn.execute_batch(
        "PRAGMA ignore_check_constraints = ON;
             UPDATE papertrail_distill_sources SET source_kind = 'commnt' WHERE source_ordinal = 1;
             PRAGMA ignore_check_constraints = OFF;",
    )
    .unwrap();

    let error = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default())
        .expect_err("an unknown source kind must fail the snapshot load");
    assert!(format!("{error:#}").contains("unknown distill source kind `commnt`"), "{error:#}");
}

#[test]
fn model_executes_after_the_read_transaction_is_released() {
    let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let conn = Connection::open(&path).unwrap();
    migrations::apply_distill_record_store(&conn).unwrap();
    migrations::apply_distill_anchor_selection(&conn).unwrap();
    migrations::apply_distill_safe_input_snapshot(&conn).unwrap();
    migrations::apply_distill_enriched_context(&conn).unwrap();
    migrations::apply_distill_evidence_source_part(&conn).unwrap();
    // The evidence insert now writes the per-thread ordinal (V111), so the evidence table must
    // be in its rebuilt shape in these partial-schema fixtures.
    migrations::apply_syncable_distill_evidence(&conn).unwrap();
    conn.execute_batch(
        "CREATE TABLE git_commits(
                 hash TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL,
                 repo_id TEXT NOT NULL, PRIMARY KEY(repo_id, hash)
             ) STRICT;
             CREATE TABLE model_probe(value INTEGER NOT NULL);
             CREATE TABLE repos(
                 repo_id TEXT NOT NULL PRIMARY KEY, display_name TEXT,
                 registered_at_ms INTEGER NOT NULL
             ) STRICT;
             INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo', 'repo', 0);
             CREATE TABLE repo_meta(
                 repo_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT, PRIMARY KEY(repo_id, key)
             ) STRICT;
             PRAGMA journal_mode = WAL;",
    )
    .unwrap();
    seed(&conn, "2", 20);
    let path_for_model = path.to_path_buf();
    let model = ScriptedModel::new(vec![Ok(valid_reply())]).before_first(move || {
        let other = Connection::open(&path_for_model).unwrap();
        other.execute("INSERT INTO model_probe VALUES (1)", []).unwrap();
    });
    let report = drain(&conn, path.as_ref(), "repo", &model, 10, 99).unwrap();
    assert_eq!(report.succeeded, 1);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM model_probe", [], |row| row.get::<_, i64>(0)).unwrap(),
        1
    );
}
