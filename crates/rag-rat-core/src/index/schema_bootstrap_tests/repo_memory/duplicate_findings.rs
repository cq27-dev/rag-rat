use rag_rat_dream::{DreamOptions, ReviewVerdict, WorklistFinding};

use super::hybrid_search::db_with_memories;
use super::*;

const BODY: &str = "Retries stop after three attempts, and the backoff doubles between them.";

fn dream(db: &IndexDatabase, now_ms: i64) -> Vec<WorklistFinding> {
    db.dream_run(DreamOptions { now_ms, limit: 0, verify: false, include_reviewed: false })
        .unwrap()
        .findings
        .into_iter()
        .filter(|finding| finding.kind == "memory_duplicate")
        .collect()
}

fn pair(a: &str, b: &str) -> String {
    if a <= b { format!("{a}|{b}") } else { format!("{b}|{a}") }
}

#[test]
fn a_restated_memory_pair_opens_one_finding() {
    let (_root, db, ids) = db_with_memories(&[
        ("Retry budget", BODY),
        ("Retry budget rule", BODY),
        ("Lock order", "Take the index lock before the meta lock."),
    ]);
    let findings = dream(&db, 1000);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].subject, pair(&ids[0], &ids[1]));
    assert!(findings[0].evidence.contains("\"Retry budget rule\""), "{}", findings[0].evidence);
}

#[test]
fn a_dismissed_pair_stays_dismissed_until_a_note_changes() {
    let (_root, db, ids) = db_with_memories(&[("Retry budget", BODY), ("Retry budget rule", BODY)]);
    let first = dream(&db, 1000);
    db.review_dream_finding(&first[0].id, ReviewVerdict::Dismiss, 1500).unwrap();
    assert!(dream(&db, 2000).is_empty(), "the dismissal holds across runs");
    db.memory_update(rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: ids[1].clone(),
        kind: None,
        title: Some("Retry budget, restated".to_string()),
        body: None,
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    })
    .unwrap();
    assert_eq!(dream(&db, 3000).len(), 1, "an edit re-opens the pair for review");
}

#[test]
fn retiring_one_memory_resolves_the_finding() {
    let (_root, db, ids) = db_with_memories(&[("Retry budget", BODY), ("Retry budget rule", BODY)]);
    assert_eq!(dream(&db, 1000).len(), 1);
    db.memory_mark_obsolete(&ids[1]).unwrap();
    assert!(dream(&db, 2000).is_empty());
}

#[test]
fn a_run_that_cannot_compare_leaves_existing_findings_alone() {
    let (_root, db, _ids) =
        db_with_memories(&[("Retry budget", BODY), ("Retry budget rule", BODY)]);
    assert_eq!(dream(&db, 1000).len(), 1);
    // all-MiniLM has no measured threshold, so this run computes no pairs at all.
    ai::set_repo_meta(
        db.storage.connection(),
        "active_embedding_model",
        rag_rat_base::embedding_models::FASTEMBED_MODEL_ID,
    )
    .unwrap();
    assert_eq!(dream(&db, 2000).len(), 1, "not re-evaluated, so not resolved");
}

/// Drop the cached vector of the memory whose embedding text contains `marker` — what a model
/// switch, a cache GC or a pending re-embed leaves behind.
fn evict_vector(db: &IndexDatabase, marker: &str) {
    let conn = db.storage.connection();
    let version = ai::active_embedding_model_version(conn, HASH_MODEL_ID).unwrap();
    let source = rag_rat_query::memory::memory_embedding_sources(conn)
        .unwrap()
        .into_iter()
        .find(|source| source.text.contains(marker))
        .expect("memory with marker");
    let hash = ai::embedding_input_hash(HASH_MODEL_ID, &version, &source.text);
    assert_eq!(
        conn.execute("DELETE FROM embedding_cache WHERE input_hash = ?1", [hash]).unwrap(),
        1
    );
}

fn dismissed(db: &IndexDatabase, now_ms: i64) -> usize {
    db.dream_run(DreamOptions { now_ms, limit: 0, verify: false, include_reviewed: true })
        .unwrap()
        .findings
        .into_iter()
        .filter(|f| {
            f.kind == "memory_duplicate" && f.status == rag_rat_dream::FindingStatus::Dismissed
        })
        .count()
}

#[test]
fn a_pair_this_run_could_not_compare_keeps_its_verdict() {
    let (_root, db, _ids) =
        db_with_memories(&[("Retry budget", BODY), ("Retry budget rule", BODY)]);
    let first = dream(&db, 1000);
    db.review_dream_finding(&first[0].id, ReviewVerdict::Dismiss, 1500).unwrap();
    evict_vector(&db, "Retry budget rule");
    assert_eq!(dismissed(&db, 2000), 1, "not compared, so carried with its verdict");
    ai::refresh_memory_vectors(db.storage.connection(), None).unwrap();
    assert_eq!(dismissed(&db, 3000), 1, "compared again, still dismissed");
    assert!(dream(&db, 4000).is_empty(), "never re-opened");
}
