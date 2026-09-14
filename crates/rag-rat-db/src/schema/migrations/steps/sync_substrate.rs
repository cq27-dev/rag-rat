use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::schema::migrations::{add_column_if_missing, column_exists};

/// V052 (#503, phase B C4): the memory op-log storage tables — a durable home for the pure `oplog`
/// primitives (op model, signed hash-chained entry, deterministic fold). Two layers (§5.4):
///
/// - **`oplog_entries`** is layer 1: the opaque signed entry log. `signed_bytes` is the sole source
///   of truth; `entry_hash` (its content address + chain link + idempotency key),
///   `device_fingerprint`, `lamport`, and `prev_hash` are denormalized from the SAME verified entry
///   for indexed access and cannot drift. NO FK — the log is authored data that must OUTLIVE a
///   reindex (the #248 content- addressed discipline); a reindex rewrites the code graph, never
///   this table. `UNIQUE(device_ fingerprint, lamport)` pins each device's chain as strictly linear
///   (a tripwire against a same-slot equivocation; per-`stream_id` fork DETECTION is a later S2
///   increment).
/// - **`oplog_projected_nodes` / `oplog_projected_edges`** are layer 2: a DERIVED shadow
///   projection, wholly rebuilt by the full-replay fold (`store::reproject`) — never a source of
///   truth, so a `DELETE`-all + reinsert is the whole update. `oplog_meta` stamps the projector
///   version so a binary that learns a new op kind re-folds on demand (§5.4 upgrade re-fold) rather
///   than trusting a stale materialization.
///
/// Idempotent (`CREATE TABLE IF NOT EXISTS`), self-transaction-free, replay-write-free (#498); the
/// tables are fresh with no backfill.
pub fn apply_oplog_storage(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS oplog_entries(
             entry_hash         BLOB PRIMARY KEY,
             device_fingerprint BLOB NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB NOT NULL,
             received_at_ms     INTEGER NOT NULL,
             UNIQUE(device_fingerprint, lamport)
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_projected_nodes(
             node_id      TEXT PRIMARY KEY,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_projected_edges(
             edge_key      TEXT PRIMARY KEY,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_meta(
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         ) STRICT;",
    )?;
    Ok(())
}

/// V053 (#509): scope the op-log by immutable stream identity. One signed chain, watermark, and
/// projection exists per `(stream_id, device)`, so the V052 tables gain a `stream_id` dimension:
/// `UNIQUE(stream_id, device_fingerprint, lamport)` pins each device's chain as strictly linear
/// PER STREAM, and the shadow-projection tables key on `(stream_id, node_id / edge_key)` so a
/// re-fold of one stream never touches another's rows. `oplog_fork_evidence` is new — the
/// quarantine that durably preserves BOTH heads of a detected equivocation (the store previously
/// only RETURNED the colliding entry to the caller); `signed_bytes` is the rejected head verbatim,
/// `conflicting_entry_hash` points at the stored entry it collided with.
///
/// INVARIANT: this is a DROP + CREATE rebuild, which is safe ONLY because nothing writes the
/// op-log tables yet (the module is un-wired until the write-path increment, so every database's
/// copies are empty — there is no data to preserve). Once the log is wired, a re-shape must use a
/// data-preserving recipe instead. `oplog_meta` is left untouched (its projector-version stamp is
/// not stream-scoped). Idempotent (a replay rebuilds the same empty tables); the self-wrapped
/// IMMEDIATE transaction makes an interrupted rebuild reconverge on the next run.
pub fn apply_oplog_stream_scoping(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         DROP TABLE IF EXISTS oplog_entries;
         CREATE TABLE oplog_entries(
             entry_hash         BLOB PRIMARY KEY,
             stream_id          BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB NOT NULL,
             received_at_ms     INTEGER NOT NULL,
             UNIQUE(stream_id, device_fingerprint, lamport)
         ) STRICT;

         DROP TABLE IF EXISTS oplog_projected_nodes;
         CREATE TABLE oplog_projected_nodes(
             stream_id    BLOB NOT NULL,
             node_id      TEXT NOT NULL,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL,
             PRIMARY KEY(stream_id, node_id)
         ) STRICT;

         DROP TABLE IF EXISTS oplog_projected_edges;
         CREATE TABLE oplog_projected_edges(
             stream_id     BLOB NOT NULL,
             edge_key      TEXT NOT NULL,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT,
             PRIMARY KEY(stream_id, edge_key)
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_fork_evidence(
             stream_id              BLOB NOT NULL,
             entry_hash             BLOB NOT NULL,
             device_fingerprint     BLOB NOT NULL,
             lamport                INTEGER NOT NULL,
             signed_bytes           BLOB NOT NULL,
             conflicting_entry_hash BLOB,
             observed_at_ms         INTEGER NOT NULL,
             PRIMARY KEY(stream_id, entry_hash)
         ) STRICT;

         COMMIT;",
    )?;
    Ok(())
}

/// V054 (#513): the op-log's persisted local device identity. ONE ed25519 keypair per store, so
/// every entry this install authors — live or backfilled — signs under a stable fingerprint
/// instead of a fresh per-process key. Store-global, NOT repo-scoped: a device is a machine
/// identity, orthogonal to the per-repo owner streams it signs (and, later, doubles as the
/// transport node key — a machine singleton). `id INTEGER PRIMARY KEY CHECK (id = 0)` is the
/// single-row guard: a second identity cannot be inserted. `seed` is the 32-byte secret scalar
/// seed; `public_key` (32 bytes) and `fingerprint` (= sha256(public_key)) are derivable from it but
/// stored too so the row is legible and a load can assert they still agree.
///
/// Purely ADDITIVE (`CREATE TABLE IF NOT EXISTS` — a brand-new table, nothing to drop or backfill),
/// unlike the V053 rebuild. Idempotent; the self-wrapped IMMEDIATE transaction reconverges an
/// interrupted create on the next run.
pub fn apply_oplog_device_identity(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE IF NOT EXISTS oplog_device_identity(
             id            INTEGER PRIMARY KEY CHECK (id = 0),
             seed          BLOB NOT NULL,
             public_key    BLOB NOT NULL,
             fingerprint   BLOB NOT NULL,
             created_at_ms INTEGER NOT NULL
         ) STRICT;

         COMMIT;",
    )?;
    Ok(())
}

/// V058 (sync phase C, §5): give the single device identity an X25519 ENCRYPTION keypair beside its
/// ed25519 signing key. Two nullable `BLOB` columns — `x25519_secret` (the 32-byte scalar, the sole
/// durable copy, D4) and `x25519_public` — added to the STRICT `oplog_device_identity` table
/// (`BLOB` is a valid STRICT type). Nullable + additive: an existing row keeps its ed25519 identity
/// and is backfilled at the next `local_device` open via a CAS UPDATE (mirroring the ed25519
/// mint-if-absent race), so a concurrent open cannot split into two encryption identities.
/// Idempotent via `add_column_if_missing`; on a fresh DB this runs right after V054 creates the
/// table, so both columns are present before the first `local_device` call.
pub fn apply_oplog_device_x25519(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "oplog_device_identity", "x25519_secret", "BLOB")?;
    add_column_if_missing(conn, "oplog_device_identity", "x25519_public", "BLOB")?;
    Ok(())
}

/// V059 (sync phase C, §16.1): the account-log CANDIDATE DAG. `account_entries` stores EVERY
/// structurally-valid, signature-valid account entry — all branches of an equivocating chain are
/// first-class, so the candidate table has NO seq-uniqueness; grow-only (I8). The `accepted` flag
/// is DERIVED, rewritten by every `refold_account` (the fold + branch selection §16.2), and the
/// partial unique index `account_accepted_slot` pins accepted-set uniqueness per `(account, log,
/// device, seq)` slot (I10a). `account_entry_status` holds the projected §16.3 taxonomy per entry;
/// `account_pre_verify` durably holds an entry whose signing device can't yet be resolved
/// (`sha256(pk) == fingerprint` not found among genesis + stored candidates), retried when a later
/// DeviceAdd/AccountGenesis for the claimed account arrives (Codex-8). Idempotent — every statement
/// is `CREATE ... IF NOT EXISTS`, so a torn replay reconverges without a wrapping txn.
pub fn apply_account_candidate_dag(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_entries(
             entry_hash         BLOB    PRIMARY KEY,
             account_id         BLOB    NOT NULL,
             log_id             INTEGER NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             seq                INTEGER NOT NULL,
             prev_hash          BLOB,
             parent_ref         BLOB,
             authority_ref      BLOB,
             entry_type         INTEGER NOT NULL,
             accepted           INTEGER NOT NULL DEFAULT 0,
             signed_bytes       BLOB    NOT NULL,
             received_at_ms     INTEGER NOT NULL
         ) STRICT;

         CREATE INDEX IF NOT EXISTS account_entries_chain
             ON account_entries(account_id, log_id, device_fingerprint, seq);

         CREATE UNIQUE INDEX IF NOT EXISTS account_accepted_slot
             ON account_entries(account_id, log_id, device_fingerprint, seq) WHERE accepted = 1;

         CREATE TABLE IF NOT EXISTS account_entry_status(
             entry_hash BLOB PRIMARY KEY,
             status     TEXT NOT NULL,
             detail     TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS account_pre_verify(
             signed_hash         BLOB    PRIMARY KEY,
             entry_hash          BLOB    NOT NULL,
             claimed_account_id  BLOB    NOT NULL,
             claimed_fingerprint BLOB    NOT NULL,
             raw_bytes           BLOB    NOT NULL,
             received_at_ms      INTEGER NOT NULL
         ) STRICT;

         -- Promotion scans the queue by claimed_account_id while holding the ingest write lock;
         -- index it so a backlog for OTHER accounts is never full-scanned per ingest.
         CREATE INDEX IF NOT EXISTS account_pre_verify_account
             ON account_pre_verify(claimed_account_id);",
    )
}

/// V066 (sync phase C2, §16): the owner-bound `/3` content candidate DAG.
///
/// Full-width unsigned wire counters are fixed-width big-endian blobs. SQLite INTEGER is signed
/// i64 and would reject or truncate valid `u64` sequence/authentication counters. Lexicographic
/// ordering of equal-width big-endian blobs is the unsigned numeric ordering required by dense
/// chain queries.
pub fn apply_content_candidate_dag(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_entries(
             entry_hash          BLOB    PRIMARY KEY CHECK(length(entry_hash) = 32),
             stream_id           BLOB    NOT NULL CHECK(length(stream_id) = 32),
             author_account_id   BLOB    NOT NULL CHECK(length(author_account_id) = 32),
             device_fingerprint  BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             seq                 BLOB    NOT NULL CHECK(length(seq) = 8),
             prev_hash           BLOB    CHECK(prev_hash IS NULL OR length(prev_hash) = 32),
             grant_id            BLOB    CHECK(grant_id IS NULL OR length(grant_id) = 32),
             roster_ref          BLOB    NOT NULL CHECK(length(roster_ref) = 32),
             owner_auth_len      BLOB    NOT NULL CHECK(length(owner_auth_len) = 8),
             author_auth_len     BLOB    NOT NULL CHECK(length(author_auth_len) = 8),
             accepted            INTEGER NOT NULL DEFAULT 0 CHECK(accepted IN (0, 1)),
             signed_bytes        BLOB    NOT NULL,
             received_at_ms      INTEGER NOT NULL
         ) STRICT;

         -- Candidate history is grow-only: equivocations share a coordinate and remain distinct.
         CREATE INDEX IF NOT EXISTS content_entries_chain
             ON content_entries(stream_id, author_account_id, device_fingerprint, seq);
         CREATE INDEX IF NOT EXISTS content_entries_predecessor
             ON content_entries(prev_hash, stream_id, author_account_id, device_fingerprint);

         -- C2 never sets accepted=1. C3 owns the atomic authority+branch refold that activates it.
         CREATE UNIQUE INDEX IF NOT EXISTS content_accepted_slot
             ON content_entries(stream_id, author_account_id, device_fingerprint, seq)
             WHERE accepted = 1;

         CREATE TABLE IF NOT EXISTS content_entry_status(
             entry_hash BLOB PRIMARY KEY CHECK(length(entry_hash) = 32),
             status     TEXT NOT NULL,
             detail     TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS content_pre_verify(
             signed_hash               BLOB    PRIMARY KEY CHECK(length(signed_hash) = 32),
             entry_hash                BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             claimed_stream_id         BLOB    NOT NULL CHECK(length(claimed_stream_id) = 32),
             claimed_author_account_id BLOB    NOT NULL CHECK(length(claimed_author_account_id) = \
         32),
             claimed_fingerprint       BLOB    NOT NULL CHECK(length(claimed_fingerprint) = 32),
             roster_ref                BLOB    NOT NULL CHECK(length(roster_ref) = 32),
             raw_bytes                 BLOB    NOT NULL,
             received_at_ms            INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS content_pre_verify_author
             ON content_pre_verify(claimed_author_account_id, roster_ref);",
    )
}

/// V069 (sync phase C3.4a): the store-global local-account pointer. `oplog_local_account` is a
/// single-row (`CHECK (id = 0)`) STRICT table naming the `genesis_entry_hash` of THIS store's one
/// local account — the seq-0, self-authorizing `AccountGenesis` minted once by
/// `rag_rat_oplog::local_account` and reused thereafter, so later C3.4 slices author owner-bound
/// `/3` content under a stable account identity. The pointer is a 32-byte content address into
/// `account_entries`, not the account_id itself: the id is resolved by looking the genesis up in
/// the candidate DAG, so the pointer + genesis stay a single source of truth (one committed
/// atomically with the other by the minting transaction). Store-global like
/// `oplog_device_identity`, not repo-scoped. Purely additive; `CREATE ... IF NOT EXISTS`, so a torn
/// replay reconverges without a wrapping transaction.
pub fn apply_oplog_local_account(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS oplog_local_account(
             id                 INTEGER PRIMARY KEY CHECK (id = 0),
             genesis_entry_hash BLOB NOT NULL CHECK (length(genesis_entry_hash) = 32),
             created_at_ms      INTEGER NOT NULL
         ) STRICT;",
    )
}

/// V070 (sync phase C3.4b-i): the accepted-`/3` → memory projection tables.
/// `content_projected_nodes` / `content_projected_edges` mirror the `/1` shadow tables
/// `oplog_projected_nodes` / `oplog_projected_edges` (stream-keyed since V053) but materialize the
/// acceptance-gated `/3` DAG: `rag_rat_oplog::reproject_accepted_content_stream` decodes each
/// `content_entries` row where `accepted = 1`, folds via the shared memory projector, and rewrites
/// the keyed rows for one `/2` stream. Kept SEPARATE from the `/1` tables on purpose (decision 7):
/// the `/1` projector sweep (`store::reproject_all_streams`) `DELETE`s the `oplog_projected_*`
/// tables wholesale and rebuilds only streams present in `oplog_entries`, so sharing them would let
/// a projector-version bump wipe the `/3` projection and never rebuild it — mass duplicate
/// re-authoring into the immutable `/3` log. These tables are owned by the memory layer and updated
/// only when acceptance changes (the content refold), never by the `/1` sweep. Purely additive;
/// `CREATE ... IF NOT EXISTS`, so a torn replay reconverges without a wrapping transaction; nothing
/// pre-existing to backfill.
pub fn apply_content_projected_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_projected_nodes(
             stream_id    BLOB NOT NULL,
             node_id      TEXT NOT NULL,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL,
             PRIMARY KEY(stream_id, node_id)
         ) STRICT;

         CREATE TABLE IF NOT EXISTS content_projected_edges(
             stream_id     BLOB NOT NULL,
             edge_key      TEXT NOT NULL,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT,
             PRIMARY KEY(stream_id, edge_key)
         ) STRICT;",
    )
}

/// V072 (issue #652): the deferred-refold work queue for the `/3` content-ingest path.
/// `content_ingest` used to fold acceptance over the whole stream on EVERY ingested entry — an
/// O(n^2) cost as an n-entry stream is built one candidate at a time under the writer lock, which
/// an attacker amplifies by varying cited `auth_len` to defeat the per-refold freshness cache. It
/// now records structural classification and enqueues the stream here instead; the settle seam
/// (`settle_pending_content_refolds`) folds each dirty stream ONCE.
/// INVARIANT: a `stream_id` is present while a refold + reproject is still owed. The row is
/// discharged ONLY by `refold_and_project_stream_in_tx` — reached either from the settle seam or
/// from a TRUSTED/local account fold (`finalize_affected_streams`) — and only after both steps
/// succeed. The untrusted remote account-ingest path never clears it here; it only ADDS debt
/// (`ACCOUNT_CHANGE`) for settle to drain.
/// Purely additive; `CREATE ... IF NOT EXISTS`, so a torn replay reconverges without a wrapping
/// transaction; nothing pre-existing to backfill.
pub fn apply_content_streams_pending_refold(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_streams_pending_refold(
             stream_id BLOB PRIMARY KEY CHECK (length(stream_id) = 32)
         ) STRICT;",
    )
}

/// V082 (#698): reasoned/ordered content-refold work and O(1) per-stream fold-cost accounting.
///
/// `candidate_bytes` matches the payload that a full refold's `load_stream_headers` query copies
/// out of SQLite: `length(signed_bytes) + 32` for the separately materialized `entry_hash` on every
/// row. It deliberately does not guess at allocator/container overhead after decode. The source
/// rows remain authoritative: this migration rebuilds the aggregate once, and database triggers
/// maintain it for every writer thereafter. The update trigger covers direct mutation of
/// `stream_id` or `signed_bytes`; runtime writers currently treat both as immutable.
///
/// The triggers do NOT cover one shape: SQLite performs `INSERT OR REPLACE`'s implicit row deletion
/// WITHOUT firing `AFTER DELETE` triggers unless `PRAGMA recursive_triggers` is on, which this
/// store never sets — so a `REPLACE` into `content_entries` fires the insert trigger only and
/// drifts the aggregate upward permanently. Upward drift is fail-safe for admission (the stream
/// looks expensive and is skipped, never silently unbounded), but it makes the stream invisible to
/// normal-mode settle forever. No writer uses `REPLACE` today and a source tripwire test keeps it
/// that way (`rag-rat-oplog`); `INSERT OR IGNORE` and non-accounting `UPDATE`s are correctly inert.
///
/// Existing V072 queue rows predate enqueue timestamps. Their first/last times derive
/// deterministically from the stream's minimum/maximum candidate `received_at_ms`; an orphan queue
/// row with no remaining candidate receives `0` for both. Timestamp defaults remain zero in this
/// schema-only slice so the existing enqueue helper continues to work until the runtime slice
/// begins stamping and merging queue metadata.
pub fn apply_content_refold_queue_and_stats(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    if !column_exists(&tx, "content_streams_pending_refold", "reason_mask")? {
        tx.execute_batch(
            "DROP TABLE IF EXISTS content_streams_pending_refold_v082;
             CREATE TABLE content_streams_pending_refold_v082(
                 stream_id           BLOB    PRIMARY KEY CHECK(length(stream_id) = 32),
                 reason_mask         INTEGER NOT NULL DEFAULT 1 CHECK(reason_mask BETWEEN 1 AND 3),
                 first_enqueued_at_ms INTEGER NOT NULL DEFAULT 0,
                 last_enqueued_at_ms  INTEGER NOT NULL DEFAULT 0
             ) STRICT;
             INSERT INTO content_streams_pending_refold_v082(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             SELECT q.stream_id,
                    1,
                    COALESCE(MIN(e.received_at_ms), 0),
                    COALESCE(MAX(e.received_at_ms), 0)
             FROM content_streams_pending_refold q
             LEFT JOIN content_entries e ON e.stream_id = q.stream_id
             GROUP BY q.stream_id;
             DROP TABLE content_streams_pending_refold;
             ALTER TABLE content_streams_pending_refold_v082
                 RENAME TO content_streams_pending_refold;",
        )?;
    }

    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS content_streams_pending_refold_order
             ON content_streams_pending_refold(first_enqueued_at_ms, stream_id);

         CREATE TABLE IF NOT EXISTS content_stream_stats(
             stream_id       BLOB    PRIMARY KEY CHECK(length(stream_id) = 32),
             candidate_count INTEGER NOT NULL CHECK(candidate_count >= 0),
             candidate_bytes INTEGER NOT NULL CHECK(candidate_bytes >= 0)
         ) STRICT;

         DROP TRIGGER IF EXISTS content_stream_stats_after_insert;
         DROP TRIGGER IF EXISTS content_stream_stats_after_delete;
         DROP TRIGGER IF EXISTS content_stream_stats_after_update;

         DELETE FROM content_stream_stats;
         INSERT INTO content_stream_stats(stream_id, candidate_count, candidate_bytes)
         SELECT stream_id, count(*), sum(length(signed_bytes) + 32)
         FROM content_entries
         GROUP BY stream_id;

         CREATE TRIGGER content_stream_stats_after_insert
         AFTER INSERT ON content_entries
         BEGIN
             INSERT INTO content_stream_stats(stream_id, candidate_count, candidate_bytes)
             VALUES (NEW.stream_id, 1, length(NEW.signed_bytes) + 32)
             ON CONFLICT(stream_id) DO UPDATE SET
                 candidate_count = candidate_count + 1,
                 candidate_bytes = candidate_bytes + length(NEW.signed_bytes) + 32;
         END;

         CREATE TRIGGER content_stream_stats_after_delete
         AFTER DELETE ON content_entries
         BEGIN
             UPDATE content_stream_stats
             SET candidate_count = candidate_count - 1,
                 candidate_bytes = candidate_bytes - length(OLD.signed_bytes) - 32
             WHERE stream_id = OLD.stream_id;
             DELETE FROM content_stream_stats
             WHERE stream_id = OLD.stream_id AND candidate_count = 0;
         END;

         CREATE TRIGGER content_stream_stats_after_update
         AFTER UPDATE OF stream_id, signed_bytes ON content_entries
         BEGIN
             UPDATE content_stream_stats
             SET candidate_count = candidate_count - 1,
                 candidate_bytes = candidate_bytes - length(OLD.signed_bytes) - 32
             WHERE stream_id = OLD.stream_id;
             DELETE FROM content_stream_stats
             WHERE stream_id = OLD.stream_id AND candidate_count = 0;
             INSERT INTO content_stream_stats(stream_id, candidate_count, candidate_bytes)
             VALUES (NEW.stream_id, 1, length(NEW.signed_bytes) + 32)
             ON CONFLICT(stream_id) DO UPDATE SET
                 candidate_count = candidate_count + 1,
                 candidate_bytes = candidate_bytes + length(NEW.signed_bytes) + 32;
         END;",
    )?;

    tx.commit()
}

/// V083 (#855/#860) — persist the direct chunk→symbol link. `chunks.symbol_id` is the rowid of the
/// symbol a code chunk was cut from, stamped at index time by `insert_chunks` from the same parse
/// that assigned the symbol its rowid. It replaces position-based resolution (match by
/// `(path, qualified_name)` then narrow by byte/line geometry), which could not disambiguate
/// same-name symbols that nest or share a physical line — `qualified_name` is `path::simple_name`,
/// not scope-qualified, so overloads and nested same-name functions collide, and no geometric
/// metric attributes a chunk to one of two coincident symbols.
pub fn apply_chunk_symbol_id(conn: &Connection) -> rusqlite::Result<()> {
    // One transaction for the column add + backfill. An established index can hold hundreds of
    // thousands of symbol-bearing chunks; committing each per-row UPDATE in its own autocommit
    // (a WAL fsync apiece) would make the startup migration prohibitively slow. It is also atomic —
    // an interrupted upgrade rolls back to no column and no partial backfill, and replay redoes it
    // from scratch (and is idempotent on any rows a later run finds already linked).
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    add_column_if_missing(&tx, "chunks", "symbol_id", "INTEGER")?;
    backfill_chunk_symbol_ids(&tx)?;
    tx.commit()
}

/// One-time backfill of `chunks.symbol_id` for chunks indexed before V083. Unlike the V079/V081
/// derived stores, chunks are NOT rewritten on a regular cadence — incremental/discover indexing
/// skips UNCHANGED files, so a stable file's chunks would keep NULL `symbol_id` (and lose their
/// drive-by records) indefinitely, not "until the next reindex".
///
/// Resolve each chunk from the identity it ALREADY carries: `symbol_path` is the defining symbol's
/// bare qualified name, except a split continuation which appends `#<n>` (a code path holds at most
/// one `#`, so stripping from the first `#` recovers the bare name; context / whole-file / markdown
/// paths keep theirs and simply match no qualified name). Link a chunk ONLY when exactly one
/// same-file symbol of that qualified name OVERLAPS it in bytes.
///
/// BYTES, not lines: `symbols.start_byte`/`end_byte` are baseline columns, always populated,
/// whereas `start_line`/`end_line` were added later with `DEFAULT 0`, so a never-reindexed legacy
/// symbol can carry `0`/`0` line spans — a line predicate would silently match nothing and strand
/// exactly the rows this repairs. A chunk is cut from the whole lines around its symbol, so it
/// always overlaps that symbol's byte span (part 0 and every continuation part alike).
///
/// This never guesses the cases the direct link exists to resolve: a continuation whose outer is
/// UNIQUELY named links back to that outer (even when a differently-named nested symbol's bytes it
/// falls within also exist), but two same-name symbols that nest or share a line match more than
/// one (`HAVING COUNT(*) = 1` excludes them → NULL), and a non-qualified-name path matches nothing
/// (→ NULL). A NULL chunk surfaces no record; a wrong guess would surface the WRONG one and
/// persist. `rag-rat index` re-stamps every chunk precisely from the parse.
///
/// One set-based statement (no per-row round-trip, no materializing every chunk in memory), run
/// inside the caller's transaction so the whole backfill is a single commit. Idempotent: only
/// chunks still NULL are considered, so a re-run never overwrites an exact index-time id.
fn backfill_chunk_symbol_ids(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        // `base.qname` strips ONLY a trailing `#<digits>` continuation suffix, matching what the
        // chunker appends. Splitting on the FIRST `#` instead would truncate a qualified name
        // whose FILE PATH legitimately contains one (`src/foo#bar.rs::run`), and since
        // unchanged files are never re-chunked those rows would keep a NULL `symbol_id` —
        // and lose their drive-by records — indefinitely. `rtrim` removes trailing digits;
        // the suffix is real only if that shortened the string AND what remains ends in
        // `#`.
        "WITH base AS (
             SELECT c.id AS chunk_id, c.file_id AS file_id,
                    c.start_byte AS start_byte, c.end_byte AS end_byte,
                    CASE
                        WHEN rtrim(c.symbol_path, '0123456789') <> c.symbol_path
                         AND substr(rtrim(c.symbol_path, '0123456789'), -1) = '#'
                        THEN substr(rtrim(c.symbol_path, '0123456789'), 1,
                                    length(rtrim(c.symbol_path, '0123456789')) - 1)
                        ELSE c.symbol_path
                    END AS qname
             FROM chunks c
             WHERE c.symbol_id IS NULL
               AND c.symbol_path IS NOT NULL
         ),
         resolved AS (
             SELECT base.chunk_id AS chunk_id, s.id AS symbol_id
             FROM base
             JOIN symbols s ON s.file_id = base.file_id
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE ns.value = base.qname
               AND s.start_byte < base.end_byte
               AND base.start_byte < s.end_byte
             GROUP BY base.chunk_id
             HAVING COUNT(*) = 1
         )
         UPDATE chunks
         SET symbol_id = resolved.symbol_id
         FROM resolved
         WHERE chunks.id = resolved.chunk_id;",
    )
}

/// V076 (sync phase C4.3b, #607): the sealing-key adoption audit log. A recipient device records a
/// row here when an accepted `StreamKeyWrap` naming it either fails to unwrap (AEAD tag failure —
/// the primary manifestation of a substituted wrap) or unwraps to a key whose `key_id` disagrees
/// with the op's signed `key_id`.
///
/// INVARIANT: local-only. These rows are never on the wire, never a fold input, and the adoption
/// seam never mutates a fold verdict — the shared fold stays device-independent (convergent), so a
/// recipient-only unwrap check can only ever be LOCAL evidence. `key_epoch` is stored as 8-byte BE
/// (not INT) so a `u64` epoch round-trips without the `i64` narrowing hazard. `UNIQUE(kind,
/// entry_hash)` + `INSERT OR IGNORE` (the write path) keep a hot seal-path retry from re-appending
/// the same evidence for one op. Purely additive; CREATE ... IF NOT EXISTS, nothing to backfill.
pub(crate) fn apply_sync_security_events(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_security_events(
             id              INTEGER PRIMARY KEY,
             kind            TEXT NOT NULL,
             account_id      BLOB NOT NULL,
             stream_id       BLOB NOT NULL,
             key_epoch       BLOB NOT NULL,
             entry_hash      BLOB NOT NULL,
             expected_key_id BLOB,
             observed_key_id BLOB,
             observed_at_ms  INT NOT NULL
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS sync_security_events_dedup
             ON sync_security_events(kind, entry_hash);",
    )
}

/// V064 (sync phase C1, §16): query-ready authority facts derived from the accepted account fold.
/// These are shadow tables, never independent sources of truth: `refold_account` deletes and
/// rewrites every row for one account inside the SAME IMMEDIATE transaction as accepted/status.
/// History intervals (`effective_at`, `closed_at`) preserve audit/projection facts. `auth_len` is
/// only a synchronization assertion: ahead parks for refetch, while behind is informational and
/// never selects historical authority. Exact citations and device cuts make authorization keyed
/// lookups instead of adversary-amplified replay of up to 4096 candidates.
pub fn apply_account_authority_projection(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_auth_state(
             account_id        BLOB PRIMARY KEY,
             classification    TEXT NOT NULL,
             contested_depth   INTEGER,
             successor_account_id BLOB,
             effective_count   INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE IF NOT EXISTS account_roster_history(
             roster_ref         BLOB PRIMARY KEY,
             account_id         BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             role               TEXT NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_roster_history_account
             ON account_roster_history(account_id, device_fingerprint);

         CREATE TABLE IF NOT EXISTS account_owner_incarnations(
             owner_id           BLOB PRIMARY KEY,
             account_id         BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_owner_incarnations_account
             ON account_owner_incarnations(account_id, device_fingerprint);

         CREATE TABLE IF NOT EXISTS account_stream_ownership(
             stream_id      BLOB PRIMARY KEY,
             account_id     BLOB NOT NULL,
             own_id         BLOB NOT NULL,
             effective_at   INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_stream_ownership_account
             ON account_stream_ownership(account_id);

         CREATE TABLE IF NOT EXISTS account_stream_grants(
             grant_id           BLOB PRIMARY KEY,
             owner_account_id   BLOB NOT NULL,
             stream_id          BLOB NOT NULL,
             grantee_account_id BLOB NOT NULL,
             role               TEXT NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_stream_grants_owner
             ON account_stream_grants(owner_account_id, stream_id, grantee_account_id);

         CREATE TABLE IF NOT EXISTS account_stream_grant_cuts(
             grant_id           BLOB NOT NULL,
             owner_account_id   BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             -- Fixed-width big-endian bytes preserve the full protocol u64 domain and sort in
             -- unsigned numeric order; SQLite INTEGER is signed and would reject high cuts.
             seq                BLOB NOT NULL CHECK(length(seq) = 8),
             entry_hash         BLOB NOT NULL,
             PRIMARY KEY(grant_id, device_fingerprint)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_stream_grant_cuts_owner
             ON account_stream_grant_cuts(owner_account_id, grant_id);",
    )?;
    // V064's transactional backfill immediately calls the current projection writer. Provision
    // the additive boundary shape here too, so a database upgrading from V063 can be backfilled by
    // this binary; V065 repeats it idempotently for databases that already recorded old V064.
    apply_account_authority_boundaries(conn)
}

/// V065: retain the exact chain boundaries of closed roster and owner citations. The V064 rows
/// remain historical facts; these columns make their valid prefix explicit instead of forcing a
/// caller to choose between accepting a revoked citation and rejecting valid late delivery.
pub fn apply_account_authority_boundaries(conn: &Connection) -> rusqlite::Result<()> {
    for (table, prefix) in [
        ("account_roster_history", "control"),
        ("account_roster_history", "secrets"),
        ("account_owner_incarnations", "control"),
        ("account_owner_incarnations", "secrets"),
    ] {
        add_column_if_missing(
            conn,
            table,
            &format!("{prefix}_boundary"),
            "TEXT NOT NULL DEFAULT 'open'",
        )?;
        add_column_if_missing(conn, table, &format!("{prefix}_seq"), "BLOB")?;
        add_column_if_missing(conn, table, &format!("{prefix}_hash"), "BLOB")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_roster_content_boundaries(
             roster_ref BLOB NOT NULL,
             account_id BLOB NOT NULL,
             stream_id  BLOB NOT NULL,
             seq        BLOB NOT NULL CHECK(length(seq) = 8),
             entry_hash BLOB NOT NULL CHECK(length(entry_hash) = 32),
             PRIMARY KEY(roster_ref, stream_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_roster_content_boundaries_account
             ON account_roster_content_boundaries(account_id, roster_ref);",
    )
}

/// V055 (#492): the anchor-status downgrade hysteresis marker. INVARIANT: NULL means "no gone
/// observation is pending" — every persisted non-deferred stamp clears it, so a non-null marker
/// only ever bridges two CONSECUTIVE gone observations of the same binding. Nullable and
/// additive: existing rows start unarmed, and the pre-V055 behavior (immediate downgrade) simply
/// becomes the two-pass rule from the next validate on.
pub(crate) fn apply_binding_downgrade_marker(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "repo_memory_bindings", "downgrade_pending_at_ms", "INTEGER")?;
    Ok(())
}
