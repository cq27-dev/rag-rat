use std::path::PathBuf;
use std::time::Duration;

use rag_rat_base::config::Config;
use rag_rat_base::{hash, time};
use rag_rat_sync::AuthPolicy;
use rusqlite::Connection;

use super::{
    AdvertisementIdentity, DISCOVERY_ADVERTISEMENT, DeviceSyncOutcome, PULL_PEER_MEMO_PREFIX,
    PerPeerSessionLimiter, PersistedAdvertisement, RESIDENT_NUDGE, RefusedPublication,
    account_is_public_kb, can_host, can_sync, device_sync_run, discovery_fetch, foreign_pull_hosts,
    foreign_pull_targets, nudge_resident_host, ordered_pull_peers, peer_identity,
    prepare_advertisement, pull_account_via_peers, pull_foreign_accounts, read_advertisement,
    refused_publication_is_due, retry_is_due, write_advertisement,
};

fn schema_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn
}

fn own_stream(conn: &Connection, mode: rag_rat_oplog::AccessMode) {
    use rusqlite::{Transaction, TransactionBehavior};
    rag_rat_oplog::local_account(conn, 1_000).unwrap();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(&tx, "repo-a", mode, 1_000).unwrap();
    tx.commit().unwrap();
}

#[test]
fn account_is_public_kb_only_for_a_published_fully_public_account() {
    // A minted-but-empty account (no owned stream) is NOT served public — else a fresh node
    // would expose itself vacuously.
    let empty = schema_conn();
    let empty_account = rag_rat_oplog::local_account(&empty, 1_000).unwrap();
    assert!(!account_is_public_kb(&empty, empty_account).unwrap());

    // A private stream is not fully public.
    let private = schema_conn();
    own_stream(&private, rag_rat_oplog::AccessMode::Private);
    let private_account = rag_rat_oplog::local_account(&private, 1_000).unwrap();
    assert!(!account_is_public_kb(&private, private_account).unwrap());

    // A published account (public stream, fully public) IS served public.
    let public = schema_conn();
    own_stream(&public, rag_rat_oplog::AccessMode::PublicRead);
    let public_account = rag_rat_oplog::local_account(&public, 1_000).unwrap();
    assert!(account_is_public_kb(&public, public_account).unwrap());
}

/// A granted CONTRIBUTOR owns no stream (#1164), so the owns-a-stream test alone would serve it
/// `Closed` and nothing could pull its account log — breaking the direction contribution needs,
/// since content is offered by AUTHOR and the owner collects a contributor's memories by
/// syncing the CONTRIBUTOR's account.
///
/// The evidence is deliberately narrow: a live grant on a stream this store is CONFIGURED to
/// contribute to. A grant alone is not enough — a stale one this store never uses must not
/// expose it — and the narrow question is also the cheap one, since it resolves to indexed
/// point lookups instead of an unindexed grantee scan on every pre-auth connection.
#[test]
fn only_a_configured_contribution_grant_makes_a_stream_less_account_servable() {
    use rusqlite::{Transaction, TransactionBehavior};

    // A real owner with a real PublicRead stream for `repo-a`, granting the contributor.
    let owner = schema_conn();
    let owner_account = rag_rat_oplog::local_account(&owner, 1_000).unwrap();
    let public_stream = {
        let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
        let s = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-a",
            rag_rat_oplog::AccessMode::PublicRead,
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();
        s
    };

    let contributor = schema_conn();
    let account = rag_rat_oplog::local_account(&contributor, 1_000).unwrap();
    contributor
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a','a',0)",
            [],
        )
        .unwrap();
    // Owns nothing and contributes nowhere: refused, as a vacuously-public account must be.
    assert!(!account_is_public_kb(&contributor, account).unwrap());

    {
        let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::author_stream_grant_in_tx(
            &tx,
            public_stream,
            account,
            rag_rat_oplog::GrantRole::Writer,
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    for entry in rag_rat_oplog::account_entries_for_sync(&owner, owner_account).unwrap() {
        rag_rat_oplog::account_ingest(&contributor, &entry.signed_bytes, 1_000).unwrap();
    }

    // The grant is held and verifiable — but this store does not contribute anywhere, so it is
    // still not servable. A grant it never uses is not a reason to expose it.
    assert!(
        !account_is_public_kb(&contributor, account).unwrap(),
        "an unused grant does not expose the account",
    );

    // Configure the contribution, and NOW it is servable.
    rag_rat_db::meta::set_repo_meta(
        &contributor,
        "repo-a",
        "memory_contribution_owner",
        &rag_rat_base::hash::hex_lower(&owner_account.to_bytes()),
    )
    .unwrap();
    assert!(
        account_is_public_kb(&contributor, account).unwrap(),
        "a live grant on the stream this store contributes to makes it servable",
    );

    // Revoking closes the door again.
    contributor.execute("UPDATE account_stream_grants SET closed_at = 2000", []).unwrap();
    assert!(
        !account_is_public_kb(&contributor, account).unwrap(),
        "a revoked grant no longer makes the account servable",
    );
}

#[test]
fn nudge_is_durable_when_no_resident_host_is_live() {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    assert!(!nudge_resident_host(&conn).unwrap());
    let nudge: String = conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [RESIDENT_NUDGE], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(nudge.parse::<i64>().is_ok(), "the hook request survives until a host observes it");
}

#[test]
fn fallback_does_not_mint_or_bind_without_an_account() {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    let config = Config::minimal_for_database(
        PathBuf::from("/nonexistent/sync.sqlite"),
        PathBuf::from("/nonexistent"),
    );

    assert_eq!(device_sync_run(&config, &conn).unwrap(), DeviceSyncOutcome::Disabled);
}

/// A pass that will not fetch reads nothing to fetch with. The opener costs an account read
/// plus a device load that re-derives and validates this device's keys, and with no
/// discovery service `discover_peers` returns before it ever calls the opening closure.
///
/// Pinned by dropping BOTH tables the opener reads, so either one fails loudly wherever it is
/// read from: with discovery off the call still succeeds, and turning discovery back on
/// surfaces the failure, so the silence is the gate and not a store with nothing to read.
/// Leaving `oplog_device_identity` present but empty would not pin the device load — it
/// answers `None` on an empty table, so a read hoisted above the gate would stay silent.
#[test]
fn discovery_off_reads_nothing_the_pass_cannot_use() {
    let poisoned = schema_conn();
    poisoned.execute("DROP TABLE oplog_local_account", []).unwrap();
    poisoned.execute("DROP TABLE oplog_device_identity", []).unwrap();
    let mut config = Config::minimal_for_database(
        PathBuf::from("/nonexistent/sync.sqlite"),
        PathBuf::from("/nonexistent"),
    );

    config.sync.discovery = false;
    assert!(
        discovery_fetch(&config, &poisoned, "https://relay.one").unwrap().is_none(),
        "no service to fetch from, so nothing is read to open with"
    );

    config.sync.discovery = true;
    assert!(
        discovery_fetch(&config, &poisoned, "https://relay.one").is_err(),
        "either read failing is enough — the reads do happen once there is a fetch to open for"
    );

    // A healthy store loads exactly one opener, carried for the whole pass.
    let conn = schema_conn();
    rag_rat_oplog::local_account(&conn, 1_000).unwrap();
    let fetch = discovery_fetch(&config, &conn, "https://relay.one")
        .unwrap()
        .expect("an account plus a valid service node id is a fetch");
    assert!(fetch.opener.is_some(), "a founder is a device that can open its own account's tag");
}

#[test]
fn only_writers_can_host_while_read_only_devices_can_dial() {
    assert!(can_host(Some(rag_rat_sync::PeerCapability::ReadWrite)));
    assert!(!can_host(Some(rag_rat_sync::PeerCapability::ReadOnly)));
    assert!(can_sync(Some(rag_rat_sync::PeerCapability::ReadOnly)));
}

const ADVERTISED_NODE: [u8; 32] = [0x11; 32];
const ADVERTISED_SERVICE: [u8; 32] = [0x22; 32];
const ADVERTISED_RELAY: &str = "https://relay.one";

/// Persist an advertisement record `prepare_advertisement` matches, so it is reused verbatim
/// rather than resealed — the shape in which an envelope sealed under an older byte ceiling or
/// wrap layout survives.
fn persist_advertised_envelope(database: &std::path::Path, envelope: Vec<u8>) {
    let storage = super::IndexConnection::open(database).unwrap();
    let conn = storage.connection();
    rag_rat_db::schema::apply(conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    rag_rat_oplog::local_account(conn, 1_000).unwrap();
    let secret = rag_rat_sync::discovery::discovery_secret(conn).unwrap().unwrap();
    write_advertisement(conn, &PersistedAdvertisement {
        identity: AdvertisementIdentity {
            tag: rag_rat_sync::discovery::account_tag(&secret),
            node: ADVERTISED_NODE,
            service: ADVERTISED_SERVICE,
            relay: ADVERTISED_RELAY.to_owned(),
        },
        roster_stamp: rag_rat_oplog::discovery::roster_stamp(conn).unwrap(),
        envelope: Some(envelope),
        published_at_ms: None,
        ttl_seconds: 600,
    })
    .unwrap();
}

/// One advertisement pass, with whatever it warned about.
///
/// Every pass goes through the capturing subscriber, and that is not decoration: `tracing`
/// caches a callsite's interest PROCESS-wide on its first use, so a single pass made with no
/// subscriber installed caches "never" for the size warning and every later capture in the same
/// test binary comes back empty. Routing all of them through one seam takes that out of the
/// hands of test ordering.
fn prepare_advertised(database: &std::path::Path) -> (Option<super::Publication>, String) {
    let mut prepared = None;
    let logged = captured_warnings(|| {
        prepared = prepare_advertisement(
            database,
            super::AdvertisementEndpoint {
                node: &ADVERTISED_NODE,
                service: &ADVERTISED_SERVICE,
                relay: ADVERTISED_RELAY,
            },
            2_000,
            600,
        )
        .unwrap();
    });
    (prepared, logged)
}

/// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
/// assert on the `tracing` events a pass emitted — and on how many times.
#[derive(Clone)]
struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `body` with warnings captured, returning what it logged. The subscriber is thread-local
/// (`with_default`), so parallel tests do not see each other's output.
fn captured_warnings(body: impl FnOnce()) -> String {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let logged = buffer.lock().unwrap().clone();
    String::from_utf8(logged).expect("formatted log lines are UTF-8")
}

/// A persisted envelope is judged against the byte ceiling too, not just a fresh seal.
///
/// The record is reused verbatim whenever the tag, endpoint, service, relay, and roster stamp
/// all still match, and none of those move when the ceiling or the wrap layout does — so an
/// envelope sealed under the old law would sail past the seal-time verdict, cost a dial, and be
/// reported at the publish boundary as a byte count with no roster in it. The under-size half
/// is what keeps this from passing for the wrong reason: the same record one byte smaller must
/// still be advertised.
#[test]
fn a_persisted_envelope_over_the_announcement_ceiling_is_not_advertised() {
    let dir = tempfile::TempDir::new().unwrap();
    let database = dir.path().join("index.sqlite");
    let ceiling = rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES;

    persist_advertised_envelope(&database, vec![0; ceiling + 1]);
    assert!(
        prepare_advertised(&database).0.is_none(),
        "an over-size persisted envelope must not be handed to the publish path"
    );

    persist_advertised_envelope(&database, vec![0; ceiling]);
    assert!(
        prepare_advertised(&database).0.is_some(),
        "the ceiling itself fits, so the same record one byte smaller is still advertised"
    );
}

/// The over-size verdict is reported ONCE, however long the roster stays too large.
///
/// `prepare_advertisement` runs on the one-second `ADVERTISEMENT_REFRESH` tick, and nothing the
/// record `matches` on moves when the ceiling does — so a verdict re-derived on every pass and
/// not latched is a warning per second, indefinitely, burying every other line the operator
/// needs. Latching the record to `envelope: None` is what holds it to one, and it is also what
/// keeps the dial suppressed: the publish rate limiter never sees this condition, because the
/// pass returns before `exchange`.
#[test]
fn an_over_size_roster_is_reported_once_and_not_on_every_pass() {
    let dir = tempfile::TempDir::new().unwrap();
    let database = dir.path().join("index.sqlite");
    let ceiling = rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES;
    persist_advertised_envelope(&database, vec![0; ceiling + 1]);

    let reported: usize = (0..10)
        .map(|_| {
            let (publication, logged) = prepare_advertised(&database);
            assert!(publication.is_none(), "an over-size roster is never advertised");
            logged.matches("roster is too large to seal into one announcement").count()
        })
        .sum();
    assert_eq!(reported, 1, "ten passes must report the over-size roster once, not once each");

    let storage = super::IndexConnection::open(&database).unwrap();
    assert_eq!(
        read_advertisement(storage.connection()).unwrap().unwrap().envelope,
        None,
        "the record is latched, so there is no envelope left for a later pass to judge"
    );
}

#[test]
fn persisted_advertisement_matches_only_the_same_endpoint_service_relay_tag_and_roster() {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    let identity = |tag: [u8; 32], node: [u8; 32], service: [u8; 32], relay: &str| {
        AdvertisementIdentity { tag, node, service, relay: relay.to_owned() }
    };
    let record = PersistedAdvertisement {
        identity: identity([1; 32], [2; 32], [3; 32], "https://relay.one"),
        roster_stamp: Some([3; 32]),
        envelope: Some(vec![4, 5, 6]),
        published_at_ms: Some(7),
        ttl_seconds: 60,
    };

    write_advertisement(&conn, &record).unwrap();
    let restored = read_advertisement(&conn).unwrap().expect("the record was persisted");
    assert_eq!(restored, record, "the exact sealed envelope survives a host restart");
    let stamp = Some(&[3u8; 32]);
    assert!(restored.matches(&identity([1; 32], [2; 32], [3; 32], "https://relay.one"), stamp));
    assert!(!restored.matches(&identity([9; 32], [2; 32], [3; 32], "https://relay.one"), stamp));
    assert!(!restored.matches(&identity([1; 32], [9; 32], [3; 32], "https://relay.one"), stamp));
    assert!(!restored.matches(&identity([1; 32], [2; 32], [9; 32], "https://relay.one"), stamp));
    assert!(!restored.matches(&identity([1; 32], [2; 32], [3; 32], "https://relay.two"), stamp));
    assert!(
        !restored
            .matches(&identity([1; 32], [2; 32], [3; 32], "https://relay.one"), Some(&[9; 32]),)
    );
    let stored = rag_rat_db::meta::read_meta(&conn, DISCOVERY_ADVERTISEMENT)
        .unwrap()
        .expect("the controller stores its state in index_meta");
    // The identity is flattened into the stored record, so every host keeps reading the flat
    // keys it has always written.
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    let mut keys: Vec<&str> = stored.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, [
        "envelope",
        "node",
        "published_at_ms",
        "relay",
        "roster_stamp",
        "service",
        "tag",
        "ttl_seconds"
    ]);
}

/// The cadence window is half-open and a stamp from the future is outside it, so the sync and
/// heartbeat cadences both come due at the interval boundary and after a backwards clock step.
#[test]
fn within_window_is_half_open_and_rejects_a_future_stamp() {
    assert!(super::within_window(1_000, 1_000, 10));
    assert!(super::within_window(1_000, 1_009, 10));
    assert!(!super::within_window(1_000, 1_010, 10), "the boundary itself is due");
    assert!(!super::within_window(1_001, 1_000, 10), "a stamp ahead of now is due");
}

#[test]
fn a_restart_reuses_matching_liveness_until_renewal_is_due() {
    let record = PersistedAdvertisement {
        identity: AdvertisementIdentity {
            tag: [1; 32],
            node: [2; 32],
            service: [3; 32],
            relay: "https://relay.one".to_owned(),
        },
        roster_stamp: Some([3; 32]),
        envelope: Some(vec![4, 5, 6]),
        published_at_ms: Some(1_000),
        ttl_seconds: 60,
    };
    assert!(
        record.live(30_999),
        "a restarted host keeps using its still-live byte-identical envelope"
    );
    assert!(!record.live(31_000), "the first renewal is still due at the established half-life");
}

#[test]
fn a_refused_advertisement_uses_the_ttl_derived_retry_delay() {
    let retry_after = rag_rat_sync::discovery::retry_after_refusal(60);
    assert_eq!(retry_after, Duration::from_millis(7_500));

    let attempted_at = tokio::time::Instant::now();
    let refusal = RefusedPublication { envelope: vec![1, 2, 3], attempted_at };
    assert!(
        !refused_publication_is_due(
            Some(&refusal),
            &[1, 2, 3],
            attempted_at + retry_after - Duration::from_millis(1),
            retry_after,
        ),
        "the fine controller timer must not turn a refusal into a request per second"
    );
    assert!(refused_publication_is_due(
        Some(&refusal),
        &[1, 2, 3],
        attempted_at + retry_after,
        retry_after,
    ));
    assert!(
        refused_publication_is_due(Some(&refusal), &[9], attempted_at, retry_after),
        "a roster reseal is a new envelope and should not wait behind an old refusal"
    );
}

#[test]
fn a_preparation_error_uses_the_ttl_derived_retry_delay() {
    let retry_after = rag_rat_sync::discovery::retry_after_refusal(60);
    let attempted_at = tokio::time::Instant::now();
    assert!(!retry_is_due(
        Some(attempted_at),
        attempted_at + retry_after - Duration::from_millis(1),
        retry_after,
    ));
    assert!(retry_is_due(Some(attempted_at), attempted_at + retry_after, retry_after,));
}

const PEER_A: [u8; 32] = [0xa; 32];
const PEER_B: [u8; 32] = [0xb; 32];

#[test]
fn per_peer_limiter_admits_up_to_max_then_denies() {
    let limiter = PerPeerSessionLimiter::default();
    let slots: Vec<_> = (0..3).map(|_| limiter.try_acquire(PEER_A, 3)).collect();
    assert!(slots.iter().all(Option::is_some), "the first `max` slots are admitted");
    assert!(limiter.try_acquire(PEER_A, 3).is_none(), "the slot past `max` is denied");
    // A denied acquire must not mutate the map (no phantom entry, count still exactly `max`).
    let map = limiter.in_flight.lock().unwrap();
    assert_eq!(map.len(), 1, "denial adds no entry");
    assert_eq!(map.get(&PEER_A).copied(), Some(3), "denial does not inflate the count");
}

#[test]
fn per_peer_limiter_releases_and_prunes_on_slot_drop() {
    let limiter = PerPeerSessionLimiter::default();
    let a = limiter.try_acquire(PEER_A, 1).expect("first slot");
    assert!(limiter.try_acquire(PEER_A, 1).is_none(), "at capacity");
    drop(a);
    assert!(
        !limiter.in_flight.lock().unwrap().contains_key(&PEER_A),
        "the entry is removed at zero — the map holds only live sessions",
    );
    assert!(limiter.try_acquire(PEER_A, 1).is_some(), "the released slot is available again");
}

#[test]
fn per_peer_limiter_is_independent_per_peer() {
    let limiter = PerPeerSessionLimiter::default();
    let _a = limiter.try_acquire(PEER_A, 1).expect("A slot");
    assert!(limiter.try_acquire(PEER_A, 1).is_none(), "A is at capacity");
    assert!(limiter.try_acquire(PEER_B, 1).is_some(), "B has its own independent cap");
}

#[test]
fn per_peer_slot_releases_even_when_dropped_during_unwind() {
    let limiter = PerPeerSessionLimiter::default();
    let taken = limiter.clone();
    // A panic while the slot is in scope must still run its Drop (release), not leak the count.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _slot = taken.try_acquire(PEER_A, 1).expect("slot");
        panic!("session task panicked mid-flight");
    }));
    assert!(
        limiter.try_acquire(PEER_A, 1).is_some(),
        "the slot was released on the panic unwind, not leaked",
    );
}

/// Real node ids, so the spelling comparison below exercises the byte path, not the fallback.
const NODE_A: &str = "3f73ab97d1b322be91b77890f1ac48f142f6e91daad428dd5fc73490a44b5b78";
const NODE_B: &str = "4fe0090702f76cfa4b74beb42e4c880d385dcc5699d909f4fd0a2b1190be3c08";
const CONFIGURED_RELAY: &str = "https://relay.configured";
const LOCATOR_RELAY: &str = "https://relay.locator";

/// A relay is ALWAYS configured — the shipped default is never empty — so a locator's relay
/// that only filled a gap would never reach the host it names.
#[test]
fn a_locator_relay_reaches_its_own_peer_though_a_relay_is_always_configured() {
    let routes = super::foreign_pull_peers(&[], CONFIGURED_RELAY, &[(
        NODE_A.to_string(),
        Some(LOCATOR_RELAY.to_string()),
    )]);
    assert_eq!(routes, [(NODE_A.to_string(), LOCATOR_RELAY.to_string())]);
}

/// Only an exact duplicate — the same node, under any spelling, through the same relay —
/// collapses. A locator route reaching a configured node through ANOTHER relay is an extra way
/// in, not a conflict, and configured routes still come first.
#[test]
fn only_an_exact_duplicate_route_collapses() {
    let routes = super::foreign_pull_peers(&[NODE_A.to_string()], CONFIGURED_RELAY, &[
        (format!("  {NODE_A}  "), None),
        (NODE_A.to_string(), Some(LOCATOR_RELAY.to_string())),
        (NODE_B.to_string(), None),
    ]);
    assert_eq!(routes, [
        (NODE_A.to_string(), CONFIGURED_RELAY.to_string()),
        (NODE_A.to_string(), LOCATOR_RELAY.to_string()),
        (NODE_B.to_string(), CONFIGURED_RELAY.to_string()),
    ]);
}

/// Two routes naming one node through different relays are both dialed, in order. Keeping only
/// the first would let a dead or hostile relay recorded for a node shadow the relay that
/// reaches it — and the first is whichever repository happened to sort first.
#[test]
fn a_dead_relay_for_a_node_cannot_shadow_a_live_one() {
    const DEAD_RELAY: &str = "https://relay.dead";
    let routes = super::foreign_pull_peers(&[], CONFIGURED_RELAY, &[
        (NODE_A.to_string(), Some(DEAD_RELAY.to_string())),
        (NODE_A.to_string(), Some(LOCATOR_RELAY.to_string())),
    ]);
    let resolved = super::route_addrs(&super::distinct_peers(&routes), &routes, CONFIGURED_RELAY);
    let addrs: Vec<_> = resolved.into_iter().map(|(_, addr)| addr.unwrap()).collect();
    assert_eq!(
        addrs,
        [
            rag_rat_sync::peer_addr(NODE_A, DEAD_RELAY).unwrap(),
            rag_rat_sync::peer_addr(NODE_A, LOCATOR_RELAY).unwrap(),
        ],
        "the live relay is still tried after the dead one",
    );
}

/// The memo keeps whichever spelling of a node id answered, which need not match the spelling
/// the route recorded — so the relay is found by node identity, or a locator peer would
/// silently fall back to the configured relay and never reach a host homed elsewhere.
#[test]
fn a_peer_dials_through_its_routes_relay_whichever_spelling_names_it() {
    let routes = [(NODE_A.to_string(), LOCATOR_RELAY.to_string())];
    let resolved = super::route_addrs(
        &[format!("  {NODE_A}  "), NODE_B.to_string()],
        &routes,
        CONFIGURED_RELAY,
    );
    assert_eq!(
        resolved[0].1.as_ref().unwrap(),
        &rag_rat_sync::peer_addr(NODE_A, LOCATOR_RELAY).unwrap(),
        "a routed peer dials through its route's relay",
    );
    assert_eq!(
        resolved[1].1.as_ref().unwrap(),
        &rag_rat_sync::peer_addr(NODE_B, CONFIGURED_RELAY).unwrap(),
        "an unrouted peer dials through the fallback",
    );
}

#[test]
fn foreign_pull_targets_cover_both_directions_and_exclude_the_local_account() {
    use rusqlite::{Transaction, TransactionBehavior};

    let store = schema_conn();
    let local = rag_rat_oplog::local_account(&store, 1_000).unwrap();
    assert!(foreign_pull_targets(&store, local).unwrap().is_empty());

    // Contributor direction: a configured contribution owner becomes a pull target.
    store
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a','a',0)",
            [],
        )
        .unwrap();
    let owner = rag_rat_oplog::AccountId::from_bytes([0x77; 32]);
    rag_rat_db::meta::set_repo_meta(
        &store,
        "repo-a",
        "memory_contribution_owner",
        &hash::hex_lower(&owner.to_bytes()),
    )
    .unwrap();
    assert_eq!(foreign_pull_targets(&store, local).unwrap(), vec![owner]);

    // Owner direction: an effective writer grantee of THIS account becomes a pull target too.
    let grantee = rag_rat_oplog::AccountId::from_bytes([0x22; 32]);
    {
        let tx = Transaction::new_unchecked(&store, TransactionBehavior::Immediate).unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-a",
            rag_rat_oplog::AccessMode::PublicRead,
            1_000,
        )
        .unwrap();
        rag_rat_oplog::author_stream_grant_in_tx(
            &tx,
            stream,
            grantee,
            rag_rat_oplog::GrantRole::Writer,
            1_000,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let targets = foreign_pull_targets(&store, local).unwrap();
    assert!(targets.contains(&owner) && targets.contains(&grantee));
    assert_eq!(targets.len(), 2);

    // A repo configured (nonsensically) to contribute to the LOCAL account never self-pulls.
    store
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b','b',0)",
            [],
        )
        .unwrap();
    rag_rat_db::meta::set_repo_meta(
        &store,
        "repo-b",
        "memory_contribution_owner",
        &hash::hex_lower(&local.to_bytes()),
    )
    .unwrap();
    assert_eq!(foreign_pull_targets(&store, local).unwrap().len(), 2);
}

/// A read-only SUBSCRIBER (#1156) is pull-only: it authors nothing and is never served, so
/// nothing but this enumeration would ever fetch the owner's account — a subscription missing
/// here is configured but permanently empty.
#[test]
fn a_subscribed_owner_is_a_foreign_pull_target() {
    let store = schema_conn();
    let local = rag_rat_oplog::local_account(&store, 1_000).unwrap();
    store
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a','a',0)",
            [],
        )
        .unwrap();
    let owner = rag_rat_oplog::AccountId::from_bytes([0x55; 32]);
    rag_rat_db::meta::set_repo_meta(
        &store,
        "repo-a",
        "memory_subscription_owner",
        &hash::hex_lower(&owner.to_bytes()),
    )
    .unwrap();
    assert_eq!(foreign_pull_targets(&store, local).unwrap(), vec![owner]);
}

#[test]
fn a_memoized_peer_orders_the_configured_set_and_a_decommissioned_one_is_cleared() {
    let conn = schema_conn();
    let key = format!("{PULL_PEER_MEMO_PREFIX}aa");
    let peers = vec!["node-a".to_string(), "node-b".to_string()];

    // No memo: configured order, nothing stored.
    assert_eq!(ordered_pull_peers(&conn, &key, &peers).unwrap(), peers);

    // A memoized answerer moves to the front without duplicating.
    rag_rat_db::meta::set_meta(&conn, &key, "node-b").unwrap();
    assert_eq!(ordered_pull_peers(&conn, &key, &peers).unwrap(), vec![
        "node-b".to_string(),
        "node-a".to_string()
    ]);

    // Decommissioned: the memoized host left [sync] server_peers, so it is neither dialed
    // nor kept — the memo orders the configured set, never extends it.
    let remaining = vec!["node-a".to_string()];
    assert_eq!(ordered_pull_peers(&conn, &key, &remaining).unwrap(), remaining);
    assert_eq!(rag_rat_db::meta::read_meta(&conn, &key).unwrap(), None, "stale memo cleared");
}

/// One foreign account's local fault must not cost the accounts after it their pull (#1285).
/// Both targets hold a memo naming a peer that left the configured set, so each turn clears its
/// memo before dialing anything; the first account's clear is made to fail.
#[tokio::test]
async fn one_accounts_local_fault_does_not_stop_the_next_accounts_pull() {
    let conn = schema_conn();
    let local = rag_rat_oplog::local_account(&conn, 1_000).unwrap();
    let first = rag_rat_oplog::AccountId::from_bytes([0x11; 32]);
    let second = rag_rat_oplog::AccountId::from_bytes([0x77; 32]);
    let memo_key = |owner: rag_rat_oplog::AccountId| {
        format!("{PULL_PEER_MEMO_PREFIX}{}", hash::hex_lower(&owner.to_bytes()))
    };
    for (repo, owner) in [("repo-a", first), ("repo-b", second)] {
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [repo],
        )
        .unwrap();
        rag_rat_db::meta::set_repo_meta(
            &conn,
            repo,
            "memory_contribution_owner",
            &hash::hex_lower(&owner.to_bytes()),
        )
        .unwrap();
        rag_rat_db::meta::set_meta(&conn, &memo_key(owner), "decommissioned-peer").unwrap();
    }
    assert_eq!(foreign_pull_targets(&conn, local).unwrap(), vec![first, second]);
    conn.execute_batch(&format!(
        "CREATE TEMP TRIGGER injected_fault BEFORE DELETE ON index_meta
             WHEN OLD.key = '{}' BEGIN SELECT RAISE(ABORT, 'injected fault'); END;",
        memo_key(first)
    ))
    .unwrap();
    let mut config = Config::minimal_for_database(
        PathBuf::from("/nonexistent/sync.sqlite"),
        PathBuf::from("/nonexistent"),
    );
    config.sync.server_peers = vec!["not-a-node-id".to_string()];
    let (endpoint, _other) = loopback_endpoints().await;

    pull_foreign_accounts(&config, &conn, &endpoint, local).await.unwrap();
    let memo = |owner| rag_rat_db::meta::read_meta(&conn, &memo_key(owner)).unwrap();
    assert_eq!(memo(first).as_deref(), Some("decommissioned-peer"), "the first turn failed");
    assert_eq!(memo(second), None, "the second account's turn still ran");
}

#[test]
fn only_pull_memo_keys_mark_a_peer_as_a_foreign_host() {
    let conn = schema_conn();
    assert!(foreign_pull_hosts(&conn).unwrap().is_empty());
    rag_rat_db::meta::set_meta(&conn, &format!("{PULL_PEER_MEMO_PREFIX}aa"), "node-f").unwrap();
    // Unrelated meta keys — including other sync keys — never mark a device-sync peer.
    rag_rat_db::meta::set_meta(&conn, RESIDENT_NUDGE, "5").unwrap();
    rag_rat_db::meta::set_meta(&conn, "sync_pull_peerless", "node-x").unwrap();
    assert_eq!(foreign_pull_hosts(&conn).unwrap(), vec![peer_identity("node-f")]);
}

/// Standard base32, no padding — an alternate spelling `parse_node_id` accepts for the node a
/// 64-char lowercase hex string names.
fn base32_nopad(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(52);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

#[test]
fn peer_comparisons_recognize_alternate_spellings_of_one_node() {
    let conn = schema_conn();
    let bytes = rag_rat_sync::node_id_from_secret([7; 32]);
    let hex = rag_rat_sync::node_id_to_string(&bytes).unwrap();
    let base32 = base32_nopad(&bytes);
    assert_ne!(hex, base32);

    // The memo holds the spelling that answered while the config spells the same node
    // differently: the memo still counts as configured (kept and fronted), and the
    // equivalent configured spelling is not dialed a second time.
    let key = format!("{PULL_PEER_MEMO_PREFIX}bb");
    rag_rat_db::meta::set_meta(&conn, &key, &base32).unwrap();
    let peers = vec![hex.clone(), "node-a".to_string()];
    assert_eq!(ordered_pull_peers(&conn, &key, &peers).unwrap(), vec![
        base32.clone(),
        "node-a".to_string()
    ]);
    assert!(rag_rat_db::meta::read_meta(&conn, &key).unwrap().is_some(), "memo kept");

    // Device sync excludes a memoized foreign host by parsed identity — the bytes a
    // resolved `EndpointAddr` carries — never by spelling.
    assert!(foreign_pull_hosts(&conn).unwrap().contains(&Ok(bytes)));
    assert_eq!(peer_identity(&hex), Ok(bytes));
}

/// A relay-free endpoint pair for exercising the pull helper over a real wire.
async fn loopback_endpoints() -> (iroh::Endpoint, iroh::Endpoint) {
    let bind = |seed: [u8; 32]| async move {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .alpns(vec![rag_rat_sync::SYNC_ALPN.to_vec(), rag_rat_sync::CONTENT_SYNC_ALPN.to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(iroh::SecretKey::from_bytes(&seed))
            .bind()
            .await
            .unwrap()
    };
    (bind([0x31; 32]).await, bind([0x32; 32]).await)
}

/// A directly dialable address for a loopback endpoint (its 127.0.0.1 socket).
fn direct_addr(endpoint: &iroh::Endpoint) -> iroh::EndpointAddr {
    let port = endpoint
        .addr()
        .ip_addrs()
        .next()
        .expect("a bound endpoint advertises at least one socket address")
        .port();
    iroh::EndpointAddr::new(endpoint.id())
        .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

#[tokio::test]
async fn pulling_a_contributors_account_lands_its_memory_in_the_owners_repo() {
    use rusqlite::{Transaction, TransactionBehavior};
    const NOW: i64 = 1_000;

    // OWNER: real account, PublicRead stream for `repo-a`, repo registered so the drain
    // mirrors the stream into `repo_memories`.
    let owner = schema_conn();
    let owner_account = rag_rat_oplog::local_account(&owner, NOW).unwrap();
    let stream = {
        let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-a",
            rag_rat_oplog::AccessMode::PublicRead,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        stream
    };
    owner
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-a','a',0)",
            [],
        )
        .unwrap();

    // CONTRIBUTOR: separate identity, granted Writer, learns the grant from the owner's log.
    let contributor = schema_conn();
    let contributor_account = rag_rat_oplog::local_account(&contributor, NOW).unwrap();
    {
        let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::author_stream_grant_in_tx(
            &tx,
            stream,
            contributor_account,
            rag_rat_oplog::GrantRole::Writer,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    for entry in rag_rat_oplog::account_entries_for_sync(&owner, owner_account).unwrap() {
        rag_rat_oplog::account_ingest(&contributor, &entry.signed_bytes, NOW).unwrap();
    }
    let grant_id = rag_rat_oplog::effective_writer_grant(
        &contributor,
        owner_account,
        stream,
        contributor_account,
    )
    .unwrap()
    .expect("the grant reached the contributor");
    {
        let tx = Transaction::new_unchecked(&contributor, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::author_grantee_content_batch_in_tx(
            &tx,
            stream,
            owner_account,
            grant_id,
            &[rag_rat_oplog::MemoryOp::NodeCreate {
                node_id: rag_rat_oplog::NodeId::from("contributed-1"),
                content: rag_rat_oplog::NodeContent {
                    kind: "Invariant".into(),
                    title: "from the contributor".into(),
                    body: "body".into(),
                    confidence: "high".into(),
                    source: "agent".into(),
                    tags: Vec::new(),
                    payload: None,
                },
            }],
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // THE AUTOMATIC DIRECTION: the owner pulls the CONTRIBUTOR's account over a real wire
    // through the shared helper the reconcile pass and `sync pull` both use.
    let (contributor_ep, owner_ep) = loopback_endpoints().await;
    let peers = vec![("contributor-host".to_string(), direct_addr(&contributor_ep))];
    // Serve inbound connections until the pull finishes — a fixed accept count would hang
    // the test forever if the pull legitimately stopped after fewer connections.
    // The serving stores use REAL time: the dialing helper verifies the acceptor's node
    // binding against the wall clock, so a fixture clock would read as an expired binding.
    let server = async {
        loop {
            let mut serve_account =
                rag_rat_sync::OplogSyncStore::new(&contributor, contributor_account, time::now_ms);
            let mut serve_content = rag_rat_sync::OplogContentSyncStore::new(
                &contributor,
                contributor_account,
                time::now_ms,
            );
            rag_rat_sync::accept_and_dispatch(
                &contributor_ep,
                &mut serve_account,
                &mut serve_content,
                AuthPolicy::PublicRead,
                time::now_ms,
            )
            .await
            .unwrap();
        }
    };
    let pull = pull_account_via_peers(&owner, &owner_ep, contributor_account, &peers);
    let outcome = tokio::select! {
        outcome = pull => outcome.unwrap(),
        _ = server => unreachable!("the serve loop never exits"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
            panic!("the pull did not finish within the test deadline")
        },
    };
    assert_eq!(outcome.peer.as_deref(), Some("contributor-host"), "{:?}", outcome.last_error);
    assert!(outcome.account_entries > 0, "the contributor's log arrived");
    assert!(outcome.content_entries > 0, "the contribution arrived");

    // The drain materializes the contribution into the owner's repo memories.
    rag_rat_oplog::settle_pending_content_refolds(
        &owner,
        &rag_rat_oplog::ContentRefoldBudget::unbounded(),
        NOW,
    )
    .unwrap();
    let effects = crate::drain_synced_memory(&owner).unwrap();
    assert!(effects.nodes_written >= 1, "the memory materialized: {effects:?}");
    let title: String = owner
        .query_row("SELECT title FROM repo_memories WHERE repo_id = 'repo-a'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(title, "from the contributor");
}
