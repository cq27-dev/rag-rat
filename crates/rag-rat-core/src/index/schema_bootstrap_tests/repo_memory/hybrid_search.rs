use super::*;

/// An indexed markdown repo with the hash embedder active and one memory per `(title, body)`,
/// reconciled so each memory's vector is in `embedding_cache`.
fn db_with_memories(notes: &[(&str, &str)]) -> (ScratchRoot, IndexDatabase, Vec<String>) {
    let (root, _config, db, ids) = db_and_config_with_memories(notes);
    (root, db, ids)
}

fn db_and_config_with_memories(
    notes: &[(&str, &str)],
) -> (ScratchRoot, Config, IndexDatabase, Vec<String>) {
    let (root, mut config) = markdown_config("# Notes\nplain markdown so the index is not empty\n");
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    let ids = notes.iter().map(|(title, body)| create_memory(&db, title, body)).collect();
    db.reconcile_with_options_progress(ai::ReconcileOptions::default(), |_| {}).unwrap();
    (root, config, db, ids)
}

/// Overwrite the cached vector of every memory whose embedding text contains `marker` with the
/// hash embedding of `query`, so the vector arm scores that memory as a perfect match for a query
/// it shares no keyword with.
fn poison_memory_vector(db: &IndexDatabase, marker: &str, query: &str) {
    set_memory_vector(db, marker, &ai::hash_query_embedding(query).unwrap().vector);
}

/// A unit vector whose cosine with the hash embedding of `query` is `similarity`.
fn vector_at_similarity(query: &str, similarity: f32) -> Vec<f32> {
    let q = ai::hash_query_embedding(query).unwrap().vector;
    let o = ai::hash_query_embedding("unrelated filler words entirely").unwrap().vector;
    let along: f32 = o.iter().zip(&q).map(|(a, b)| a * b).sum();
    let mut perp = o.iter().zip(&q).map(|(o, q)| o - along * q).collect::<Vec<_>>();
    let norm = perp.iter().map(|x| x * x).sum::<f32>().sqrt();
    perp.iter_mut().for_each(|x| *x /= norm);
    let rest = (1.0 - similarity * similarity).sqrt();
    q.iter().zip(&perp).map(|(q, p)| similarity * q + rest * p).collect()
}

fn set_memory_vector(db: &IndexDatabase, marker: &str, vector: &[f32]) {
    let conn = db.storage.connection();
    let version = ai::active_embedding_model_version(conn, HASH_MODEL_ID).unwrap();
    let blob = ai::encode_vector(vector);
    let mut written = 0;
    for source in rag_rat_query::memory::memory_embedding_sources(conn).unwrap() {
        if !source.text.contains(marker) {
            continue;
        }
        let hash = ai::embedding_input_hash(HASH_MODEL_ID, &version, &source.text);
        written += conn
            .execute(
                "UPDATE embedding_cache SET vector_blob = ?1 WHERE input_hash = ?2",
                rusqlite::params![blob, hash],
            )
            .unwrap();
    }
    assert!(written > 0, "the reconcile must have cached a vector for `{marker}`");
}

fn create_memory(db: &IndexDatabase, title: &str, body: &str) -> String {
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: title.to_string(),
        body: body.to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            path: Some("README.md".to_string()),
            ..Default::default()
        },
    })
    .unwrap()
    .memory
    .memory_id
}

fn hash_dim() -> usize {
    rag_rat_base::embedding_models::spec(HASH_MODEL_ID).unwrap().dim
}

/// The memory's cached vector row under the hash model, if any: `(last_used_at_ms)`.
fn cached_last_used(db: &IndexDatabase, marker: &str) -> Option<i64> {
    let conn = db.storage.connection();
    let version = ai::active_embedding_model_version(conn, HASH_MODEL_ID).unwrap();
    let source = rag_rat_query::memory::memory_embedding_sources(conn)
        .unwrap()
        .into_iter()
        .find(|source| source.text.contains(marker))
        .expect("memory with marker");
    let hash = ai::embedding_input_hash(HASH_MODEL_ID, &version, &source.text);
    conn.query_row(
        "SELECT last_used_at_ms FROM embedding_cache WHERE input_hash = ?1",
        [hash],
        |r| r.get(0),
    )
    .ok()
}

/// The hash embedder, refusing any batch that contains `POISON` — a backend rejecting one input.
struct RefusesPoison;

impl ai::Embedder for RefusesPoison {
    fn model_id(&self) -> &str {
        HASH_MODEL_ID
    }

    fn dim(&self) -> usize {
        ai::HashEmbedder.dim()
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::ensure!(!texts.iter().any(|text| text.contains("POISON")), "input refused");
        ai::HashEmbedder.embed_batch(texts)
    }
}

fn search_ids(db: &IndexDatabase, query: &str) -> Vec<String> {
    db.memory_search(query, 10, rag_rat_base::config::MemorySurface::Full)
        .unwrap()
        .into_iter()
        .map(|memory| memory.memory_id)
        .collect()
}

#[test]
fn reconcile_embeds_live_memories_into_the_cache() {
    let (_root, db, _ids) = db_with_memories(&[
        ("Retry budget", "Retries stop after three attempts."),
        ("Lock order", "Take the index lock before the meta lock."),
    ]);
    let conn = db.storage.connection();
    let version = ai::active_embedding_model_version(conn, HASH_MODEL_ID).unwrap();
    for source in rag_rat_query::memory::memory_embedding_sources(conn).unwrap() {
        let hash = ai::embedding_input_hash(HASH_MODEL_ID, &version, &source.text);
        let cached: i64 = conn
            .query_row("SELECT COUNT(*) FROM embedding_cache WHERE input_hash = ?1", [&hash], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cached, 1, "memory {} has a cached vector", source.memory_id);
    }
    assert_eq!(
        ai::refresh_memory_vectors(conn, None).unwrap(),
        0,
        "a second refresh embeds nothing"
    );
}

#[test]
fn the_vector_arm_reaches_a_memory_no_keyword_matches() {
    let (_root, db, ids) = db_with_memories(&[
        ("Retry budget", "Retries stop after three attempts."),
        ("Lock order", "Take the index lock before the meta lock."),
    ]);
    assert!(search_ids(&db, "zebra quokka").is_empty(), "no keyword and no vector match");
    poison_memory_vector(&db, "Lock order", "zebra quokka");
    assert_eq!(search_ids(&db, "zebra quokka"), [ids[1].clone()]);
}

#[test]
fn the_vector_arm_reorders_keyword_hits() {
    let (_root, db, ids) = db_with_memories(&[
        ("Widget cache", "The widget cache widget keys are widget ids."),
        ("Widget lock", "Hold the widget lock while flushing."),
    ]);
    let keyword_order = search_ids(&db, "widget");
    assert_eq!(keyword_order.len(), 2);
    let (first, second) = (keyword_order[0].clone(), keyword_order[1].clone());
    let marker = |id: &str| if id == ids[0] { "Widget cache" } else { "Widget lock" };
    poison_memory_vector(&db, marker(&first), "zebra quokka");
    poison_memory_vector(&db, marker(&second), "widget");
    assert_eq!(search_ids(&db, "widget"), [second, first], "vector similarity reorders BM25 hits");
}

#[test]
fn an_edited_memory_never_serves_its_old_vector() {
    let (_root, db, ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    poison_memory_vector(&db, "Lock order", "zebra quokka");
    assert_eq!(search_ids(&db, "zebra quokka"), [ids[0].clone()]);
    db.memory_update(rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: ids[0].clone(),
        kind: None,
        title: None,
        body: Some("Take the meta lock first.".to_string()),
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    })
    .unwrap();
    assert!(search_ids(&db, "zebra quokka").is_empty(), "the edit is a cache miss");
}

#[test]
fn a_weak_vector_only_match_is_gated_out() {
    let (_root, db, ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    set_memory_vector(&db, "Lock order", &vector_at_similarity("zebra quokka", 0.3));
    assert!(search_ids(&db, "zebra quokka").is_empty(), "0.3 is below the vector-only floor");
    set_memory_vector(&db, "Lock order", &vector_at_similarity("zebra quokka", 0.6));
    assert_eq!(search_ids(&db, "zebra quokka"), [ids[0].clone()], "0.6 clears it");
}

#[test]
fn a_noop_refresh_never_builds_the_embedder() {
    let (_root, db, _ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    let embedded = ai::refresh_memory_vectors_with(
        db.storage.connection(),
        HASH_MODEL_ID,
        hash_dim(),
        None,
        || panic!("nothing is missing, so no model may be loaded"),
    )
    .unwrap();
    assert_eq!(embedded, 0);
}

#[test]
fn a_refused_memory_does_not_block_the_others() {
    let (_root, db, _ids) = db_with_memories(&[
        ("Alpha note", "POISON the backend rejects this one."),
        ("Beta note", "An ordinary note."),
        ("Gamma note", "Another ordinary note."),
    ]);
    db.storage.connection().execute("DELETE FROM embedding_cache", []).unwrap();
    let embedded = ai::refresh_memory_vectors_with(
        db.storage.connection(),
        HASH_MODEL_ID,
        hash_dim(),
        None,
        || Ok(Box::new(RefusesPoison)),
    )
    .unwrap();
    assert_eq!(embedded, 2, "the batch falls back to one-by-one and skips only the refused note");
    assert!(cached_last_used(&db, "Alpha note").is_none());
    assert!(cached_last_used(&db, "Beta note").is_some());
    assert!(cached_last_used(&db, "Gamma note").is_some());
}

/// An embedder whose backend is down: every call fails, and each call is counted.
struct Unreachable(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ai::Embedder for Unreachable {
    fn model_id(&self) -> &str {
        HASH_MODEL_ID
    }

    fn dim(&self) -> usize {
        ai::HashEmbedder.dim()
    }

    fn embed_batch(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("connection timed out")
    }
}

#[test]
fn an_unreachable_backend_costs_a_few_calls_not_one_per_memory() {
    let notes = (0..6).map(|i| (format!("Note {i}"), format!("Body {i}."))).collect::<Vec<_>>();
    let notes = notes.iter().map(|(t, b)| (t.as_str(), b.as_str())).collect::<Vec<_>>();
    let (_root, db, _ids) = db_with_memories(&notes);
    db.storage.connection().execute("DELETE FROM embedding_cache", []).unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let embedded = ai::refresh_memory_vectors_with(
        db.storage.connection(),
        HASH_MODEL_ID,
        hash_dim(),
        None,
        move || Ok(Box::new(Unreachable(counter))),
    )
    .unwrap();
    assert_eq!(embedded, 0);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "one batch call, then two single calls in a row fail and end the pass"
    );
}

#[test]
fn a_written_memory_is_embedded_by_the_write_itself() {
    let (_root, db, ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    let created = create_memory(&db, "Retry budget", "Retries stop after three attempts.");
    assert!(cached_last_used(&db, "Retry budget").is_some(), "memory_create embedded it");
    db.memory_update(rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: ids[0].clone(),
        kind: None,
        title: None,
        body: Some("Take the meta lock first.".to_string()),
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    })
    .unwrap();
    assert!(cached_last_used(&db, "meta lock first").is_some(), "memory_update embedded it");
    assert!(!created.is_empty());
}

#[test]
fn search_on_a_read_only_connection_uses_the_vector_arm() {
    let (_root, config, db, ids) = db_and_config_with_memories(&[
        ("Retry budget", "Retries stop after three attempts."),
        ("Lock order", "Take the index lock before the meta lock."),
    ]);
    poison_memory_vector(&db, "Lock order", "zebra quokka");
    let read_only = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("index");
    assert!(read_only.storage.connection().is_readonly(rusqlite::MAIN_DB).unwrap());
    assert_eq!(search_ids(&read_only, "zebra quokka"), [ids[1].clone()]);
}

#[test]
fn a_reconciled_memory_vector_survives_the_cache_gc() {
    let (_root, db, _ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    let conn = db.storage.connection();
    conn.execute("UPDATE embedding_cache SET last_used_at_ms = 0", []).unwrap();
    ai::refresh_memory_vectors(conn, None).unwrap();
    assert!(cached_last_used(&db, "Lock order").unwrap() > 0, "the refresh bumped it");
    ai::prune_embedding_cache_unreferenced(conn).unwrap();
    assert!(cached_last_used(&db, "Lock order").is_some(), "no chunk references it, yet it lives");
}

#[test]
fn a_vector_from_another_model_is_never_scored() {
    let (_root, db, _ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    let mut query = ai::hash_query_embedding("lock order").unwrap();
    query.model_id = "some-other-model".to_string();
    let hits = ai::memory_vector_similarities(db.storage.connection(), &query).unwrap();
    assert!(hits.is_empty(), "same dim, different model: {hits:?}");
}

#[test]
fn a_lone_memory_is_tried_once_against_an_unreachable_backend() {
    let (_root, db, _ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    db.storage.connection().execute("DELETE FROM embedding_cache", []).unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    ai::refresh_memory_vectors_with(
        db.storage.connection(),
        HASH_MODEL_ID,
        hash_dim(),
        None,
        move || Ok(Box::new(Unreachable(counter))),
    )
    .unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no one-by-one retry of one input"
    );
}

#[test]
fn a_spent_budget_loads_no_model() {
    let (_root, db, _ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    db.storage.connection().execute("DELETE FROM embedding_cache", []).unwrap();
    let embedded = ai::refresh_memory_vectors_with(
        db.storage.connection(),
        HASH_MODEL_ID,
        hash_dim(),
        Some(std::time::Instant::now()),
        || panic!("the budget is spent, so no model may be loaded"),
    )
    .unwrap();
    assert_eq!(embedded, 0);
}

#[test]
fn a_recently_bumped_vector_is_not_rewritten() {
    let (_root, db, _ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    let conn = db.storage.connection();
    let recent = rag_rat_base::time::now_ms() - 60 * 60 * 1000;
    conn.execute("UPDATE embedding_cache SET last_used_at_ms = ?1", [recent]).unwrap();
    ai::refresh_memory_vectors(conn, None).unwrap();
    assert_eq!(cached_last_used(&db, "Lock order"), Some(recent), "bumped at most once a day");
}

#[test]
fn a_failed_vector_read_falls_back_to_bm25() {
    let (_root, db, ids) = db_with_memories(&[("Lock order", "Take the index lock first.")]);
    db.storage.connection().execute_batch("DROP TABLE embedding_cache").unwrap();
    assert_eq!(search_ids(&db, "lock"), [ids[0].clone()]);
}
