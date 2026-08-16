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

/// The memory titles this store holds for the repo, sorted.
fn memory_titles(conn: &Connection) -> Vec<String> {
    let mut stmt =
        conn.prepare("SELECT title FROM repo_memories WHERE repo_id = ?1 ORDER BY title").unwrap();
    stmt.query_map([REPO], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
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

/// An owner that published + authored `owner-note` and granted a SEPARATE contributor identity
/// Writer, with the owner's log synced in and contribution configured on the contributor side.
/// Returns `(owner, contributor, owner_account)`.
fn contribution_pair() -> (Connection, Connection, rag_rat_oplog::AccountId) {
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

    // Configure contribution (the paste flow).
    let owner_hex = rag_rat_base::hash::hex_lower(&owner_account.to_bytes());
    crate::memory_write::set_contribution_owner(&contributor, &owner_hex, NOW).unwrap();
    (owner, contributor, owner_account)
}

#[test]
fn a_configured_contributor_authors_onto_the_owners_stream() {
    let (_owner, contributor, owner_account) = contribution_pair();
    let contributor_account = local_account(&contributor, NOW).unwrap();
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

    // Read-back (#1164): draining the OWNER's synced stream materializes the owner's memory into
    // the contributor's own tables — while its own memory (a direct table write) stays. So the
    // contributor reads the union, not just what it wrote.
    crate::drain_synced_memory(&contributor).unwrap();
    let titles = memory_titles(&contributor);
    assert!(
        titles.contains(&"owner-note".to_string()),
        "the contributor reads back the owner's memory after draining the owner's stream: \
         {titles:?}",
    );
    assert!(titles.contains(&"contributor-note".to_string()), "and still sees its own: {titles:?}",);
}

/// EXACTLY ONE stream materializes a repo. A contributor's own owned stream is NOT it — nothing is
/// ever authored there, so its projection is a rival authority whose removal anti-join reads every
/// row the owner's stream materialized as condemned. Draining both would delete the owner's
/// memories (and, on the next pass with a moved epoch, restore them and delete the contributor's) —
/// a ping-pong the per-stream watermarks then freeze. Poison the local stream's projection and
/// assert the drain never reads it.
#[test]
fn the_contributors_own_stream_is_not_a_rival_authority_over_the_repo() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    crate::drain_synced_memory(&contributor).unwrap();
    assert!(memory_titles(&contributor).contains(&"owner-note".to_string()));

    // Plant a row on the contributor's OWN owned stream and make that stream look freshly changed.
    // If the drain still treated it as an authority it would materialize `local-stream-ghost` and
    // condemn `owner-note` (absent from this projection) on the very next pass.
    let own_stream = rag_rat_oplog::owned_stream_v2_id(&contributor, REPO).unwrap().unwrap();
    contributor
        .execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'ghost', ?2, 'active')",
            params![
                own_stream.to_bytes().as_slice(),
                serde_json::json!({
                    "kind": "Concept", "title": "local-stream-ghost", "body": "b",
                    "confidence": "high", "source": "agent", "tags": [], "payload": null,
                })
                .to_string(),
            ],
        )
        .unwrap();
    let tx = contributor.unchecked_transaction().unwrap();
    rag_rat_oplog::clear_content_drain_watermark(&tx, own_stream).unwrap();
    tx.commit().unwrap();

    crate::drain_synced_memory(&contributor).unwrap();
    let titles = memory_titles(&contributor);
    assert!(
        titles.contains(&"owner-note".to_string()),
        "the owner's stream stays authoritative: {titles:?}"
    );
    assert!(
        !titles.contains(&"local-stream-ghost".to_string()),
        "the contributor's own stream is not drained into the repo: {titles:?}",
    );
}

/// `origin` is AUTHORSHIP, not drain authority. A contribution is written by THIS store, so it
/// stays `'local'` — that is what a public seed (`origin='local'` only) reads to decide whose
/// memories it carries, and re-purposing the column to mean "removable by the drain" would silently
/// drop every grantee write from a seed. The accepted consequence: a condemned contribution stays
/// readable HERE (this store's own writing), while content it RECEIVED is `'synced'` and the
/// drain's anti-join does remove it — a revoke never leaves another account's condemned content
/// behind.
#[test]
fn a_contribution_keeps_its_local_authorship_while_received_content_stays_removable() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    create_memory(&contributor, concept("contributor-note")).unwrap();
    crate::drain_synced_memory(&contributor).unwrap();

    let origin = |title: &str| -> String {
        contributor
            .query_row("SELECT origin FROM repo_memories WHERE title = ?1", [title], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        origin("contributor-note"),
        "local",
        "this store authored it, so a public seed must still carry it",
    );
    assert_eq!(
        origin("owner-note"),
        "synced",
        "content received from the owner's stream stays under the drain's removal authority",
    );
}

/// The import reconcile FAILS in contribution mode rather than establishing a stream the
/// contributor does not own. Its callers author freshly-IMPORTED rows and then move on (legacy
/// consolidation renames the source database away), so silently reporting success would strand
/// those rows with no `NodeCreate` and leave every later op on them inert.
#[test]
fn the_import_reconcile_refuses_in_contribution_mode_instead_of_half_applying() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    create_memory(&contributor, concept("contributor-note")).unwrap();
    let err = crate::memory_write::reconcile_owner_stream_for_repo(&contributor, REPO, NOW)
        .unwrap_err()
        .to_string();
    assert!(err.contains("contribution mode"), "refuses with an actionable message: {err}");

    // And it established nothing on the way out. Scoped to the CONTRIBUTOR's account — the table
    // also holds the owner's record, ingested with the owner's log.
    let contributor_account = local_account(&contributor, NOW).unwrap();
    let owned: i64 = contributor
        .query_row(
            "SELECT COUNT(*) FROM account_stream_ownership WHERE account_id = ?1",
            [contributor_account.to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owned, 0, "no stream established for the contributor");
}

/// A configured owner is not yet an AUTHORITY. `sync contribute` succeeds before the owner's log
/// is synced (configure, then sync), and a mistyped owner id derives a stream that never exists —
/// both leave an EMPTY projection. Draining it would let the removal anti-joins condemn every
/// synced row the repo currently reads, so the drain does nothing until ownership has folded.
#[test]
fn an_unsynced_or_mistyped_owner_never_becomes_removal_authority() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    crate::drain_synced_memory(&contributor).unwrap();
    assert!(memory_titles(&contributor).contains(&"owner-note".to_string()));

    // Re-point at an owner whose log was never synced (the mistyped-id / configure-before-sync
    // case): `set_contribution_owner` clears the incoming stream's watermark, so the next drain
    // would otherwise make a FULL pass over an empty projection.
    crate::memory_write::set_contribution_owner(&contributor, &"ab".repeat(32), NOW).unwrap();
    crate::drain_synced_memory(&contributor).unwrap();

    let titles = memory_titles(&contributor);
    assert!(
        titles.contains(&"owner-note".to_string()),
        "an unverified owner stream removes nothing: {titles:?}",
    );
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

/// A store that owns a PRIVATE stream cannot be a contributor: content is served by its AUTHOR
/// account, the owner is not enrolled here, and an account is servable only when every stream it
/// owns is public — so the owner could never fetch what this store authored. Refuse while the
/// operator can still act on it, rather than authoring contributions into permanent unreachability.
#[test]
fn contributing_from_a_store_with_a_private_stream_is_refused() {
    let contributor = scoped_conn();
    local_account(&contributor, NOW).unwrap();
    // A second repo in the same index, synced privately — the ordinary way this arises.
    contributor
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', 'b', 0)",
            [],
        )
        .unwrap();
    {
        use rusqlite::{Transaction, TransactionBehavior};
        let tx = Transaction::new_unchecked(&contributor, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-b",
            AccessMode::Private,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let err = crate::memory_write::set_contribution_owner(&contributor, &"ab".repeat(32), NOW)
        .unwrap_err()
        .to_string();
    assert!(err.contains("private memory streams"), "the refusal names the cause: {err}");
    assert!(err.contains("dedicated index"), "and hands over the escape: {err}");
}

/// The configure-time refusal is a ONE-TIME check, so it cannot be the only one: ordinary authoring
/// in a SECOND repo would later establish that repo's default PRIVATE stream, and an account is
/// fetchable by a peer only while every stream it owns is public. The contributions this store has
/// already authored would silently become unreachable. Enforce it where the conflict is created.
#[test]
fn a_contributing_store_refuses_to_establish_a_private_stream_later() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    create_memory(&contributor, concept("contributor-note")).unwrap();

    // A second repo in the same index, authored the ordinary way — no sync configuration at all.
    contributor
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', 'b', 0)",
            [],
        )
        .unwrap();
    contributor
        .execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', \
             'repo-b')",
            [],
        )
        .unwrap();

    let err = create_memory(&contributor, concept("private-repo-note")).unwrap_err().to_string();
    assert!(err.contains("PRIVATE memory stream"), "the refusal names the conflict: {err}");
    assert!(err.contains("separate database"), "and hands over an escape: {err}");
    assert!(err.contains("sync publish"), "and the other escape: {err}");
}
