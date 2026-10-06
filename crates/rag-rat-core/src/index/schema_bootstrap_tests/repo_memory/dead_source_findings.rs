use rag_rat_dream::{DreamOptions, ReviewVerdict, WorklistFinding};
use rag_rat_query::memory::EdgeTarget;

use super::hybrid_search::db_with_memories;
use super::*;

fn dream(db: &IndexDatabase, now_ms: i64) -> Vec<WorklistFinding> {
    db.dream_run(DreamOptions { now_ms, limit: 0, verify: false, include_reviewed: false })
        .unwrap()
        .findings
        .into_iter()
        .filter(|finding| finding.kind == "dependent_of_dead_source")
        .collect()
}

fn link(db: &IndexDatabase, from: &str, relation: &str, to: &str) {
    db.memory_edge_add(from, relation, EdgeTarget::Node { repo_id: None, node_id: to.to_string() })
        .unwrap();
}

fn set_status(db: &IndexDatabase, id: &str, status: &str) {
    db.memory_update(rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: id.to_string(),
        kind: None,
        title: None,
        body: None,
        confidence: None,
        status: Some(status.to_string()),
        tags: None,
        payload_json: None,
    })
    .unwrap();
}

fn notes() -> [(&'static str, &'static str); 4] {
    [
        ("Source rule", "Retries stop after three attempts."),
        ("Summary of the rule", "Retries are bounded; see the source rule."),
        ("Checklist from the summary", "Check the retry bound before shipping."),
        ("Replacement rule", "Retries stop after five attempts."),
    ]
}

#[test]
fn a_memory_derived_from_an_obsolete_one_is_flagged() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    assert!(dream(&db, 1000).is_empty(), "a standing source flags nothing");
    set_status(&db, &ids[0], "obsolete");
    let findings = dream(&db, 2000);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].subject, ids[1]);
    assert!(findings[0].evidence.contains(&ids[0]), "{}", findings[0].evidence);
    assert!(findings[0].evidence.contains("which is obsolete"), "{}", findings[0].evidence);
}

#[test]
fn suspicion_propagates_along_derived_from_and_names_the_root() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    link(&db, &ids[2], "derived_from", &ids[1]);
    set_status(&db, &ids[0], "obsolete");
    let findings = dream(&db, 1000);
    let subjects = findings.iter().map(|f| f.subject.as_str()).collect::<Vec<_>>();
    assert!(subjects.contains(&ids[1].as_str()) && subjects.contains(&ids[2].as_str()));
    let indirect = findings.iter().find(|f| f.subject == ids[2]).unwrap();
    assert!(
        indirect.evidence.contains(&format!("which rests on {}", ids[0])),
        "{}",
        indirect.evidence
    );
}

#[test]
fn the_finding_points_at_what_superseded_the_source() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    link(&db, &ids[3], "supersedes", &ids[0]);
    set_status(&db, &ids[0], "obsolete");
    let findings = dream(&db, 1000);
    assert!(
        findings[0].evidence.contains(&format!("; {} supersedes it", ids[3])),
        "{}",
        findings[0].evidence
    );
}

#[test]
fn restoring_the_source_clears_the_finding() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    set_status(&db, &ids[0], "obsolete");
    assert_eq!(dream(&db, 1000).len(), 1);
    set_status(&db, &ids[0], "active");
    assert!(dream(&db, 2000).is_empty());
}

#[test]
fn a_source_anchored_only_to_gone_code_is_dead() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    db.storage
        .connection()
        .execute("UPDATE repo_memory_bindings SET anchor_status = 'gone' WHERE memory_id = ?1", [
            &ids[0],
        ])
        .unwrap();
    let findings = dream(&db, 1000);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].evidence.contains("anchored only to code that is gone"),
        "{}",
        findings[0].evidence
    );
}

#[test]
fn a_derived_from_cycle_with_no_dead_member_flags_nothing() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[0], "derived_from", &ids[1]);
    link(&db, &ids[1], "derived_from", &ids[0]);
    assert!(dream(&db, 1000).is_empty());
}

#[test]
fn a_synced_edge_with_no_resolved_target_is_still_followed() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    // What the sync drain stores: the durable anchor, with this device's resolution left unset.
    db.storage
        .connection()
        .execute(
            "UPDATE repo_node_edges SET target_node_id = NULL, anchor_status = 'unresolved',
                 origin = 'synced' WHERE source_node_id = ?1",
            [&ids[1]],
        )
        .unwrap();
    set_status(&db, &ids[0], "obsolete");
    let findings = dream(&db, 1000);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].evidence.contains(&format!("derived from {}, which is obsolete", ids[0])),
        "the edge resolves to its real source: {}",
        findings[0].evidence
    );
}

/// A target missing from this store is unknown, not dead: the rows that vanish in production are
/// the sync drain's (a quarantined peer update, an edge that arrived before its target), memories
/// still alive elsewhere that a reviewer must not be told to act on.
#[test]
fn a_target_missing_from_this_store_flags_nothing() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    let conn = db.storage.connection();
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = ?1", [&ids[0]]).unwrap();
    conn.execute("DELETE FROM repo_memories WHERE id = ?1", [&ids[0]]).unwrap();
    assert!(dream(&db, 1000).is_empty());
}

#[test]
fn renaming_the_dead_source_keeps_a_dismissal() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[1], "derived_from", &ids[0]);
    set_status(&db, &ids[0], "obsolete");
    let first = dream(&db, 1000);
    db.review_dream_finding(&first[0].id, ReviewVerdict::Dismiss, 1500).unwrap();
    db.storage
        .connection()
        .execute("UPDATE repo_memories SET title = 'Source rule, renamed' WHERE id = ?1", [&ids[0]])
        .unwrap();
    assert!(dream(&db, 2000).is_empty(), "the claim names ids, so the dismissal holds");
}

#[test]
fn a_rewrite_that_supersedes_its_source_is_not_its_dependent() {
    let (_root, db, ids) = db_with_memories(&notes());
    link(&db, &ids[3], "derived_from", &ids[0]);
    link(&db, &ids[3], "supersedes", &ids[0]);
    set_status(&db, &ids[0], "obsolete");
    assert!(dream(&db, 1000).is_empty());
}
