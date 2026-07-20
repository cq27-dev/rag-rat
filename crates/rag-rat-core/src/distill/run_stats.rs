//! Durable counters for one distill ladder run.

use rusqlite::{Connection, params};

use super::LadderStats;

/// Persist one completed run. `rung_guided` is cumulative (every thread starts guided); the other
/// rung counters plus `failed` are mutually exclusive terminal outcomes.
pub(crate) fn record_distill_run(
    conn: &Connection,
    repo_id: &str,
    run_at_ms: i64,
    threads: u64,
    stats: LadderStats,
) -> anyhow::Result<()> {
    if stats.rung_guided != threads {
        anyhow::bail!(
            "distill run invariant: guided attempts {} != threads {threads}",
            stats.rung_guided
        );
    }
    if stats.terminal_count() != threads {
        anyhow::bail!(
            "distill run invariant: terminal outcomes {} != threads {threads}",
            stats.terminal_count()
        );
    }
    let threads = i64::try_from(threads)?;
    let rung_guided = i64::try_from(stats.rung_guided)?;
    let rung_serde = i64::try_from(stats.rung_serde)?;
    let rung_unguided = i64::try_from(stats.rung_unguided)?;
    let rung_tolerant = i64::try_from(stats.rung_tolerant)?;
    let failed = i64::try_from(stats.failed)?;
    let stats_json = serde_json::to_string(&stats)?;
    conn.execute(
        "INSERT INTO papertrail_distill_runs
             (run_at_ms, threads, rung_guided, rung_serde, rung_unguided, rung_tolerant,
              failed, stats_json, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            run_at_ms,
            threads,
            rung_guided,
            rung_serde,
            rung_unguided,
            rung_tolerant,
            failed,
            stats_json,
            repo_id,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::record_distill_run;
    use crate::distill::LadderStats;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE papertrail_distill_runs(
                 id INTEGER PRIMARY KEY,
                 run_at_ms INTEGER NOT NULL,
                 threads INTEGER NOT NULL,
                 rung_guided INTEGER NOT NULL,
                 rung_serde INTEGER NOT NULL,
                 rung_unguided INTEGER NOT NULL,
                 rung_tolerant INTEGER NOT NULL,
                 failed INTEGER NOT NULL,
                 stats_json TEXT,
                 repo_id TEXT NOT NULL
             ) STRICT;",
        )
        .unwrap();
        conn
    }

    #[test]
    fn records_cumulative_guided_and_exclusive_terminal_counters() {
        let conn = fixture();
        let stats = LadderStats {
            rung_guided: 4,
            rung_serde: 1,
            rung_unguided: 1,
            rung_tolerant: 1,
            failed: 1,
        };
        record_distill_run(&conn, "repo", 42, 4, stats).unwrap();
        let row: (i64, i64, i64, i64, i64, i64, String) = conn
            .query_row(
                "SELECT threads, rung_guided, rung_serde, rung_unguided, rung_tolerant, failed,
                        stats_json
                 FROM papertrail_distill_runs WHERE repo_id = 'repo'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!((row.0, row.1, row.2, row.3, row.4, row.5), (4, 4, 1, 1, 1, 1));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&row.6).unwrap()["failed"], 1);
    }

    #[test]
    fn rejects_incomplete_or_double_counted_runs() {
        let conn = fixture();
        let missing_guided = LadderStats { rung_guided: 1, rung_serde: 1, ..Default::default() };
        assert!(record_distill_run(&conn, "repo", 42, 2, missing_guided).is_err());
        let double_terminal =
            LadderStats { rung_guided: 1, rung_serde: 1, failed: 1, ..Default::default() };
        assert!(record_distill_run(&conn, "repo", 42, 1, double_terminal).is_err());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM papertrail_distill_runs", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
    }
}
