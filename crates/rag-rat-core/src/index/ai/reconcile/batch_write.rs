use std::collections::{BTreeMap, HashMap};

use super::super::*;
use crate::config::RemoteEmbeddingConfig;

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
