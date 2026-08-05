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
    Ok(())
}
