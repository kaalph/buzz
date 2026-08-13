//! Per-subscription / per-kind retention policy storage and lifecycle.
//!
//! Retention is modelled as a normalized `retention_policies` table keyed
//! `(identity_pubkey, relay_url, scope_type, scope_value, kind)` with a
//! nullable `days` column (`NULL` = keep forever). The policy lifecycle is
//! deliberately **independent of `save_subscriptions`**: disabling a kind or
//! deleting a subscription leaves its policy behind so already-archived data
//! keeps expiring. A policy is only ever removed by the explicit
//! `delete_retention_policy` command.
//!
//! Kept in a sibling file (not `store.rs`) to respect the 1000-line gate, per
//! the existing `metric_store.rs` / `pipeline.rs` / `store_migrations.rs`
//! precedent.

use rusqlite::{params, Connection, OptionalExtension};

// ── Constants ────────────────────────────────────────────────────────────────

/// Kind 24200 (agent observer frames) — the unbounded-growth driver that
/// defaults to a rolling window rather than Forever.
pub const OBSERVER_FRAME_KIND: i64 = 24200;

/// Default rolling window for observer frames (Will's ruling: "~14–30"; 30 is
/// the shipped default, trivially changeable).
pub const DEFAULT_OBSERVER_RETENTION_DAYS: i64 = 30;

/// Upper bound on an explicit retention window (~100 years). Guards against a
/// day count large enough to overflow `archived_at` cutoff arithmetic while
/// still admitting any realistic user choice.
pub const MAX_RETENTION_DAYS: i64 = 36_500;

// ── Schema (created by migration M4, not the base SCHEMA) ─────────────────────

/// `retention_policies` + `archive_meta`. Created inside M4 under
/// `BEGIN IMMEDIATE` (see `store_migrations::migrate_add_retention_policies`).
pub(super) const RETENTION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS retention_policies (
    identity_pubkey TEXT NOT NULL,
    relay_url       TEXT NOT NULL,
    scope_type      TEXT NOT NULL,
    scope_value     TEXT NOT NULL,
    kind            INTEGER NOT NULL,
    days            INTEGER,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, relay_url, scope_type, scope_value, kind)
);

CREATE TABLE IF NOT EXISTS archive_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Covering index for the retention prune candidate scan, seeking on the
/// scope-row `archived_at` age basis (Phase 2 uses this; built in M4 so the
/// one-time cost over the existing 1.3M-row archive is paid behind the init
/// barrier rather than on the prune hot path). The scope PK
/// `(identity, relay, id, scope_type, scope_value)` puts `id` before the
/// scope keys and lacks `archived_at`, so it cannot range-seek by age.
pub(super) const SCOPE_AGE_INDEX_DDL: &str = "
CREATE INDEX IF NOT EXISTS idx_archived_event_scopes_age
    ON archived_event_scopes
       (identity_pubkey, relay_url, scope_type, scope_value, archived_at, id);
";

/// Name of the scope-age index, shared by the DDL, the shape check, and the
/// M4 drop-and-rebuild repair path.
pub(super) const SCOPE_AGE_INDEX_NAME: &str = "idx_archived_event_scopes_age";

// ── Expected schema shape (the single source of truth M4 validates against) ────

/// One column's expected shape: `(name, declared_type, not_null, pk_position)`.
/// `pk_position` is 0 when the column is not part of the primary key, else its
/// 1-based position in the PK — this is exactly what `PRAGMA table_info` reports
/// in its `pk` field, so PK column ORDER is validated, not just membership.
type ColumnShape = (&'static str, &'static str, bool, i64);

/// Expected `retention_policies` columns in `cid` order. Mirrors
/// [`RETENTION_SCHEMA`]; a drift here or there is caught by the M4 shape check.
const RETENTION_POLICIES_SHAPE: &[ColumnShape] = &[
    ("identity_pubkey", "TEXT", true, 1),
    ("relay_url", "TEXT", true, 2),
    ("scope_type", "TEXT", true, 3),
    ("scope_value", "TEXT", true, 4),
    ("kind", "INTEGER", true, 5),
    ("days", "INTEGER", false, 0),
    ("updated_at", "INTEGER", true, 0),
];

/// Expected `archive_meta` columns in `cid` order. A `TEXT PRIMARY KEY` is
/// nullable in SQLite (only `INTEGER PRIMARY KEY` implies NOT NULL), so `key`
/// carries `not_null = false` with `pk_position = 1`.
const ARCHIVE_META_SHAPE: &[ColumnShape] = &[("key", "TEXT", false, 1), ("value", "TEXT", true, 0)];

/// Expected key columns of [`SCOPE_AGE_INDEX_NAME`] in seqno order — the
/// age-range access path Phase 2 depends on. Order is load-bearing: a covering
/// seek needs the scope keys before `archived_at`.
const SCOPE_AGE_INDEX_SHAPE: &[&str] = &[
    "identity_pubkey",
    "relay_url",
    "scope_type",
    "scope_value",
    "archived_at",
    "id",
];

// ── Shape validation (used by migration M4) ────────────────────────────────────

/// Whether `table`'s live columns exactly match `expected` (name, declared
/// type, nullability, and PK position, all in `cid` order). `table` is always a
/// compile-time constant from this module, never user input, so interpolating
/// it into the table-valued `pragma_table_info` call carries no injection risk.
fn table_shape_matches(
    conn: &Connection,
    table: &str,
    expected: &[ColumnShape],
) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT name, type, \"notnull\", pk FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .map_err(|e| format!("shape check: prepare table_info({table}): {e}"))?;
    let actual: Vec<(String, String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| format!("shape check: query table_info({table}): {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("shape check: read table_info({table}): {e}"))?;

    Ok(actual.len() == expected.len()
        && actual.iter().zip(expected).all(
            |((name, ty, not_null, pk), (exp_name, exp_ty, exp_not_null, exp_pk))| {
                name == exp_name
                    && ty.eq_ignore_ascii_case(exp_ty)
                    && (*not_null != 0) == *exp_not_null
                    && pk == exp_pk
            },
        ))
}

/// Whether the scope-age index exists on `archived_event_scopes` with exactly
/// the expected key columns in order. False when the index is absent, sits on
/// the wrong table, or indexes the wrong columns — all of which M4 repairs by
/// dropping and recreating it (an index carries no data, so a rebuild is safe).
pub(super) fn scope_age_index_is_correct(conn: &Connection) -> Result<bool, String> {
    let table: Option<String> = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![SCOPE_AGE_INDEX_NAME],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("shape check: scope-age index table: {e}"))?;
    if table.as_deref() != Some("archived_event_scopes") {
        return Ok(false);
    }

    let mut stmt = conn
        .prepare(&format!(
            "SELECT name FROM pragma_index_info('{SCOPE_AGE_INDEX_NAME}') ORDER BY seqno"
        ))
        .map_err(|e| format!("shape check: prepare index_info: {e}"))?;
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("shape check: query index_info: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("shape check: read index_info: {e}"))?;

    Ok(columns.len() == SCOPE_AGE_INDEX_SHAPE.len()
        && columns
            .iter()
            .zip(SCOPE_AGE_INDEX_SHAPE)
            .all(|(actual, expected)| actual == expected))
}

/// Validate the COMPLETE expected shape of the M4 objects: both tables' columns
/// (names, types, nullability, PK positions) and the scope-age index's key
/// order. `CREATE ... IF NOT EXISTS` silently preserves a wrong-shaped object
/// that already carries the expected name, so name presence alone cannot
/// certify the schema — a wrong-shaped named table would otherwise let M4 mark
/// itself applied and hand Phase 2 an unusable table or missing access path.
/// Called inside M4's `BEGIN IMMEDIATE`; an `Err` rolls the transaction back
/// with no marker written.
pub(super) fn validate_retention_schema_shape(conn: &Connection) -> Result<(), String> {
    if !table_shape_matches(conn, "retention_policies", RETENTION_POLICIES_SHAPE)? {
        return Err(
            "migration M4: retention_policies has an unexpected column or primary-key shape"
                .to_string(),
        );
    }
    if !table_shape_matches(conn, "archive_meta", ARCHIVE_META_SHAPE)? {
        return Err(
            "migration M4: archive_meta has an unexpected column or primary-key shape".to_string(),
        );
    }
    if !scope_age_index_is_correct(conn)? {
        return Err(format!(
            "migration M4: {SCOPE_AGE_INDEX_NAME} has an unexpected shape after rebuild"
        ));
    }
    Ok(())
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// A retention policy row with its live-subscription status, returned by
/// `list_retention_policies`.
///
/// `active` is `true` when a `save_subscriptions` row for the same
/// `(scope_type, scope_value)` still lists this `kind`; `false` marks an
/// orphaned policy that continues to expire historical data after the kind was
/// disabled or the subscription deleted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RetentionPolicyStatus {
    pub identity_pubkey: String,
    pub relay_url: String,
    pub scope_type: String,
    pub scope_value: String,
    pub kind: i64,
    /// `None` = keep forever; otherwise the rolling-window day count.
    pub days: Option<i64>,
    pub updated_at: i64,
    pub active: bool,
}

// ── Defaults & validation ──────────────────────────────────────────────────────

/// The default retention for a freshly-seeded kind: observer frames (24200)
/// default to a rolling window; every other kind defaults to Forever (`None`).
pub fn default_days_for_kind(kind: i64) -> Option<i64> {
    if kind == OBSERVER_FRAME_KIND {
        Some(DEFAULT_OBSERVER_RETENTION_DAYS)
    } else {
        None
    }
}

/// Fail-closed validation for an explicit retention choice: `None` (Forever) or
/// a day count in `1..=MAX_RETENTION_DAYS`. Zero, negatives, and out-of-range
/// values are rejected so no policy can encode a non-expiring "0 days" or an
/// overflowing cutoff.
pub fn validate_days(days: Option<i64>) -> Result<(), String> {
    if let Some(d) = days {
        if !(1..=MAX_RETENTION_DAYS).contains(&d) {
            return Err(format!(
                "retention days must be between 1 and {MAX_RETENTION_DAYS}, or Forever (null); got {d}"
            ));
        }
    }
    Ok(())
}

// ── Policy row mutation (single transactional path) ─────────────────────────────

/// Seed the default policy for one `(scope, kind)` tuple, only if no row exists
/// yet. Idempotent (`ON CONFLICT DO NOTHING`): an existing row — whether a
/// user's explicit choice or a prior seed — always wins, so re-enabling a kind
/// never resets its retention. Runs on the caller's connection/transaction.
pub fn seed_default_policy(
    conn: &Connection,
    identity_pubkey: &str,
    relay_url: &str,
    scope_type: &str,
    scope_value: &str,
    kind: i64,
    now: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO retention_policies
             (identity_pubkey, relay_url, scope_type, scope_value, kind, days, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (identity_pubkey, relay_url, scope_type, scope_value, kind) DO NOTHING",
        params![
            identity_pubkey,
            relay_url,
            scope_type,
            scope_value,
            kind,
            default_days_for_kind(kind),
            now
        ],
    )
    .map_err(|e| format!("failed to seed default retention policy: {e}"))?;
    Ok(())
}

/// Seed default policies for every kind in a subscription's `kinds` list.
/// Runs on the caller's connection/transaction so `kinds` and policy rows
/// mutate together.
pub fn seed_default_policies_for_kinds(
    conn: &Connection,
    identity_pubkey: &str,
    relay_url: &str,
    scope_type: &str,
    scope_value: &str,
    kinds: &[i64],
    now: i64,
) -> Result<(), String> {
    for &kind in kinds {
        seed_default_policy(
            conn,
            identity_pubkey,
            relay_url,
            scope_type,
            scope_value,
            kind,
            now,
        )?;
    }
    Ok(())
}

/// Set (create or replace) an explicit retention policy for one `(scope, kind)`
/// tuple. Fail-closed: rejects an out-of-range `days` before writing. A single
/// idempotent upsert is atomic under autocommit; no explicit transaction is
/// needed for this one-statement mutation.
#[allow(clippy::too_many_arguments)] // one param per PK column + days + now
pub fn set_policy(
    conn: &Connection,
    identity_pubkey: &str,
    relay_url: &str,
    scope_type: &str,
    scope_value: &str,
    kind: i64,
    days: Option<i64>,
    now: i64,
) -> Result<(), String> {
    validate_days(days)?;
    conn.execute(
        "INSERT INTO retention_policies
             (identity_pubkey, relay_url, scope_type, scope_value, kind, days, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (identity_pubkey, relay_url, scope_type, scope_value, kind)
         DO UPDATE SET days = excluded.days, updated_at = excluded.updated_at",
        params![
            identity_pubkey,
            relay_url,
            scope_type,
            scope_value,
            kind,
            days,
            now
        ],
    )
    .map_err(|e| format!("failed to set retention policy: {e}"))?;
    Ok(())
}

/// Delete a retention policy row. Returns `true` if a row was removed. This is
/// the ONLY path that removes a policy — disabling a kind or deleting a
/// subscription never does. Remaining historical data for the `(scope, kind)`
/// then becomes ungoverned (Forever) until an explicit purge (Phase 2+).
pub fn delete_policy(
    conn: &Connection,
    identity_pubkey: &str,
    relay_url: &str,
    scope_type: &str,
    scope_value: &str,
    kind: i64,
) -> Result<bool, String> {
    let affected = conn
        .execute(
            "DELETE FROM retention_policies
             WHERE identity_pubkey = ?1
               AND relay_url       = ?2
               AND scope_type      = ?3
               AND scope_value     = ?4
               AND kind            = ?5",
            params![identity_pubkey, relay_url, scope_type, scope_value, kind],
        )
        .map_err(|e| format!("failed to delete retention policy: {e}"))?;
    Ok(affected > 0)
}

/// List every retention policy for the identity + relay, independent of live
/// subscriptions, tagging each with `active` (a matching subscription still
/// lists the kind) vs orphaned. `json_each` expands the subscription's `kinds`
/// JSON array to test membership.
pub fn list_policies(
    conn: &Connection,
    identity_pubkey: &str,
    relay_url: &str,
) -> Result<Vec<RetentionPolicyStatus>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT rp.identity_pubkey, rp.relay_url, rp.scope_type, rp.scope_value,
                    rp.kind, rp.days, rp.updated_at,
                    EXISTS (
                        SELECT 1 FROM save_subscriptions ss
                        WHERE ss.identity_pubkey = rp.identity_pubkey
                          AND ss.relay_url       = rp.relay_url
                          AND ss.scope_type      = rp.scope_type
                          AND ss.scope_value     = rp.scope_value
                          AND rp.kind IN (SELECT value FROM json_each(ss.kinds))
                    ) AS active
             FROM retention_policies rp
             WHERE rp.identity_pubkey = ?1
               AND rp.relay_url       = ?2
             ORDER BY rp.scope_type, rp.scope_value, rp.kind",
        )
        .map_err(|e| format!("prepare list_policies: {e}"))?;

    let rows = stmt
        .query_map(params![identity_pubkey, relay_url], |row| {
            Ok(RetentionPolicyStatus {
                identity_pubkey: row.get(0)?,
                relay_url: row.get(1)?,
                scope_type: row.get(2)?,
                scope_value: row.get(3)?,
                kind: row.get(4)?,
                days: row.get(5)?,
                updated_at: row.get(6)?,
                active: row.get::<_, i64>(7)? != 0,
            })
        })
        .map_err(|e| format!("query list_policies: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read list_policies row: {e}"))
}

/// Create a save subscription and seed default policies for its kinds in ONE
/// `BEGIN IMMEDIATE` transaction, so a reader never observes the subscription
/// without its governing policies. `IMMEDIATE` takes the write lock up front so
/// concurrent creators serialize on `busy_timeout` rather than racing to a
/// `BUSY_SNAPSHOT` error (same rationale as `store::merge_owner_p_kinds`).
#[allow(clippy::too_many_arguments)] // mirrors upsert_save_subscription's column set + kinds
pub fn create_subscription_with_policies(
    conn: &Connection,
    identity_pubkey: &str,
    relay_url: &str,
    scope_type: &str,
    scope_value: &str,
    kinds: &[i64],
    kinds_json: &str,
    now: i64,
) -> Result<(), String> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| format!("create_subscription_with_policies begin immediate: {e}"))?;

    let result = (|| -> Result<(), String> {
        super::store::upsert_save_subscription(
            conn,
            identity_pubkey,
            relay_url,
            scope_type,
            scope_value,
            kinds_json,
            now,
        )?;
        seed_default_policies_for_kinds(
            conn,
            identity_pubkey,
            relay_url,
            scope_type,
            scope_value,
            kinds,
            now,
        )
    })();

    if result.is_ok() {
        conn.execute_batch("COMMIT")
            .map_err(|e| format!("create_subscription_with_policies commit: {e}"))?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "retention_tests.rs"]
mod retention_tests;
