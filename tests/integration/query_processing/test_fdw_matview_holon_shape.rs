//! Reproducer for Holon's chat-view failure: `CREATE MATERIALIZED VIEW ... FROM
//! <foreign table>` with the exact clause shape Holon's watch path emits.
//!
//! `test_fdw_matview.rs` already proves the plain `SELECT * FROM <fdw>` shape
//! works. These cases add one clause at a time so the failing clause is named,
//! rather than "matviews don't work over FDWs".

use crate::common::{self, ExecRows, TempDatabase};
use crate::query_processing::fdw_test_driver::MemFdw;
use std::io::Write;
use turso_core::Value;

/// Foreign table `cc_message(uuid, session_id, role, content, timestamp)` over
/// a CSV file, mirroring the columns Holon's `cc_message_fdw` exposes.
fn setup_messages(tmp_db: &TempDatabase, conn: &std::sync::Arc<turso_core::Connection>) {
    let csv_path = tmp_db.path.parent().unwrap().join("messages.csv");
    let mut f = std::fs::File::create(&csv_path).unwrap();
    writeln!(f, "uuid,session_id,role,content,timestamp").unwrap();
    writeln!(f, "m1,s1,user,hello,100").unwrap();
    writeln!(f, "m2,s1,assistant,hi there,200").unwrap();
    writeln!(f, "m3,s2,user,other session,300").unwrap();
    drop(f);

    common::run_query(tmp_db, conn, "CREATE SERVER csv_srv OPTIONS (driver 'csv')").unwrap();
    common::run_query(
        tmp_db,
        conn,
        &format!(
            "CREATE FOREIGN TABLE cc_message_fdw \
             (uuid TEXT, session_id TEXT, role TEXT, content TEXT, timestamp TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true')",
            csv_path.display()
        ),
    )
    .unwrap();
}

/// Rung 1: `IF NOT EXISTS` on the CREATE. Holon always emits it.
#[turso_macros::test(views)]
fn test_fdw_matview_if_not_exists(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_messages(&tmp_db, &conn);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS mv_ine AS SELECT * FROM cc_message_fdw",
    )?;
    let rows: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM mv_ine");
    assert_eq!(rows.len(), 3);
    Ok(())
}

/// Rung 2: explicit projection plus a column alias (`timestamp AS ts`).
#[turso_macros::test(views)]
fn test_fdw_matview_projection_alias(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_messages(&tmp_db, &conn);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_alias AS \
         SELECT uuid, role, content, timestamp AS ts FROM cc_message_fdw",
    )?;
    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, ts FROM mv_alias");
    assert_eq!(rows.len(), 3);
    Ok(())
}

/// Rung 3: equality filter on a foreign-table column.
#[turso_macros::test(views)]
fn test_fdw_matview_where_equality(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_messages(&tmp_db, &conn);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_eq AS \
         SELECT uuid, role FROM cc_message_fdw WHERE session_id = 's1'",
    )?;
    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, role FROM mv_eq");
    assert_eq!(rows.len(), 2);
    Ok(())
}

/// Rung 4: `IS NOT NULL` conjunct on a foreign-table column.
#[turso_macros::test(views)]
fn test_fdw_matview_where_is_not_null(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_messages(&tmp_db, &conn);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_nn AS \
         SELECT uuid FROM cc_message_fdw WHERE role IS NOT NULL",
    )?;
    let rows: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM mv_nn");
    assert_eq!(rows.len(), 3);
    Ok(())
}

/// Rung 5: the full statement Holon's `MatviewManager` emits for the chat view
/// (ORDER BY already stripped by `strip_order_by` before it reaches Turso).
#[turso_macros::test(views)]
fn test_fdw_matview_full_holon_shape(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_messages(&tmp_db, &conn);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS watch_view_9ef7f36587fc903c AS \
         SELECT uuid, role, content, timestamp AS ts FROM cc_message_fdw \
         WHERE session_id = 's1' AND role IS NOT NULL",
    )?;
    let rows: Vec<(String, String, String, String)> =
        conn.exec_rows("SELECT uuid, role, content, ts FROM watch_view_9ef7f36587fc903c");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "m1");
    Ok(())
}

/// Rung 6: the property the chat view actually needs — the matview must STREAM.
/// A message arriving at the foreign source must appear in the matview WITHOUT
/// an explicit `REFRESH MATERIALIZED VIEW`. This is the IVM-over-FDW question.
///
/// The source is a push-capable driver rather than the CSV file the other rungs
/// use: streaming needs the source to *say* it changed, and a file cannot.
/// Reading the file on every query would make a `SELECT` take the write lock
/// and fire CDC mid-read, which is a bigger semantic commitment than this rung
/// is asking for. CSV therefore stays REFRESH-only, as
/// `test_refresh_matview_on_fdw` pins.
#[turso_macros::test(views)]
fn test_fdw_matview_streams_new_source_rows(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new(
        "CREATE TABLE cc_message_push(uuid TEXT, session_id TEXT, role TEXT, content TEXT, ts TEXT)",
        vec![0],
    );
    let message = |uuid: &str, ts: &str| {
        vec![
            Value::build_text(uuid.to_string()),
            Value::build_text("s1".to_string()),
            Value::build_text("user".to_string()),
            Value::build_text("hello".to_string()),
            Value::build_text(ts.to_string()),
        ]
    };
    rows.set(vec![
        message("m1", "100"),
        message("m2", "200"),
        message("m3", "300"),
    ]);
    conn.register_foreign_table("cc_message_push", fdw.clone())?;

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_stream AS SELECT uuid FROM cc_message_push",
    )?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;
    let seen: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM mv_stream");
    assert_eq!(seen.len(), 3);

    // A new message arrives at the foreign source.
    fdw.push(turso_core::foreign::FdwChange {
        values: message("m4", "400"),
        weight: 1,
    });
    conn.drain_fdw_stream("cc_message_push", &stream)?;

    let seen: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM mv_stream");
    assert_eq!(
        seen.len(),
        4,
        "matview over a foreign table did not pick up a new source row without REFRESH"
    );
    Ok(())
}
