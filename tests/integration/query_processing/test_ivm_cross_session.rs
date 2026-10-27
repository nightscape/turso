//! Test for IVM cross-session negative weight bug
//!
//! When a recursive CTE materialized view is loaded from disk (persisted from
//! a previous session), INSERT OR REPLACE operations corrupt IVM weights.
//! Fresh (same-session) matviews work correctly.

use tempfile::TempDir;

#[tokio::test]
async fn test_cross_session_recursive_matview_insert_or_replace() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("cross-session.db");
    let db_path = db_path.to_str().unwrap();

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS
        WITH RECURSIVE tree AS (
            SELECT id, pid, val, '/' || id AS path FROM t WHERE pid = 'root'
            UNION ALL
            SELECT c.id, c.pid, c.val, p.path || '/' || c.id
            FROM t c INNER JOIN tree p ON c.pid = p.id
        ) SELECT * FROM tree";

    // Session 1: Create table, matview, insert tree data
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute(
            "CREATE TABLE t (id TEXT PRIMARY KEY, pid TEXT, val TEXT)",
            (),
        )
        .await?;
        conn.execute(matview_sql, ()).await?;

        for r in 0..4 {
            conn.execute(
                &format!("INSERT INTO t VALUES ('r{r}', 'root', 'H{r}')"),
                (),
            )
            .await?;
            for c in 0..5 {
                conn.execute(
                    &format!("INSERT INTO t VALUES ('r{r}_c{c}', 'r{r}', 'val_{r}_{c}')"),
                    (),
                )
                .await?;
            }
        }

        // Verify: 4 roots + 4*5 children = 24 rows
        let mut rows = conn.query("SELECT COUNT(*) FROM mv", ()).await?;
        let row = rows.next().await?.unwrap();
        let count: i64 = row.get(0)?;
        assert_eq!(count, 24, "Session 1: expected 24 rows in matview");

        drop(conn);
        drop(db);
    }

    // Session 2: Reopen DB, INSERT OR REPLACE all rows
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute(matview_sql, ()).await?;

        // Verify matview loaded from disk
        let mut rows = conn.query("SELECT COUNT(*) FROM mv", ()).await?;
        let row = rows.next().await?.unwrap();
        let count: i64 = row.get(0)?;
        assert_eq!(
            count, 24,
            "Session 2 (before modify): expected 24 rows from disk"
        );

        // INSERT OR REPLACE all rows with updated values
        for r in 0..4 {
            conn.execute(
                &format!("INSERT OR REPLACE INTO t VALUES ('r{r}', 'root', 'H{r}_v2')"),
                (),
            )
            .await?;
            for c in 0..5 {
                conn.execute(
                    &format!(
                        "INSERT OR REPLACE INTO t VALUES ('r{r}_c{c}', 'r{r}', 'val_{r}_{c}_v2')"
                    ),
                    (),
                )
                .await?;
            }
        }

        // This is the bug: should still be 24 rows, but cross-session
        // produces negative weights or lost rows
        let mut rows = conn.query("SELECT COUNT(*) FROM mv", ()).await?;
        let row = rows.next().await?.unwrap();
        let count: i64 = row.get(0)?;
        assert_eq!(
            count, 24,
            "Session 2 (after modify): expected 24 rows, got {count}"
        );

        drop(conn);
        drop(db);
    }

    Ok(())
}
