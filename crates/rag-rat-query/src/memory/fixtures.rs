//! Shared persisted rows for memory tests; callers name only the fields their scenario varies.
use rusqlite::{Connection, params};

pub(crate) struct MemorySeed<'a> {
    pub id: &'a str,
    pub repo_id: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub created_by: Option<&'a str>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub origin: &'a str,
    pub source_text_hash: Option<&'a str>,
}

impl Default for MemorySeed<'_> {
    fn default() -> Self {
        Self {
            id: "m1",
            repo_id: "r",
            title: "t",
            body: "b",
            created_by: Some("agent"),
            created_at_ms: 1,
            updated_at_ms: 1,
            origin: "local",
            source_text_hash: None,
        }
    }
}

pub(crate) fn seed_memory(conn: &Connection, seed: MemorySeed<'_>) {
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by,
         created_at_ms, updated_at_ms, source, memory_version, repo_id, origin, source_text_hash)
         VALUES (?1, 'Invariant', ?2, ?3, 'high', 'active', ?4, ?5, ?6, 'agent', 'v1', ?7, ?8, ?9)",
        params![
            seed.id,
            seed.title,
            seed.body,
            seed.created_by,
            seed.created_at_ms,
            seed.updated_at_ms,
            seed.repo_id,
            seed.origin,
            seed.source_text_hash
        ],
    )
    .unwrap();
}

pub(crate) fn seed_file(conn: &Connection, path: &str, repo_id: &str) -> i64 {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
         commit_sha, worktree_id, repo_id, generation)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, '', '', ?3, 0)",
        params![path, format!("sha-{path}"), repo_id],
    )
    .unwrap();
    conn.last_insert_rowid()
}
