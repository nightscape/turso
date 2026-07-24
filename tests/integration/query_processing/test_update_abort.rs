use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

#[turso_macros::test]
fn test_update_abort_unique_constraint(tmp_db: TempDatabase) -> anyhow::Result<()> {
    // When a multi-row UPDATE violates a UNIQUE constraint, the default ABORT
    // behavior should roll back ALL changes from that statement.
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (pk TEXT PRIMARY KEY, val INTEGER NOT NULL UNIQUE, data TEXT)")?;
    conn.execute("INSERT INTO t VALUES ('a', 1, 'original_a')")?;
    conn.execute("INSERT INTO t VALUES ('b', 2, 'original_b')")?;
    conn.execute("INSERT INTO t VALUES ('c', 3, 'original_c')")?;

    // This UPDATE tries to set val=99 for ALL 3 rows, violating UNIQUE on the 2nd row.
    let result = conn.execute("UPDATE t SET val = 99, data = 'updated'");
    assert!(result.is_err(), "UPDATE should fail with UNIQUE constraint");

    // All rows should still have their original values (ABORT rolls back entire statement)
    let rows = limbo_exec_rows(&conn, "SELECT pk, val, data FROM t ORDER BY pk");
    assert_eq!(rows.len(), 3, "Should still have 3 rows");
    assert_eq!(rows[0][0], Value::Text("a".to_string()));
    assert_eq!(rows[0][1], Value::Integer(1));
    assert_eq!(
        rows[0][2],
        Value::Text("original_a".to_string()),
        "Row 'a' should be unchanged after ABORT, but val={:?} data={:?}",
        rows[0][1],
        rows[0][2]
    );

    Ok(())
}

#[turso_macros::test]
fn test_update_abort_unique_in_explicit_tx(tmp_db: TempDatabase) -> anyhow::Result<()> {
    // Same test but inside an explicit transaction — this uses the subjournal path
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t2 (pk TEXT PRIMARY KEY, val INTEGER NOT NULL UNIQUE, data TEXT)")?;
    conn.execute("INSERT INTO t2 VALUES ('a', 1, 'original_a')")?;
    conn.execute("INSERT INTO t2 VALUES ('b', 2, 'original_b')")?;
    conn.execute("INSERT INTO t2 VALUES ('c', 3, 'original_c')")?;

    conn.execute("BEGIN")?;
    let result = conn.execute("UPDATE t2 SET val = 99, data = 'updated'");
    assert!(result.is_err(), "UPDATE should fail with UNIQUE constraint");
    // In ABORT mode, the transaction should still be active
    conn.execute("COMMIT")?;

    let rows = limbo_exec_rows(&conn, "SELECT pk, val, data FROM t2 ORDER BY pk");
    assert_eq!(rows.len(), 3, "Should still have 3 rows");
    assert_eq!(rows[0][0], Value::Text("a".to_string()));
    assert_eq!(
        rows[0][1],
        Value::Integer(1),
        "Row 'a' should be unchanged after ABORT in explicit tx, but val={:?}",
        rows[0][1]
    );

    Ok(())
}
