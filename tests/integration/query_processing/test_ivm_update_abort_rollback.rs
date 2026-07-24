//! Test: IVM matview must remain in sync with the base table when an UPDATE
//! statement aborts mid-way (e.g., due to a UNIQUE constraint violation).
//!
//! Bug: a multi-row UPDATE that fails on row N after successfully writing rows
//! 0..N-1 rolls back the table's btree atomically (the table reverts to its
//! pre-UPDATE state), but the IVM matview was already updated with deltas for
//! rows 0..N-1 and does not roll those back. The matview ends up out of sync
//! with the underlying table.
//!
//! Discovered by the differential fuzzer at seed 12371730315876910877 while
//! investigating the compound `IS NOT NULL AND IS NULL` filter — the failing
//! `UPDATE t SET focused_okeefe = <const>` ran into a UNIQUE index on that
//! column, exposing the rollback gap.

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

#[turso_macros::test(views)]
fn test_matview_rolls_back_on_unique_update_failure(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER)")?;
    conn.execute("CREATE UNIQUE INDEX idx_t_b ON t (b)")?;
    conn.execute("INSERT INTO t VALUES (1, NULL)")?;
    conn.execute("INSERT INTO t VALUES (2, NULL)")?;

    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT a, b FROM t")?;

    // Sanity: matview matches table.
    let mv_before = limbo_exec_rows(&conn, "SELECT a, b FROM v ORDER BY a");
    assert_eq!(mv_before.len(), 2, "matview should have 2 rows initially");

    // Mass UPDATE that fails: setting both rows to b=999 violates UNIQUE on b.
    let result = conn.execute("UPDATE t SET b = 999");
    assert!(
        result.is_err(),
        "UPDATE should fail with UNIQUE constraint, got {result:?}"
    );

    // The base table is correctly rolled back: both rows remain (1, NULL), (2, NULL).
    let table_after = limbo_exec_rows(&conn, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(table_after.len(), 2);
    assert_eq!(table_after[0][0], Value::Integer(1));
    assert_eq!(table_after[0][1], Value::Null);
    assert_eq!(table_after[1][0], Value::Integer(2));
    assert_eq!(table_after[1][1], Value::Null);

    // Bug: matview does NOT roll back — row 1 may now show b=999 even though
    // the table has b=NULL, or row 1 may be missing entirely with a filter view.
    let mv_after = limbo_exec_rows(&conn, "SELECT a, b FROM v ORDER BY a");
    assert_eq!(
        mv_after, table_after,
        "matview must match table state after UPDATE rollback"
    );

    Ok(())
}

#[turso_macros::test(views)]
fn test_matview_with_null_filter_rolls_back_on_unique_update_failure(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    // Original fuzzer scenario: matview with a compound `IS NOT NULL AND IS NULL`
    // WHERE clause, plus a UNIQUE index on the IS-NULL column.
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER)")?;
    conn.execute("CREATE UNIQUE INDEX idx_t_b ON t (b)")?;
    conn.execute("INSERT INTO t VALUES (1, NULL)")?;
    conn.execute("INSERT INTO t VALUES (2, NULL)")?;

    conn.execute(
        "CREATE MATERIALIZED VIEW v AS \
         SELECT a, b FROM t WHERE a IS NOT NULL AND b IS NULL",
    )?;

    let mv_before = limbo_exec_rows(&conn, "SELECT a, b FROM v ORDER BY a");
    assert_eq!(mv_before.len(), 2);

    let result = conn.execute("UPDATE t SET b = 999");
    assert!(result.is_err(), "UPDATE should fail with UNIQUE constraint");

    // Table is unchanged: both rows still have b=NULL.
    let table_after = limbo_exec_rows(&conn, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(table_after.len(), 2);
    assert!(matches!(table_after[0][1], Value::Null));
    assert!(matches!(table_after[1][1], Value::Null));

    // Matview filter `b IS NULL` still matches both rows; both should be present.
    let mv_after = limbo_exec_rows(&conn, "SELECT a, b FROM v ORDER BY a");
    assert_eq!(
        mv_after.len(),
        2,
        "matview should still contain both rows after UPDATE rollback, got {mv_after:?}"
    );
    assert_eq!(mv_after[0][0], Value::Integer(1));
    assert_eq!(mv_after[1][0], Value::Integer(2));

    Ok(())
}
