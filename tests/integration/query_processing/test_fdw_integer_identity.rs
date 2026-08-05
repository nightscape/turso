//! A foreign table whose single identity column is declared `INTEGER`.
//!
//! `PRIMARY KEY (id)` over an `INTEGER` column of a rowid table makes that
//! column an alias of the rowid, which has two consequences the mirror cannot
//! live with: no automatic index is created for it, and a NULL is auto-assigned
//! a rowid instead of being refused. The mirror needs both — the index so the
//! schema layer finds what the creation path wrote, the refusal so the identity
//! contract holds. These cases pin that an `INTEGER` identity behaves exactly
//! like a `TEXT` one.

use crate::common::{self, ExecRows, TempDatabase};
use crate::query_processing::fdw_test_driver::MemFdw;
use std::io::Write;
use turso_core::Value;

fn mirror_name(view: &str, source: &str) -> String {
    format!("__turso_internal_fdw_mirror_v1_{view}__{source}")
}

/// Foreign table `msg_int(id INTEGER, body TEXT)` over a CSV file, declaring
/// `id` as its identity.
fn setup_int_fdw(
    tmp_db: &TempDatabase,
    conn: &std::sync::Arc<turso_core::Connection>,
    csv_path: &std::path::Path,
    rows: &[(i64, &str)],
) {
    let mut f = std::fs::File::create(csv_path).unwrap();
    writeln!(f, "id,body").unwrap();
    for (id, body) in rows {
        writeln!(f, "{id},{body}").unwrap();
    }
    drop(f);

    common::run_query(tmp_db, conn, "CREATE SERVER csv_srv OPTIONS (driver 'csv')").unwrap();
    common::run_query(
        tmp_db,
        conn,
        &format!(
            "CREATE FOREIGN TABLE msg_int (id INTEGER, body TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true', identity 'id')",
            csv_path.display()
        ),
    )
    .unwrap();
}

/// The regression: creating the view writes a mirror plus an automatic index
/// for its primary key, and reopening the database reparses both. If the
/// mirror's identity column is a rowid alias the schema layer creates no index
/// to match the one on disk, and the reparse trips over the leftover.
#[test]
fn test_integer_identity_mirror_reparses_on_reopen() {
    let dir = tempfile::TempDir::new().unwrap().keep();
    let db_path = dir.join("int_identity.db");
    let csv_path = dir.join("int_identity.csv");
    let opts = turso_core::DatabaseOpts::new().with_views(true);

    {
        let db = TempDatabase::new_with_existent_with_opts(&db_path, opts.clone());
        let conn = db.connect_limbo();
        setup_int_fdw(&db, &conn, &csv_path, &[(1, "one"), (2, "two")]);
        common::run_query(
            &db,
            &conn,
            "CREATE MATERIALIZED VIEW mv_int AS SELECT id, body FROM msg_int",
        )
        .unwrap();
        let rows: Vec<(i64, String)> = conn.exec_rows("SELECT id, body FROM mv_int ORDER BY id");
        assert_eq!(
            rows,
            vec![(1, "one".to_string()), (2, "two".to_string())],
            "the view must be populated before the reopen means anything"
        );
        conn.close().unwrap();
    }

    {
        let db = TempDatabase::new_with_existent_with_opts(&db_path, opts);
        let conn = db.connect_limbo();

        let indexes: Vec<(String,)> = conn.exec_rows(&format!(
            "SELECT name FROM sqlite_schema WHERE type = 'index' AND tbl_name = '{}'",
            mirror_name("mv_int", "msg_int")
        ));
        assert_eq!(
            indexes,
            vec![(format!(
                "sqlite_autoindex_{}_1",
                mirror_name("mv_int", "msg_int")
            ),)],
            "the mirror's primary key must have exactly the automatic index the \
             schema layer expects"
        );

        let rows: Vec<(i64, String)> = conn.exec_rows("SELECT id, body FROM mv_int ORDER BY id");
        assert_eq!(rows, vec![(1, "one".to_string()), (2, "two".to_string())]);
        conn.close().unwrap();
    }
}

/// A NULL in an `INTEGER` identity must be refused, not silently handed a
/// generated rowid: a row the source cannot name again can never be updated or
/// retracted.
#[turso_macros::test(views)]
fn test_integer_identity_null_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_mem_int(id INTEGER, body TEXT)", vec![0]);
    rows.set(vec![vec![Value::Null, Value::build_text("orphan")]]);
    conn.register_foreign_table("msg_mem_int", fdw)?;

    let err = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_int_null AS SELECT id, body FROM msg_mem_int",
    )
    .expect_err("a NULL integer identity must be refused");
    let msg = err.to_string();

    assert!(
        msg.contains("msg_mem_int") && msg.contains("id") && msg.to_lowercase().contains("null"),
        "the error must diagnose a NULL identity on the source: {msg}"
    );
    assert!(
        !msg.contains("more than one row"),
        "a NULL identity is not a duplicate identity: {msg}"
    );
    Ok(())
}

/// The same refusal on the push path, where a rowid alias would quietly invent
/// an identity for the pushed row.
#[turso_macros::test(views)]
fn test_integer_identity_null_push_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_push_int(id INTEGER, body TEXT)", vec![0]);
    rows.set(vec![vec![Value::from_i64(1), Value::build_text("one")]]);
    conn.register_foreign_table("msg_push_int", fdw.clone())?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_int_push AS SELECT id, body FROM msg_push_int",
    )?;

    let err = conn
        .inject_fdw_changes(
            "msg_push_int",
            &[turso_core::foreign::FdwChange {
                values: vec![Value::Null, Value::build_text("orphan")],
                weight: 1,
            }],
        )
        .expect_err("a pushed NULL identity must be refused");
    assert!(
        err.to_string().to_lowercase().contains("null"),
        "the push must diagnose a NULL identity: {err}"
    );

    let rows: Vec<(i64, String)> = conn.exec_rows("SELECT id, body FROM mv_int_push ORDER BY id");
    assert_eq!(
        rows,
        vec![(1, "one".to_string())],
        "the refused push must leave the view untouched"
    );
    Ok(())
}

/// Pushed i64 identities must survive the round trip through the mirror as
/// integers, and the matview must track every batch.
///
/// The `INT` redeclaration that keeps the column off the rowid changes nothing
/// about its affinity, so a pushed identity must come back out of the view the
/// integer it went in as — not a string, and not the rowid the engine would
/// have generated for it. Large magnitudes are included because that is where
/// a lossy hop through `REAL` would show.
#[turso_macros::test(views)]
fn test_integer_identity_push_round_trips(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new(
        "CREATE TABLE msg_rt_int(id INTEGER, seq INTEGER, body TEXT)",
        vec![0],
    );
    let row = |id: i64, seq: i64, body: &str| {
        vec![
            Value::from_i64(id),
            Value::from_i64(seq),
            Value::build_text(body.to_string()),
        ]
    };
    let change =
        |values: Vec<Value>, weight: i64| turso_core::foreign::FdwChange { values, weight };
    // i64::MIN is left out: text of it converts lossily to REAL, a known defect
    // of the value path that has nothing to do with the mirror.
    const BIG: i64 = i64::MAX;
    const SMALL: i64 = i64::MIN + 1;

    rows.set(vec![row(1, 10, "one")]);
    conn.register_foreign_table("msg_rt_int", fdw.clone())?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_rt_int AS SELECT id, seq, body FROM msg_rt_int",
    )?;
    let stream = turso_core::foreign::StreamingForeignData::subscribe(fdw.as_ref(), &[])?;

    let view = |conn: &std::sync::Arc<turso_core::Connection>| -> Vec<(i64, i64, String)> {
        conn.exec_rows("SELECT id, seq, body FROM mv_rt_int ORDER BY id")
    };

    // Insert a batch spanning the whole i64 range the mirror must carry.
    fdw.push(change(row(BIG, 20, "biggest"), 1));
    fdw.push(change(row(SMALL, 30, "smallest"), 1));
    fdw.push(change(row(0, 40, "zero"), 1));
    conn.drain_fdw_stream("msg_rt_int", &stream)?;
    assert_eq!(
        view(&conn),
        vec![
            (SMALL, 30, "smallest".to_string()),
            (0, 40, "zero".to_string()),
            (1, 10, "one".to_string()),
            (BIG, 20, "biggest".to_string()),
        ],
        "pushed i64 identities must reach the view as the integers they were"
    );

    // Update by identity: the row is replaced, not duplicated.
    fdw.push(change(row(BIG, 21, "biggest v2"), 1));
    conn.drain_fdw_stream("msg_rt_int", &stream)?;
    assert_eq!(
        view(&conn).last().cloned(),
        Some((BIG, 21, "biggest v2".to_string())),
        "an update keyed on an i64 identity must land on the row it names"
    );
    assert_eq!(view(&conn).len(), 4, "an update must not add a row");

    // The engine must have stored integers, not text that merely prints alike.
    let types: Vec<(String, String)> =
        conn.exec_rows("SELECT typeof(id), typeof(seq) FROM mv_rt_int ORDER BY id LIMIT 1");
    assert_eq!(
        types,
        vec![("integer".to_string(), "integer".to_string())],
        "an INT-declared identity must keep integer affinity through the mirror"
    );

    // Delete by identity, at the extreme where a REAL round trip would miss.
    fdw.push(change(row(SMALL, 30, "smallest"), -1));
    conn.drain_fdw_stream("msg_rt_int", &stream)?;
    assert_eq!(
        view(&conn),
        vec![
            (0, 40, "zero".to_string()),
            (1, 10, "one".to_string()),
            (BIG, 21, "biggest v2".to_string()),
        ],
        "a retraction keyed on an i64 identity must remove exactly that row"
    );
    Ok(())
}

/// Two source rows sharing an `INTEGER` identity are still a duplicate, not two
/// rows the engine invents rowids for.
#[turso_macros::test(views)]
fn test_integer_identity_duplicate_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_dup_int(id INTEGER, body TEXT)", vec![0]);
    rows.set(vec![
        vec![Value::from_i64(1), Value::build_text("one")],
        vec![Value::from_i64(1), Value::build_text("two")],
    ]);
    conn.register_foreign_table("msg_dup_int", fdw)?;

    let err = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_int_dup AS SELECT id, body FROM msg_dup_int",
    )
    .expect_err("a repeated integer identity must be refused");
    assert!(
        err.to_string().contains("more than one row"),
        "a repeated identity must be diagnosed as a duplicate: {err}"
    );
    Ok(())
}
