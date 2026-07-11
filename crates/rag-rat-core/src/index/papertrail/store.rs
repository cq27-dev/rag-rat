use super::*;

// KEY SCOPING (V044 + V045, phase A7): every github cache key folds `repo_id`. The `(owner,
// repo, number)`-style natural keys are `(repo_id, owner, repo, number)` (V044:
// `github_issues` / `github_pull_requests` UNIQUE, `github_ref_sync` PK, `idx_github_refs_unique`
// leading column), and the id-keyed CHILD caches (`github_comments` / `github_reviews` /
// `github_review_comments`) are `(repo_id, id)` unique (V045). So on a consolidated DB two repos
// referencing the SAME external item each own a full copy — parent AND children — and a conflict
// only ever fires WITHIN one repo (this repo re-syncing its own item). The natural-key writers'
// `ON CONFLICT` targets name the widened keys and refresh content in place; the id-keyed writers
// keep `INSERT OR REPLACE`, which resolves through the widened unique index — a same-repo re-sync
// replaces in place, a sibling repo's sync inserts its own row, and neither can restamp the
// other's copy (the pre-V045 last-syncer-owns oscillation: repo A's scoped papertrail lost a
// shared PR's comments the moment repo B synced). The `github_fts` mirror follows automatically
// because every sync path ends in the whole-table [`rebuild_fts`], which re-derives each mirror
// row's `repo_id` from its base row.

pub(crate) fn store_ref(conn: &Connection, reference: &PapertrailRef) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    // The widened `idx_github_refs_unique` (V044) leads with `repo_id`, so a conflict is always
    // THIS repo re-discovering its OWN ref — keep the first-sighting row untouched (`DO
    // NOTHING` preserves the original `discovered_at_ms`/`ref_kind`, the pre-V044 same-repo
    // semantics). A sibling repo referencing the same `(owner, repo, number)` gets its own
    // distinct row rather than conflicting, so the old cross-owner reclaim guard is no longer
    // reachable.
    conn.execute(
        "
        INSERT INTO github_refs(
            owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text, \
         discovered_at_ms, repo_id
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(repo_id, owner, repo, number, source_kind, COALESCE(source_path, ''), \
         COALESCE(source_commit, ''), source_text) DO NOTHING
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
            repo_id,
        ],
    )?;
    Ok(())
}
/// Split a normalized item identity back onto the github_* cache key columns. The tables keep
/// the provider-shaped `(owner, repo, number)` key until the schema normalization; this glue is
/// the only place that reverse mapping lives.
fn github_key(project: &str, item_key: &str) -> anyhow::Result<(String, String, i64)> {
    let (owner, repo) = split_repo(project)
        .ok_or_else(|| anyhow::anyhow!("malformed papertrail project `{project}`"))?;
    let number = item_key
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("non-numeric github item key `{item_key}`"))?;
    Ok((owner.to_string(), repo.to_string(), number))
}
/// Store one normalized item into the github_* cache. Returns the number of rows written: every
/// item keeps an issue-shadow row (GitHub's shared numbering — a change request IS an issue on
/// the issues endpoints, and refs resolve through one table regardless of kind), and a change
/// request additionally refreshes its `github_pull_requests` row.
pub(crate) fn store_item(conn: &Connection, item: &PapertrailItem) -> anyhow::Result<usize> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let (owner, repo, number) = github_key(&item.project, &item.item_key)?;
    conn.execute(
        "
        INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, author, \
         created_at, updated_at, is_pull_request, synced_at_ms, repo_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(repo_id, owner, repo, number) DO UPDATE SET
            html_url = excluded.html_url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, is_pull_request = excluded.is_pull_request,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            owner,
            repo,
            number,
            item.url,
            item.state,
            item.title,
            item.body,
            item.author,
            item.created_at,
            item.updated_at,
            item.item_kind == ItemKind::ChangeRequest,
            now_ms(),
            repo_id,
        ],
    )?;
    if item.item_kind != ItemKind::ChangeRequest {
        return Ok(1);
    }
    conn.execute(
        "
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body, \
         author, created_at, updated_at, merged_at, synced_at_ms, repo_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(repo_id, owner, repo, number) DO UPDATE SET
            html_url = excluded.html_url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, merged_at = excluded.merged_at,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            owner,
            repo,
            number,
            item.url,
            item.state,
            item.title,
            item.body,
            item.author,
            item.created_at,
            item.updated_at,
            item.merged_at,
            now_ms(),
            repo_id,
        ],
    )?;
    Ok(2)
}
/// Store one normalized comment into the github_* cache, routed by its markers: a file-anchored
/// comment lands in `github_review_comments`, a review event in `github_reviews`, and a plain
/// thread comment in `github_comments`.
pub(crate) fn store_comment(conn: &Connection, comment: &PapertrailComment) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let (owner, repo, number) = github_key(&comment.project, &comment.item_key)?;
    let id = comment
        .comment_id
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("non-numeric github comment id `{}`", comment.comment_id))?;
    if comment.anchor_path.is_some() {
        conn.execute(
            "
            INSERT OR REPLACE INTO github_review_comments(id, owner, repo, number, path, html_url, \
             body, author, created_at, updated_at, synced_at_ms, repo_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            params![
                id,
                owner,
                repo,
                number,
                comment.anchor_path,
                comment.url.as_deref().unwrap_or_default(),
                comment.body,
                comment.author,
                comment.created_at,
                comment.updated_at,
                now_ms(),
                repo_id,
            ],
        )?;
        return Ok(());
    }
    if let Some(review_state) = &comment.review_state {
        conn.execute(
            "
            INSERT OR REPLACE INTO github_reviews(id, owner, repo, number, html_url, state, body, \
             author, submitted_at, synced_at_ms, repo_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
            params![
                id,
                owner,
                repo,
                number,
                comment.url,
                review_state,
                comment.body,
                comment.author,
                comment.created_at,
                now_ms(),
                repo_id,
            ],
        )?;
        return Ok(());
    }
    conn.execute(
        "
        INSERT OR REPLACE INTO github_comments(id, owner, repo, number, html_url, body, author, \
         created_at, updated_at, synced_at_ms, repo_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ",
        params![
            id,
            owner,
            repo,
            number,
            comment.url.as_deref().unwrap_or_default(),
            comment.body,
            comment.author,
            comment.created_at,
            comment.updated_at,
            now_ms(),
            repo_id,
        ],
    )?;
    Ok(())
}
pub(crate) fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    // github_fts is a standalone (own-content) FTS5 table, so a DELETE + re-insert is the correct
    // rebuild — unlike the external-content chunk_fts / commit_fts, which must rebuild via
    // INSERT(t) VALUES('rebuild'). This is a WHOLE-TABLE rebuild (every repo's rows in a
    // consolidated DB): the DELETE clears all rows and each per-kind SELECT re-reads the entire
    // base table, so github_fts is repopulated complete for EVERY repo, each row stamped its
    // base row's `repo_id` (V041). The papertrail cache is small, so the whole-table cost is
    // acceptable — a per-repo incremental rebuild is not worth the bookkeeping. Each query
    // selects the eight `FtsRow` columns in order (id, owner, repo, number, url, title, body,
    // repo_id); kinds without a title (comment, review) select an empty string for that slot,
    // and review comments surface the file path as the title.
    conn.execute("DELETE FROM github_fts", [])?;
    insert_fts_rows(
        conn,
        "issue",
        "SELECT id, owner, repo, number, html_url, title, body, repo_id FROM github_issues",
    )?;
    insert_fts_rows(
        conn,
        "comment",
        "SELECT id, owner, repo, number, html_url, '', body, repo_id FROM github_comments",
    )?;
    insert_fts_rows(
        conn,
        "pull",
        "SELECT id, owner, repo, number, html_url, title, body, repo_id FROM github_pull_requests",
    )?;
    insert_fts_rows(
        conn,
        "review",
        "SELECT id, owner, repo, number, COALESCE(html_url, ''), '', body, repo_id FROM \
         github_reviews",
    )?;
    insert_fts_rows(
        conn,
        "review_comment",
        "SELECT id, owner, repo, number, html_url, COALESCE(path, ''), body, repo_id FROM \
         github_review_comments",
    )?;
    Ok(())
}
/// Bulk-load one GitHub item kind into `github_fts`. `sql` must select exactly the eight `FtsRow`
/// columns in order — `id, owner, repo, number, url, title, body, repo_id` — using an empty-string
/// literal in the title slot for kinds that carry no title. Adding a new item kind is a one-line
/// call here, not another copy of the load loop.
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
            row.get::<_, String>(7)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, title, body, repo_id) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind,
            item_id: &id.to_string(),
            url: &url,
            title: &title,
            body: &body,
            repo_id: &repo_id,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_fts(conn: &Connection, row: FtsRow<'_>) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            row.owner,
            row.repo,
            row.number,
            row.kind,
            row.item_id,
            row.url,
            row.title,
            row.body,
            classify_text(&format!("{}\n{}", row.title, row.body)),
            row.repo_id,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod fts_rebuild_tests {
    use rusqlite::Connection;

    use super::*;
    use crate::index::schema;

    fn item(kind: ItemKind, key: &str, title: &str, body: &str) -> PapertrailItem {
        PapertrailItem {
            project: "o/r".into(),
            item_kind: kind,
            item_key: key.into(),
            url: format!("http://item/{key}"),
            state: "open".into(),
            title: title.into(),
            body: body.into(),
            author: None,
            created_at: None,
            updated_at: None,
            merged_at: None,
        }
    }

    fn comment(key: &str, id: &str, body: &str) -> PapertrailComment {
        PapertrailComment {
            project: "o/r".into(),
            item_kind: ItemKind::Issue,
            item_key: key.into(),
            comment_id: id.into(),
            url: Some(format!("http://comment/{id}")),
            body: body.into(),
            author: None,
            created_at: None,
            updated_at: None,
            review_state: None,
            anchor_path: None,
        }
    }

    // rebuild_fts collapses five near-identical per-kind loaders into insert_fts_rows. This pins
    // the behaviour they shared: every comment routing target lands in github_fts under its own
    // item_kind, the body is tokenized/searchable, and the title slot is populated the way each
    // old loader did — the title text for issues/pulls, the file path for review comments, empty
    // for the rest. A change request keeps its issue-shadow row (shared GitHub numbering), so it
    // surfaces under BOTH the issue and pull kinds.
    #[test]
    fn rebuild_fts_indexes_every_item_kind_with_the_right_title() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();

        store_item(&conn, &item(ItemKind::Issue, "1", "issuetitle", "issuebody")).unwrap();
        store_comment(&conn, &comment("1", "10", "commentbody")).unwrap();
        let stored =
            store_item(&conn, &item(ItemKind::ChangeRequest, "2", "pulltitle", "pullbody"))
                .unwrap();
        assert_eq!(stored, 2, "a change request writes its issue-shadow row AND the pull row");
        // url None exercises the review loader's COALESCE(html_url, '').
        store_comment(&conn, &PapertrailComment {
            url: None,
            review_state: Some("approved".into()),
            ..comment("2", "20", "reviewbody")
        })
        .unwrap();
        store_comment(&conn, &PapertrailComment {
            anchor_path: Some("src/lib.rs".into()),
            ..comment("2", "30", "reviewcommentbody")
        })
        .unwrap();

        rebuild_fts(&conn).unwrap();

        // One row per stored table row, each keyed by its item_kind, with the expected title.
        let rows: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("SELECT item_kind, title FROM github_fts ORDER BY item_kind, title")
                .unwrap();
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
                ("issue".to_string(), "pulltitle".to_string()),
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

#[cfg(test)]
mod store_glue_tests {
    use rusqlite::Connection;

    use super::*;
    use crate::index::schema;

    // The reverse (owner, repo, number) mapping is the ONLY place normalized identities meet the
    // github_* key columns until the schema normalization — a non-GitHub-shaped identity must be
    // rejected loudly, never coerced into a wrong key.
    #[test]
    fn store_glue_rejects_non_github_shaped_identities() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();

        let item = PapertrailItem {
            project: "no-slash".into(),
            item_kind: ItemKind::Issue,
            item_key: "7".into(),
            url: String::new(),
            state: "open".into(),
            title: String::new(),
            body: String::new(),
            author: None,
            created_at: None,
            updated_at: None,
            merged_at: None,
        };
        let err = store_item(&conn, &item).unwrap_err().to_string();
        assert!(err.contains("malformed papertrail project"), "{err}");

        let err = store_item(&conn, &PapertrailItem {
            project: "o/r".into(),
            item_key: "PROJ-7".into(),
            ..item.clone()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("non-numeric github item key"), "{err}");

        let err = store_comment(&conn, &PapertrailComment {
            project: "o/r".into(),
            item_kind: ItemKind::Issue,
            item_key: "7".into(),
            comment_id: "abc".into(),
            url: None,
            body: String::new(),
            author: None,
            created_at: None,
            updated_at: None,
            review_state: None,
            anchor_path: None,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("non-numeric github comment id"), "{err}");
    }
}
