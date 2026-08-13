//! Behavior tests for retention policy storage, lifecycle, and the M4 migration.
//!
//! Kept in a sibling file so `retention.rs` stays under the 1000-line gate;
//! `#[path]`-included from there. `super::*` brings the retention API (and its
//! `rusqlite::{params, Connection}` imports) into scope; `super::super::store`
//! reaches the neighbouring subscription mutators and the base `SCHEMA`.

use super::super::store;
use super::*;
use std::path::Path;
use std::sync::{Arc, Barrier};
use tempfile::NamedTempFile;

const ID: &str = "idpk";
const RELAY: &str = "wss://r";
const OWNER: &str = "owner_p";

/// Open a fresh archive DB (runs the full schema + every migration incl. M4).
fn fresh(db: &NamedTempFile) -> Connection {
    store::open_archive_db(db.path()).expect("open_archive_db must succeed")
}

/// Build a legacy DB that has the base schema and M1–M3 markers but NOT M4,
/// so the next `open_archive_db` pends only the retention migration. This
/// isolates the M4 first-open race from the separately-tested M1–M3 chain.
fn build_pre_m4_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "busy_timeout", 5000).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(store::SCHEMA).unwrap();
    for name in [
        "add_harness_to_metric_index",
        "add_cache_read_tokens",
        "add_cache_write_and_pricing",
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO archive_migrations (name, applied_at) VALUES (?1, 0)",
            params![name],
        )
        .unwrap();
    }
}

fn m4_marker_count(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM archive_migrations WHERE name = 'add_retention_policies'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn policy_days(conn: &Connection, kind: i64) -> Option<i64> {
    list_policies(conn, ID, RELAY)
        .unwrap()
        .into_iter()
        .find(|p| p.kind == kind)
        .unwrap_or_else(|| panic!("expected a policy for kind {kind}"))
        .days
}

// ── Defaults & validation (pure) ──────────────────────────────────────────────

#[test]
fn test_default_days_observer_kind_is_thirty() {
    assert_eq!(
        default_days_for_kind(OBSERVER_FRAME_KIND),
        Some(DEFAULT_OBSERVER_RETENTION_DAYS)
    );
}

#[test]
fn test_default_days_other_kinds_are_forever() {
    assert_eq!(default_days_for_kind(1), None);
    assert_eq!(default_days_for_kind(44200), None);
}

#[test]
fn test_validate_days_accepts_forever_one_and_max() {
    assert!(validate_days(None).is_ok());
    assert!(validate_days(Some(1)).is_ok());
    assert!(validate_days(Some(MAX_RETENTION_DAYS)).is_ok());
}

#[test]
fn test_validate_days_rejects_zero_negative_and_over_max() {
    assert!(validate_days(Some(0)).is_err());
    assert!(validate_days(Some(-1)).is_err());
    assert!(validate_days(Some(MAX_RETENTION_DAYS + 1)).is_err());
}

// ── Seeding ─────────────────────────────────────────────────────────────────

#[test]
fn test_seed_default_policies_sets_window_for_observer_and_forever_for_others() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    seed_default_policies_for_kinds(&conn, ID, RELAY, OWNER, ID, &[OBSERVER_FRAME_KIND, 1], 100)
        .unwrap();
    assert_eq!(policy_days(&conn, OBSERVER_FRAME_KIND), Some(30));
    assert_eq!(policy_days(&conn, 1), None);
}

#[test]
fn test_seed_default_policy_preserves_existing_explicit_choice() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    set_policy(
        &conn,
        ID,
        RELAY,
        OWNER,
        ID,
        OBSERVER_FRAME_KIND,
        Some(7),
        100,
    )
    .unwrap();
    // Re-seeding must never clobber a user's explicit window.
    seed_default_policy(&conn, ID, RELAY, OWNER, ID, OBSERVER_FRAME_KIND, 200).unwrap();
    assert_eq!(policy_days(&conn, OBSERVER_FRAME_KIND), Some(7));
}

// ── set / delete ──────────────────────────────────────────────────────────────

#[test]
fn test_set_policy_upserts_and_overwrites_days() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    set_policy(&conn, ID, RELAY, OWNER, ID, 1, Some(10), 100).unwrap();
    assert_eq!(policy_days(&conn, 1), Some(10));
    set_policy(&conn, ID, RELAY, OWNER, ID, 1, Some(20), 200).unwrap();
    assert_eq!(policy_days(&conn, 1), Some(20));
    set_policy(&conn, ID, RELAY, OWNER, ID, 1, None, 300).unwrap();
    assert_eq!(policy_days(&conn, 1), None);
}

#[test]
fn test_set_policy_rejects_out_of_range_without_writing() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    assert!(set_policy(&conn, ID, RELAY, OWNER, ID, 1, Some(0), 100).is_err());
    assert!(set_policy(
        &conn,
        ID,
        RELAY,
        OWNER,
        ID,
        1,
        Some(MAX_RETENTION_DAYS + 1),
        100
    )
    .is_err());
    // Nothing was written for the rejected kind.
    assert!(list_policies(&conn, ID, RELAY).unwrap().is_empty());
}

#[test]
fn test_delete_policy_reports_affected_and_removes_row() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    set_policy(&conn, ID, RELAY, OWNER, ID, 1, Some(5), 100).unwrap();
    assert!(delete_policy(&conn, ID, RELAY, OWNER, ID, 1).unwrap());
    assert!(list_policies(&conn, ID, RELAY).unwrap().is_empty());
    // A second delete finds nothing.
    assert!(!delete_policy(&conn, ID, RELAY, OWNER, ID, 1).unwrap());
}

// ── list: active vs orphaned ──────────────────────────────────────────────────

#[test]
fn test_list_policies_tags_active_and_orphaned() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    // A live subscription lists 24200; 44200 has a policy but no subscription.
    store::upsert_save_subscription(&conn, ID, RELAY, OWNER, ID, "[24200]", 100).unwrap();
    seed_default_policy(&conn, ID, RELAY, OWNER, ID, OBSERVER_FRAME_KIND, 100).unwrap();
    seed_default_policy(&conn, ID, RELAY, OWNER, ID, 44200, 100).unwrap();

    let policies = list_policies(&conn, ID, RELAY).unwrap();
    let active: std::collections::HashMap<i64, bool> =
        policies.iter().map(|p| (p.kind, p.active)).collect();
    assert_eq!(active.get(&OBSERVER_FRAME_KIND), Some(&true));
    assert_eq!(active.get(&44200), Some(&false));
}

#[test]
fn test_list_policies_scoped_to_identity_and_relay() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    seed_default_policy(&conn, ID, RELAY, OWNER, ID, 1, 100).unwrap();
    seed_default_policy(&conn, "other", RELAY, OWNER, "other", 1, 100).unwrap();
    seed_default_policy(&conn, ID, "wss://other", OWNER, ID, 1, 100).unwrap();

    let policies = list_policies(&conn, ID, RELAY).unwrap();
    assert_eq!(policies.len(), 1, "only the matching identity+relay policy");
    assert_eq!(policies[0].identity_pubkey, ID);
    assert_eq!(policies[0].relay_url, RELAY);
}

// ── create_subscription_with_policies (atomic) ────────────────────────────────

#[test]
fn test_create_subscription_with_policies_seeds_all_kinds() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    create_subscription_with_policies(
        &conn,
        ID,
        RELAY,
        OWNER,
        ID,
        &[OBSERVER_FRAME_KIND, 44200],
        "[24200,44200]",
        100,
    )
    .unwrap();

    let subs = store::list_save_subscriptions(&conn, ID, RELAY).unwrap();
    assert_eq!(subs.len(), 1, "subscription row created");
    assert_eq!(policy_days(&conn, OBSERVER_FRAME_KIND), Some(30));
    assert_eq!(policy_days(&conn, 44200), None);
    // Both policies are active — the subscription lists both kinds.
    assert!(list_policies(&conn, ID, RELAY)
        .unwrap()
        .iter()
        .all(|p| p.active));
}

// ── subscription mutation seeds/keeps policies ────────────────────────────────

#[test]
fn test_merge_owner_p_kind_seeds_default_and_preserves_explicit() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    // First merge seeds the observer default (30 days).
    store::merge_owner_p_kinds(&conn, ID, RELAY, ID, 24200, 100).unwrap();
    assert_eq!(policy_days(&conn, OBSERVER_FRAME_KIND), Some(30));
    // A user narrows it, then the same kind is merged again (idempotent enable):
    // the explicit choice must survive.
    set_policy(&conn, ID, RELAY, OWNER, ID, 24200, Some(3), 200).unwrap();
    store::merge_owner_p_kinds(&conn, ID, RELAY, ID, 24200, 300).unwrap();
    assert_eq!(policy_days(&conn, OBSERVER_FRAME_KIND), Some(3));
}

#[test]
fn test_remove_owner_p_kind_leaves_policy_orphaned() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    store::merge_owner_p_kinds(&conn, ID, RELAY, ID, 24200, 100).unwrap();
    store::merge_owner_p_kinds(&conn, ID, RELAY, ID, 44200, 100).unwrap();
    // Disabling a kind removes it from the subscription but NOT its policy.
    store::remove_owner_p_kind(&conn, ID, RELAY, ID, 24200).unwrap();

    let policies = list_policies(&conn, ID, RELAY).unwrap();
    let observer = policies
        .iter()
        .find(|p| p.kind == OBSERVER_FRAME_KIND)
        .expect("observer policy must remain after the kind is disabled");
    assert!(!observer.active, "policy is now orphaned");
    assert_eq!(observer.days, Some(30), "it keeps expiring historical data");
}

#[test]
fn test_delete_subscription_leaves_policies_orphaned() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    create_subscription_with_policies(&conn, ID, RELAY, OWNER, ID, &[24200], "[24200]", 100)
        .unwrap();
    assert!(store::delete_save_subscription(&conn, ID, RELAY, OWNER, ID).unwrap());

    let policies = list_policies(&conn, ID, RELAY).unwrap();
    assert_eq!(policies.len(), 1, "policy survives subscription deletion");
    assert!(
        !policies[0].active,
        "orphaned once the subscription is gone"
    );
}

// ── Migration M4 ──────────────────────────────────────────────────────────────

#[test]
fn test_m4_fresh_open_creates_schema_and_marker() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);
    let objects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE (type = 'table' AND name IN ('retention_policies', 'archive_meta'))
                OR (type = 'index' AND name = 'idx_archived_event_scopes_age')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(objects, 3, "two tables + one index");
    assert_eq!(m4_marker_count(&conn), 1, "M4 marker recorded");
}

#[test]
fn test_m4_reopen_is_idempotent() {
    let db = NamedTempFile::new().unwrap();
    let first = fresh(&db);
    // Seed a policy through the first connection, then reopen.
    set_policy(&first, ID, RELAY, OWNER, ID, 1, Some(9), 100).unwrap();
    drop(first);
    let second = fresh(&db);
    assert_eq!(
        m4_marker_count(&second),
        1,
        "exactly one marker after reopen"
    );
    // The pre-existing policy is untouched by the re-run.
    assert_eq!(policy_days(&second, 1), Some(9));
}

#[test]
fn test_m4_seeds_defaults_from_existing_subscriptions() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // A pre-M4 subscription with a mix of kinds.
    {
        let conn = Connection::open(db.path()).unwrap();
        store::upsert_save_subscription(&conn, ID, RELAY, OWNER, ID, "[24200,44200,1]", 100)
            .unwrap();
    }
    let conn = fresh(&db); // triggers M4, which seeds from the subscription
    assert_eq!(policy_days(&conn, OBSERVER_FRAME_KIND), Some(30));
    assert_eq!(policy_days(&conn, 44200), None);
    assert_eq!(policy_days(&conn, 1), None);
    assert_eq!(list_policies(&conn, ID, RELAY).unwrap().len(), 3);
}

#[test]
fn test_m4_malformed_kinds_json_aborts_and_leaves_no_marker_then_recovers() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // A subscription whose `kinds` is not a JSON int array (defensive guard).
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute(
            "INSERT INTO save_subscriptions
                 (identity_pubkey, relay_url, scope_type, scope_value, kinds, created_at)
             VALUES (?1, ?2, ?3, ?4, 'not-json', 100)",
            params![ID, RELAY, OWNER, ID],
        )
        .unwrap();
    }
    // Fail-closed: M4 aborts, so open_archive_db surfaces an error.
    assert!(store::open_archive_db(db.path()).is_err());
    {
        let verify = Connection::open(db.path()).unwrap();
        assert_eq!(m4_marker_count(&verify), 0, "no marker written on abort");
    }
    // Repairing the row lets the next open complete cleanly.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute(
            "UPDATE save_subscriptions SET kinds = '[1]' WHERE identity_pubkey = ?1",
            params![ID],
        )
        .unwrap();
    }
    let conn = fresh(&db);
    assert_eq!(m4_marker_count(&conn), 1, "retry succeeds after repair");
    assert_eq!(policy_days(&conn, 1), None);
}

#[test]
fn test_m4_preexisting_retention_policies_without_marker_fails_closed() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // A `retention_policies` table present without the M4 marker is
    // unreachable via shipped code — M4 runs the whole body (create tables,
    // build index, seed, marker) in one transactional `BEGIN IMMEDIATE`, so a
    // crash rolls back the table too. The only way to reach this state is an
    // externally-created table. M4 refuses to certify it: it fails closed and
    // rolls back with no marker rather than adopting a table it did not build.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE retention_policies (
                 identity_pubkey TEXT NOT NULL, relay_url TEXT NOT NULL,
                 scope_type TEXT NOT NULL, scope_value TEXT NOT NULL,
                 kind INTEGER NOT NULL, days INTEGER, updated_at INTEGER NOT NULL,
                 PRIMARY KEY (identity_pubkey, relay_url, scope_type, scope_value, kind));",
        )
        .unwrap();
    }
    assert!(
        store::open_archive_db(db.path()).is_err(),
        "M4 must fail closed on a pre-existing retention_policies"
    );
    let verify = Connection::open(db.path()).unwrap();
    assert_eq!(
        m4_marker_count(&verify),
        0,
        "no marker may certify an externally-created table"
    );
    let archive_meta_exists: bool = verify
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'archive_meta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0;
    assert!(
        !archive_meta_exists,
        "the rollback left no partially-created schema behind"
    );
}

#[test]
fn test_m4_preexisting_archive_meta_without_marker_fails_closed() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Same fail-closed contract, exercised through the second guarded table.
    // The table's shape is irrelevant — its mere presence without the marker
    // means M4 did not create it, so M4 refuses rather than adopting it.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch("CREATE TABLE archive_meta (key TEXT PRIMARY KEY);")
            .unwrap();
    }
    assert!(
        store::open_archive_db(db.path()).is_err(),
        "M4 must fail closed on a pre-existing archive_meta"
    );
    let verify = Connection::open(db.path()).unwrap();
    assert_eq!(
        m4_marker_count(&verify),
        0,
        "no marker may certify an externally-created table"
    );
    // The rollback did not create retention_policies alongside the bad table.
    let retention_policies_exists: bool = verify
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'retention_policies'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0;
    assert!(
        !retention_policies_exists,
        "no policy row was seeded and no sibling table was created"
    );
}

#[test]
fn test_m4_preexisting_index_under_name_is_silently_rebuilt() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Unlike a table (which carries data and is fail-closed), an index carries
    // no data, so M4 drops any index sharing the name and recreates it
    // unconditionally — no shape inspection. A bogus pre-existing index under
    // the name is silently replaced with the correct one and the marker lands.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch(&format!(
            "CREATE INDEX {SCOPE_AGE_INDEX_NAME} ON archived_event_scopes (id);"
        ))
        .unwrap();
    }
    let conn = fresh(&db);
    assert_eq!(m4_marker_count(&conn), 1, "M4 completes after the rebuild");
    // The rebuilt index has the six expected key columns in the covering order.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT name FROM pragma_index_xinfo('{SCOPE_AGE_INDEX_NAME}') \
             WHERE key = 1 ORDER BY seqno"
        ))
        .unwrap();
    let keys: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        keys,
        [
            "identity_pubkey",
            "relay_url",
            "scope_type",
            "scope_value",
            "archived_at",
            "id"
        ],
        "the index was rebuilt with the covering key order"
    );
}

// ── Concurrency ───────────────────────────────────────────────────────────────

#[test]
fn test_m4_two_conn_first_open_race_neither_times_out_and_marks_once() {
    use std::thread;
    let db = NamedTempFile::new().unwrap();
    let path = db.path().to_path_buf();
    build_pre_m4_db(&path);
    // A realistically-populated legacy DB: a multi-kind subscription plus
    // archived scope rows so M4's index build touches real data.
    {
        let conn = Connection::open(&path).unwrap();
        store::upsert_save_subscription(&conn, ID, RELAY, OWNER, ID, "[24200,44200,1]", 100)
            .unwrap();
        for i in 0..8 {
            conn.execute(
                "INSERT INTO archived_event_scopes
                     (identity_pubkey, relay_url, id, scope_type, scope_value, archived_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![ID, RELAY, format!("e{i}"), OWNER, ID, 1000 + i],
            )
            .unwrap();
        }
    }

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let p = path.clone();
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait(); // maximise the first-open race window
                store::open_archive_db(&p).map(|_| ())
            })
        })
        .collect();
    for h in handles {
        assert!(
            h.join().unwrap().is_ok(),
            "both racing opens must complete within busy_timeout"
        );
    }

    let verify = store::open_archive_db(&path).unwrap();
    assert_eq!(m4_marker_count(&verify), 1, "M4 applied exactly once");
    // Seeding happened once inside the winner's transaction — no duplicates.
    assert_eq!(list_policies(&verify, ID, RELAY).unwrap().len(), 3);
    assert_eq!(policy_days(&verify, OBSERVER_FRAME_KIND), Some(30));
}

#[test]
fn test_concurrent_merge_seeds_both_policies() {
    use std::thread;
    let db = NamedTempFile::new().unwrap();
    let path = db.path().to_path_buf();
    // Initialise the schema (incl. M4) once so both threads see it.
    drop(fresh(&db));

    let barrier = Arc::new(Barrier::new(2));
    let kinds = [24200_u32, 44200_u32];
    let handles: Vec<_> = kinds
        .into_iter()
        .map(|kind| {
            let p = path.clone();
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                let conn = store::open_archive_db(&p).unwrap();
                b.wait();
                store::merge_owner_p_kinds(&conn, ID, RELAY, ID, kind, 100)
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap().is_ok(), "concurrent merge must succeed");
    }

    let verify = store::open_archive_db(&path).unwrap();
    // Both kinds landed in the subscription and both got a seeded policy.
    let subs = store::list_save_subscriptions(&verify, ID, RELAY).unwrap();
    let sub_kinds: Vec<u32> = serde_json::from_str(&subs[0].kinds).unwrap();
    assert!(sub_kinds.contains(&24200) && sub_kinds.contains(&44200));
    assert_eq!(policy_days(&verify, OBSERVER_FRAME_KIND), Some(30));
    assert_eq!(policy_days(&verify, 44200), None);
}

/// Deterministic interleaving of merge / remove / set on ONE `(scope, kind)`
/// across two connections to the same WAL file. Each mutator takes `BEGIN
/// IMMEDIATE`, so a barrier held before both begin forces them to serialize on
/// `busy_timeout` rather than race to a `BUSY_SNAPSHOT`. Whichever order the OS
/// picks, the invariants must hold: the subscription stays valid, the explicit
/// policy choice set by one thread is never silently reset by the other's
/// merge-seed (which is `ON CONFLICT DO NOTHING`), and no policy row is deleted
/// by a `kinds` mutation. Removing the `BEGIN IMMEDIATE` guard from the
/// mutators makes one contender fail with `BUSY_SNAPSHOT`, tripping this test.
#[test]
fn test_concurrent_merge_remove_set_keeps_state_valid_and_choice() {
    use std::thread;
    let db = NamedTempFile::new().unwrap();
    let path = db.path().to_path_buf();
    // Start from a subscription that already lists 24200 with an EXPLICIT
    // 7-day policy — the choice both racing threads must not clobber.
    {
        let conn = fresh(&db);
        store::merge_owner_p_kinds(&conn, ID, RELAY, ID, 24200, 100).unwrap();
        set_policy(&conn, ID, RELAY, OWNER, ID, 24200, Some(7), 100).unwrap();
    }

    let barrier = Arc::new(Barrier::new(3));
    // T1: enable a second kind (seeds its default policy).
    // T2: disable 24200 (removes it from `kinds`, must NOT touch its policy).
    // T3: re-set 24200's explicit policy to 5 days.
    let t1 = {
        let (p, b) = (path.clone(), Arc::clone(&barrier));
        thread::spawn(move || {
            let conn = store::open_archive_db(&p).unwrap();
            b.wait();
            store::merge_owner_p_kinds(&conn, ID, RELAY, ID, 44200, 200)
        })
    };
    let t2 = {
        let (p, b) = (path.clone(), Arc::clone(&barrier));
        thread::spawn(move || {
            let conn = store::open_archive_db(&p).unwrap();
            b.wait();
            store::remove_owner_p_kind(&conn, ID, RELAY, ID, 24200)
        })
    };
    let t3 = {
        let (p, b) = (path.clone(), Arc::clone(&barrier));
        thread::spawn(move || {
            let conn = store::open_archive_db(&p).unwrap();
            b.wait();
            set_policy(&conn, ID, RELAY, OWNER, ID, 24200, Some(5), 300)
        })
    };
    assert!(t1.join().unwrap().is_ok(), "concurrent merge must succeed");
    assert!(t2.join().unwrap().is_ok(), "concurrent remove must succeed");
    assert!(t3.join().unwrap().is_ok(), "concurrent set must succeed");

    let verify = store::open_archive_db(&path).unwrap();
    // Subscription is valid JSON and now lists 44200 (added) but not 24200
    // (removed) — the two `kinds` mutations composed cleanly.
    let subs = store::list_save_subscriptions(&verify, ID, RELAY).unwrap();
    assert_eq!(subs.len(), 1, "one owner_p row survives the interleaving");
    let sub_kinds: Vec<u32> = serde_json::from_str(&subs[0].kinds).unwrap();
    assert!(sub_kinds.contains(&44200), "the merged kind is present");
    assert!(!sub_kinds.contains(&24200), "the removed kind is gone");
    // Both policy rows still exist — no `kinds` mutation deleted one.
    let policies = list_policies(&verify, ID, RELAY).unwrap();
    assert_eq!(policies.len(), 2, "both policy rows survive; none deleted");
    // T3's explicit 5-day choice for 24200 is the final value: T1's default
    // seed for 44200 never touches 24200, and T2's remove leaves policies
    // alone, so the observer policy reflects the explicit set, not a reset.
    assert_eq!(
        policy_days(&verify, 24200),
        Some(5),
        "explicit choice survives the concurrent merge/remove"
    );
    assert_eq!(
        policy_days(&verify, 44200),
        None,
        "44200 kept its Forever default"
    );
}

/// One evolving state walked through the full policy lifecycle in a single
/// test: an active observer policy → the kind is disabled (policy orphaned but
/// preserved) → the whole subscription is deleted (still orphaned) → the orphan
/// is edited → the orphan is explicitly deleted. This proves the transitions
/// compose on the SAME rows, which the separate per-transition tests cannot.
#[test]
fn test_policy_lifecycle_active_orphaned_edited_deleted_on_one_state() {
    let db = NamedTempFile::new().unwrap();
    let conn = fresh(&db);

    // 1. Active: a subscription lists 24200, its policy is the seeded default.
    create_subscription_with_policies(&conn, ID, RELAY, OWNER, ID, &[24200], "[24200]", 100)
        .unwrap();
    let active = list_policies(&conn, ID, RELAY).unwrap();
    assert_eq!(active.len(), 1);
    assert!(
        active[0].active,
        "policy is active while the kind is listed"
    );
    assert_eq!(active[0].days, Some(30), "seeded observer default");

    // 2. Kind disabled: 24200 leaves `kinds`; the policy stays but goes orphaned.
    store::remove_owner_p_kind(&conn, ID, RELAY, ID, 24200).unwrap();
    // Removing the last kind deleted the subscription row entirely.
    assert!(
        store::list_save_subscriptions(&conn, ID, RELAY)
            .unwrap()
            .is_empty(),
        "the last kind off removes the subscription row"
    );
    let disabled = list_policies(&conn, ID, RELAY).unwrap();
    assert_eq!(disabled.len(), 1, "the policy is preserved, not deleted");
    assert!(
        !disabled[0].active,
        "policy is orphaned once the kind is gone"
    );
    assert_eq!(
        disabled[0].days,
        Some(30),
        "it keeps expiring historical data"
    );

    // 3. Subscription deleted: an explicit delete on an already-absent row is a
    // no-op; the orphaned policy is unaffected.
    assert!(
        !store::delete_save_subscription(&conn, ID, RELAY, OWNER, ID).unwrap(),
        "no subscription row remains to delete"
    );
    let still_orphaned = list_policies(&conn, ID, RELAY).unwrap();
    assert_eq!(still_orphaned.len(), 1);
    assert!(!still_orphaned[0].active, "policy remains orphaned");

    // 4. Edit the orphan: a user can still change retention for historical data.
    set_policy(&conn, ID, RELAY, OWNER, ID, 24200, Some(90), 400).unwrap();
    let edited = list_policies(&conn, ID, RELAY).unwrap();
    assert_eq!(edited.len(), 1);
    assert_eq!(edited[0].days, Some(90), "orphaned policy is editable");
    assert!(
        !edited[0].active,
        "editing does not resurrect the subscription"
    );

    // 5. Delete the orphan: the only path that removes a policy row.
    assert!(
        delete_policy(&conn, ID, RELAY, OWNER, ID, 24200).unwrap(),
        "the orphaned policy is deleted"
    );
    assert!(
        list_policies(&conn, ID, RELAY).unwrap().is_empty(),
        "no policy rows remain after the lifecycle completes"
    );
}
