use super::*;

#[cfg(test)]
mod corruption_tests {
    use super::*;

    fn corrupt_error() -> anyhow::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt,
                extended_code: 267, // SQLITE_CORRUPT_VTAB — the FTS5 shadow variant
            },
            Some("database disk image is malformed".to_string()),
        )
        .into()
    }

    #[test]
    fn corruption_retries_once_after_heal() {
        let mut calls = 0;
        let mut healed = false;
        let result = retry_once_on_fts_corruption(
            || {
                calls += 1;
                if calls == 1 { Err(corrupt_error()) } else { Ok(42) }
            },
            || {
                healed = true;
                Ok(Vec::new())
            },
        );
        assert_eq!(result.unwrap(), 42);
        assert!(healed, "the heal ran between the attempts");
    }

    #[test]
    fn non_corruption_errors_do_not_heal() {
        let result: anyhow::Result<i32> = retry_once_on_fts_corruption(
            || anyhow::bail!("some other failure"),
            || panic!("a non-corruption error must not trigger the heal"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn corruption_that_survives_the_heal_surfaces_without_looping() {
        let mut calls = 0;
        let result: anyhow::Result<i32> = retry_once_on_fts_corruption(
            || {
                calls += 1;
                Err(corrupt_error())
            },
            || Ok(Vec::new()),
        );
        assert!(error_is_fts_corruption(&result.unwrap_err()));
        assert_eq!(calls, 2, "exactly one retry, no loop");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn the_ranked_probe_sees_docsize_corruption_on_a_non_ascii_corpus() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE probe_t USING fts5(body, content='', contentless_delete=1);
             INSERT INTO probe_t(rowid, body) VALUES (1, '\u{7d22}\u{5f15} \u{640d}\u{58ca} \
             \u{691c}\u{67fb}');",
        )
        .unwrap();
        assert!(
            !ranked_probe_is_corrupt(&conn, "probe_t", None).unwrap(),
            "intact mirror probes clean"
        );
        conn.execute_batch("DELETE FROM probe_t_docsize").unwrap();
        assert!(
            ranked_probe_is_corrupt(&conn, "probe_t", None).unwrap(),
            "a real vocab term ranks the corpus regardless of alphabet — an ASCII-prefix probe \
             reports this corrupt mirror healthy"
        );
    }

    /// docsize is PER ROW: corrupting a row the sampled term never matches must still be
    /// caught — this is the class the LIMIT-1 rank read alone cannot see.
    #[test]
    fn per_row_corruption_outside_the_sampled_row_is_still_detected() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE probe_t USING fts5(body, content='', contentless_delete=1);
             INSERT INTO probe_t(rowid, body) VALUES (1, 'aardvark aardvark aardvark');
             INSERT INTO probe_t(rowid, body) VALUES (2, 'zebra');",
        )
        .unwrap();
        assert!(!ranked_probe_is_corrupt(&conn, "probe_t", None).unwrap());
        // Malform row 2's blob only: the vocab sample ('aardvark' sorts first) ranks row 1 and
        // passes; the shadow scan must flag the truncated varint on row 2.
        conn.execute("UPDATE probe_t_docsize SET sz = x'FF' WHERE id = 2", []).unwrap();
        assert!(
            ranked_probe_is_corrupt(&conn, "probe_t", None).unwrap(),
            "a malformed blob on an unranked row is a ranked query waiting to fail"
        );
    }

    /// Count parity needs a docsize-INDEPENDENT row source: a contentless mirror's own count(*)
    /// enumerates through docsize (comparing the shadow to itself), so the caller supplies the
    /// durable source; content-carrying mirrors use their own count.
    #[test]
    fn a_missing_single_docsize_row_is_detected_by_count_parity() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE src(id INTEGER PRIMARY KEY, body TEXT);
             INSERT INTO src VALUES (1, 'alpha'), (2, 'beta');
             CREATE VIRTUAL TABLE probe_t USING fts5(body, content='', contentless_delete=1);
             INSERT INTO probe_t(rowid, body) SELECT id, body FROM src;",
        )
        .unwrap();
        let rows = Some("SELECT count(*) FROM src");
        assert!(!ranked_probe_is_corrupt(&conn, "probe_t", rows).unwrap());
        conn.execute("DELETE FROM probe_t_docsize WHERE id = 2", []).unwrap();
        assert!(
            ranked_probe_is_corrupt(&conn, "probe_t", rows).unwrap(),
            "a vanished docsize row (invisible to the mirror's own scans) fails parity against \
             the durable source"
        );
    }

    #[test]
    fn varint_shape_validation_is_exact() {
        assert!(blob_is_exactly_n_varints(&[0x05], 1), "one small varint");
        assert!(blob_is_exactly_n_varints(&[0x81, 0x05], 1), "two-byte varint");
        assert!(blob_is_exactly_n_varints(&[0x05, 0x07], 2), "two columns");
        assert!(!blob_is_exactly_n_varints(&[0xFF], 1), "truncated continuation");
        assert!(!blob_is_exactly_n_varints(&[0x05, 0x07], 1), "trailing bytes");
        assert!(!blob_is_exactly_n_varints(&[], 1), "missing varint");
        assert!(blob_is_exactly_n_varints(&[], 0), "zero columns, empty blob");
    }

    #[test]
    fn an_empty_mirror_probes_clean() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE probe_t USING fts5(body, content='')").unwrap();
        assert!(!ranked_probe_is_corrupt(&conn, "probe_t", None).unwrap());
    }
}

#[cfg(test)]
mod fence_tests {
    use super::*;

    fn conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x INTEGER)").unwrap();
        conn
    }

    #[test]
    fn standalone_callers_get_all_or_nothing() {
        let conn = conn();
        let error = fenced_when_autocommit(&conn, || {
            conn.execute("INSERT INTO t(x) VALUES (1)", [])?;
            anyhow::bail!("interrupted mid-sequence")
        })
        .unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error:#}");
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 0, "the partial write must roll back — no torn mirror");
        assert!(conn.is_autocommit(), "the failed fence must not leave a transaction open");

        fenced_when_autocommit(&conn, || {
            conn.execute("INSERT INTO t(x) VALUES (2)", [])?;
            Ok(())
        })
        .unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn fenced_callers_run_bare_inside_their_own_transaction() {
        let conn = conn();
        let outer =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
        fenced_when_autocommit(&conn, || {
            conn.execute("INSERT INTO t(x) VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
        // The outer fence is still the single atomic unit: rolling it back drops the write.
        outer.rollback().unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 0, "no nested commit may escape the caller's fence");
    }
}
