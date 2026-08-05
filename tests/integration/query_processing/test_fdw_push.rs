//! Pushed changes from a streaming foreign source.
//!
//! `REFRESH` asks the source what it holds now; a push tells the engine what
//! changed. Both end at the same place — DML on the mirror, whose deltas
//! maintain the view at commit — so a push costs the changed rows and nothing
//! else, and a `REFRESH` right after one has nothing left to do.
//!
//! Detection is still the driver's job. What these cases pin is that a known
//! change is applied atomically, incrementally, and consistently with whatever
//! local writes it is interleaved with.

use crate::common::{self, ExecRows, TempDatabase};
use crate::query_processing::fdw_test_driver::MemFdw;
use std::sync::Arc;
use turso_core::foreign::FdwChange;
use turso_core::Value;

fn text(s: &str) -> Value {
    Value::build_text(s.to_string())
}

fn row(uuid: &str, body: &str) -> Vec<Value> {
    vec![text(uuid), text(body)]
}

fn insert(uuid: &str, body: &str) -> FdwChange {
    FdwChange {
        values: row(uuid, body),
        weight: 1,
    }
}

fn delete(uuid: &str, body: &str) -> FdwChange {
    FdwChange {
        values: row(uuid, body),
        weight: -1,
    }
}

/// A view over `msg_push(uuid, body)` seeded with `rows`, plus the driver so the
/// case can push to it.
fn setup(
    tmp_db: &TempDatabase,
    conn: &Arc<turso_core::Connection>,
    view: &str,
    seed: Vec<Vec<Value>>,
) -> anyhow::Result<Arc<MemFdw>> {
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_push(uuid TEXT, body TEXT)", vec![0]);
    rows.set(seed);
    conn.register_foreign_table("msg_push", fdw.clone())?;
    common::run_query(
        tmp_db,
        conn,
        &format!("CREATE MATERIALIZED VIEW {view} AS SELECT uuid, body FROM msg_push"),
    )?;
    Ok(fdw)
}

fn view_rows(conn: &Arc<turso_core::Connection>, view: &str) -> Vec<(String, String)> {
    conn.exec_rows(&format!("SELECT uuid, body FROM {view} ORDER BY uuid"))
}

/// The acceptance property: a row appearing at the source reaches the view with
/// no `REFRESH` at all.
#[turso_macros::test(views)]
fn test_push_appends_reach_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &conn, "mv_push_add", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push(insert("m2", "two"));
    fdw.push(insert("m3", "three"));
    conn.drain_fdw_stream("msg_push", &stream)?;

    assert_eq!(
        view_rows(&conn, "mv_push_add"),
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// A retraction reaches the view the same way.
#[turso_macros::test(views)]
fn test_push_deletes_retract_from_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(
        &tmp_db,
        &conn,
        "mv_push_del",
        vec![row("m1", "one"), row("m2", "two")],
    )?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push(delete("m1", "one"));
    conn.drain_fdw_stream("msg_push", &stream)?;

    assert_eq!(
        view_rows(&conn, "mv_push_del"),
        vec![("m2".to_string(), "two".to_string())]
    );
    Ok(())
}

/// Inserts and deletes in one batch, including a replacement of a row that
/// stays: the batch is applied in order and lands as one commit.
#[turso_macros::test(views)]
fn test_push_mixed_batch(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(
        &tmp_db,
        &conn,
        "mv_push_mixed",
        vec![row("m1", "one"), row("m2", "two")],
    )?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push(delete("m1", "one"));
    fdw.push(insert("m2", "two v2"));
    fdw.push(insert("m3", "three"));
    conn.drain_fdw_stream("msg_push", &stream)?;

    assert_eq!(
        view_rows(&conn, "mv_push_mixed"),
        vec![
            ("m2".to_string(), "two v2".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// After a push the mirror already agrees with the source, so a `REFRESH` is a
/// no-op — the sweep finds nothing to do and the view does not move. This is
/// what says the push wrote the mirror the sweep would have written, rather
/// than something merely equivalent downstream.
#[turso_macros::test(views)]
fn test_refresh_after_push_is_inert(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &conn, "mv_push_inert", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push(insert("m2", "two"));
    conn.drain_fdw_stream("msg_push", &stream)?;
    let after_push = view_rows(&conn, "mv_push_inert");

    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_push_inert")?;
    assert_eq!(
        view_rows(&conn, "mv_push_inert"),
        after_push,
        "a sweep after a push must find the mirror already in step"
    );
    Ok(())
}

/// A push interleaved with local writes commits with them, so a view joining
/// both sides never shows one without the other.
#[turso_macros::test(views)]
fn test_push_joins_an_open_transaction(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &conn, "mv_push_txn", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push(insert("m2", "two"));
    common::run_query(&tmp_db, &conn, "BEGIN")?;
    conn.drain_fdw_stream("msg_push", &stream)?;
    common::run_query(&tmp_db, &conn, "ROLLBACK")?;

    assert_eq!(
        view_rows(&conn, "mv_push_txn"),
        vec![("m1".to_string(), "one".to_string())],
        "a push inside a transaction the caller opened must roll back with it"
    );

    // And the same push, committed, does arrive.
    fdw.push(insert("m3", "three"));
    conn.drain_fdw_stream("msg_push", &stream)?;
    assert_eq!(
        view_rows(&conn, "mv_push_txn"),
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// A push carrying a row the view's predicate excludes is mirrored but does not
/// reach the view: the compiled circuit re-applies the predicate to every
/// runtime delta, so a driver need not know the view's scope to push safely.
#[turso_macros::test(views)]
fn test_push_outside_the_predicate_does_not_reach_the_view(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_push(uuid TEXT, body TEXT)", vec![0]);
    rows.set(vec![row("m1", "keep")]);
    conn.register_foreign_table("msg_push", fdw.clone())?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_push_pred AS \
         SELECT uuid, body FROM msg_push WHERE body = 'keep'",
    )?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push(insert("m2", "drop"));
    fdw.push(insert("m3", "keep"));
    conn.drain_fdw_stream("msg_push", &stream)?;

    assert_eq!(
        view_rows(&conn, "mv_push_pred"),
        vec![
            ("m1".to_string(), "keep".to_string()),
            ("m3".to_string(), "keep".to_string()),
        ]
    );
    Ok(())
}

/// A push must never be half-visible. While the pushing transaction is open, a
/// concurrent reader still sees the pre-push view; when it commits, the reader
/// sees the whole batch.
#[turso_macros::test(views)]
fn test_push_is_all_or_nothing_to_a_concurrent_reader(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let writer = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &writer, "mv_push_atomic", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;
    let reader = tmp_db.connect_limbo();

    assert_eq!(view_rows(&reader, "mv_push_atomic").len(), 1);

    fdw.push(insert("m2", "two"));
    fdw.push(insert("m3", "three"));
    common::run_query(&tmp_db, &writer, "BEGIN")?;
    writer.drain_fdw_stream("msg_push", &stream)?;
    assert_eq!(
        view_rows(&reader, "mv_push_atomic").len(),
        1,
        "an uncommitted push must be invisible to another connection"
    );

    common::run_query(&tmp_db, &writer, "COMMIT")?;
    assert_eq!(
        view_rows(&reader, "mv_push_atomic"),
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
            ("m3".to_string(), "three".to_string()),
        ],
        "the whole batch must appear at once"
    );
    Ok(())
}

/// A batch that fails partway through inside a transaction the caller opened
/// must leave none of itself behind — the same all-or-nothing the engine gives
/// a batch it owns the transaction for. The caller's own transaction survives:
/// the failure retracts the batch, not the work the caller had already done and
/// not its ability to keep going.
#[turso_macros::test(views)]
fn test_failed_push_inside_a_caller_transaction_applies_nothing(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &conn, "mv_push_atomic_txn", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;
    common::run_query(&tmp_db, &conn, "CREATE TABLE local_notes (n TEXT)")?;

    // Good, then a row whose declared identity is NULL, then good again: the
    // batch fails in the middle with rows applied on either side of the failure.
    fdw.push_raw(insert("m2", "two"));
    fdw.push_raw(FdwChange {
        values: vec![Value::Null, text("bad")],
        weight: 1,
    });
    fdw.push_raw(insert("m3", "three"));

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "INSERT INTO local_notes VALUES ('keepme')")?;
    let failed = conn.drain_fdw_stream("msg_push", &stream);
    assert!(failed.is_err(), "a NULL identity must fail the batch");

    assert_eq!(
        view_rows(&conn, "mv_push_atomic_txn"),
        vec![("m1".to_string(), "one".to_string())],
        "a failed batch must leave none of itself in the open transaction"
    );

    // The caller's transaction is still its own: it can keep writing and commit.
    common::run_query(&tmp_db, &conn, "INSERT INTO local_notes VALUES ('after')")?;
    common::run_query(&tmp_db, &conn, "COMMIT")?;

    let notes: Vec<(String,)> = conn.exec_rows("SELECT n FROM local_notes ORDER BY n");
    assert_eq!(
        notes,
        vec![("after".to_string(),), ("keepme".to_string(),)],
        "the caller's own writes must survive a failed push"
    );
    assert_eq!(
        view_rows(&conn, "mv_push_atomic_txn"),
        vec![("m1".to_string(), "one".to_string())],
        "committing after a failed push must not persist half the batch"
    );
    Ok(())
}

/// A retraction carrying fewer values than the identity needs is a malformed
/// push, and must be named as one. Indexing past the end of the payload is not
/// a diagnosis.
#[turso_macros::test(views)]
fn test_push_retraction_narrower_than_the_identity_is_refused(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &conn, "mv_push_narrow_del", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push_raw(FdwChange {
        values: Vec::new(),
        weight: -1,
    });
    let err = conn
        .drain_fdw_stream("msg_push", &stream)
        .expect_err("a retraction with no identity values must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("msg_push"),
        "the error must name the foreign table: {msg}"
    );

    assert_eq!(
        view_rows(&conn, "mv_push_narrow_del"),
        vec![("m1".to_string(), "one".to_string())],
        "a refused push must leave nothing behind"
    );
    Ok(())
}

/// An insert carrying fewer values than the mirror has columns is malformed
/// too. Binding what arrived and letting the rest default to NULL would let a
/// bad push fabricate cells the source never had.
#[turso_macros::test(views)]
fn test_push_insert_narrower_than_the_mirror_is_refused(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &conn, "mv_push_narrow_ins", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    fdw.push_raw(FdwChange {
        values: vec![text("m2")],
        weight: 1,
    });
    let err = conn
        .drain_fdw_stream("msg_push", &stream)
        .expect_err("an insert narrower than the mirror must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("msg_push"),
        "the error must name the foreign table: {msg}"
    );

    assert_eq!(
        view_rows(&conn, "mv_push_narrow_ins"),
        vec![("m1".to_string(), "one".to_string())],
        "a malformed push must not reach the view with NULLs for what it omitted"
    );
    Ok(())
}

/// A push contending with a local writer takes the ordinary write lock: it is
/// refused while the lock is held, and applies cleanly once it is released.
/// Nothing about it is special-cased, which is the point — it is a writer like
/// any other.
#[turso_macros::test(views)]
fn test_push_contends_with_a_local_writer(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let pusher = tmp_db.connect_limbo();
    let fdw = setup(&tmp_db, &pusher, "mv_push_lock", vec![row("m1", "one")])?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    let local = tmp_db.connect_limbo();
    common::run_query(&tmp_db, &local, "CREATE TABLE notes (n TEXT)")?;
    common::run_query(&tmp_db, &local, "BEGIN IMMEDIATE")?;
    common::run_query(&tmp_db, &local, "INSERT INTO notes VALUES ('held')")?;

    fdw.push(insert("m2", "two"));
    let refused = pusher.drain_fdw_stream("msg_push", &stream);
    assert!(
        refused.is_err(),
        "a push must not proceed past a held write lock"
    );
    assert_eq!(
        view_rows(&pusher, "mv_push_lock").len(),
        1,
        "a refused push must leave nothing behind"
    );

    common::run_query(&tmp_db, &local, "COMMIT")?;
    // The change is still the driver's to replay; the engine dropped it with
    // the transaction it could not open.
    fdw.push(insert("m2", "two"));
    pusher.drain_fdw_stream("msg_push", &stream)?;
    assert_eq!(
        view_rows(&pusher, "mv_push_lock"),
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
        ],
        "once the lock is free the same push must apply"
    );
    Ok(())
}
