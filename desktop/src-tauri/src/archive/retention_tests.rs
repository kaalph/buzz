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
