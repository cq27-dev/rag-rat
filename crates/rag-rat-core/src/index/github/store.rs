use super::*;

pub(crate) fn store_ref(conn: &Connection, reference: &GitHubRef) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR IGNORE INTO github_refs(
            owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text, \
         discovered_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ",
        params![
            reference.owner,
            reference.repo,
            reference.number,
            reference.ref_kind,
            reference.source_kind,
            reference.source_path,
            reference.source_commit,
            reference.source_text,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_issue(conn: &Connection, issue: &GitHubIssue) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, author, \
         created_at, updated_at, is_pull_request, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(owner, repo, number) DO UPDATE SET
            html_url = excluded.html_url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, is_pull_request = excluded.is_pull_request,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            issue.owner,
            issue.repo,
            issue.number,
            issue.html_url,
            issue.state,
            issue.title,
            issue.body,
            issue.author,
            issue.created_at,
            issue.updated_at,
            issue.is_pull_request,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_comment(conn: &Connection, comment: &GitHubComment) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_comments(id, owner, repo, number, html_url, body, author, \
         created_at, updated_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ",
        params![
            comment.id,
            comment.owner,
            comment.repo,
            comment.number,
            comment.html_url,
            comment.body,
            comment.author,
            comment.created_at,
            comment.updated_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_pull(conn: &Connection, pull: &GitHubPullRequest) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body, \
         author, created_at, updated_at, merged_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(owner, repo, number) DO UPDATE SET
            html_url = excluded.html_url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, merged_at = excluded.merged_at,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            pull.owner,
            pull.repo,
            pull.number,
            pull.html_url,
            pull.state,
            pull.title,
            pull.body,
            pull.author,
            pull.created_at,
            pull.updated_at,
            pull.merged_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_review(conn: &Connection, review: &GitHubReview) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_reviews(id, owner, repo, number, html_url, state, body, \
         author, submitted_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ",
        params![
            review.id,
            review.owner,
            review.repo,
            review.number,
            review.html_url,
            review.state,
            review.body,
            review.author,
            review.submitted_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_review_comment(
    conn: &Connection,
    comment: &GitHubReviewComment,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_review_comments(id, owner, repo, number, path, html_url, \
         body, author, created_at, updated_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ",
        params![
            comment.id,
            comment.owner,
            comment.repo,
            comment.number,
            comment.path,
            comment.html_url,
            comment.body,
            comment.author,
            comment.created_at,
            comment.updated_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    // github_fts is a standalone (own-content) FTS5 table, so a DELETE + re-insert is the correct
    // rebuild — unlike the external-content chunk_fts / commit_fts, which must rebuild via
    // INSERT(t) VALUES('rebuild'). Each query below selects the seven FtsRow columns in order
    // (id, owner, repo, number, url, title, body); kinds without a title (comment, review) select
    // an empty string for that slot, and review comments surface the file path as the title.
    conn.execute("DELETE FROM github_fts", [])?;
    insert_fts_rows(
        conn,
        "issue",
        "SELECT id, owner, repo, number, html_url, title, body FROM github_issues",
    )?;
    insert_fts_rows(
        conn,
        "comment",
        "SELECT id, owner, repo, number, html_url, '', body FROM github_comments",
    )?;
    insert_fts_rows(
        conn,
        "pull",
        "SELECT id, owner, repo, number, html_url, title, body FROM github_pull_requests",
    )?;
    insert_fts_rows(
        conn,
        "review",
        "SELECT id, owner, repo, number, COALESCE(html_url, ''), '', body FROM github_reviews",
    )?;
    insert_fts_rows(
        conn,
        "review_comment",
        "SELECT id, owner, repo, number, html_url, COALESCE(path, ''), body FROM \
         github_review_comments",
    )?;
    Ok(())
}
/// Bulk-load one GitHub item kind into `github_fts`. `sql` must select exactly the seven `FtsRow`
/// columns in order — `id, owner, repo, number, url, title, body` — using an empty-string literal
/// in the title slot for kinds that carry no title. Adding a new item kind is a one-line call here,
/// not another copy of the load loop.
fn insert_fts_rows(conn: &Connection, kind: &str, sql: &str) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, title, body) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind,
            item_id: &id.to_string(),
            url: &url,
            title: &title,
            body: &body,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_fts(conn: &Connection, row: FtsRow<'_>) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            row.owner,
            row.repo,
            row.number,
            row.kind,
            row.item_id,
            row.url,
            row.title,
            row.body,
            classify_text(&format!("{}\n{}", row.title, row.body))
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod fts_rebuild_tests {
    use rusqlite::Connection;

    use super::*;
    use crate::index::schema;

    // rebuild_fts collapses five near-identical per-kind loaders into insert_fts_rows. This pins
    // the behaviour they shared: every item kind lands in github_fts under its own item_kind, the
    // body is tokenized/searchable, and the title slot is populated the way each old loader did —
    // the title text for issues/pulls, the file path for review comments, empty for the rest.
    #[test]
    fn rebuild_fts_indexes_every_item_kind_with_the_right_title() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();

        store_issue(&conn, &GitHubIssue {
            owner: "o".into(),
            repo: "r".into(),
            number: 1,
            html_url: "http://i".into(),
            state: "open".into(),
            title: "issuetitle".into(),
            body: "issuebody".into(),
            author: None,
            created_at: None,
            updated_at: None,
            is_pull_request: false,
        })
        .unwrap();
        store_comment(&conn, &GitHubComment {
            id: 10,
            owner: "o".into(),
            repo: "r".into(),
            number: 1,
            html_url: "http://c".into(),
            body: "commentbody".into(),
            author: None,
            created_at: None,
            updated_at: None,
        })
        .unwrap();
        store_pull(&conn, &GitHubPullRequest {
            owner: "o".into(),
            repo: "r".into(),
            number: 2,
            html_url: "http://p".into(),
            state: "open".into(),
            title: "pulltitle".into(),
            body: "pullbody".into(),
            author: None,
            created_at: None,
            updated_at: None,
            merged_at: None,
        })
        .unwrap();
        // html_url None exercises the review loader's COALESCE(html_url, '').
        store_review(&conn, &GitHubReview {
            id: 20,
            owner: "o".into(),
            repo: "r".into(),
            number: 2,
            html_url: None,
            state: "approved".into(),
            body: "reviewbody".into(),
            author: None,
            submitted_at: None,
        })
        .unwrap();
        store_review_comment(&conn, &GitHubReviewComment {
            id: 30,
            owner: "o".into(),
            repo: "r".into(),
            number: 2,
            path: Some("src/lib.rs".into()),
            html_url: "http://rc".into(),
            body: "reviewcommentbody".into(),
            author: None,
            created_at: None,
            updated_at: None,
        })
        .unwrap();

        rebuild_fts(&conn).unwrap();

        // One row per stored item, each keyed by its own item_kind, with the expected title slot.
        let rows: Vec<(String, String)> = {
            let mut stmt =
                conn.prepare("SELECT item_kind, title FROM github_fts ORDER BY item_kind").unwrap();
            let mapped = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .unwrap();
            mapped.map(Result::unwrap).collect()
        };
        assert_eq!(
            rows,
            vec![
                ("comment".to_string(), String::new()),
                ("issue".to_string(), "issuetitle".to_string()),
                ("pull".to_string(), "pulltitle".to_string()),
                ("review".to_string(), String::new()),
                ("review_comment".to_string(), "src/lib.rs".to_string()),
            ],
            "every kind is indexed once with the title slot the old per-kind loaders produced"
        );

        // The body column is tokenized and queryable via MATCH.
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM github_fts WHERE github_fts MATCH 'reviewcommentbody'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "review-comment body is tokenized and searchable after rebuild");
    }
}
