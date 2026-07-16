//! Tests for the windowed file-pair change-coupling pass (`crate::index::change_coupling`, V056,
//! #566). The STORED table is a pure function of `(git-history window, params)` — no `files`-view
//! dependence — so the compute-level fixtures are direct synthetic inserts into `git_commits` /
//! `git_file_changes` on a raw connection. The generated / existence filter and the dedup are
//! READ-time (`coupled_files_for_path`), so the read-level and integration tests also seed `files`.

use rusqlite::{Connection, params};

use super::*;
use crate::index::change_coupling::{
    COUPLING_WINDOW_COMMITS, coupled_files_for_path, ensure_coupling_fresh, recompute_couplings,
};

/// The active repo id a fresh `schema::apply` connection resolves to (the legacy placeholder), used
/// for the single-repo fixtures whose inserts default `repo_id` to the same value.
fn repo() -> String {
    rag_rat_base::repo_identity::LEGACY_REPO_ID.to_string()
}

fn fresh_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn
}

fn add_file(conn: &Connection, path: &str, generated: i64) {
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         generated) VALUES (?1, 'rust', 'source', 'sha', 0, 0, ?2)",
        params![path, generated],
    )
    .unwrap();
}

/// Insert a non-generated `files` row for `path` scoped to a specific `commit_sha` — lets a test
/// seed MULTIPLE `main.files` rows for the same path at the same live generation (what the bare
/// repo-generation view serves), which the `UNIQUE(repo_id, path, commit_sha, worktree_id,
/// generation)` key allows.
fn add_file_at_commit(conn: &Connection, path: &str, commit_sha: &str) {
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         generated, commit_sha) VALUES (?1, 'rust', 'source', 'sha', 0, 0, 0, ?2)",
        params![path, commit_sha],
    )
    .unwrap();
}

/// Insert a commit for `repo_id` touching `paths` (each recorded as a change row).
/// `changed_file_count` is set to the change-row count, mirroring `insert_history_rows`, so the
/// eligibility filter reads a realistic value. An empty `paths` slice models a MERGE tip
/// (`changed_file_count = 0`, no rows).
fn add_commit(conn: &Connection, repo_id: &str, hash: &str, committed_at_s: i64, paths: &[&str]) {
    add_commit_counted(conn, repo_id, hash, committed_at_s, paths, paths.len() as i64);
}

/// Like [`add_commit`] but forces `changed_file_count` independently of the recorded rows — used to
/// exercise the mega-commit gate at the exact boundary.
fn add_commit_counted(
    conn: &Connection,
    repo_id: &str,
    hash: &str,
    committed_at_s: i64,
    paths: &[&str],
    changed_file_count: i64,
) {
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, committed_at_s, \
         subject, body, changed_file_count, repo_id) VALUES (?1, 'a', 'a@b', ?2, ?2, 's', '', ?3, \
         ?4)",
        params![hash, committed_at_s, changed_file_count, repo_id],
    )
    .unwrap();
    for path in paths {
        conn.execute(
            "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
             repo_id) VALUES (?1, ?2, 0, 0, 'modified', ?3)",
            params![hash, path, repo_id],
        )
        .unwrap();
    }
}

/// Add `count` distinct 2-file filler commits (each a unique pair → co=1, dropped by the support
/// floor) starting at `start_ts`, purely to raise the eligible-window count N so a real pair's
/// `lift = co * N / (a * b)` clears the floor. Filler paths embed `start_ts` so repeated calls
/// never collide into a surviving pair; they are never asserted on.
fn add_fillers(conn: &Connection, repo_id: &str, count: usize, start_ts: i64) {
    for i in 0..count {
        add_commit(conn, repo_id, &format!("fill-{start_ts}-{i}"), start_ts + i as i64, &[
            &format!("fa-{start_ts}-{i}.rs"),
            &format!("fb-{start_ts}-{i}.rs"),
        ]);
    }
}

/// `(path_a, path_b, co, a_count, b_count, window, last_at_s)` rows for a repo, ordered by pair.
fn coupling_rows(
    conn: &Connection,
    repo_id: &str,
) -> Vec<(String, String, i64, i64, i64, i64, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT path_a, path_b, co_change_count, path_a_change_count, path_b_change_count, \
             window_commit_count, last_co_change_at_s FROM git_change_couplings WHERE repo_id = \
             ?1 ORDER BY path_a, path_b",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![repo_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// Insert a `git_change_couplings` row DIRECTLY, bypassing recompute and its write-time floors, so
/// a read-path test can pin behavior on a row it fully controls — a row that predates a lift-floor
/// increase, or a crafted tie set for the read ordering. `path_a < path_b` (the stored invariant)
/// is the caller's responsibility.
fn insert_coupling(
    conn: &Connection,
    repo_id: &str,
    path_a: &str,
    path_b: &str,
    counts: (i64, i64, i64, i64), // (co, a_count, b_count, window)
) {
    let (co, a_count, b_count, window) = counts;
    conn.execute(
        "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
         path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
         computed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0)",
        params![repo_id, path_a, path_b, co, a_count, b_count, window],
    )
    .unwrap();
}

/// The read re-applies the lift floor as a defense (the write floor is the storage authority, but a
/// row can predate a floor increase before its params-version recompute lands). A stored pair whose
/// lift is below the floor must be filtered at read, never surfaced.
#[test]
fn read_drops_a_stored_pair_below_the_lift_floor() {
    let conn = fresh_conn();
    let repo = repo();
    add_file(&conn, "b.rs", 0); // partner needs a non-generated `files` row for the read join
    // lift = co * window / (a * b) = 2 * 10 / (10 * 10) = 0.2, below MIN_COUPLING_LIFT (1.5).
    insert_coupling(&conn, &repo, "a.rs", "b.rs", (2, 10, 10, 10));
    assert!(
        coupled_files_for_path(&conn, &repo, "a.rs", 10).unwrap().is_empty(),
        "a stored row below the lift floor must be dropped at read, not surfaced"
    );
}

/// The read comparator is deterministic — confidence DESC, then co DESC, then path ASC — so the
/// `truncate(limit)` top-K is stable regardless of row/scan order. `pa`/`pb` tie on (confidence,
/// co) and split by path; `pc` outranks both on confidence. (`aaa.rs` sorts before every partner,
/// so it is `path_a` in each row and its count is `path_a_change_count`.)
#[test]
fn read_orders_partners_by_confidence_then_co_then_path() {
    let conn = fresh_conn();
    let repo = repo();
    for p in ["pa.rs", "pb.rs", "pc.rs"] {
        add_file(&conn, p, 0);
    }
    insert_coupling(&conn, &repo, "aaa.rs", "pc.rs", (6, 8, 6, 100)); // confidence 0.75
    insert_coupling(&conn, &repo, "aaa.rs", "pa.rs", (4, 8, 8, 100)); // confidence 0.50, co 4
    insert_coupling(&conn, &repo, "aaa.rs", "pb.rs", (4, 8, 4, 100)); // confidence 0.50, co 4 (tie)
    let got: Vec<String> = coupled_files_for_path(&conn, &repo, "aaa.rs", 10)
        .unwrap()
        .into_iter()
        .map(|c| c.other_path)
        .collect();
    assert_eq!(got, vec!["pc.rs", "pa.rs", "pb.rs"]);
}

/// A window commit whose change rows collapse to fewer than two DISTINCT paths (here
/// `changed_file_count` is eligible at 2 but only one path row is recorded — a count/row mismatch)
/// yields no pair: the `(2..=MAX_COUPLING_COMMIT_FILES).contains(&n)` guard's false arm.
#[test]
fn commit_with_fewer_than_two_distinct_paths_contributes_no_pairs() {
    let conn = fresh_conn();
    let repo = repo();
    add_commit_counted(&conn, &repo, "one", 10, &["only.rs"], 2);
    add_fillers(&conn, &repo, 2, 100);
    recompute_couplings(&conn, &repo, 1).unwrap();
    assert!(
        coupling_rows(&conn, &repo).is_empty(),
        "a commit with a single distinct path must produce no coupling pairs"
    );
}

/// Exact support / endpoint counts / recency for a scripted commit set. Pins the derived confidence
/// (asymmetric) and lift, computed from the stored counts. Fillers lift the surviving pair over the
/// write-time lift floor without touching its endpoint counts.
#[test]
fn exact_support_confidence_lift_from_scripted_commits() {
    let conn = fresh_conn();
    let repo = repo();
    // a,b co-change twice; a also touches x once (so a_count=3 != b_count=2 — asymmetric
    // confidence). Two fillers raise N to 5, so (a,b)'s lift = 2*5/(3*2) = 1.667 clears the floor.
    add_commit(&conn, &repo, "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c2", 20, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c3", 30, &["a.rs", "x.rs"]);
    add_fillers(&conn, &repo, 2, 100);

    recompute_couplings(&conn, &repo, 777).unwrap();

    // (a,x) is support 1 and fillers are support 1; only (a,b) clears BOTH the support and lift
    // floors at write.
    let rows = coupling_rows(&conn, &repo);
    assert_eq!(rows.len(), 1, "only (a,b) clears support>=2 AND lift>=1.5: {rows:?}");
    let (path_a, path_b, co, a_count, b_count, window, last_at_s) = rows[0].clone();
    assert_eq!((path_a.as_str(), path_b.as_str()), ("a.rs", "b.rs"), "canonical path order");
    assert_eq!(co, 2, "co-changed in both {{a,b}} commits");
    assert_eq!(a_count, 3, "a.rs touched c1,c2,c3");
    assert_eq!(b_count, 2, "b.rs touched c1,c2");
    assert_eq!(window, 5, "N = 3 real + 2 filler eligible commits");
    assert_eq!(last_at_s, 20, "newest (a,b) co-change committed_at_s");

    let confidence_a_to_b = co as f64 / a_count as f64; // 2/3
    let confidence_b_to_a = co as f64 / b_count as f64; // 2/2
    let lift = co as f64 * window as f64 / (a_count as f64 * b_count as f64); // 2*5/(3*2)
    assert!((confidence_a_to_b - 2.0 / 3.0).abs() < 1e-9);
    assert!((confidence_b_to_a - 1.0).abs() < 1e-9);
    assert!((lift - 10.0 / 6.0).abs() < 1e-9, "lift = {lift}");
}

/// The lift floor is the WRITE-time storage bound: a below-threshold pair is never stored (nothing
/// to read), while an above-threshold pair is stored and surfaced.
#[test]
fn lift_floor_drops_below_threshold_pairs_and_keeps_above() {
    // Below the floor: a is a mini-hub (in every commit), so lift(a,b) = 2*4/(4*2) = 1.0 → the pair
    // is dropped AT WRITE. Nothing is stored, so nothing reads back.
    let conn = fresh_conn();
    let repo = repo();
    add_commit(&conn, &repo, "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c2", 20, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c3", 30, &["a.rs", "c.rs"]);
    add_commit(&conn, &repo, "c4", 40, &["a.rs", "c.rs"]);
    recompute_couplings(&conn, &repo, 1).unwrap();
    assert!(coupling_rows(&conn, &repo).is_empty(), "hub pairs (lift 1.0) dropped at write");
    assert!(coupled_files_for_path(&conn, &repo, "a.rs", 10).unwrap().is_empty());

    // Above the floor: a,b co-change exclusively; fillers raise N to 4 → lift = 2.0.
    let conn = fresh_conn();
    add_file(&conn, "a.rs", 0);
    add_file(&conn, "b.rs", 0);
    add_commit(&conn, &repo, "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c2", 20, &["a.rs", "b.rs"]);
    add_fillers(&conn, &repo, 2, 100);
    recompute_couplings(&conn, &repo, 1).unwrap();
    let coupled = coupled_files_for_path(&conn, &repo, "a.rs", 10).unwrap();
    assert_eq!(coupled.len(), 1, "one coupled file above the floor: {coupled:?}");
    assert_eq!(coupled[0].other_path, "b.rs");
    assert!((coupled[0].confidence - 1.0).abs() < 1e-9, "confidence(a->b) = 2/2");
    assert!((coupled[0].lift - 2.0).abs() < 1e-9, "lift = 2*4/(2*2) = 2.0");
    assert_eq!(coupled[0].window_commit_count, 4, "N = 2 real + 2 filler");
}

/// The LIFT FLOOR is the storage bound that replaces the per-file cap: a HUB file touched in ~all
/// window commits yields NO stored pairs (each scores lift ~= 1), while a genuine high-lift
/// low-count pair IS stored regardless of how few commits it spans. This is what makes dropping the
/// per-file cap safe — no ranking, so nothing can mis-rank an unsurfaced partner.
#[test]
fn lift_floor_bounds_storage() {
    let conn = fresh_conn();
    let repo = repo();
    let tx = conn.unchecked_transaction().unwrap();
    // Hub H co-changes (support 2) with 10 distinct partners → 20 commits, so H is in 20 of the 22
    // window commits. Each (H, Xi): co=2, a_count(H)=20, b_count(Xi)=2 → lift = 2*22/(20*2) = 1.1.
    for i in 0..10 {
        let partner = format!("x{i:02}.rs");
        add_commit(&conn, &repo, &format!("h{i:02}a"), 10, &["hub.rs", &partner]);
        add_commit(&conn, &repo, &format!("h{i:02}b"), 20, &["hub.rs", &partner]);
    }
    // A tight, low-count pair {p,q} co-changes exclusively → co=2, a=b=2, lift = 2*22/(2*2) = 11.
    add_commit(&conn, &repo, "pq1", 30, &["p.rs", "q.rs"]);
    add_commit(&conn, &repo, "pq2", 40, &["p.rs", "q.rs"]);
    tx.commit().unwrap();

    recompute_couplings(&conn, &repo, 1).unwrap();
    let rows = coupling_rows(&conn, &repo);

    // Every one of the hub's 10 partner pairs (lift 1.1 < 1.5) is dropped at write — no per-file
    // cap.
    assert!(
        !rows.iter().any(|(a, b, ..)| a == "hub.rs" || b == "hub.rs"),
        "the hub's pairs are pruned by the lift floor, not a cap: {rows:?}"
    );
    // The genuine high-lift pair survives, however few its commits.
    assert_eq!(rows.len(), 1, "only the tight high-lift pair is stored: {rows:?}");
    let (path_a, path_b, co, a_count, b_count, window) =
        (&rows[0].0, &rows[0].1, rows[0].2, rows[0].3, rows[0].4, rows[0].5);
    assert_eq!((path_a.as_str(), path_b.as_str()), ("p.rs", "q.rs"));
    assert_eq!((co, a_count, b_count, window), (2, 2, 2, 22), "co=2, endpoints=2, N=22");
    let lift = co as f64 * window as f64 / (a_count as f64 * b_count as f64);
    assert!(lift >= 1.5, "the low-count pair's lift = {lift} clears the floor");
}

/// A 41-file commit contributes zero pairs (mega-commit exclusion); a 40-file commit contributes.
#[test]
fn mega_commit_excluded_but_max_files_commit_contributes() {
    let conn = fresh_conn();
    let repo = repo();
    let mega: Vec<String> = (0..41).map(|i| format!("mega_{i:02}.rs")).collect();
    let maxed: Vec<String> = (0..40).map(|i| format!("max_{i:02}.rs")).collect();
    let mega_paths: Vec<&str> = mega.iter().map(String::as_str).collect();
    let maxed_paths: Vec<&str> = maxed.iter().map(String::as_str).collect();
    let tx = conn.unchecked_transaction().unwrap();
    // Two identical commits per group so any surviving pair clears the support floor.
    add_commit(&conn, &repo, "mega1", 10, &mega_paths);
    add_commit(&conn, &repo, "mega2", 20, &mega_paths);
    add_commit(&conn, &repo, "max1", 30, &maxed_paths);
    add_commit(&conn, &repo, "max2", 40, &maxed_paths);
    // Fillers raise N to 4 so each maxed pair's lift = 2*4/(2*2) = 2.0 clears the floor.
    add_fillers(&conn, &repo, 2, 100);
    tx.commit().unwrap();

    recompute_couplings(&conn, &repo, 1).unwrap();
    let rows = coupling_rows(&conn, &repo);

    assert!(
        !rows.iter().any(|(a, b, ..)| a.starts_with("mega_") || b.starts_with("mega_")),
        "the 41-file commit contributes no pairs"
    );
    let maxed_rows =
        rows.iter().filter(|(a, b, ..)| a.starts_with("max_") && b.starts_with("max_")).count();
    assert_eq!(maxed_rows, 780, "C(40,2) = 780 surviving pairs from the eligible 40-file commit");
    assert!(
        rows.iter().any(|(a, b, ..)| a == "max_00.rs" && b == "max_01.rs"),
        "a specific 40-file pair is present"
    );
}

/// A merge tip (`changed_file_count = 0`, no change rows) contributes nothing and is excluded from
/// the base-rate window count.
#[test]
fn merge_commit_contributes_nothing() {
    let conn = fresh_conn();
    let repo = repo();
    add_commit(&conn, &repo, "real1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "real2", 20, &["a.rs", "b.rs"]);
    add_fillers(&conn, &repo, 2, 100); // N=4 so (a,b) clears the lift floor
    // Merge: no change rows, changed_file_count stays 0 (mirrors read_history_inner numstat
    // parity).
    add_commit(&conn, &repo, "merge", 30, &[]);

    recompute_couplings(&conn, &repo, 1).unwrap();
    let rows = coupling_rows(&conn, &repo);
    assert_eq!(rows.len(), 1, "only the (a,b) pair survives (fillers/merge contribute none)");
    assert_eq!((rows[0].0.as_str(), rows[0].1.as_str()), ("a.rs", "b.rs"));
    // window_commit_count = 4 (2 real + 2 filler); the merge (changed_file_count=0) is not counted.
    // (If the merge counted it would be 5.)
    assert_eq!(rows[0].5, 4, "the merge does not inflate the base-rate window: {rows:?}");
}

/// The window keeps the newest `COUPLING_WINDOW_COMMITS` eligible commits; a pair whose commits
/// fall entirely outside it is evicted, and `window_commit_count` caps at the window size.
#[test]
fn window_evicts_commits_beyond_the_cap() {
    let conn = fresh_conn();
    let repo = repo();
    let window = COUPLING_WINDOW_COMMITS as usize;
    let tx = conn.unchecked_transaction().unwrap();
    // The two OLDEST eligible commits pair {y,z} at ts 1,2.
    add_commit(&conn, &repo, "yz1", 1, &["y.rs", "z.rs"]);
    add_commit(&conn, &repo, "yz2", 2, &["y.rs", "z.rs"]);
    // (window - 2) distinct filler pairs fill the middle of the window (ts 1000..).
    add_fillers(&conn, &repo, window - 2, 1000);
    // {a,b} co-change in the two NEWEST commits (high ts) → high lift, well inside the window.
    add_commit(&conn, &repo, "ab1", 100_000, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "ab2", 100_001, &["a.rs", "b.rs"]);
    tx.commit().unwrap();

    recompute_couplings(&conn, &repo, 1).unwrap();
    let rows = coupling_rows(&conn, &repo);

    let ab = rows.iter().find(|(a, b, ..)| a == "a.rs" && b == "b.rs").expect("(a,b) in window");
    assert_eq!(ab.2, 2, "(a,b) co-changed in the two newest commits");
    assert_eq!(ab.5, window as i64, "window_commit_count caps at COUPLING_WINDOW_COMMITS");
    assert!(
        !rows.iter().any(|(a, b, ..)| a == "y.rs" && b == "z.rs"),
        "the two oldest {{y,z}} commits are evicted (outside the newest {window})"
    );
}

/// Pure git history: a generated partner IS stored (the compute has no `files` dependence), but is
/// NOT surfaced at read (the partner join requires `files.generated = 0`).
#[test]
fn generated_partner_stored_but_filtered_at_read() {
    let conn = fresh_conn();
    let repo = repo();
    add_file(&conn, "real.rs", 0);
    add_file(&conn, "part.rs", 0);
    add_file(&conn, "gen.rs", 1); // generated
    // real co-changes with part AND with gen (support 2 each). real_count = 4, so with N=7 both
    // pairs clear the lift floor: 2*7/(4*2) = 1.75.
    add_commit(&conn, &repo, "rp1", 10, &["real.rs", "part.rs"]);
    add_commit(&conn, &repo, "rp2", 20, &["real.rs", "part.rs"]);
    add_commit(&conn, &repo, "rg1", 30, &["real.rs", "gen.rs"]);
    add_commit(&conn, &repo, "rg2", 40, &["real.rs", "gen.rs"]);
    add_fillers(&conn, &repo, 3, 100);

    recompute_couplings(&conn, &repo, 1).unwrap();

    // Both pairs are STORED — the generated flag has no bearing on the pure-history storage.
    let rows = coupling_rows(&conn, &repo);
    assert!(
        rows.iter().any(|(a, b, ..)| a == "gen.rs" && b == "real.rs"),
        "the generated partner IS stored (pure git history): {rows:?}"
    );
    assert!(
        rows.iter().any(|(a, b, ..)| a == "part.rs" && b == "real.rs"),
        "the non-generated partner is stored: {rows:?}"
    );

    // At READ, the generated partner is filtered; only the non-generated one surfaces.
    let coupled = coupled_files_for_path(&conn, &repo, "real.rs", 10).unwrap();
    assert!(
        coupled.iter().all(|c| c.other_path != "gen.rs"),
        "the generated partner is not surfaced at read: {coupled:?}"
    );
    assert!(
        coupled.iter().any(|c| c.other_path == "part.rs"),
        "the non-generated partner surfaces: {coupled:?}"
    );
}

/// A generated sibling in the same commits adds its OWN pairs but never perturbs a non-generated
/// pair's stored co / endpoint / window counts.
#[test]
fn generated_sibling_does_not_perturb_non_generated_pair() {
    let counts = |with_gen: bool| -> (i64, i64, i64, i64) {
        let conn = fresh_conn();
        let repo = repo();
        let commit_paths: Vec<&str> = if with_gen {
            vec!["real.rs", "other.rs", "gen.rs"]
        } else {
            vec!["real.rs", "other.rs"]
        };
        add_commit(&conn, &repo, "c1", 10, &commit_paths);
        add_commit(&conn, &repo, "c2", 20, &commit_paths);
        add_fillers(&conn, &repo, 2, 100); // N=4 → (real,other) lift = 2.0
        recompute_couplings(&conn, &repo, 1).unwrap();
        let row = coupling_rows(&conn, &repo)
            .into_iter()
            .find(|(a, b, ..)| a == "other.rs" && b == "real.rs")
            .expect("(other.rs, real.rs) row");
        (row.2, row.3, row.4, row.5) // co, path_a_change_count, path_b_change_count, window
    };
    assert_eq!(
        counts(false),
        counts(true),
        "a generated sibling never perturbs the non-generated pair's counts (co / endpoints / N)"
    );
}

/// Codex #566 finding 4: the read query must return each coupled partner EXACTLY ONCE even when the
/// bare repo-generation `files` view holds multiple rows for that path (distinct commit_sha /
/// worktree_id at one generation). The accumulator already dedups (one stored pair per (repo,
/// path_a, path_b)); the risk is purely the partner `files` join cardinality, fixed by
/// pre-aggregating it to one row per path. Without that dedup this returns "partner.rs" twice.
#[test]
fn coupled_partner_deduplicated_across_multiple_files_rows() {
    let conn = fresh_conn();
    let repo = repo();
    add_file(&conn, "hub.rs", 0);
    // The coupled partner has TWO `main.files` rows at the same generation — as the bare
    // repo-generation view (all commits/worktrees, un-deduped) can serve.
    add_file_at_commit(&conn, "partner.rs", "commit-a");
    add_file_at_commit(&conn, "partner.rs", "commit-b");
    // hub <-> partner co-change; fillers raise N so the pair clears the lift floor (lift = 2.0).
    add_commit(&conn, &repo, "c1", 10, &["hub.rs", "partner.rs"]);
    add_commit(&conn, &repo, "c2", 20, &["hub.rs", "partner.rs"]);
    add_fillers(&conn, &repo, 2, 100);

    recompute_couplings(&conn, &repo, 1).unwrap();

    // Exactly one stored pair, and the read returns the partner ONCE despite its two `files` rows.
    assert_eq!(coupling_rows(&conn, &repo).len(), 1, "one stored pair (compute dedups per commit)");
    let coupled = coupled_files_for_path(&conn, &repo, "hub.rs", 10).unwrap();
    let partner_rows = coupled.iter().filter(|c| c.other_path == "partner.rs").count();
    assert_eq!(
        partner_rows, 1,
        "the coupled partner is not duplicated by its two files rows: {coupled:?}"
    );
    assert_eq!(coupled.len(), 1, "no phantom duplicate consumes the caller's limit: {coupled:?}");
}

/// Repo scoping (V040): two repos sharing commit hashes AND paths never cross. A recompute for one
/// leaves the other repo's rows and stamp untouched.
#[test]
fn repo_scoping_isolates_couplings_and_stamps() {
    let conn = fresh_conn();
    // git_commits.repo_id REFERENCES repos(repo_id), so both sibling repos must be registered.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo_a', 'a', 1), \
         ('repo_b', 'b', 2)",
        [],
    )
    .unwrap();
    add_file(&conn, "a.rs", 0);
    add_file(&conn, "b.rs", 0);

    // Both repos reuse hash "c1"/"c2" (the fork collision). repo_a co(a,b)=2 (N=4, lift 2.0);
    // repo_b co(a,b)=3 (N=5, lift = 3*5/(3*3) = 1.667). Fillers keep each above the floor.
    add_commit(&conn, "repo_a", "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, "repo_a", "c2", 20, &["a.rs", "b.rs"]);
    add_fillers(&conn, "repo_a", 2, 100);
    add_commit(&conn, "repo_b", "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, "repo_b", "c2", 20, &["a.rs", "b.rs"]);
    add_commit(&conn, "repo_b", "c3", 25, &["a.rs", "b.rs"]);
    add_fillers(&conn, "repo_b", 2, 200);

    recompute_couplings(&conn, "repo_a", 100).unwrap();
    recompute_couplings(&conn, "repo_b", 200).unwrap();

    assert_eq!(coupling_rows(&conn, "repo_a")[0].2, 2, "repo_a co-count independent");
    assert_eq!(coupling_rows(&conn, "repo_b")[0].2, 3, "repo_b co-count independent");
    // repo_a's read never surfaces repo_b's rows (same path, sibling repo).
    let read_a = coupled_files_for_path(&conn, "repo_a", "a.rs", 10).unwrap();
    assert_eq!(read_a.len(), 1, "repo_a reads exactly its own coupling: {read_a:?}");
    assert_eq!(read_a[0].co_change_count, 2, "read is scoped to repo_a, not repo_b's co=3");

    // Recomputing repo_a again must not disturb repo_b's rows or its stamp.
    let stamp_b_before: String = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = 'repo_b' AND key = 'git_coupling_stamp'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    recompute_couplings(&conn, "repo_a", 300).unwrap();
    assert_eq!(coupling_rows(&conn, "repo_b")[0].2, 3, "repo_b rows untouched by repo_a recompute");
    let stamp_b_after: String = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = 'repo_b' AND key = 'git_coupling_stamp'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stamp_b_before, stamp_b_after, "repo_b stamp untouched");
}

/// Seed the four git-history cursor meta rows (the `is_history_current` freshness snapshot the
/// coupling stamp folds in) so the stamp's history key is non-empty and moves with them.
fn set_history_cursors(
    conn: &Connection,
    repo_id: &str,
    head: &str,
    root: &str,
    shallow: bool,
    complete: bool,
) {
    let bit = |b: bool| if b { "1" } else { "0" };
    rag_rat_db::meta::set_repo_meta(conn, repo_id, "git_history_indexed_head", head).unwrap();
    rag_rat_db::meta::set_repo_meta(conn, repo_id, "git_history_indexed_root", root).unwrap();
    rag_rat_db::meta::set_repo_meta(conn, repo_id, "git_history_indexed_shallow", bit(shallow))
        .unwrap();
    rag_rat_db::meta::set_repo_meta(conn, repo_id, "git_history_indexed_complete", bit(complete))
        .unwrap();
}

fn coupling_computed_at(conn: &Connection, repo_id: &str) -> i64 {
    conn.query_row(
        "SELECT computed_at_ms FROM git_change_couplings WHERE repo_id = ?1 LIMIT 1",
        params![repo_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// Seed a two-commit {a,b} fixture (+ fillers so it clears the lift floor at N=4), ready for the
/// stamp tests to establish cursors and drive ensure_coupling_fresh.
fn stamp_fixture() -> (Connection, String) {
    let conn = fresh_conn();
    let repo = repo();
    add_commit(&conn, &repo, "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c2", 20, &["a.rs", "b.rs"]);
    add_fillers(&conn, &repo, 2, 100);
    (conn, repo)
}

/// The freshness stamp gates recompute: a no-op when the history cursors + params are unchanged,
/// and a recompute on a HEAD move or a params-version drift.
#[test]
fn stamp_gates_recompute_on_head_and_params() {
    let (conn, repo) = stamp_fixture();

    set_history_cursors(&conn, &repo, "h1", "root-key", false, true);
    ensure_coupling_fresh(&conn, 100).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 100, "first read recomputes");

    // Cursors + params unchanged → no-op.
    ensure_coupling_fresh(&conn, 200).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 100, "unchanged stamp is a no-op");

    // HEAD moves (rewrite) → recompute.
    set_history_cursors(&conn, &repo, "h2", "root-key", false, true);
    ensure_coupling_fresh(&conn, 300).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 300, "head move forces recompute");

    // Params version drift (simulated by rewriting the stored stamp) → recompute.
    rag_rat_db::meta::set_repo_meta(&conn, &repo, "git_coupling_stamp", "bogus").unwrap();
    ensure_coupling_fresh(&conn, 400).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 400, "a params mismatch forces recompute");
}

/// Codex #566 finding 1: git_file_changes can be REWRITTEN at the SAME HEAD (unshallow / deepen, or
/// an indexed-subtree re-point). The stamp folds the full `is_history_current` cursor snapshot, so
/// a cursor change with an UNCHANGED head still invalidates and forces a recompute.
#[test]
fn history_rewrite_at_same_head_invalidates_stamp() {
    let (conn, repo) = stamp_fixture();

    // Shallow, incomplete history first.
    set_history_cursors(&conn, &repo, "h1", "root-key", true, false);
    ensure_coupling_fresh(&conn, 100).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 100, "first read recomputes");

    // Deepen at the SAME head: shallow 1→0, complete 0→1. Head unchanged, cursor snapshot moved.
    set_history_cursors(&conn, &repo, "h1", "root-key", false, true);
    ensure_coupling_fresh(&conn, 200).unwrap();
    assert_eq!(
        coupling_computed_at(&conn, &repo),
        200,
        "same-head unshallow/deepen forces recompute"
    );

    // Subtree re-point at the same head (root_key changes) → recompute.
    set_history_cursors(&conn, &repo, "h1", "root-key-2", false, true);
    ensure_coupling_fresh(&conn, 300).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 300, "root-key change forces recompute");
}

/// The stamp is PURE (history:params) — it does NOT fold the files generation, because the stored
/// table is a pure function of git history. So a files-generation bump alone does NOT recompute.
/// (Inverts the earlier composite-stamp behavior.)
#[test]
fn files_generation_change_does_not_invalidate_stamp() {
    let (conn, repo) = stamp_fixture();

    set_history_cursors(&conn, &repo, "h1", "root-key", false, true);
    ensure_coupling_fresh(&conn, 100).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 100, "first read recomputes");

    // Bump the live files generation — the stored table doesn't depend on it, so NO recompute.
    rag_rat_db::meta::set_repo_meta(&conn, &repo, "live_files_generation", "1").unwrap();
    ensure_coupling_fresh(&conn, 200).unwrap();
    assert_eq!(
        coupling_computed_at(&conn, &repo),
        100,
        "a files-generation bump does NOT recompute (stamp is pure git-history)"
    );
}

/// A partner flipped to generated stops surfacing at READ with NO recompute: the stamp is pure
/// git-history (unchanged by a files-view edit) and the generated filter lives on the read path.
#[test]
fn generated_flag_change_does_not_recompute_but_read_reflects_it() {
    let conn = fresh_conn();
    let repo = repo();
    add_file(&conn, "a.rs", 0);
    add_file(&conn, "b.rs", 0);
    add_commit(&conn, &repo, "c1", 10, &["a.rs", "b.rs"]);
    add_commit(&conn, &repo, "c2", 20, &["a.rs", "b.rs"]);
    add_fillers(&conn, &repo, 2, 100); // N=4 → lift 2.0
    set_history_cursors(&conn, &repo, "h1", "root-key", false, true);
    ensure_coupling_fresh(&conn, 100).unwrap();
    assert_eq!(coupling_computed_at(&conn, &repo), 100, "first read recomputes");
    assert_eq!(coupled_files_for_path(&conn, &repo, "a.rs", 10).unwrap().len(), 1, "b surfaces");

    // Flip b to generated — no history / params change.
    conn.execute("UPDATE files SET generated = 1 WHERE path = 'b.rs'", []).unwrap();
    ensure_coupling_fresh(&conn, 200).unwrap();
    assert_eq!(
        coupling_computed_at(&conn, &repo),
        100,
        "the generated flip does NOT recompute (stamp unchanged)"
    );
    assert!(
        coupled_files_for_path(&conn, &repo, "a.rs", 10).unwrap().is_empty(),
        "b no longer surfaces at read (now generated), with no recompute"
    );
}

/// End-to-end: the `impact_surface` report carries the coupling section, it self-heals on the git
/// path, and it is omitted when `include_git` is off; the section participates in
/// `truncated_sections`.
#[test]
fn impact_report_surfaces_and_gates_coupling_section() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/store.rs"), "pub fn target_symbol() {}\n").unwrap();
    fs::write(root.join("src/helper.rs"), "pub fn helper_fn() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Synthetic history on the indexed DB: store.rs + helper.rs co-change twice; two filler commits
    // (non-indexed paths) raise N = 4 so (store,helper)'s lift = 2.0 clears the floor.
    let conn = db.storage.connection();
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    add_commit(conn, &repo_id, "c1", 10, &["src/store.rs", "src/helper.rs"]);
    add_commit(conn, &repo_id, "c2", 20, &["src/store.rs", "src/helper.rs"]);
    add_commit(conn, &repo_id, "c3", 30, &["ext_a.rs", "ext_b.rs"]);
    add_commit(conn, &repo_id, "c4", 40, &["ext_c.rs", "ext_d.rs"]);

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("target_symbol".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol");

    // include_git on (default): the coupling section surfaces helper.rs (lazy recompute on read).
    let report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let coupled = &report.files_co_changed_with_symbol_path;
    assert_eq!(coupled.len(), 1, "one coupled file: {coupled:?}");
    assert!(coupled[0].path.ends_with("src/helper.rs"), "coupled to helper.rs: {coupled:?}");
    assert_eq!(coupled[0].reason, "file_co_changed_in_recent_commits");

    // include_git off: the section is empty (and no recompute is triggered on that path).
    let git_off =
        crate::query::impact::ImpactSurfaceOptions { include_git: false, ..Default::default() };
    let report_off = db.impact_surface_report_for_selected_symbol(&symbol, 50, &git_off).unwrap();
    assert!(
        report_off.files_co_changed_with_symbol_path.is_empty(),
        "include_git off omits the coupling section"
    );

    // At limit=1 the (exactly-full) section is flagged in truncated_sections (#49 no silent caps).
    let capped =
        db.impact_surface_report_for_selected_symbol(&symbol, 1, &Default::default()).unwrap();
    assert!(
        capped
            .completeness_and_caveats
            .truncated_sections
            .iter()
            .any(|s| s == "files_co_changed_with_symbol_path"),
        "coupling section participates in truncated_sections: {:?}",
        capped.completeness_and_caveats.truncated_sections
    );

    let _ = fs::remove_dir_all(root);
}

/// Codex #566 finding 2: a co-changed file that is dirty-since-index must count toward
/// `stale_files`, else the caller trusts a surface whose coupling lane has moved under it. The
/// coupled file is NOT a caller / callee / test / doc / text-match of the symbol, so it reaches the
/// stale scan ONLY through the `files_co_changed_with_symbol_path` lane.
#[test]
fn dirty_co_changed_file_counts_toward_stale_files() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/store.rs"), "pub fn target_symbol() {}\n").unwrap();
    fs::write(root.join("src/helper.rs"), "pub fn helper_fn() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let conn = db.storage.connection();
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    add_commit(conn, &repo_id, "c1", 10, &["src/store.rs", "src/helper.rs"]);
    add_commit(conn, &repo_id, "c2", 20, &["src/store.rs", "src/helper.rs"]);
    add_commit(conn, &repo_id, "c3", 30, &["ext_a.rs", "ext_b.rs"]);
    add_commit(conn, &repo_id, "c4", 40, &["ext_c.rs", "ext_d.rs"]);

    // Dirty the coupled file on disk AFTER indexing — its content hash now differs from the index.
    fs::write(root.join("src/helper.rs"), "pub fn helper_fn() { let _changed = 1; }\n").unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("target_symbol".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();

    assert!(
        report.files_co_changed_with_symbol_path.iter().any(|i| i.path.ends_with("src/helper.rs")),
        "helper.rs surfaces in the coupling lane"
    );
    assert!(
        report.completeness_and_caveats.stale_files >= 1,
        "the dirty co-changed file is counted in stale_files: {}",
        report.completeness_and_caveats.stale_files
    );

    let _ = fs::remove_dir_all(root);
}
