//! End-to-end (#1164): a granted contributor's `memory_create` authors onto the OWNER's stream.
//! The owner publishes + grants a separate identity; the grant + ownership sync to the contributor
//! (modelled by ingesting the owner's account log + content); the contributor is configured with
//! `set_contribution_owner`; and its live authoring folds accepted on the owner's `/2` stream — not
//! on any stream of its own.

use rag_rat_oplog::{
    AccessMode, ContentRefoldBudget, account_entries_for_sync, account_ingest,
    content_entries_for_sync, content_ingest, local_account, owner_stream_v2_id_for_account,
    settle_pending_content_refolds,
};
use rag_rat_query::memory::RepoMemoryCreate;
use rusqlite::{Connection, params};

use crate::memory_write::{create_memory, enable_public_authoring, grant_repo_writer};

const NOW: i64 = 1_700_000_000_000;
const REPO: &str = "repo-a";

fn scoped_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
        [REPO],
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [REPO],
    )
    .unwrap();
    conn
}

fn concept(title: &str) -> RepoMemoryCreate {
    RepoMemoryCreate {
        kind: "Concept".into(),
        title: title.into(),
        body: "body".into(),
        confidence: "high".into(),
        created_by: Some("t".into()),
        source: Some("agent".into()),
        tags: Vec::new(),
        payload_json: None,
        bind: Default::default(),
    }
}

/// Replicate `src`'s account log (roster/ownership/grant) + content into `dst` — the state a
/// contributor gains by syncing the owner's log.
fn sync_account_into(dst: &Connection, src: &Connection, account: rag_rat_oplog::AccountId) {
    for e in account_entries_for_sync(src, account).unwrap() {
        account_ingest(dst, &e.signed_bytes, NOW).unwrap();
    }
    for e in content_entries_for_sync(src, account).unwrap() {
        content_ingest(dst, &e.signed_bytes, NOW).unwrap();
    }
    settle_pending_content_refolds(dst, &ContentRefoldBudget::unbounded(), NOW).unwrap();
}

#[test]
fn a_configured_contributor_authors_onto_the_owners_stream() {
    // OWNER: publish, author, and mint its account.
    let owner = scoped_conn();
    let owner_account = local_account(&owner, NOW).unwrap();
    assert!(enable_public_authoring(&owner, NOW).unwrap());
    create_memory(&owner, concept("owner-note")).unwrap();

    // CONTRIBUTOR: a separate identity for the same repo.
    let contributor = scoped_conn();
    let contributor_account = local_account(&contributor, NOW).unwrap();
    assert_ne!(owner_account, contributor_account, "separate identities");

    // The owner grants the contributor Writer, then the grant + ownership sync to the contributor.
    grant_repo_writer(&owner, contributor_account, NOW).unwrap();
    sync_account_into(&contributor, &owner, owner_account);

    // Configure contribution (paste flow) and author.
    let owner_hex = rag_rat_base::hash::hex_lower(&owner_account.to_bytes());
    crate::memory_write::set_contribution_owner(&contributor, &owner_hex, NOW).unwrap();
    create_memory(&contributor, concept("contributor-note")).unwrap();

    // The contributor's memory is an ACCEPTED entry on the OWNER's stream, authored under the
    // contributor's own account (the grant is the cross-account authorization).
    let owner_stream =
        owner_stream_v2_id_for_account(REPO, owner_account, AccessMode::PublicRead).unwrap();
    let on_owner_stream: i64 = contributor
        .query_row(
            "SELECT COUNT(*) FROM content_entries
             WHERE stream_id = ?1 AND accepted = 1 AND author_account_id = ?2",
            params![owner_stream.to_bytes().as_slice(), contributor_account.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        on_owner_stream, 1,
        "the contributor's memory folds accepted on the owner's stream, authored by the \
         contributor",
    );

    // And the contributor did NOT establish a stream of its own (it does not own; backfill
    // skipped).
    let contributor_owned: i64 = contributor
        .query_row(
            "SELECT COUNT(*) FROM account_stream_ownership WHERE account_id = ?1",
            [contributor_account.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(contributor_owned, 0, "a contributor owns no stream of its own");
}

#[test]
fn contributing_without_a_synced_grant_fails_loud() {
    // Configure contribution to an owner whose grant was never synced: authoring must error with a
    // clear message, not silently author to the wrong place.
    let contributor = scoped_conn();
    local_account(&contributor, NOW).unwrap();
    let owner_hex = "ab".repeat(32);
    crate::memory_write::set_contribution_owner(&contributor, &owner_hex, NOW).unwrap();
    let err = create_memory(&contributor, concept("x")).unwrap_err().to_string();
    assert!(err.contains("writer grant"), "missing grant is a loud failure: {err}");
}
