use std::collections::{BTreeMap, HashMap};

use rag_rat_base::config::RemoteEmbeddingConfig;

use super::super::*;

pub(crate) fn automatic_reconcile_can_skip_noop(
    conn: &Connection,
    options: &ReconcileOptions,
) -> bool {
    if options.provision_remote || options.force || options.max_seconds.is_none() {
        return false;
    }
    match active_remote_config(conn) {
        Ok(Some(remote)) if remote.is_ephemeral() => false,
        Ok(_) => true,
        Err(_) => false,
    }
}

pub(crate) fn empty_current_reconcile_report(
    active_model_id: String,
    model_version: String,
    embedding_dim: usize,
    batch_size: usize,
    max_embedding_chars: usize,
    options: &ReconcileOptions,
) -> ReconcileReport {
    ReconcileReport {
        processed_chunks: 0,
        embeddings_written: 0,
        skipped_chunks: 0,
        failed_chunks: 0,
        blocked_chunks: 0,
        model_id: active_model_id,
        model_version,
        embedding_dim,
        batch_size,
        max_embedding_chars,
        forced: options.force,
        changed_first: options.changed_first,
        until_clean: options.until_clean,
        max_seconds: options.max_seconds,
        work_reasons: BTreeMap::new(),
        skipped_by_policy: BTreeMap::new(),
        input_chars: 0,
        truncated_inputs: 0,
        elapsed_ms: 0,
        chunks_per_sec: 0.0,
        chars_per_sec: 0.0,
        avg_chars_per_chunk: 0.0,
        status: "Current".to_string(),
        message: None,
    }
}

pub(crate) fn remote_reconcile_batch_size(
    remote: &RemoteEmbeddingConfig,
    batch_size: usize,
    max_seconds: Option<u64>,
) -> usize {
    if max_seconds.is_some() {
        return batch_size.max(1);
    }
    let remote_batch_size = (remote.batch_size as usize).max(1);
    let concurrency = remote.bounded_concurrency() as usize;
    batch_size.max(remote_batch_size.saturating_mul(concurrency))
}

pub(crate) struct EmbeddingJobGroup {
    primary: PreparedEmbeddingJob,
    duplicates: Vec<PreparedEmbeddingJob>,
}

impl EmbeddingJobGroup {
    fn input_text(&self) -> &str {
        &self.primary.input_text
    }

    fn jobs(&self) -> impl Iterator<Item = &PreparedEmbeddingJob> {
        std::iter::once(&self.primary).chain(self.duplicates.iter())
    }
}

pub(crate) fn group_embedding_jobs_by_input_hash(
    jobs: Vec<PreparedEmbeddingJob>,
) -> Vec<EmbeddingJobGroup> {
    let mut groups: Vec<EmbeddingJobGroup> = Vec::new();
    let mut group_by_input_hash: HashMap<String, usize> = HashMap::new();
    for job in jobs {
        if let Some(&group_idx) = group_by_input_hash.get(&job.input_hash) {
            groups[group_idx].duplicates.push(job);
        } else {
            let group_idx = groups.len();
            group_by_input_hash.insert(job.input_hash.clone(), group_idx);
            groups.push(EmbeddingJobGroup { primary: job, duplicates: Vec::new() });
        }
    }
    groups
}

pub(crate) fn embed_and_write_jobs(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    jobs: Vec<PreparedEmbeddingJob>,
    remote: Option<&RemoteEmbeddingConfig>,
) -> anyhow::Result<(u64, u64)> {
    let groups = group_embedding_jobs_by_input_hash(jobs);
    let texts = groups.iter().map(|group| group.input_text().to_string()).collect::<Vec<_>>();
    match embedder.embed_batch(&texts) {
        Ok(vectors) if vectors.len() == groups.len() => {
            let written =
                write_current_embedding_groups(conn, embedder, model_version, &groups, &vectors)?;
            Ok((written, 0))
        },
        Ok(vectors) => {
            let error = vector_count_error(embedder, vectors.len(), groups.len());
            write_remote_scoped_or_failed(conn, embedder, model_version, &groups, remote, &error)
        },
        Err(err) => {
            let error = err.to_string();
            write_remote_scoped_or_failed(conn, embedder, model_version, &groups, remote, &error)
        },
    }
}

fn write_current_embedding_groups(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    groups: &[EmbeddingJobGroup],
    vectors: &[Vec<f32>],
) -> anyhow::Result<u64> {
    let mut written = 0u64;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for (group, vector) in groups.iter().zip(vectors) {
            for job in group.jobs() {
                store_embedding(conn, embedder, model_version, job, vector)?;
                written = written.saturating_add(1);
            }
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)?;
    Ok(written)
}

fn write_failed_embedding_groups(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    groups: &[EmbeddingJobGroup],
    error: &str,
) -> anyhow::Result<u64> {
    let mut failed = 0u64;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for group in groups {
            for job in group.jobs() {
                store_failed_embedding(conn, embedder, model_version, job, error)?;
                failed = failed.saturating_add(1);
            }
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)?;
    Ok(failed)
}

pub(crate) fn write_remote_scoped_or_failed(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    groups: &[EmbeddingJobGroup],
    remote: Option<&RemoteEmbeddingConfig>,
    error: &str,
) -> anyhow::Result<(u64, u64)> {
    let Some(remote) = remote else {
        let failed = write_failed_embedding_groups(conn, embedder, model_version, groups, error)?;
        return Ok((0, failed));
    };

    let mut written = 0u64;
    let mut failed = 0u64;
    let mut abort_remaining_error = None::<String>;
    let mut consecutive_endpoint_failures = 0usize;
    for (start, end) in remote_request_group_ranges(groups, remote) {
        let scoped_groups = &groups[start..end];
        if let Some(error) = abort_remaining_error.as_deref() {
            let scoped_failed =
                write_failed_embedding_groups(conn, embedder, model_version, scoped_groups, error)?;
            failed = failed.saturating_add(scoped_failed);
            continue;
        }
        let texts =
            scoped_groups.iter().map(|group| group.input_text().to_string()).collect::<Vec<_>>();
        match embedder.embed_batch(&texts) {
            Ok(vectors) if vectors.len() == scoped_groups.len() => {
                consecutive_endpoint_failures = 0;
                let scoped_written = write_current_embedding_groups(
                    conn,
                    embedder,
                    model_version,
                    scoped_groups,
                    &vectors,
                )?;
                written = written.saturating_add(scoped_written);
            },
            Ok(vectors) => {
                consecutive_endpoint_failures = 0;
                let scoped_error = vector_count_error(embedder, vectors.len(), scoped_groups.len());
                let scoped_failed = write_failed_embedding_groups(
                    conn,
                    embedder,
                    model_version,
                    scoped_groups,
                    &scoped_error,
                )?;
                failed = failed.saturating_add(scoped_failed);
            },
            Err(err) => {
                let scoped_error = err.to_string();
                let scoped_failed = write_failed_embedding_groups(
                    conn,
                    embedder,
                    model_version,
                    scoped_groups,
                    &scoped_error,
                )?;
                failed = failed.saturating_add(scoped_failed);
                match classify_remote_scoped_retry_error(&scoped_error) {
                    RemoteScopedRetryError::AbortImmediately => {
                        abort_remaining_error = Some(scoped_error);
                    },
                    RemoteScopedRetryError::EndpointFailure => {
                        consecutive_endpoint_failures =
                            consecutive_endpoint_failures.saturating_add(1);
                        if consecutive_endpoint_failures
                            >= REMOTE_SCOPED_RETRY_CONSECUTIVE_ENDPOINT_FAILURE_LIMIT
                        {
                            abort_remaining_error = Some(scoped_error);
                        }
                    },
                    RemoteScopedRetryError::Other => {
                        consecutive_endpoint_failures = 0;
                    },
                }
            },
        }
    }
    Ok((written, failed))
}

fn vector_count_error(embedder: &dyn Embedder, got: usize, expected: usize) -> String {
    format!("embedder {} returned {} vectors for {} texts", embedder.model_id(), got, expected)
}

pub(crate) const REMOTE_SCOPED_RETRY_CONSECUTIVE_ENDPOINT_FAILURE_LIMIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteScopedRetryError {
    AbortImmediately,
    EndpointFailure,
    Other,
}

pub(crate) fn classify_remote_scoped_retry_error(error: &str) -> RemoteScopedRetryError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("connection refused")
        || lower.contains("failed to connect")
        || lower.contains("connect error")
    {
        return RemoteScopedRetryError::AbortImmediately;
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("http status 504")
    {
        return RemoteScopedRetryError::EndpointFailure;
    }
    RemoteScopedRetryError::Other
}

fn remote_request_group_ranges(
    groups: &[EmbeddingJobGroup],
    remote: &RemoteEmbeddingConfig,
) -> Vec<(usize, usize)> {
    let batch_size = (remote.batch_size as usize).max(1);
    let max_batch_chars = remote.max_batch_chars.max(1);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut chars = 0usize;
    for (idx, group) in groups.iter().enumerate() {
        let text_chars = group.input_text().chars().count();
        let count_full = idx.saturating_sub(start) >= batch_size;
        let chars_full = idx > start && chars.saturating_add(text_chars) > max_batch_chars;
        if count_full || chars_full {
            ranges.push((start, idx));
            start = idx;
            chars = 0;
        }
        chars = chars.saturating_add(text_chars);
    }
    if start < groups.len() {
        ranges.push((start, groups.len()));
    }
    ranges
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rag_rat_base::config::{RemoteBackend, RemoteEmbeddingConfig};
    use rag_rat_base::embedding_models::FASTEMBED_MODEL_ID;
    use rusqlite::{Connection, params};

    use super::*;
    use crate::index::ai::{self, ReconcileReason};

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        ai::ensure_model_manifest(&conn).unwrap();
        conn
    }

    fn remote_at(endpoint: &str) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: Some(endpoint.to_string()),
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 5,
        }
    }

    fn ephemeral_remote() -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: None,
            cookbook: Some("@rag-rat/cookbook/modal".to_string()),
            query_endpoint: Some("http://127.0.0.1:11434".to_string()),
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 5,
        }
    }

    fn seed_embedding_chunk(conn: &Connection, i: i64) -> i64 {
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES (?1, 'rust', 'source', ?2, 0, 0)",
            params![format!("src/file_{i}.rs"), format!("sha-{i}")],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        let text = format!("pub fn item_{i}() {{}}");
        conn.execute(
            "INSERT INTO chunks(
                 file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line,
                 text_hash, source_revision
             )
             VALUES (?1, 'code', ?2, 0, ?3, 1, 1, ?4, ?5)",
            params![
                file_id,
                format!("crate::item_{i}"),
                i64::try_from(text.len()).unwrap(),
                format!("hash-{i}"),
                format!("rev-{i}")
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn prepared_job(chunk_id: i64, i: i64, input_text: &str) -> PreparedEmbeddingJob {
        PreparedEmbeddingJob {
            id: chunk_id,
            text_hash: format!("hash-{i}"),
            input_hash: format!("input-hash-{i}"),
            input_chars: input_text.chars().count(),
            input_text: input_text.to_string(),
            input_truncated: false,
            policy: "Embed".to_string(),
            priority: 0,
            reason: ReconcileReason::Missing,
        }
    }

    struct OkEmbedder {
        dim: usize,
        calls: AtomicUsize,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl Embedder for OkEmbedder {
        fn model_id(&self) -> &str {
            FASTEMBED_MODEL_ID
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.batch_sizes.lock().unwrap().push(texts.len());
            Ok(vec![vec![0.1; self.dim]; texts.len()])
        }
    }

    struct WrongCountEmbedder {
        dim: usize,
    }

    impl Embedder for WrongCountEmbedder {
        fn model_id(&self) -> &str {
            FASTEMBED_MODEL_ID
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(vec![vec![0.1; self.dim]; texts.len().saturating_sub(1)])
        }
    }

    struct RefusedEmbedder {
        calls: AtomicUsize,
    }

    impl Embedder for RefusedEmbedder {
        fn model_id(&self) -> &str {
            FASTEMBED_MODEL_ID
        }

        fn dim(&self) -> usize {
            8
        }

        fn embed_batch(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("connection refused")
        }
    }

    #[test]
    fn automatic_reconcile_can_skip_when_bounded_and_remote_is_connect() {
        let conn = schema_conn();
        let spec = rag_rat_base::embedding_models::spec(FASTEMBED_MODEL_ID).unwrap();
        ai::set_active_remote_config(&conn, &remote_at("http://127.0.0.1:11434")).unwrap();
        ai::set_repo_meta(&conn, ai::ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
        ai::set_repo_meta(&conn, ai::ACTIVE_EMBEDDING_MODEL_VERSION_META, spec.version).unwrap();

        assert!(automatic_reconcile_can_skip_noop(&conn, &ReconcileOptions {
            max_seconds: Some(1),
            ..ReconcileOptions::default()
        },));
    }

    #[test]
    fn automatic_reconcile_cannot_skip_when_forced_or_unbounded_or_ephemeral() {
        let conn = schema_conn();
        let bounded = ReconcileOptions { max_seconds: Some(1), ..ReconcileOptions::default() };

        assert!(!automatic_reconcile_can_skip_noop(&conn, &ReconcileOptions {
            force: true,
            ..bounded.clone()
        },));
        assert!(!automatic_reconcile_can_skip_noop(&conn, &ReconcileOptions {
            provision_remote: true,
            ..bounded.clone()
        },));
        assert!(!automatic_reconcile_can_skip_noop(&conn, &ReconcileOptions {
            max_seconds: None,
            ..ReconcileOptions::default()
        },));

        ai::set_active_remote_config(&conn, &ephemeral_remote()).unwrap();
        assert!(!automatic_reconcile_can_skip_noop(&conn, &bounded));
    }

    #[test]
    fn empty_current_reconcile_report_reflects_options() {
        let options = ReconcileOptions {
            force: true,
            changed_first: true,
            until_clean: true,
            max_seconds: Some(42),
            ..ReconcileOptions::default()
        };
        let report = empty_current_reconcile_report(
            "model".to_string(),
            "v1".to_string(),
            384,
            16,
            8_000,
            &options,
        );
        assert_eq!(report.status, "Current");
        assert_eq!(report.model_id, "model");
        assert_eq!(report.model_version, "v1");
        assert!(report.forced);
        assert!(report.changed_first);
        assert!(report.until_clean);
        assert_eq!(report.max_seconds, Some(42));
        assert_eq!(report.embeddings_written, 0);
    }

    #[test]
    fn group_embedding_jobs_by_input_hash_collapses_duplicates() {
        let first = prepared_job(1, 1, "alpha");
        let mut dup = prepared_job(2, 2, "beta");
        dup.input_hash = first.input_hash.clone();
        dup.input_text = first.input_text.clone();
        let groups =
            group_embedding_jobs_by_input_hash(vec![first, dup, prepared_job(3, 3, "gamma")]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].duplicates.len(), 1);
        assert!(groups[1].duplicates.is_empty());
    }

    #[test]
    fn embed_and_write_jobs_without_remote_marks_vector_mismatch_failed() {
        let conn = schema_conn();
        let chunk_id = seed_embedding_chunk(&conn, 0);
        let dim = rag_rat_base::embedding_models::spec(FASTEMBED_MODEL_ID).unwrap().dim;
        let embedder = WrongCountEmbedder { dim };

        let (written, failed) = embed_and_write_jobs(
            &conn,
            &embedder,
            "v1",
            vec![prepared_job(chunk_id, 0, "one")],
            None,
        )
        .unwrap();

        assert_eq!((written, failed), (0, 1));
        let status: String = conn
            .query_row(
                "SELECT status FROM chunk_embeddings WHERE chunk_id = ?1",
                params![chunk_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "Failed");
    }

    #[test]
    fn write_remote_scoped_without_remote_marks_all_failed() {
        let conn = schema_conn();
        let jobs = (0..2)
            .map(|i| prepared_job(seed_embedding_chunk(&conn, i), i, &format!("text-{i}")))
            .collect::<Vec<_>>();
        let groups = group_embedding_jobs_by_input_hash(jobs);
        let embedder =
            OkEmbedder { dim: 8, calls: AtomicUsize::new(0), batch_sizes: Mutex::new(Vec::new()) };

        let (written, failed) = write_remote_scoped_or_failed(
            &conn,
            &embedder,
            "v1",
            &groups,
            None,
            "upstream embed failed",
        )
        .unwrap();

        assert_eq!((written, failed), (0, 2));
        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            0,
            "no remote scope means no retry embed"
        );
    }

    #[test]
    fn remote_scoped_splits_ranges_by_max_batch_chars() {
        let conn = schema_conn();
        let dim = rag_rat_base::embedding_models::spec(FASTEMBED_MODEL_ID).unwrap().dim;
        let jobs = (0..3)
            .map(|i| {
                let text = format!("{}{}", "a".repeat(4), i);
                prepared_job(seed_embedding_chunk(&conn, i), i, &text)
            })
            .collect::<Vec<_>>();
        let groups = group_embedding_jobs_by_input_hash(jobs);
        let mut remote = remote_at("http://127.0.0.1:11434");
        remote.batch_size = 8;
        remote.max_batch_chars = 5;
        let embedder =
            OkEmbedder { dim, calls: AtomicUsize::new(0), batch_sizes: Mutex::new(Vec::new()) };

        let (written, failed) = write_remote_scoped_or_failed(
            &conn,
            &embedder,
            "v1",
            &groups,
            Some(&remote),
            "seed error",
        )
        .unwrap();

        assert_eq!((written, failed), (3, 0));
        assert_eq!(
            *embedder.batch_sizes.lock().unwrap(),
            vec![1, 1, 1],
            "char budget should split one text per request range"
        );
    }

    #[test]
    fn remote_scoped_abort_immediately_skips_later_embed_calls() {
        let conn = schema_conn();
        let jobs = (0..3)
            .map(|i| prepared_job(seed_embedding_chunk(&conn, i), i, &format!("text-{i}")))
            .collect::<Vec<_>>();
        let groups = group_embedding_jobs_by_input_hash(jobs);
        let mut remote = remote_at("http://127.0.0.1:11434");
        remote.batch_size = 1;
        remote.max_batch_chars = usize::MAX;
        let embedder = RefusedEmbedder { calls: AtomicUsize::new(0) };

        let (written, failed) = write_remote_scoped_or_failed(
            &conn,
            &embedder,
            "v1",
            &groups,
            Some(&remote),
            "seed error",
        )
        .unwrap();

        assert_eq!((written, failed), (0, 3));
        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            1,
            "connection refused should abort remaining ranges without further embed calls"
        );
    }
}
