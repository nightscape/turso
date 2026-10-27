//! A bulk INSERT of 300 rows in one transaction must reach a materialized view
//! over a recursive CTE.

use crate::common::TempDatabase;

#[turso_macros::test(views)]
fn test_bulk_insert_with_recursive_matview_no_pager_panic(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY, parent_id TEXT, content TEXT DEFAULT '',
            content_type TEXT DEFAULT 'text', properties TEXT DEFAULT '{}'
        )",
    )?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;

    conn.execute(
        "CREATE MATERIALIZED VIEW block_tree AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM block WHERE parent_id LIKE 'doc:%'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id
             FROM block b INNER JOIN paths p ON b.parent_id = p.id
         ) SELECT * FROM paths",
    )?;

    // Bulk insert: 256+ blocks in a single transaction
    conn.execute("BEGIN TRANSACTION")?;
    for i in 0..300 {
        conn.execute(format!(
            "INSERT INTO block VALUES ('block-{i}', 'doc:test', 'Content {i}', 'text', '{{}}')"
        ))?;
    }
    conn.execute("COMMIT")?;

    let rows = crate::common::limbo_exec_rows(&conn, "SELECT COUNT(*) FROM block_tree");
    let count = match &rows[0][0] {
        rusqlite::types::Value::Integer(n) => *n,
        _ => 0,
    };
    assert_eq!(count, 300, "recursive matview should have all 300 rows");

    Ok(())
}
