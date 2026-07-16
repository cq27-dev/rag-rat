use super::*;

// KEY SCOPING: every papertrail cache key folds `repo_id` (the V044/V045 discipline, carried into
// the provider-neutral tables). Items key `(repo_id, tracker, project, item_kind, item_key)` —
// `item_kind` is part of the identity because namespaced providers (GitLab) can hold an issue and
// a change request under the same key. Comments key `(repo_id, tracker, project, comment_id)`.
// Refs scope uniqueness through `idx_papertrail_refs_unique` with a leading `repo_id`. So on a
// consolidated DB two repos referencing the SAME external item each own a full copy — item AND
// comments — and a conflict only ever fires WITHIN one repo (this repo re-syncing its own rows):
// the writers' `ON CONFLICT` upserts refresh content in place and can never restamp a sibling
// repo's copy.
//
// FTS MIRROR (incremental): `papertrail_fts` is maintained INCREMENTALLY — each writer deletes and
// reinserts ONLY its own mirror row(s), keyed the same way as its base row (`doc_kind = 'item'`
// rows by the item identity, `doc_kind = 'comment'` rows by the comment identity). The
// whole-table [`rebuild_fts`] survives ONLY for the full re-walk / recovery paths (and the schema
// migration backfill); routine syncs never pay the whole-mirror cost, which matters once the
// mirror is a whole-project cache instead of a small referenced-only one.

pub fn store_ref(conn: &Connection, reference: &PapertrailRef) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    // `idx_papertrail_refs_unique` leads with `repo_id`, so a conflict is always THIS repo
    // re-discovering its OWN ref — keep the first-sighting row untouched (`DO NOTHING` preserves
    // the original `discovered_at_ms`/`ref_kind`). A sibling repo referencing the same item gets
    // its own distinct row rather than conflicting.
    conn.execute(
        "
        INSERT INTO papertrail_refs(
            tracker, project, item_key, item_kind, ref_kind, source_kind, source_path, \
         source_commit, source_text, discovered_at_ms, repo_id
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ON CONFLICT(repo_id, tracker, project, COALESCE(item_kind, ''), item_key, source_kind, \
         COALESCE(source_path, ''), COALESCE(source_commit, ''), source_text) DO NOTHING
        ",
        params![
            reference.tracker.as_db_str(),
            reference.project,
            reference.item_key,
            reference.item_kind.map(ItemKind::as_db_str),
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

/// Store one normalized item into the papertrail cache: exactly ONE `papertrail_items` row per
/// item — `item_kind` is part of the identity, so a change request is NOT shadowed by an issue row
/// (the github_* schema's shared-numbering shadow is gone). The item's own `papertrail_fts` mirror
/// row is refreshed INCREMENTALLY: delete + reinsert of this item's `doc_kind = 'item'` row only.
pub fn store_item(
    conn: &Connection,
    tracker: Tracker,
    item: &PapertrailItem,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    conn.execute(
        "
        INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, author, created_at, updated_at, merged_at, synced_at_ms, repo_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(repo_id, tracker, project, item_kind, item_key) DO UPDATE SET
            url = excluded.url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, merged_at = excluded.merged_at,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            tracker.as_db_str(),
            item.project,
            item.item_kind.as_db_str(),
            item.item_key,
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
    conn.execute(
        "DELETE FROM papertrail_fts
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
           AND item_key = ?5 AND doc_kind = 'item'",
        params![
            repo_id,
            tracker.as_db_str(),
            item.project,
            item.item_kind.as_db_str(),
            item.item_key
        ],
    )?;
    insert_fts(conn, FtsRow {
        tracker: tracker.as_db_str(),
        project: &item.project,
        item_kind: item.item_kind.as_db_str(),
        item_key: &item.item_key,
        doc_kind: "item",
        comment_id: "",
        url: &item.url,
        title: &item.title,
        body: &item.body,
        repo_id: &repo_id,
    })?;
    Ok(())
}

/// Store one normalized comment into the unified `papertrail_comments` cache. The old github_*
/// three-way routing (comments / reviews / review comments) collapses into one row shape: a
/// review event carries `review_state`, a file-anchored comment carries `anchor_path`, a plain
/// thread comment carries neither. The comment's own `papertrail_fts` mirror row is refreshed
/// INCREMENTALLY, keyed by its `(repo_id, tracker, project, comment_id)` identity.
pub fn store_comment(
    conn: &Connection,
    tracker: Tracker,
    comment: &PapertrailComment,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    conn.execute(
        "
        INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, author, created_at, updated_at, review_state, anchor_path, synced_at_ms, repo_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(repo_id, tracker, project, comment_id) DO UPDATE SET
            item_kind = excluded.item_kind, item_key = excluded.item_key, url = excluded.url,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, review_state = excluded.review_state,
            anchor_path = excluded.anchor_path, synced_at_ms = excluded.synced_at_ms
        ",
        params![
            tracker.as_db_str(),
            comment.project,
            comment.item_kind.as_db_str(),
            comment.item_key,
            comment.comment_id,
            comment.url,
            comment.body,
            comment.author,
            comment.created_at,
            comment.updated_at,
            comment.review_state,
            comment.anchor_path,
            now_ms(),
            repo_id,
        ],
    )?;
    conn.execute(
        "DELETE FROM papertrail_fts
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND comment_id = ?4
           AND doc_kind = 'comment'",
        params![repo_id, tracker.as_db_str(), comment.project, comment.comment_id],
    )?;
    insert_fts(conn, FtsRow {
        tracker: tracker.as_db_str(),
        project: &comment.project,
        item_kind: comment.item_kind.as_db_str(),
        item_key: &comment.item_key,
        doc_kind: "comment",
        comment_id: &comment.comment_id,
        url: comment.url.as_deref().unwrap_or_default(),
        // A file-anchored comment surfaces its path in the title slot (the affordance the old
        // review-comment loader had); other comments carry no title.
        title: comment.anchor_path.as_deref().unwrap_or_default(),
        body: &comment.body,
        repo_id: &repo_id,
    })?;
    Ok(())
}

/// WHOLE-TABLE rebuild of the `papertrail_fts` mirror from the base tables — the full re-walk /
/// recovery path ONLY (and the V060 migration backfill). Routine syncs maintain the mirror
/// incrementally in [`store_item`] / [`store_comment`]; calling this per sync would re-pay the
/// whole-mirror cost the incremental writers exist to avoid. `papertrail_fts` is a standalone
/// (own-content) FTS5 table, so DELETE + re-insert is the correct rebuild — unlike the
/// external-content chunk_fts / commit_fts, which must rebuild via INSERT(t) VALUES('rebuild').
/// Every repo's rows are re-derived (each stamped its base row's `repo_id`), and `classification`
/// is recomputed by [`insert_fts`].
pub fn rebuild_fts(conn: &Connection) -> rusqlite::Result<()> {
    // Whole-table delete + per-row reinserts must be one atomic unit when standalone (#610);
    // the full-rewalk caller already runs inside its own transaction and SQLite rejects nested
    // BEGINs, so the fence only wraps autocommit callers (the corruption heals hold their own).
    if conn.is_autocommit() {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = rebuild_fts_inner(conn).and_then(|()| conn.execute_batch("COMMIT"));
        // A failed COMMIT does not always auto-rollback (e.g. SQLITE_BUSY keeps the transaction
        // open) — never leave this long-lived connection stuck inside one.
        if result.is_err() && !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        return result;
    }
    rebuild_fts_inner(conn)
}

fn rebuild_fts_inner(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM papertrail_fts", [])?;
    {
        let mut stmt = conn.prepare(
            "SELECT tracker, project, item_kind, item_key, url, title, body, repo_id
             FROM papertrail_items",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            insert_fts(conn, FtsRow {
                tracker: &row.get::<_, String>(0)?,
                project: &row.get::<_, String>(1)?,
                item_kind: &row.get::<_, String>(2)?,
                item_key: &row.get::<_, String>(3)?,
                doc_kind: "item",
                comment_id: "",
                url: &row.get::<_, String>(4)?,
                title: &row.get::<_, String>(5)?,
                body: &row.get::<_, String>(6)?,
                repo_id: &row.get::<_, String>(7)?,
            })?;
        }
    }
    let mut stmt = conn.prepare(
        "SELECT tracker, project, item_kind, item_key, comment_id, COALESCE(url, ''),
                COALESCE(anchor_path, ''), body, repo_id
         FROM papertrail_comments",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        insert_fts(conn, FtsRow {
            tracker: &row.get::<_, String>(0)?,
            project: &row.get::<_, String>(1)?,
            item_kind: &row.get::<_, String>(2)?,
            item_key: &row.get::<_, String>(3)?,
            doc_kind: "comment",
            comment_id: &row.get::<_, String>(4)?,
            url: &row.get::<_, String>(5)?,
            title: &row.get::<_, String>(6)?,
            body: &row.get::<_, String>(7)?,
            repo_id: &row.get::<_, String>(8)?,
        })?;
    }
    Ok(())
}

pub(crate) fn insert_fts(conn: &Connection, row: FtsRow<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO papertrail_fts(tracker, project, item_kind, item_key, doc_kind, comment_id, \
         url, title, body, classification, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            row.tracker,
            row.project,
            row.item_kind,
            row.item_key,
            row.doc_kind,
            row.comment_id,
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
mod fts_mirror_tests {
    use rag_rat_db::schema;
    use rusqlite::Connection;

    use super::*;

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
            tags: Vec::new(),
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

    fn fts_rows(conn: &Connection) -> Vec<(String, String, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT doc_kind, item_kind, title, body FROM papertrail_fts
                 ORDER BY doc_kind, item_kind, comment_id, body",
            )
            .unwrap();
        let mapped = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .unwrap();
        mapped.map(Result::unwrap).collect()
    }

    // Every stored row lands in papertrail_fts exactly once: items under doc_kind='item' with the
    // item title, comments under doc_kind='comment' with the anchor path (when file-anchored) in
    // the title slot. A change request is ONE row — the github issue-shadow duplication is gone.
    #[test]
    fn writers_mirror_every_row_with_the_right_title_slot() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();

        store_item(&conn, Tracker::Github, &item(ItemKind::Issue, "1", "issuetitle", "issuebody"))
            .unwrap();
        store_comment(&conn, Tracker::Github, &comment("1", "10", "commentbody")).unwrap();
        store_item(
            &conn,
            Tracker::Github,
            &item(ItemKind::ChangeRequest, "2", "crtitle", "crbody"),
        )
        .unwrap();
        // url None exercises the COALESCE('') slots.
        store_comment(&conn, Tracker::Github, &PapertrailComment {
            url: None,
            review_state: Some("approved".into()),
            item_kind: ItemKind::ChangeRequest,
            ..comment("2", "20", "reviewbody")
        })
        .unwrap();
        store_comment(&conn, Tracker::Github, &PapertrailComment {
            anchor_path: Some("src/lib.rs".into()),
            item_kind: ItemKind::ChangeRequest,
            ..comment("2", "30", "reviewcommentbody")
        })
        .unwrap();

        assert_eq!(fts_rows(&conn), vec![
            ("comment".into(), "change_request".into(), String::new(), "reviewbody".into()),
            (
                "comment".into(),
                "change_request".into(),
                "src/lib.rs".into(),
                "reviewcommentbody".into()
            ),
            ("comment".into(), "issue".into(), String::new(), "commentbody".into()),
            ("item".into(), "change_request".into(), "crtitle".into(), "crbody".into()),
            ("item".into(), "issue".into(), "issuetitle".into(), "issuebody".into()),
        ]);

        // The body column is tokenized and queryable via MATCH.
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM papertrail_fts WHERE papertrail_fts MATCH \
                 'reviewcommentbody'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "comment body is tokenized and searchable");

        // And the whole-table rebuild reconverges onto the SAME derived set — the writers and the
        // full re-walk path must never drift apart.
        let incremental = fts_rows(&conn);
        rebuild_fts(&conn).unwrap();
        assert_eq!(fts_rows(&conn), incremental, "rebuild reconverges onto the incremental set");
    }

    // Pins the INCREMENTAL path: the writers must touch ONLY their own mirror rows. The test
    // poisons the mirror with a foreign row no base table backs — a whole-table rebuild (DELETE
    // all + re-derive) would ERASE it, so this test FAILS if a writer is ever routed through
    // rebuild_fts. It also pins that a re-store replaces the item's own row (no duplicate) while a
    // sibling item's mirror row is left byte-identical.
    #[test]
    fn store_item_touches_only_its_own_mirror_rows() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();

        store_item(&conn, Tracker::Github, &item(ItemKind::Issue, "1", "one", "first body"))
            .unwrap();
        store_item(&conn, Tracker::Github, &item(ItemKind::Issue, "2", "two", "second body"))
            .unwrap();
        // The poison row: present ONLY in the mirror. Any whole-table rebuild would delete it
        // (no base row re-derives it); the incremental writer must leave it alone.
        conn.execute(
            "INSERT INTO papertrail_fts(tracker, project, item_kind, item_key, doc_kind, \
             comment_id, url, title, body, classification, repo_id)
             VALUES ('github', 'o/r', 'issue', '999', 'item', '', 'u', 'poison title', 'poison \
             body', 'other', 'poison-repo')",
            [],
        )
        .unwrap();

        store_item(&conn, Tracker::Github, &item(ItemKind::Issue, "1", "one", "updated body"))
            .unwrap();

        let poison: i64 = conn
            .query_row(
                "SELECT count(*) FROM papertrail_fts WHERE item_key = '999' AND title = 'poison \
                 title' AND body = 'poison body' AND repo_id = 'poison-repo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            poison, 1,
            "the incremental writer must not touch foreign mirror rows — a whole-table rebuild \
             would have erased the poison row"
        );
        let sibling: i64 = conn
            .query_row(
                "SELECT count(*) FROM papertrail_fts WHERE item_key = '2' AND body = 'second body'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sibling, 1, "a sibling item's mirror row is untouched");
        let own: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT body FROM papertrail_fts WHERE item_key = '1' AND doc_kind = 'item'",
                )
                .unwrap();
            let rows = stmt.query_map([], |row| row.get(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(own, vec!["updated body".to_string()], "re-store replaced its own row exactly");
    }

    // Same pin for the comment writer: only its own (repo, tracker, project, comment_id) mirror
    // row is replaced.
    #[test]
    fn store_comment_touches_only_its_own_mirror_row() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();

        store_comment(&conn, Tracker::Github, &comment("1", "10", "keep me")).unwrap();
        conn.execute(
            "INSERT INTO papertrail_fts(tracker, project, item_kind, item_key, doc_kind, \
             comment_id, url, title, body, classification, repo_id)
             VALUES ('github', 'o/r', 'issue', '1', 'comment', '888', 'u', '', 'poison comment', \
             'other', 'poison-repo')",
            [],
        )
        .unwrap();

        store_comment(&conn, Tracker::Github, &comment("1", "10", "replaced body")).unwrap();

        let poison: i64 = conn
            .query_row(
                "SELECT count(*) FROM papertrail_fts WHERE comment_id = '888' AND body = 'poison \
                 comment'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(poison, 1, "foreign comment mirror rows are untouched");
        let own: Vec<String> = {
            let mut stmt =
                conn.prepare("SELECT body FROM papertrail_fts WHERE comment_id = '10'").unwrap();
            let rows = stmt.query_map([], |row| row.get(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(own, vec!["replaced body".to_string()], "re-store replaced its own row");
    }
}
