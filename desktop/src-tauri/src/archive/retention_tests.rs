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
fn test_m4_partial_schema_marker_absent_recovers_on_next_open() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Simulate an interrupted earlier run: retention_policies exists but
    // archive_meta, the age index, and the marker do not.
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
    // The idempotent DDL fills in the missing objects and records the marker.
    let conn = fresh(&db);
    assert_eq!(m4_marker_count(&conn), 1);
    let objects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE (type = 'table' AND name IN ('retention_policies', 'archive_meta'))
                OR (type = 'index' AND name = 'idx_archived_event_scopes_age')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(objects, 3, "partial schema repaired to the full shape");
}

#[test]
fn test_m4_wrong_shaped_named_table_rejected_no_marker() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Precreate an `archive_meta` table carrying the expected NAME but the
    // wrong shape (its required `value` column is missing). `CREATE ... IF NOT
    // EXISTS` preserves it, so only the explicit shape check can catch it.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch("CREATE TABLE archive_meta (key TEXT PRIMARY KEY);")
            .unwrap();
    }
    // M4 must reject the incompatible named table and roll back with no marker,
    // rather than certify a table Phase 2 could not use.
    assert!(
        store::open_archive_db(db.path()).is_err(),
        "M4 must reject a wrong-shaped named archive_meta"
    );
    let verify = Connection::open(db.path()).unwrap();
    assert_eq!(
        m4_marker_count(&verify),
        0,
        "no marker may certify the incompatible table"
    );
    // The rollback left the schema untouched: the bad table is still one-column
    // and retention_policies was never committed.
    assert!(
        !scope_age_index_is_correct(&verify).unwrap(),
        "the index build rolled back with the rest of the transaction"
    );
    let value_cols: i64 = verify
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('archive_meta') WHERE name = 'value'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        value_cols, 0,
        "the wrong-shaped table was not silently altered"
    );
}

#[test]
fn test_m4_wrong_index_order_dropped_and_rebuilt() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Precreate the scope-age index with the expected NAME on the right table
    // but the WRONG key order (`archived_at` first). A covering age-range seek
    // needs the scope keys before `archived_at`, so this order is unusable.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch(&format!(
            "CREATE INDEX {SCOPE_AGE_INDEX_NAME}
                 ON archived_event_scopes
                    (archived_at, identity_pubkey, relay_url, scope_type, scope_value, id);"
        ))
        .unwrap();
    }
    // M4 rebuilds a wrong-shaped index (safe — an index carries no data) rather
    // than rejecting, so the open succeeds and the marker lands.
    let conn = fresh(&db);
    assert_eq!(m4_marker_count(&conn), 1, "M4 completes after the rebuild");
    assert!(
        scope_age_index_is_correct(&conn).unwrap(),
        "the index was rebuilt into the correct key order"
    );
}

#[test]
fn test_m4_partial_age_index_dropped_and_rebuilt_non_partial() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Precreate the scope-age index with the expected NAME, right table, and
    // EXACT key order — but a `WHERE` predicate making it PARTIAL. It holds the
    // right columns, so `pragma_index_info` (key columns only) cannot tell it
    // apart from the required index; only the partiality probe catches it. A
    // partial index cannot serve the unrestricted prune-age range scan, so M4
    // must treat it like any other wrong shape and rebuild it non-partial.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch(&format!(
            "CREATE INDEX {SCOPE_AGE_INDEX_NAME}
                 ON archived_event_scopes
                    (identity_pubkey, relay_url, scope_type, scope_value, archived_at, id)
                 WHERE archived_at > 1000;"
        ))
        .unwrap();
        // The validator must reject the partial index BEFORE M4 runs — this is
        // the exact check that fails without the partiality probe.
        assert!(
            !scope_age_index_is_correct(&conn).unwrap(),
            "a partial index with correct columns must not be certified"
        );
    }
    // M4 rebuilds the partial index into a non-partial one, then the marker lands.
    let conn = fresh(&db);
    assert_eq!(m4_marker_count(&conn), 1, "M4 completes after the rebuild");
    assert!(
        scope_age_index_is_correct(&conn).unwrap(),
        "the index was rebuilt non-partial with the correct key order"
    );
    // Prove the rebuilt index carries no `WHERE` predicate.
    let partial: i64 = conn
        .query_row(
            "SELECT partial FROM pragma_index_list('archived_event_scopes') WHERE name = ?1",
            params![SCOPE_AGE_INDEX_NAME],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(partial, 0, "the rebuilt index must not be partial");
}

#[test]
fn test_m4_wrong_collation_age_index_dropped_and_rebuilt_binary() {
    let db = NamedTempFile::new().unwrap();
    build_pre_m4_db(db.path());
    // Precreate the scope-age index with the expected NAME, right table, exact
    // key order, and non-partial — but `COLLATE NOCASE` on the first key. It
    // holds the right column names, is non-partial, and passes both the earlier
    // name/order and partiality probes; only the xinfo collation check catches
    // it. A NOCASE key makes the binary-equality prune-age query fall back to
    // the PK autoindex plus a temp B-tree sort, so M4 must rebuild it BINARY.
    {
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch(&format!(
            "CREATE INDEX {SCOPE_AGE_INDEX_NAME}
                 ON archived_event_scopes
                    (identity_pubkey COLLATE NOCASE, relay_url, scope_type,
                     scope_value, archived_at, id);"
        ))
        .unwrap();
        // The validator must reject the NOCASE index BEFORE M4 runs — this is
        // the exact check that fails without the xinfo collation probe.
        assert!(
            !scope_age_index_is_correct(&conn).unwrap(),
            "an index with a non-BINARY key collation must not be certified"
        );
    }
    // M4 rebuilds the wrong-collation index BINARY, then the marker lands.
    let conn = fresh(&db);
    assert_eq!(m4_marker_count(&conn), 1, "M4 completes after the rebuild");
    assert!(
        scope_age_index_is_correct(&conn).unwrap(),
        "the index was rebuilt with BINARY collation on every key"
    );
    // Prove every rebuilt key carries the default BINARY collation.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT coll FROM pragma_index_xinfo('{SCOPE_AGE_INDEX_NAME}') \
             WHERE key = 1 ORDER BY seqno"
        ))
        .unwrap();
    let colls: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(colls.len(), 6, "all six key columns are indexed");
    assert!(
        colls.iter().all(|c| c.eq_ignore_ascii_case("BINARY")),
        "every rebuilt key uses BINARY collation, got {colls:?}"
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
