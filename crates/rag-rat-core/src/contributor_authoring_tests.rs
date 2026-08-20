//! End-to-end (#1164): a granted contributor's `memory_create` authors onto the OWNER's stream.
//! The owner publishes + grants a separate identity; the grant + ownership sync to the contributor
//! (modelled by ingesting the owner's account log + content); the contributor is configured with
//! `set_contribution_owner`; and its live authoring folds accepted on the owner's `/2` stream — not
//! on any stream of its own.
//!
//! Plus the read-only half (#1156): a SUBSCRIBER mirrors the same published owner's stream without
//! a grant and without publishing itself, and keeps authoring its own memories onto its own stream.

use rag_rat_oplog::{
    AccessMode, ContentRefoldBudget, account_entries_for_sync, account_ingest,
    content_entries_for_sync, content_ingest, local_account, owner_stream_v2_id_for_account,
    settle_pending_content_refolds,
};
use rag_rat_query::memory::RepoMemoryCreate;
use rusqlite::{Connection, params};

use crate::memory_write::{
    create_memory, enable_public_authoring, grant_repo_writer, revoke_repo_writer,
};

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

/// The memory titles this store holds for the repo as `origin='synced'` rows — the ones the drain
/// materialized from a stream, and the only ones its removal anti-joins condemn.
fn synced_memory_titles(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT title FROM repo_memories WHERE repo_id = ?1 AND origin = 'synced' ORDER BY \
             title",
        )
        .unwrap();
    stmt.query_map([REPO], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// Put one node into `stream`'s projection directly — the shape a peer's accepted entry folds into,
/// without needing that peer's device key. What the drain reads is the projection, so this is the
/// state a sibling device's memory arrives in.
fn plant_projected_node(conn: &Connection, stream: rag_rat_oplog::StreamId, id: &str, title: &str) {
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
         VALUES (?1, ?2, ?3, 'active')",
        params![
            stream.to_bytes().as_slice(),
            id,
            serde_json::json!({
                "kind": "Concept", "title": title, "body": "b",
                "confidence": "high", "source": "agent", "tags": [], "payload": null,
            })
            .to_string(),
        ],
    )
    .unwrap();
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
    plant_projected_node(&contributor, own_stream, "ghost", "local-stream-ghost");
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

/// A SUBSCRIBER owns its stream, so the import reconcile would happily sign onto it — and the next
/// drain, reading the OWNER's stream, would condemn every `origin='synced'` row the import brought
/// in. The guard covers both configurations for that reason, not just the one that owns no stream.
#[test]
fn the_import_reconcile_refuses_while_subscribed() {
    let (_owner, subscriber, _owner_account) = subscription_pair();
    let err = crate::memory_write::reconcile_owner_stream_for_repo(&subscriber, REPO, NOW)
        .unwrap_err()
        .to_string();
    assert!(err.contains("while subscribed"), "refuses with an actionable message: {err}");
    assert!(err.contains("sync unsubscribe"), "and hands over the escape: {err}");
}

/// The drain resolves the subscription owner LAZILY — a contributing repo never consults the key,
/// so a corrupt value there must not error the drain of a repo whose authority is already decided.
#[test]
fn a_corrupt_subscription_owner_does_not_break_a_contributing_repos_drain() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    rag_rat_db::meta::set_repo_meta(&contributor, REPO, "memory_subscription_owner", "not-hex")
        .unwrap();
    crate::drain_synced_memory(&contributor).unwrap();
    assert!(
        memory_titles(&contributor).contains(&"owner-note".to_string()),
        "the contribution owner decides the authority without reading the subscription key",
    );
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
    // case): `set_contribution_owner` forgets the OUTGOING stream's drain watermark, so the next
    // drain would otherwise make a FULL pass over the new owner's empty projection.
    crate::memory_write::set_contribution_owner(&contributor, &"ab".repeat(32), NOW).unwrap();
    crate::drain_synced_memory(&contributor).unwrap();

    let titles = memory_titles(&contributor);
    assert!(
        titles.contains(&"owner-note".to_string()),
        "an unverified owner stream removes nothing: {titles:?}",
    );
}

/// `sync contribute` is re-pointable too, and by the same one-stream rule the owner's memories are
/// REMOVED when it is cleared: the repo's own stream becomes the authority again and the owner's
/// projection stops materializing here. What the contributor authored keeps its `origin='local'`
/// and never depended on the mirror. (The unset's watermark discipline is exercised on the
/// SUBSCRIPTION side —
/// `unsubscribing_restores_the_sibling_device_memories_the_subscription_removed` — where the repo
/// has an own stream that was drained to a current watermark before the re-point; a contributor
/// owns no stream at all, so there is no watermark here to be wrong about.)
#[test]
fn uncontributing_re_points_the_repo_back_at_its_own_stream() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    create_memory(&contributor, concept("contributor-note")).unwrap();
    crate::drain_synced_memory(&contributor).unwrap();
    assert_eq!(
        synced_memory_titles(&contributor),
        vec!["owner-note".to_string()],
        "the owner's memory is the mirrored row",
    );

    assert!(
        crate::memory_write::clear_contribution_owner(&contributor).unwrap(),
        "a contribution was configured",
    );
    crate::drain_synced_memory(&contributor).unwrap();
    let titles = memory_titles(&contributor);
    assert!(
        !titles.contains(&"owner-note".to_string()),
        "the owner's stream stops materializing this repo: {titles:?}",
    );
    assert!(
        titles.contains(&"contributor-note".to_string()),
        "what this store authored is its own, not the mirror's: {titles:?}",
    );
    assert!(
        !crate::memory_write::clear_contribution_owner(&contributor).unwrap(),
        "clearing an absent contribution reports it rather than re-pointing anything",
    );
}

/// Fixture for the revoke tests: a contribution pair where the contributor has authored one
/// accepted memory and the OWNER has collected it (synced the contributor's account and drained).
fn revocable_pair() -> (Connection, Connection, rag_rat_oplog::AccountId, rag_rat_oplog::AccountId)
{
    let (owner, contributor, owner_account) = contribution_pair();
    let contributor_account = local_account(&contributor, NOW).unwrap();
    create_memory(&contributor, concept("contributor-note")).unwrap();
    sync_account_into(&owner, &contributor, contributor_account);
    crate::drain_synced_memory(&owner).unwrap();
    assert!(
        memory_titles(&owner).contains(&"contributor-note".to_string()),
        "the owner collected the contribution before the revoke",
    );
    (owner, contributor, owner_account, contributor_account)
}

#[test]
fn a_departed_revoke_keeps_accepted_contributions_and_condemns_later_ones() {
    let (owner, contributor, owner_account, contributor_account) = revocable_pair();

    // Soft revoke, addressed by an unambiguous PREFIX of the grantee id (what the operator sees
    // in `sync grants`). The chain-tail cut vouches for what this store has accepted.
    let contributor_hex = rag_rat_base::hash::hex_lower(&contributor_account.to_bytes());
    let report = revoke_repo_writer(
        &owner,
        &contributor_hex[..8],
        rag_rat_oplog::RevokeReason::Departed,
        None,
        NOW,
    )
    .unwrap();
    assert_eq!(report.grantee_account_id, contributor_hex, "the prefix resolved");
    assert_eq!(report.cuts.len(), 1, "one grantee device tail is vouched");
    crate::drain_synced_memory(&owner).unwrap();
    assert!(
        memory_titles(&owner).contains(&"contributor-note".to_string()),
        "prior accepted work survives a departed revoke",
    );

    // Work the contributor authors AFTER the cut (it has not yet learned of the revoke) folds
    // condemned at the owner — collected but never materialized.
    create_memory(&contributor, concept("late-note")).unwrap();
    sync_account_into(&owner, &contributor, contributor_account);
    crate::drain_synced_memory(&owner).unwrap();
    let titles = memory_titles(&owner);
    assert!(!titles.contains(&"late-note".to_string()), "beyond-cut work is condemned: {titles:?}");
    assert!(titles.contains(&"contributor-note".to_string()), "the vouched prefix stays");

    // Once the revocation syncs back, the contributor's authoring fails loud.
    sync_account_into(&contributor, &owner, owner_account);
    let err = create_memory(&contributor, concept("post-revoke")).unwrap_err().to_string();
    assert!(err.contains("grant"), "authoring after a synced revoke names the cause: {err}");
}

#[test]
fn a_compromised_revoke_evicts_everything_the_grantee_authored() {
    let (owner, _contributor, _owner_account, contributor_account) = revocable_pair();

    let contributor_hex = rag_rat_base::hash::hex_lower(&contributor_account.to_bytes());
    let report = revoke_repo_writer(
        &owner,
        &contributor_hex,
        rag_rat_oplog::RevokeReason::Compromised,
        None,
        NOW,
    )
    .unwrap();
    assert!(report.cuts.is_empty(), "a compromised key's own timeline is not trusted");
    crate::drain_synced_memory(&owner).unwrap();
    let titles = memory_titles(&owner);
    assert!(
        !titles.contains(&"contributor-note".to_string()),
        "everything from the grantee is quarantined: {titles:?}",
    );
    assert!(titles.contains(&"owner-note".to_string()), "the owner's own work is untouched");
}

#[test]
fn keep_until_carves_an_accepted_prefix_back_into_a_compromised_revoke() {
    let (owner, _contributor, _owner_account, contributor_account) = revocable_pair();

    // The vouched (device, seq) comes from the OWNER's own accepted copy — the witness the
    // revoked side cannot rewrite.
    let (device, seq): (Vec<u8>, Vec<u8>) = owner
        .query_row(
            "SELECT device_fingerprint, seq FROM content_entries
             WHERE author_account_id = ?1 AND accepted = 1",
            [contributor_account.to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let device = rag_rat_oplog::DeviceFingerprint::from_bytes(device.try_into().unwrap());
    let seq = u64::from_be_bytes(seq.try_into().unwrap());

    let contributor_hex = rag_rat_base::hash::hex_lower(&contributor_account.to_bytes());
    let report = revoke_repo_writer(
        &owner,
        &contributor_hex,
        rag_rat_oplog::RevokeReason::Compromised,
        Some((device, seq)),
        NOW,
    )
    .unwrap();
    assert_eq!(report.cuts, vec![(rag_rat_base::hash::hex_lower(&device.to_bytes()), seq)]);
    crate::drain_synced_memory(&owner).unwrap();
    assert!(
        memory_titles(&owner).contains(&"contributor-note".to_string()),
        "the carved-back prefix survives the hard revoke",
    );
}

/// Re-pointing `sync contribute` at another owner must not strand contributions already authored
/// (#1185): servability rests on EVIDENCE of past authorship, not on the mutable configuration,
/// so the previous owner can still pull what this store wrote — and a revoked grant withdraws
/// the exposure again.
#[test]
fn re_pointing_contribution_keeps_the_authored_account_servable() {
    let (owner, contributor, _owner_account, contributor_account) = revocable_pair();
    assert!(
        crate::sync_driver::account_is_public_kb(&contributor, contributor_account).unwrap(),
        "a configured, granted contributor serves its log",
    );

    // The re-point (typo, or a deliberate second contribution before its grant exists): the
    // configured target no longer verifies, but the authored entries on the previous owner's
    // stream are durable evidence.
    crate::memory_write::set_contribution_owner(&contributor, &"ab".repeat(32), NOW).unwrap();
    assert!(
        crate::sync_driver::account_is_public_kb(&contributor, contributor_account).unwrap(),
        "authored evidence keeps the log servable after a re-point",
    );

    // Revoking the grant withdraws the exposure: evidence of authorship alone is not enough
    // without a LIVE grant on the target stream.
    let contributor_hex = rag_rat_base::hash::hex_lower(&contributor_account.to_bytes());
    revoke_repo_writer(
        &owner,
        &contributor_hex,
        rag_rat_oplog::RevokeReason::Compromised,
        None,
        NOW,
    )
    .unwrap();
    sync_account_into(&contributor, &owner, local_account(&owner, NOW).unwrap());
    assert!(
        !crate::sync_driver::account_is_public_kb(&contributor, contributor_account).unwrap(),
        "a revoked grant withdraws the authored-evidence exposure",
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

/// The guard keys on EVIDENCE, not configuration. `sync uncontribute` empties the configured
/// targets, but the entries this store already authored onto the owner's stream stay there and the
/// owner can fetch them only while this account owns no private stream. Keyed on configuration, the
/// unset would let the very next memory write author a `Private` `StreamOwn` — append-only, never
/// un-authorable — permanently un-serving the account and stranding those contributions.
#[test]
fn an_ex_contributor_still_refuses_to_establish_a_private_stream() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    let contributor_account = local_account(&contributor, NOW).unwrap();
    create_memory(&contributor, concept("contributor-note")).unwrap();

    assert!(
        crate::memory_write::clear_contribution_owner(&contributor).unwrap(),
        "a contribution was configured",
    );
    assert!(
        crate::memory_write::contribution_targets(&contributor).unwrap().is_empty(),
        "nothing is configured any more — the authored entries are all that is left",
    );

    let err = create_memory(&contributor, concept("post-unset-note")).unwrap_err().to_string();
    assert!(err.contains("PRIVATE memory stream"), "the refusal names the conflict: {err}");
    assert!(err.contains("already authored"), "and cites the evidence, not the config: {err}");
    assert!(err.contains("sync publish"), "and hands over the escape: {err}");
    assert!(
        rag_rat_oplog::account_is_fully_public(&contributor, contributor_account).unwrap(),
        "and nothing private was authored, so the owner can still fetch the contributions",
    );
}

/// The refusal is a stream-establishment POLICY and belongs where a user is asking for a write.
/// INDEX MAINTENANCE reaches the very same reconcile — `rag-rat reconcile`, every watcher
/// incremental pass and `rag-rat index` all route the idle-repo ghost heal through
/// `heal_memory_oplog_ghosts` — and there the refusal must be SKIPPED. Propagated, an ordinary
/// `sync uncontribute` would break all three on the next pass, with no ghost to heal, while the
/// `uncontribute` itself still reported success.
#[test]
fn index_maintenance_survives_the_ex_contributors_private_stream_refusal() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    let contributor_account = local_account(&contributor, NOW).unwrap();
    create_memory(&contributor, concept("contributor-note")).unwrap();
    assert!(crate::memory_write::clear_contribution_owner(&contributor).unwrap());

    // Authoring — where a user asked for a write — still refuses.
    assert!(
        create_memory(&contributor, concept("post-unset-note"))
            .unwrap_err()
            .to_string()
            .contains("PRIVATE memory stream"),
        "the authoring path keeps the refusal",
    );

    // Maintenance does not. Twice, because the pass runs on every reconcile and must not become
    // sticky on the first refusal.
    for pass in 1..=2 {
        crate::memory_write::heal_memory_oplog_ghosts(&contributor, NOW)
            .unwrap_or_else(|err| panic!("the ghost heal must survive pass {pass}: {err:#}"));
    }
    assert!(
        rag_rat_oplog::account_is_fully_public(&contributor, contributor_account).unwrap(),
        "and it skipped rather than established the private stream it refused",
    );
}

/// The maintenance tolerance covers the private-stream refusal and nothing else. A real failure in
/// the ghost heal means the reconcile did NOT run, and swallowing it would hide that on every
/// `rag-rat reconcile` and every watcher pass — silently, forever, since maintenance is the only
/// caller that could report it. Break a table the heal reads and require the call to say so.
#[test]
fn the_ghost_heal_propagates_a_failure_that_is_not_the_private_stream_refusal() {
    let conn = scoped_conn();
    local_account(&conn, NOW).unwrap();
    create_memory(&conn, concept("note")).unwrap();
    conn.execute_batch("ALTER TABLE account_stream_ownership RENAME TO ownership_moved_away")
        .unwrap();

    let err = crate::memory_write::heal_memory_oplog_ghosts(&conn, NOW)
        .expect_err("a non-refusal failure must fail the pass rather than be skipped");
    assert!(
        format!("{err:#}").contains("account_stream_ownership"),
        "and the read failure surfaces instead of being swallowed: {err:#}",
    );
}

/// The guard must block on exactly what the SERVING side would still serve. Once the owner runs
/// `sync revoke`, its own pull already cannot reach this account for that stream, so establishing a
/// private stream here strands nothing — and refusing anyway would be permanent, with no recourse
/// the store can take. The authorship evidence itself survives the revoke (the entries stay
/// `accepted = 1`), so only the servability filter can tell the two apart.
#[test]
fn a_revoked_ex_contributor_may_establish_its_private_stream() {
    // A DEPARTED revoke, whose chain-tail cut vouches the contribution the owner already accepted:
    // the grant goes, the accepted entry stays.
    let (owner, contributor, owner_account, contributor_account) = revocable_pair();
    revoke_repo_writer(
        &owner,
        &rag_rat_base::hash::hex_lower(&contributor_account.to_bytes()),
        rag_rat_oplog::RevokeReason::Departed,
        None,
        NOW,
    )
    .unwrap();
    sync_account_into(&contributor, &owner, owner_account);
    assert!(crate::memory_write::clear_contribution_owner(&contributor).unwrap());
    assert!(
        !rag_rat_oplog::authored_foreign_streams(&contributor, contributor_account)
            .unwrap()
            .is_empty(),
        "the authorship evidence outlives the revoke — an unfiltered guard would still block",
    );

    create_memory(&contributor, concept("post-revoke-note"))
        .expect("a revoked authorship strands nothing, so the private stream is allowed");
    assert!(
        memory_titles(&contributor).contains(&"post-revoke-note".to_string()),
        "and the memory is really there",
    );
}

/// Both unsetters must work in the state the drain's LAZY owner resolution was made tolerant of: a
/// `memory_subscription_owner` value that will not parse. Resolving it strictly on the re-point
/// would bail and roll back, leaving the recovery commands unusable in precisely the state they are
/// for. A side that cannot resolve had no stream to drain, so tolerating THAT loses nothing — and
/// the tolerance stops there, see
/// `clearing_a_foreign_owner_propagates_a_resolution_failure_that_is_not_a_parse`.
#[test]
fn clearing_a_foreign_owner_tolerates_an_unparseable_owner_key() {
    // OUTGOING side: the corrupt key is the one being cleared, so the FIRST resolution meets it.
    let (_owner, subscriber, _owner_account) = subscription_pair();
    rag_rat_db::meta::set_repo_meta(&subscriber, REPO, "memory_subscription_owner", "not-hex")
        .unwrap();
    assert!(
        crate::memory_write::clear_subscription_owner(&subscriber).unwrap(),
        "the corrupt subscription is cleared rather than defended",
    );
    assert!(
        rag_rat_db::meta::repo_meta(&subscriber, REPO, "memory_subscription_owner")
            .unwrap()
            .is_none(),
        "and the row is gone, not rolled back",
    );

    // INCOMING side: the contribution resolves fine on the way out, and the corrupt subscription
    // key is what the repo falls back to once it is gone.
    let (_owner2, contributor, _owner_account2) = contribution_pair();
    rag_rat_db::meta::set_repo_meta(&contributor, REPO, "memory_subscription_owner", "not-hex")
        .unwrap();
    assert!(
        crate::memory_write::clear_contribution_owner(&contributor).unwrap(),
        "the contribution clears even though the fallback owner key is corrupt",
    );
    assert!(
        rag_rat_db::meta::repo_meta(&contributor, REPO, "memory_contribution_owner")
            .unwrap()
            .is_none(),
        "and the row is gone, not rolled back",
    );
}

/// The unsetters' tolerance covers the unparseable owner key and nothing else. A real read failure
/// means the outgoing stream is UNKNOWN, not absent — swallowing it skips a watermark clear the
/// command's own promise depends on, and reports success for a repo whose memories will not come
/// back. Break the ownership read the resolution goes through and require the command to say so.
#[test]
fn clearing_a_foreign_owner_propagates_a_resolution_failure_that_is_not_a_parse() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    contributor
        .execute_batch("ALTER TABLE account_stream_ownership RENAME TO ownership_moved_away")
        .unwrap();

    let err = crate::memory_write::clear_contribution_owner(&contributor).unwrap_err();
    assert!(
        format!("{err:#}").contains("account_stream_ownership"),
        "the read failure surfaces instead of being swallowed: {err:#}",
    );
    contributor
        .execute_batch("ALTER TABLE ownership_moved_away RENAME TO account_stream_ownership")
        .unwrap();
    assert!(
        rag_rat_db::meta::repo_meta(&contributor, REPO, "memory_contribution_owner")
            .unwrap()
            .is_some(),
        "and the configuration is rolled back, not half-cleared",
    );
}

/// A published owner plus a SUBSCRIBER of it: the owner's log + content are synced in, and the
/// subscriber is configured read-only. Deliberately no grant and no `enable_public_authoring` on
/// the subscriber — a read-only mirror needs neither. The subscriber authors one memory FIRST, so
/// it owns a PRIVATE stream of its own before it subscribes: that is what makes the two absent
/// guards observable (an account that owns nothing is vacuously fully-public and holds no rival
/// stream at all). Returns `(owner, subscriber, owner_account)`.
fn subscription_pair() -> (Connection, Connection, rag_rat_oplog::AccountId) {
    let owner = scoped_conn();
    let owner_account = local_account(&owner, NOW).unwrap();
    assert!(enable_public_authoring(&owner, NOW).unwrap());
    create_memory(&owner, concept("owner-note")).unwrap();

    let subscriber = scoped_conn();
    let subscriber_account = local_account(&subscriber, NOW).unwrap();
    assert_ne!(owner_account, subscriber_account, "separate identities");
    create_memory(&subscriber, concept("subscriber-note")).unwrap();
    assert!(
        !rag_rat_oplog::account_is_fully_public(&subscriber, subscriber_account).unwrap(),
        "the subscriber owns a private stream — the guard `sync contribute` carries would refuse \
         it",
    );
    sync_account_into(&subscriber, &owner, owner_account);

    let owner_hex = rag_rat_base::hash::hex_lower(&owner_account.to_bytes());
    crate::memory_write::set_subscription_owner(&subscriber, &owner_hex, NOW).unwrap();
    (owner, subscriber, owner_account)
}

/// The read-only half of cross-account mirroring (#1156): the two guards `sync contribute` carries
/// (an effective Writer grant, a fully-public local account) exist only because a contributor
/// AUTHORS onto the owner's stream. A subscriber writes nothing there and is never pulled from, so
/// it mirrors the same published stream with neither.
#[test]
fn a_subscriber_mirrors_a_published_owner_without_a_grant_or_a_public_account() {
    let (_owner, subscriber, owner_account) = subscription_pair();
    let subscriber_account = local_account(&subscriber, NOW).unwrap();
    let owner_stream =
        owner_stream_v2_id_for_account(REPO, owner_account, AccessMode::PublicRead).unwrap();
    assert!(
        rag_rat_oplog::effective_writer_grant(
            &subscriber,
            owner_account,
            owner_stream,
            subscriber_account,
        )
        .unwrap()
        .is_none(),
        "the subscriber holds no writer grant",
    );
    assert!(
        !rag_rat_oplog::account_is_fully_public(&subscriber, subscriber_account).unwrap(),
        "and its own account is not fully public",
    );

    crate::drain_synced_memory(&subscriber).unwrap();
    let titles = memory_titles(&subscriber);
    assert!(
        titles.contains(&"owner-note".to_string()),
        "the owner's memories materialize here: {titles:?}",
    );
}

/// The re-point is DRAIN-side only. A subscriber's own memories keep being authored onto its OWN
/// stream (never the owner's — it holds no grant and would be rejected), and they stay
/// `origin='local'`, which the drain's synced-only removal anti-joins spare even though they are
/// absent from the mirrored owner's projection.
#[test]
fn a_subscribers_own_memories_go_to_its_own_stream_and_survive_the_mirror() {
    let (_owner, subscriber, owner_account) = subscription_pair();
    let subscriber_account = local_account(&subscriber, NOW).unwrap();
    create_memory(&subscriber, concept("my-own-note")).unwrap();

    // Authored on a stream the SUBSCRIBER owns, not on the owner's.
    let owner_stream =
        owner_stream_v2_id_for_account(REPO, owner_account, AccessMode::PublicRead).unwrap();
    let on_owner_stream: i64 = subscriber
        .query_row(
            "SELECT COUNT(*) FROM content_entries WHERE stream_id = ?1 AND author_account_id = ?2",
            params![owner_stream.to_bytes().as_slice(), subscriber_account.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(on_owner_stream, 0, "a subscriber authors nothing onto the owner's stream");
    let owned: i64 = subscriber
        .query_row(
            "SELECT COUNT(*) FROM account_stream_ownership WHERE account_id = ?1",
            [subscriber_account.to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owned, 1, "it established and authored a stream of its own");

    // Draining the owner's stream must not condemn it: it is `origin='local'`, not synced.
    crate::drain_synced_memory(&subscriber).unwrap();
    let titles = memory_titles(&subscriber);
    assert!(
        titles.contains(&"my-own-note".to_string()),
        "own work survives the mirror: {titles:?}"
    );
    assert!(titles.contains(&"owner-note".to_string()), "and the owner's arrived: {titles:?}");
}

/// A configured owner is not yet an AUTHORITY — the subscription twin of the contribution case.
/// `sync subscribe` succeeds before the owner's log is synced, and a mistyped id derives a stream
/// that never exists; both leave an EMPTY projection, and draining it would let the removal
/// anti-joins condemn every synced row the repo currently reads.
#[test]
fn an_unsynced_or_mistyped_subscription_owner_never_becomes_removal_authority() {
    let (_owner, subscriber, _owner_account) = subscription_pair();
    crate::drain_synced_memory(&subscriber).unwrap();
    assert!(memory_titles(&subscriber).contains(&"owner-note".to_string()));

    // Re-point at an owner whose log was never synced. `set_subscription_owner` forgets the
    // OUTGOING stream's drain watermark, so the next drain would otherwise make a FULL pass over
    // the new owner's empty projection.
    crate::memory_write::set_subscription_owner(&subscriber, &"ab".repeat(32), NOW).unwrap();
    crate::drain_synced_memory(&subscriber).unwrap();
    let titles = memory_titles(&subscriber);
    assert!(
        titles.contains(&"owner-note".to_string()),
        "an unverified owner stream removes nothing: {titles:?}",
    );
}

/// Contribution and subscription both RE-POINT the one stream that materializes a repo's memories,
/// so configuring the second would make which one the drain honors ambiguous. Both setters refuse,
/// rather than silently superseding the other's setup.
#[test]
fn subscription_and_contribution_refuse_to_coexist() {
    let (_owner, contributor, _owner_account) = contribution_pair();
    let err = crate::memory_write::set_subscription_owner(&contributor, &"ab".repeat(32), NOW)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already contributes"), "subscribe names the conflict: {err}");

    let (_owner2, subscriber, _owner_account2) = subscription_pair();
    let err = crate::memory_write::set_contribution_owner(&subscriber, &"ab".repeat(32), NOW)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already subscribes"), "contribute names the conflict: {err}");
}

/// The subscriber's OWN account is the rival authority the contribution half never has: a
/// contributor's own stream is empty by construction, but a subscriber's is actively authored — and
/// it is also where this account's OTHER DEVICES' memories arrive, materialized `origin='synced'`.
/// Re-pointing the repo at the owner condemns every one of them (absent from the owner's
/// projection), which is correct — exactly one stream materializes a repo — but must not be a
/// one-way door. `sync unsubscribe` re-points back and the rows come home, the owner's going in
/// turn, each side re-materialized from its own stream's projection.
///
/// The fixture: a subscriber that already held sibling-device rows when it subscribed, drained
/// once. Returns the subscriber connection; the assertions about what the subscription removed are
/// made here, so both recovery routes below start from the same verified state.
fn subscribed_store_that_held_sibling_device_memories() -> Connection {
    let owner = scoped_conn();
    let owner_account = local_account(&owner, NOW).unwrap();
    assert!(enable_public_authoring(&owner, NOW).unwrap());
    create_memory(&owner, concept("owner-note")).unwrap();

    // A store of a DIFFERENT account holding two memories of its own account's: one this device
    // authored (`origin='local'`) and one a SIBLING DEVICE authored, which reaches this device as a
    // row in the shared stream's projection and materializes `origin='synced'`.
    let subscriber = scoped_conn();
    local_account(&subscriber, NOW).unwrap();
    create_memory(&subscriber, concept("my-own-note")).unwrap();
    let own_stream = rag_rat_oplog::owned_stream_v2_id(&subscriber, REPO).unwrap().unwrap();
    plant_projected_node(&subscriber, own_stream, "sibling", "sibling-device-note");
    crate::drain_synced_memory(&subscriber).unwrap();
    assert_eq!(
        synced_memory_titles(&subscriber),
        vec!["sibling-device-note".to_string()],
        "the sibling device's memory is materialized as a synced row",
    );

    // Subscribe. The owner's stream becomes the one authority, so the sibling's row is condemned.
    sync_account_into(&subscriber, &owner, owner_account);
    let owner_hex = rag_rat_base::hash::hex_lower(&owner_account.to_bytes());
    crate::memory_write::set_subscription_owner(&subscriber, &owner_hex, NOW).unwrap();
    crate::drain_synced_memory(&subscriber).unwrap();
    let titles = memory_titles(&subscriber);
    assert!(
        !titles.contains(&"sibling-device-note".to_string()),
        "the subscription REMOVES the sibling device's memories, it does not leave them stale: \
         {titles:?}",
    );
    assert!(titles.contains(&"owner-note".to_string()), "the owner's arrived: {titles:?}");
    assert!(titles.contains(&"my-own-note".to_string()), "own work is spared: {titles:?}");
    subscriber
}

/// The recovery route the operator has: `sync unsubscribe`.
#[test]
fn unsubscribing_restores_the_sibling_device_memories_the_subscription_removed() {
    let subscriber = subscribed_store_that_held_sibling_device_memories();

    // Unsubscribe: the own stream is the authority again, and only a FULL pass restores what the
    // re-point condemned — which is why the re-point forgets both streams' drain watermarks.
    assert!(
        crate::memory_write::clear_subscription_owner(&subscriber).unwrap(),
        "a subscription was configured",
    );
    crate::drain_synced_memory(&subscriber).unwrap();
    let titles = memory_titles(&subscriber);
    assert!(
        titles.contains(&"sibling-device-note".to_string()),
        "unsubscribing re-materializes the sibling device's memories: {titles:?}",
    );
    assert!(
        !titles.contains(&"owner-note".to_string()),
        "and the owner's go in turn — one stream materializes the repo: {titles:?}",
    );
    assert!(titles.contains(&"my-own-note".to_string()), "own work is spared again: {titles:?}");

    assert!(
        !crate::memory_write::clear_subscription_owner(&subscriber).unwrap(),
        "clearing an absent subscription reports it rather than re-pointing anything",
    );
}

/// The other route back is not a command at all: the operator (or a future write path) drops the
/// `memory_subscription_owner` row directly. That bypasses the unsetter entirely, so recovery rests
/// on what the SETTER did — it forgot the outgoing (own) stream's drain watermark on the way out,
/// and nothing has drained that stream since, so the next pass is still a full one.
#[test]
fn dropping_the_subscription_meta_row_also_re_materializes_the_repo() {
    let subscriber = subscribed_store_that_held_sibling_device_memories();
    rag_rat_db::meta::delete_repo_meta(&subscriber, REPO, "memory_subscription_owner").unwrap();

    crate::drain_synced_memory(&subscriber).unwrap();
    let titles = memory_titles(&subscriber);
    assert!(
        titles.contains(&"sibling-device-note".to_string()),
        "the own stream drains in full again: {titles:?}",
    );
    assert!(!titles.contains(&"owner-note".to_string()), "and the owner's go in turn: {titles:?}",);
}

/// Subscribing to your OWN account would persist a configuration the drain ignores
/// (`authoritative_content_stream` falls through when the configured owner is this store), so it
/// is refused rather than stored as a lie.
#[test]
fn subscribing_to_your_own_account_is_refused() {
    let store = scoped_conn();
    let account = local_account(&store, NOW).unwrap();
    let own_hex = rag_rat_base::hash::hex_lower(&account.to_bytes());
    let err =
        crate::memory_write::set_subscription_owner(&store, &own_hex, NOW).unwrap_err().to_string();
    assert!(err.contains("your own account"), "the refusal names the cause: {err}");
}
