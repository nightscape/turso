//! A foreign row whose declared identity is NULL.
//!
//! Identity is a contract the driver declares and the engine trusts, and a NULL
//! breaks it: nothing else in the source can be recognised as that same row on a
//! later scan, and the sweep's anti-join — `NOT IN (SELECT <identity> …)` —
//! answers NULL for every candidate once a NULL is in the set, so it would stop
//! deleting anything at all. The mirror's `NOT NULL` on the identity columns
//! rejects the row before it gets that far; what these cases pin is that the
//! rejection says so.

use crate::common::{self, TempDatabase};
use crate::query_processing::fdw_test_driver::MemFdw;
use turso_core::Value;

/// The engine must name the source table and its identity columns, and say the
/// problem is a NULL identity — not report it as a duplicate, which is a
/// different mistake with a different fix.
#[turso_macros::test(views)]
fn test_null_identity_is_reported_as_such(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_mem(uuid TEXT, body TEXT)", vec![0]);
    rows.set(vec![vec![Value::Null, Value::build_text("orphan")]]);
    conn.register_foreign_table("msg_mem", fdw)?;

    let err = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_null_ident AS SELECT uuid, body FROM msg_mem",
    )
    .expect_err("a NULL identity must be refused");
    let msg = err.to_string();

    assert!(
        msg.contains("msg_mem"),
        "the error must name the foreign table: {msg}"
    );
    assert!(
        msg.contains("uuid"),
        "the error must name the identity column: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("null"),
        "the error must say the identity was NULL: {msg}"
    );
    assert!(
        !msg.contains("more than one row"),
        "a NULL identity is not a duplicate identity: {msg}"
    );
    Ok(())
}

/// The duplicate-identity diagnosis must survive the split: two rows sharing an
/// identity is still reported as a duplicate, not as a NULL.
#[turso_macros::test(views)]
fn test_duplicate_identity_still_reported_as_duplicate(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_dup(uuid TEXT, body TEXT)", vec![0]);
    rows.set(vec![
        vec![Value::build_text("u1"), Value::build_text("one")],
        vec![Value::build_text("u1"), Value::build_text("two")],
    ]);
    conn.register_foreign_table("msg_dup", fdw)?;

    let err = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_dup_ident AS SELECT uuid, body FROM msg_dup",
    )
    .expect_err("a repeated identity must be refused");
    let msg = err.to_string();

    assert!(
        msg.contains("more than one row"),
        "a repeated identity must still be diagnosed as a duplicate: {msg}"
    );
    Ok(())
}

/// The same check on the incremental path: a source that grows a NULL identity
/// between syncs must be refused by `REFRESH`, not silently mirrored.
#[turso_macros::test(views)]
fn test_null_identity_refused_by_refresh(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_late(uuid TEXT, body TEXT)", vec![0]);
    rows.set(vec![vec![
        Value::build_text("u1"),
        Value::build_text("one"),
    ]]);
    conn.register_foreign_table("msg_late", fdw)?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_late_null AS SELECT uuid, body FROM msg_late",
    )?;

    rows.set(vec![
        vec![Value::build_text("u1"), Value::build_text("one")],
        vec![Value::Null, Value::build_text("orphan")],
    ]);
    let err = common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_late_null")
        .expect_err("a NULL identity must be refused by the sweep too");
    let msg = err.to_string();
    assert!(
        msg.contains("msg_late") && msg.to_lowercase().contains("null"),
        "the sweep must diagnose the NULL identity: {msg}"
    );
    assert!(
        !msg.contains("more than one row"),
        "a NULL identity is not a duplicate identity: {msg}"
    );
    Ok(())
}

/// A scan holding one NULL identity and nothing repeated is a NULL, and the
/// sweep must say so.
///
/// The trap is `count(DISTINCT x)`, which ignores NULLs: against `count(*)` it
/// reads a single NULL-identity row as a duplicate and sends the user looking
/// for two rows that do not exist. Two NULLs are used because that is the case
/// the naive count is most confidently wrong about.
///
/// This pins the user-visible classification only: the mirror's `NOT NULL` says
/// the same thing, so it passes with the guard removed. The guard's own NULL
/// branch is pinned by `guard_separates_null_identities_from_repeated_ones`.
#[turso_macros::test(views)]
fn test_null_identities_are_not_reported_as_duplicates_at_refresh(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_nulls(uuid TEXT, body TEXT)", vec![0]);
    rows.set(vec![vec![
        Value::build_text("u1"),
        Value::build_text("one"),
    ]]);
    conn.register_foreign_table("msg_nulls", fdw)?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_nulls AS SELECT uuid, body FROM msg_nulls",
    )?;

    rows.set(vec![
        vec![Value::build_text("u1"), Value::build_text("one")],
        vec![Value::Null, Value::build_text("orphan")],
        vec![Value::Null, Value::build_text("another orphan")],
    ]);
    let err = common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_nulls")
        .expect_err("a NULL identity must be refused by the sweep");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("null"),
        "the sweep must diagnose the NULL identity: {msg}"
    );
    assert!(
        !msg.contains("more than one row"),
        "NULL identities are not duplicates, however many of them there are: {msg}"
    );
    Ok(())
}

/// A scan that breaks both promises at once is reported as the NULL, because a
/// NULL identity is the one the user cannot work around: a duplicate names two
/// rows that exist, a NULL names a row nothing can refer to again.
///
/// This pins the user-visible classification only: the mirror's `NOT NULL` says
/// the same thing, so it passes with the guard removed. The guard's own NULL
/// branch is pinned by `guard_separates_null_identities_from_repeated_ones`.
#[turso_macros::test(views)]
fn test_a_null_identity_outranks_a_duplicate_at_refresh(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_both(uuid TEXT, body TEXT)", vec![0]);
    rows.set(vec![vec![
        Value::build_text("u1"),
        Value::build_text("one"),
    ]]);
    conn.register_foreign_table("msg_both", fdw)?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_both AS SELECT uuid, body FROM msg_both",
    )?;

    rows.set(vec![
        vec![Value::build_text("u1"), Value::build_text("one")],
        vec![Value::build_text("u1"), Value::build_text("again")],
        vec![Value::Null, Value::build_text("orphan")],
    ]);
    let err = common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_both")
        .expect_err("a scan breaking the identity contract must be refused");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("null") && !msg.contains("more than one row"),
        "the NULL identity must be the diagnosis: {msg}"
    );
    Ok(())
}
