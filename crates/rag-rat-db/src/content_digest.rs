//! The incrementally-maintained `content_revision` digest (#828).
//!
//! `content_revision` must be a pure function of the current multiset
//! `{(path, sha256) : row ∈ main.files, kind != 'deleted'}` — identical content ⇒ identical
//! digest, across rebuilds, generation flips, gc, row-order/rowid changes, and
//! delete-then-reinsert-same-content cycles — so every freshness consumer (FTS, the clone graph,
//! `status`) can compare it for equality. This module maintains that digest as a 256-bit additive
//! multiset hash (AdHash-style): each contributing row hashes to four little-endian `u64` lanes,
//! folded into a one-row `content_digest_state` table by component-wise wrapping addition. Removal
//! subtracts the same lanes, so the group `(Z/2^64)^4` preserves multiplicity (duplicates during a
//! generation-staged rebuild do NOT cancel, unlike XOR) and is independent of operation order.
//!
//! The fold is driven by three `AFTER INSERT/UPDATE/DELETE` triggers on `files` (see
//! [`ensure_content_digest`]) that call the registered scalar function `rr_content_digest_fold`
//! (see [`register_content_digest_fold`]). Attaching maintenance to the table — not to call sites
//! — is what makes it complete: every writer, including the dynamic-SQL repo purge and any future
//! seam, maintains the digest automatically. A connection that has NOT registered the function
//! fails a `files` write loudly (`no such function`) and rolls back, so version skew can never
//! silently drift the digest.
//!
//! The per-row hash, the lane fold, and the state codec here are shared by BOTH the trigger fold
//! (via the scalar function) and the from-scratch Rust fold the migration seed, the parity
//! self-check, and the `content_revision()` read fallback use — one implementation, so the two can
//! never disagree.

use std::fmt;

use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use sha2::{Digest, Sha256};

/// Domain tag versioning the per-row hash function itself. Bumping it is a digest-algorithm change
/// (a new migration that reseeds and re-stamps), which is exactly what the rendered `ms1-` prefix
/// announces in stored stamps.
const ROW_HASH_DOMAIN: &[u8] = b"rag-rat/content-revision/1";

/// Rendered-digest prefix (algorithm generation). Self-describing in stored freshness stamps and
/// guaranteed disjoint from the legacy 64-hex SHA digest, so a stale legacy stamp never
/// accidentally equals a new one.
pub const CONTENT_REVISION_PREFIX: &str = "ms1-";

/// The `files.kind` value excluded from the multiset (a tombstone). The trigger fold no-ops each
/// side whose kind is this, matching the current digest's `kind != 'deleted'` inclusion predicate.
const TOMBSTONE_KIND: &str = "deleted";

/// Four little-endian `u64` lanes — the 256-bit additive multiset-hash state. All-zero is the
/// empty multiset.
pub type DigestState = [u64; 4];

/// A `content_digest_state.state` value that is not 64 lowercase hex chars. The scalar fold raises
/// this (aborting the statement + transaction) rather than fold garbage into garbage — fail-closed.
#[derive(Debug)]
pub struct MalformedDigestState(String);

impl fmt::Display for MalformedDigestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed content_digest_state.state: {}", self.0)
    }
}

impl std::error::Error for MalformedDigestState {}

/// The §6.1 per-row contribution: an injective, length-framed SHA-256 over `(path, sha256)` split
/// into four little-endian `u64` lanes. Length framing makes the encoding injective for any byte
/// content — a `:`- or NUL-bearing path cannot alias another row. `kind` is deliberately NOT hashed
/// (it is the inclusion predicate only, matching the legacy digest exactly).
pub fn content_row_hash(path: &str, sha256: &str) -> DigestState {
    let mut hasher = Sha256::new();
    hasher.update(ROW_HASH_DOMAIN);
    hasher.update([0u8]);
    hasher.update((path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    hasher.update((sha256.len() as u64).to_le_bytes());
    hasher.update(sha256.as_bytes());
    let digest = hasher.finalize();
    let mut lanes = [0u64; 4];
    for (i, lane) in lanes.iter_mut().enumerate() {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[i * 8..i * 8 + 8]);
        *lane = u64::from_le_bytes(bytes);
    }
    lanes
}

/// Fold a contributing row's hash into `state`: wrapping add (`add = true`, an insert) or subtract
/// (`add = false`, a removal), lane-wise. Every element is invertible, so the state is a pure
/// function of the current multiset.
pub fn fold_row(state: &mut DigestState, hash: &DigestState, add: bool) {
    for (lane, contribution) in state.iter_mut().zip(hash.iter()) {
        *lane =
            if add { lane.wrapping_add(*contribution) } else { lane.wrapping_sub(*contribution) };
    }
}

/// Encode the state as 64 lowercase hex chars (4 LE `u64` lanes = 32 bytes) — the opaque TEXT the
/// `content_digest_state.state` column stores. SQL never does arithmetic on this, sidestepping
/// SQLite's i64-overflow-to-REAL promotion; all wrapping happens in Rust.
pub fn encode_state(state: &DigestState) -> String {
    let mut out = String::with_capacity(64);
    for lane in state {
        for byte in lane.to_le_bytes() {
            use fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
    }
    out
}

/// Decode a 64-hex `state` back to four LE `u64` lanes. Errs (fail-closed) on any length other than
/// 64 or any non-hex byte — the malformed-state case the scalar fold raises.
pub fn decode_state(hex: &str) -> Result<DigestState, MalformedDigestState> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(MalformedDigestState(format!("expected 64 hex chars, got {}", bytes.len())));
    }
    let mut raw = [0u8; 32];
    for (out, chunk) in raw.iter_mut().zip(bytes.chunks_exact(2)) {
        let hi = rag_rat_base::hash::hex_nibble(chunk[0])
            .ok_or_else(|| MalformedDigestState(format!("non-hex byte {:#04x}", chunk[0])))?;
        let lo = rag_rat_base::hash::hex_nibble(chunk[1])
            .ok_or_else(|| MalformedDigestState(format!("non-hex byte {:#04x}", chunk[1])))?;
        *out = (hi << 4) | lo;
    }
    let mut lanes = [0u64; 4];
    for (i, lane) in lanes.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&raw[i * 8..i * 8 + 8]);
        *lane = u64::from_le_bytes(b);
    }
    Ok(lanes)
}

/// The rendered digest a `content_revision()` read returns: `"ms1-" || encode_state(state)`. The
/// raw state (not a hash of it) is rendered so a parity check can compare states directly.
pub fn render_revision(state: &DigestState) -> String {
    format!("{CONTENT_REVISION_PREFIX}{}", encode_state(state))
}

/// Register the deterministic scalar `rr_content_digest_fold(state, path, sha256, kind, sign)` on
/// `conn` (§7.2). Idempotent (a re-register replaces the identical definition), cheap, and required
/// on EVERY connection that can write `files` — the three triggers call it. The flags:
/// `DETERMINISTIC` (required for use inside triggers under SQLite strictness) and `INNOCUOUS` (the
/// function is pure, so it stays callable from the schema even under `trusted_schema = OFF`; NOT
/// `DIRECTONLY`, which would forbid trigger use).
pub fn register_content_digest_fold(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "rr_content_digest_fold",
        5,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let state_hex: String = ctx.get(0)?;
            // 'deleted' rows are not multiset members: return the state unchanged so the inclusion
            // predicate lives in ONE place and the triggers stay simple (the UPDATE trigger folds
            // both OLD and NEW unconditionally, and this no-ops each tombstone side).
            let kind: String = ctx.get(3)?;
            if kind == TOMBSTONE_KIND {
                return Ok(state_hex);
            }
            // Decode AFTER the tombstone check would still be correct, but decoding first keeps the
            // fail-closed guarantee uniform: a malformed state aborts the statement whatever the
            // kind.
            let mut state = decode_state(&state_hex)
                .map_err(|err| rusqlite::Error::UserFunctionError(Box::new(err)))?;
            let path: String = ctx.get(1)?;
            let sha256: String = ctx.get(2)?;
            let sign: i64 = ctx.get(4)?;
            let hash = content_row_hash(&path, &sha256);
            fold_row(&mut state, &hash, sign >= 0);
            Ok(encode_state(&state))
        },
    )
}

/// Create the `content_digest_state` table and the three `files` fold triggers (§7.1 + §7.3),
/// idempotently. Shared by the seeding migration and any FUTURE `files`-rebuild migration, which
/// MUST call this (a `DROP TABLE files` silently drops the triggers) and then reseed. Does NOT
/// insert the state row — the caller seeds it with a from-scratch fold, so no write can slip
/// between trigger creation and the seed inside the migration transaction.
pub fn ensure_content_digest(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CONTENT_DIGEST_SCHEMA_SQL)
}

/// The state table + fold triggers. Kept as one const so trigger bodies and the invariant comment
/// travel together; every statement is `IF NOT EXISTS`, so re-running is a no-op.
const CONTENT_DIGEST_SCHEMA_SQL: &str = "
-- Invariant: state == fold over {(path, sha256) : main.files, kind != 'deleted'} at every
-- transaction boundary, maintained exclusively by the files_content_digest_* triggers below.
-- Exactly one row (id = 1), seeded by the migration; rows_folded is the multiset cardinality
-- (diagnostic only — never part of the rendered digest).
CREATE TABLE IF NOT EXISTS content_digest_state(
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    state       TEXT    NOT NULL,   -- 64 lowercase hex chars: 4 LE u64 lanes
    rows_folded INTEGER NOT NULL
) STRICT;

-- Inserted rows join the multiset unless they are tombstones.
CREATE TRIGGER IF NOT EXISTS files_content_digest_ai
AFTER INSERT ON files WHEN NEW.kind != 'deleted'
BEGIN
    UPDATE content_digest_state
       SET state = rr_content_digest_fold(state, NEW.path, NEW.sha256, NEW.kind, 1),
           rows_folded = rows_folded + 1
     WHERE id = 1;
END;

-- Deleted rows leave the multiset unless they were tombstones.
CREATE TRIGGER IF NOT EXISTS files_content_digest_ad
AFTER DELETE ON files WHEN OLD.kind != 'deleted'
BEGIN
    UPDATE content_digest_state
       SET state = rr_content_digest_fold(state, OLD.path, OLD.sha256, OLD.kind, -1),
           rows_folded = rows_folded - 1
     WHERE id = 1;
END;

-- Digest-relevant column updates swap the old contribution for the new one; the fold function
-- no-ops each side whose kind is 'deleted', which handles tombstone flips in both directions.
-- UPDATE OF path, sha256, kind: digest-neutral writers (commit_sha, worktree_id, generation,
-- generated, indexed_* re-stamps) never enter this trigger at all.
CREATE TRIGGER IF NOT EXISTS files_content_digest_au
AFTER UPDATE OF path, sha256, kind ON files
BEGIN
    UPDATE content_digest_state
       SET state = rr_content_digest_fold(
                       rr_content_digest_fold(state, OLD.path, OLD.sha256, OLD.kind, -1),
                       NEW.path, NEW.sha256, NEW.kind, 1),
           rows_folded = rows_folded
                         + (NEW.kind != 'deleted') - (OLD.kind != 'deleted')
     WHERE id = 1;
END;
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_multiset_is_all_zero() {
        let state = [0u64; 4];
        assert_eq!(encode_state(&state), "0".repeat(64));
        assert_eq!(render_revision(&state), format!("{CONTENT_REVISION_PREFIX}{}", "0".repeat(64)));
    }

    #[test]
    fn encode_decode_round_trips() {
        let mut state = [0u64; 4];
        fold_row(&mut state, &content_row_hash("src/a.rs", "aa"), true);
        fold_row(&mut state, &content_row_hash("src/b.rs", "bb"), true);
        let hex = encode_state(&state);
        assert_eq!(hex.len(), 64);
        assert_eq!(decode_state(&hex).unwrap(), state);
    }

    #[test]
    fn fold_is_order_independent_and_invertible() {
        let a = content_row_hash("src/a.rs", "aa");
        let b = content_row_hash("src/b.rs", "bb");
        let mut ab = [0u64; 4];
        fold_row(&mut ab, &a, true);
        fold_row(&mut ab, &b, true);
        let mut ba = [0u64; 4];
        fold_row(&mut ba, &b, true);
        fold_row(&mut ba, &a, true);
        assert_eq!(ab, ba, "insertion order must not matter");
        // Removing b returns to the a-only state.
        let mut only_a = [0u64; 4];
        fold_row(&mut only_a, &a, true);
        fold_row(&mut ab, &b, false);
        assert_eq!(ab, only_a, "removal is exact");
    }

    #[test]
    fn multiset_keeps_multiplicity() {
        // Two identical (path, sha256) contributions do NOT cancel (the XOR-regression pin).
        let h = content_row_hash("src/a.rs", "aa");
        let mut once = [0u64; 4];
        fold_row(&mut once, &h, true);
        let mut twice = [0u64; 4];
        fold_row(&mut twice, &h, true);
        fold_row(&mut twice, &h, true);
        assert_ne!(once, twice, "a duplicate row must change the digest");
    }

    #[test]
    fn length_framing_is_injective() {
        // Without length framing, ("a:b", "c") and ("a", "b:c") could alias via a ':' join.
        assert_ne!(content_row_hash("a:b", "c"), content_row_hash("a", "b:c"));
        assert_ne!(content_row_hash("a", "bc"), content_row_hash("ab", "c"));
    }

    #[test]
    fn decode_rejects_malformed_state() {
        assert!(decode_state("").is_err());
        assert!(decode_state("zz").is_err());
        assert!(decode_state(&"0".repeat(63)).is_err());
        assert!(decode_state(&"g".repeat(64)).is_err());
    }

    /// Drive the triggers against a MINIMAL `files` table (only the three columns the triggers
    /// touch) so the trigger SQL + scalar fold are exercised in isolation from the full schema.
    /// The full-schema seam sweep lives in rag-rat-core.
    mod triggers {
        use rusqlite::Connection;

        use super::*;

        fn open() -> Connection {
            let conn = Connection::open_in_memory().unwrap();
            register_content_digest_fold(&conn).unwrap();
            conn.execute_batch("CREATE TABLE files(path TEXT, sha256 TEXT, kind TEXT);").unwrap();
            ensure_content_digest(&conn).unwrap();
            conn.execute(
                "INSERT INTO content_digest_state(id, state, rows_folded) VALUES (1, ?1, 0)",
                [encode_state(&[0u64; 4])],
            )
            .unwrap();
            conn
        }

        /// The independent from-scratch fold the triggers must always agree with.
        fn scan(conn: &Connection) -> (String, i64) {
            let mut state = [0u64; 4];
            let mut count = 0i64;
            let mut stmt =
                conn.prepare("SELECT path, sha256 FROM files WHERE kind != 'deleted'").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let path: String = row.get(0).unwrap();
                let sha256: String = row.get(1).unwrap();
                fold_row(&mut state, &content_row_hash(&path, &sha256), true);
                count += 1;
            }
            (encode_state(&state), count)
        }

        fn stored(conn: &Connection) -> (String, i64) {
            conn.query_row(
                "SELECT state, rows_folded FROM content_digest_state WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        }

        fn assert_parity(conn: &Connection) {
            assert_eq!(stored(conn), scan(conn), "trigger-maintained state must equal a scan fold");
        }

        #[test]
        fn insert_update_delete_and_tombstone_flips_stay_in_parity() {
            let conn = open();
            // Insert two real files.
            conn.execute("INSERT INTO files VALUES ('a.rs', 'aa', 'source')", []).unwrap();
            conn.execute("INSERT INTO files VALUES ('b.rs', 'bb', 'docs')", []).unwrap();
            assert_parity(&conn);
            assert_eq!(stored(&conn).1, 2);

            // A tombstone insert is excluded (WHEN NEW.kind != 'deleted').
            conn.execute("INSERT INTO files VALUES ('c.rs', '', 'deleted')", []).unwrap();
            assert_parity(&conn);
            assert_eq!(stored(&conn).1, 2, "a tombstone does not join the multiset");

            // sha change (delete+reinsert equivalent): swap old for new.
            conn.execute("UPDATE files SET sha256 = 'aa2' WHERE path = 'a.rs'", []).unwrap();
            assert_parity(&conn);

            // Tombstone flip via UPDATE OF kind: 'source' -> 'deleted' removes the contribution.
            conn.execute("UPDATE files SET kind = 'deleted', sha256 = '' WHERE path = 'b.rs'", [])
                .unwrap();
            assert_parity(&conn);
            assert_eq!(stored(&conn).1, 1);

            // Flip back: 'deleted' -> 'source' re-adds it.
            conn.execute("UPDATE files SET kind = 'source', sha256 = 'bb3' WHERE path = 'b.rs'", [
            ])
            .unwrap();
            assert_parity(&conn);
            assert_eq!(stored(&conn).1, 2);

            // Delete a real row.
            conn.execute("DELETE FROM files WHERE path = 'a.rs'", []).unwrap();
            assert_parity(&conn);
            assert_eq!(stored(&conn).1, 1);

            // Deleting a tombstone is digest-neutral (WHEN OLD.kind != 'deleted' skips it).
            conn.execute("DELETE FROM files WHERE path = 'c.rs'", []).unwrap();
            assert_parity(&conn);
            assert_eq!(stored(&conn).1, 1);
        }

        #[test]
        fn add_then_remove_same_pair_returns_to_prior_state() {
            let conn = open();
            conn.execute("INSERT INTO files VALUES ('x.rs', 'xx', 'source')", []).unwrap();
            let before = stored(&conn);
            conn.execute("INSERT INTO files VALUES ('y.rs', 'yy', 'source')", []).unwrap();
            conn.execute("DELETE FROM files WHERE path = 'y.rs'", []).unwrap();
            assert_eq!(
                stored(&conn),
                before,
                "add+remove of the same pair is a no-op on the digest"
            );
        }

        #[test]
        fn a_write_without_the_registered_function_fails_closed() {
            // A connection that never registered the fold: the trigger's function reference is
            // unresolved, so the `files` INSERT fails loudly and the state never drifts.
            let conn = Connection::open_in_memory().unwrap();
            register_content_digest_fold(&conn).unwrap();
            conn.execute_batch("CREATE TABLE files(path TEXT, sha256 TEXT, kind TEXT);").unwrap();
            ensure_content_digest(&conn).unwrap();
            conn.execute(
                "INSERT INTO content_digest_state(id, state, rows_folded) VALUES (1, ?1, 0)",
                [encode_state(&[0u64; 4])],
            )
            .unwrap();
            // Now open a SECOND connection to the same in-memory DB is not possible; instead drop
            // the function by removing it. rusqlite has no de-register, so simulate skew by writing
            // through a fresh connection that shares nothing — use a file-backed DB.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("skew.sqlite");
            {
                let setup = Connection::open(&path).unwrap();
                register_content_digest_fold(&setup).unwrap();
                setup
                    .execute_batch("CREATE TABLE files(path TEXT, sha256 TEXT, kind TEXT);")
                    .unwrap();
                ensure_content_digest(&setup).unwrap();
                setup
                    .execute(
                        "INSERT INTO content_digest_state(id, state, rows_folded) VALUES (1, ?1, \
                         0)",
                        [encode_state(&[0u64; 4])],
                    )
                    .unwrap();
            }
            let unregistered = Connection::open(&path).unwrap();
            let err = unregistered
                .execute("INSERT INTO files VALUES ('a.rs', 'aa', 'source')", [])
                .unwrap_err();
            assert!(
                err.to_string().contains("no such function"),
                "an unregistered writer must fail closed, got: {err}"
            );
            let count: i64 = unregistered
                .query_row("SELECT rows_folded FROM content_digest_state WHERE id = 1", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "the rolled-back INSERT left the digest untouched");
        }
    }
}
