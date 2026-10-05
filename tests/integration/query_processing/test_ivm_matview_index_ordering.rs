//! Reading an indexed materialized view through the index: ordering, grouping
//! and outer-join NULL padding.
//!
//! `CREATE INDEX` on a matview changes which access path the planner picks —
//! a real index instead of the ephemeral index it used to build for the inner
//! side of a join. That reaches `Cursor::MaterializedView` code paths nothing
//! reached before, so these tests hold the READ side of an indexed matview to
//! the same answers a plain table gives.
//!
//! Every test is a differential: the same query over a matview and over a
//! rowid table holding the same rows, each with the same index, plus the same
//! query with the index defeated (`+col`) as a recompute oracle.

use std::sync::Arc;

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

const ROWS: [(&str, &str); 6] = [
    ("b0", "TODO"),
    ("b1", "TODO"),
    ("b2", "TODO"),
    ("b3", "TODO"),
    ("b4", "TODO"),
    ("b8", "DONE"),
];

/// `v` is the matview, `t` a rowid table with the same rows. Both carry an
/// index on `id`, so the two arms differ ONLY in matview-ness.
fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE TABLE t (rid INTEGER PRIMARY KEY, id TEXT, st TEXT)")?;
    conn.execute(
        "CREATE TABLE req (block_id TEXT, required_id TEXT, \
         PRIMARY KEY (block_id, required_id))",
    )?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    for (i, (id, st)) in ROWS.iter().enumerate() {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('{id}', '{st}')"
        ))?;
        conn.execute(&format!(
            "INSERT INTO t (rid, id, st) VALUES ({}, '{id}', '{st}')",
            i + 1
        ))?;
    }
    conn.execute("INSERT INTO req (block_id, required_id) VALUES ('b0', 'b8')")?;
    conn.execute("CREATE INDEX idx_v_id ON v(id)")?;
    conn.execute("CREATE INDEX idx_t_id ON t(id)")?;
    Ok(())
}

fn rows(conn: &Arc<turso_core::Connection>, sql: &str) -> Vec<Vec<Value>> {
    limbo_exec_rows(conn, sql)
}

/// Run `sql` with `@B@` bound to the matview, to the control table, and to the
/// matview with the index defeated. All three must agree.
fn assert_matview_matches_table(
    conn: &Arc<turso_core::Connection>,
    what: &str,
    sql_template: &str,
) {
    let view = rows(conn, &sql_template.replace("@B@", "v"));
    let table = rows(conn, &sql_template.replace("@B@", "t"));
    assert_eq!(
        view, table,
        "{what}: the matview answer differs from the same query over a plain \
         rowid table with the same rows and the same index"
    );
    assert!(!view.is_empty(), "{what}: the fixture must return rows");
}

/// The root case: on a LEFT JOIN miss the matview side must be NULL, not the
/// previous row's value. Everything else in this file is downstream of it.
#[turso_macros::test(views)]
fn left_join_miss_nulls_the_matview_side(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    let sql = "SELECT b.id, br.required_id, bl.st \
               FROM @B@ b \
               LEFT JOIN req br ON br.block_id = b.id \
               LEFT JOIN @B@ bl ON bl.id = br.required_id \
               WHERE b.st = 'TODO' ORDER BY b.id";
    assert_matview_matches_table(&conn, "LEFT JOIN miss", sql);

    // Spelled out, so a failure names the defect rather than a row diff: only
    // b0 has a `req` row, so every other row's joined columns must be NULL.
    let view = rows(&conn, &sql.replace("@B@", "v"));
    for row in &view {
        let Value::Text(id) = &row[0] else {
            panic!("unexpected id {row:?}")
        };
        if id == "b0" {
            continue;
        }
        assert_eq!(
            row[1],
            Value::Null,
            "{id}: unmatched LEFT JOIN must give NULL required_id, got {row:?}"
        );
        assert_eq!(
            row[2],
            Value::Null,
            "{id}: unmatched LEFT JOIN must give NULL on the MATVIEW side — a \
             non-NULL here is the previous row's value leaking through the \
             materialized-view cursor, got {row:?}"
        );
    }
    Ok(())
}

/// The shape the Holon `now_for_agent` / antijoin queries use: GROUP BY the
/// indexed column with a HAVING over the outer-joined side.
#[turso_macros::test(views)]
fn group_by_indexed_column_over_left_join(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    let sql = "SELECT b.id FROM @B@ b \
               LEFT JOIN req br ON br.block_id = b.id \
               LEFT JOIN @B@ bl ON bl.id = br.required_id \
               WHERE b.st = 'TODO' \
               GROUP BY b.id \
               HAVING count(br.required_id) = sum(iif(COALESCE(bl.st,'') = 'DONE', 1, 0)) \
               ORDER BY b.id";
    assert_matview_matches_table(&conn, "GROUP BY over LEFT JOIN", sql);

    assert_eq!(
        rows(&conn, &sql.replace("@B@", "v")),
        ["b0", "b1", "b2", "b3", "b4"]
            .iter()
            .map(|s| vec![Value::Text((*s).into())])
            .collect::<Vec<_>>(),
        "every TODO whose requirements are all DONE (or absent) must survive HAVING"
    );
    Ok(())
}

/// GROUP BY / ORDER BY / DISTINCT / min-max directly on the indexed column.
/// The planner may elide a sorter because the index scan is ordered; these
/// pin that the rows really do come back ordered and grouped.
#[turso_macros::test(views)]
fn ordering_operators_on_the_indexed_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    for (what, sql) in [
        (
            "GROUP BY",
            "SELECT id, count(*) FROM @B@ GROUP BY id ORDER BY id",
        ),
        ("ORDER BY asc", "SELECT id FROM @B@ ORDER BY id"),
        ("ORDER BY desc", "SELECT id FROM @B@ ORDER BY id DESC"),
        ("DISTINCT", "SELECT DISTINCT id FROM @B@ ORDER BY id"),
        ("min/max", "SELECT min(id), max(id) FROM @B@"),
        (
            "GROUP BY with filter",
            "SELECT id, count(*) FROM @B@ WHERE st = 'TODO' GROUP BY id ORDER BY id",
        ),
        (
            "GROUP BY a non-indexed column",
            "SELECT st, count(*) FROM @B@ GROUP BY st ORDER BY st",
        ),
        (
            "ORDER BY with a range on the indexed column",
            "SELECT id, st FROM @B@ WHERE id >= 'b1' ORDER BY id",
        ),
    ] {
        assert_matview_matches_table(&conn, what, sql);
    }

    // The index must not change the answer: same query, index defeated.
    assert_eq!(
        rows(&conn, "SELECT id FROM v ORDER BY id"),
        rows(&conn, "SELECT +id FROM v ORDER BY +id"),
        "an ORDER BY served from the matview index must equal the sorted recompute"
    );
    Ok(())
}

/// A self-join on the indexed matview with no aggregation: the inner side is
/// reached through the index and a DeferredSeek into the view's btree.
#[turso_macros::test(views)]
fn self_join_through_the_matview_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    for (what, sql) in [
        (
            "INNER self-join",
            "SELECT a.id, b.st FROM @B@ a JOIN @B@ b ON b.id = a.id ORDER BY a.id",
        ),
        (
            "LEFT self-join on a key that mostly misses",
            "SELECT a.id, b.id, b.st FROM @B@ a \
             LEFT JOIN @B@ b ON b.id = 'b' || (CAST(substr(a.id, 2) AS INTEGER) + 4) \
             ORDER BY a.id",
        ),
    ] {
        assert_matview_matches_table(&conn, what, sql);
    }
    Ok(())
}

/// The defect must stay fixed after the view's contents move under the index.
#[turso_macros::test(views)]
fn left_join_miss_stays_null_after_churn(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    for step in 0..12 {
        conn.execute(&format!(
            "UPDATE t_raw SET st = '{}' WHERE id = 'b{}'",
            if step % 2 == 0 { "DONE" } else { "TODO" },
            step % 5
        ))?;
        conn.execute(&format!(
            "UPDATE t SET st = '{}' WHERE id = 'b{}'",
            if step % 2 == 0 { "DONE" } else { "TODO" },
            step % 5
        ))?;

        let sql = "SELECT b.id, bl.st FROM @B@ b \
                   LEFT JOIN req br ON br.block_id = b.id \
                   LEFT JOIN @B@ bl ON bl.id = br.required_id \
                   ORDER BY b.id";
        assert_matview_matches_table(&conn, &format!("after churn step {step}"), sql);
    }
    Ok(())
}
