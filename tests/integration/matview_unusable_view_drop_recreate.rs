//! A stored materialized view whose SELECT no longer compiles against the
//! current base table is DEGRADED, not fatal: the database opens, and
//! `DROP VIEW` + `CREATE MATERIALIZED VIEW` repairs it in place.
//!
//! The repair half is what makes the boot half worth anything. Before the fix,
//! `translate_drop_view` decided a view existed by consulting `broken_views`
//! (rows whose SQL failed to PARSE) and `materialized_view_names` (which a view
//! that failed to COMPILE never joins). A view in `incompatible_views` matched
//! neither, so `DROP VIEW IF EXISTS` silently did nothing and the following
//! `CREATE MATERIALIZED VIEW IF NOT EXISTS` appended a SECOND row for the same
//! name to `sqlite_schema`. Reconciliation corrupted the schema instead of
//! fixing it.
//!
//! The skew is injected through SQLite's `writable_schema`, which is the only
//! way to reach the state a real database gets into: `ALTER TABLE` refuses to
//! touch a table that has dependent matviews, so no DDL sequence can leave a
//! stored `block` its own `block_raw` cannot satisfy. Binaries of different
//! versions get there anyway.

use crate::common::{limbo_exec_rows, TempDatabase};
use std::path::Path;
use std::sync::Arc;
use turso_core::Connection;

const CURRENT_BLOCK_SQL: &str =
    "CREATE MATERIALIZED VIEW block AS SELECT id, parent_id FROM block_raw";

/// What an older binary left behind: it selects `depth`, a column the current
/// `block_raw` does not have.
const STALE_BLOCK_SQL: &str =
    "CREATE MATERIALIZED VIEW block AS SELECT id, parent_id, depth FROM block_raw";

fn open(path: &Path) -> (TempDatabase, Arc<Connection>) {
    let db = TempDatabase::builder()
        .with_db_path(path)
        .with_views(true)
        .build();
    let conn = db.connect_limbo();
    (db, conn)
}

fn block_view_definitions(conn: &Arc<Connection>) -> Vec<String> {
    limbo_exec_rows(
        conn,
        "SELECT sql FROM sqlite_master WHERE type='view' AND name='block'",
    )
    .into_iter()
    .map(|row| match &row[0] {
        rusqlite::types::Value::Text(sql) => sql.clone(),
        other => panic!("unexpected sql value: {other:?}"),
    })
    .collect()
}

/// Replaces the stored `block` definition with one that cannot compile, without
/// touching the base table or the view's data.
fn write_stale_definition(path: &Path) {
    let sqlite = rusqlite::Connection::open(path).expect("open with sqlite");
    sqlite
        .pragma_update(None, "writable_schema", "ON")
        .expect("enable writable_schema");
    let updated = sqlite
        .execute(
            "UPDATE sqlite_master SET sql = ?1 WHERE type='view' AND name='block'",
            [STALE_BLOCK_SQL],
        )
        .expect("rewrite the stored definition");
    assert_eq!(updated, 1, "expected exactly one `block` row to rewrite");
    sqlite
        .pragma_update(None, "writable_schema", "OFF")
        .expect("disable writable_schema");
    drop(sqlite);
}

#[test]
fn unusable_view_is_dropped_and_recreated_without_duplicating_its_schema_row() -> anyhow::Result<()>
{
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("unusable_view.db");

    {
        let (db, conn) = open(&path);
        conn.execute("CREATE TABLE block_raw (id INTEGER PRIMARY KEY, parent_id INTEGER)")?;
        conn.execute("INSERT INTO block_raw VALUES (1, NULL), (2, 1), (3, 1)")?;
        conn.execute(CURRENT_BLOCK_SQL)?;
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT count(*) FROM block").len(),
            1
        );
        drop(conn);
        drop(db);
    }

    write_stale_definition(&path);

    // Part 1: the open must return. One view that no longer compiles is a
    // degraded view, never a reason to refuse the whole database.
    let (_db, conn) = open(&path);
    assert_eq!(
        block_view_definitions(&conn),
        vec![STALE_BLOCK_SQL.to_string()],
        "the skew should be in the file, exactly once"
    );

    // Part 2: DROP must delete the row of a view the engine could not load.
    conn.execute("DROP VIEW IF EXISTS block")?;
    assert!(
        block_view_definitions(&conn).is_empty(),
        "DROP VIEW left the unusable view's schema row behind"
    );

    conn.execute(CURRENT_BLOCK_SQL)?;
    assert_eq!(
        block_view_definitions(&conn),
        vec![CURRENT_BLOCK_SQL.to_string()],
        "recreating over a dropped unusable view must leave exactly one row"
    );

    // The repaired view is a working view, not just a schema row.
    let rows = limbo_exec_rows(&conn, "SELECT count(*) FROM block");
    assert_eq!(
        rows[0][0],
        rusqlite::types::Value::Integer(3),
        "the recreated view did not populate from its base table"
    );
    Ok(())
}

/// The same repair through the idempotent DDL a reconciler actually issues.
/// `CREATE ... IF NOT EXISTS` is the shape that produced the duplicate row.
#[test]
fn create_if_not_exists_after_dropping_an_unusable_view_creates_exactly_one() -> anyhow::Result<()>
{
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("unusable_view_idempotent.db");

    {
        let (db, conn) = open(&path);
        conn.execute("CREATE TABLE block_raw (id INTEGER PRIMARY KEY, parent_id INTEGER)")?;
        conn.execute("INSERT INTO block_raw VALUES (1, NULL), (2, 1)")?;
        conn.execute(CURRENT_BLOCK_SQL)?;
        drop(conn);
        drop(db);
    }

    write_stale_definition(&path);

    let (_db, conn) = open(&path);
    conn.execute("DROP VIEW IF EXISTS block")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS block AS SELECT id, parent_id FROM block_raw",
    )?;

    assert_eq!(
        block_view_definitions(&conn).len(),
        1,
        "the reconciler's DROP + CREATE IF NOT EXISTS duplicated the schema row"
    );

    let rows = limbo_exec_rows(&conn, "SELECT count(*) FROM block");
    assert_eq!(rows[0][0], rusqlite::types::Value::Integer(2));
    Ok(())
}
