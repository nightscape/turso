//! A materialized view over a recursive CTE must never persist a truncated answer.
//!
//! `RecursiveOperator::process_iteration_result` used to log a `tracing::warn!` and report
//! `done = true` when the fixed-point iteration hit its guard. The rows accumulated so far
//! were then finalized and written into the matview's btree, so a recursion deeper than the
//! guard produced a durable, silently wrong view and `CREATE MATERIALIZED VIEW` reported
//! success. Nothing downstream could distinguish "converged" from "gave up".
//!
//! These tests assert the invariant rather than the guard's numeric value: for a chain of
//! `CHAIN_LEN` nodes the view must either contain the complete answer or the statement must
//! fail. A partially-populated view that reports success is the bug.

use turso::Value;

/// Deep enough to exceed a small guard, cheap enough to build in a debug test.
const CHAIN_LEN: i64 = 200;

const VIEW_SQL: &str = "CREATE MATERIALIZED VIEW chain AS \
     WITH RECURSIVE walk(id, depth) AS ( \
       SELECT id, 0 FROM node WHERE parent IS NULL \
       UNION ALL \
       SELECT n.id, w.depth + 1 FROM node n JOIN walk w ON n.parent = w.id \
     ) \
     SELECT id, depth FROM walk";

async fn seed_chain(conn: &turso::Connection) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE node (id INTEGER PRIMARY KEY, parent INTEGER)",
        (),
    )
    .await?;
    conn.execute("INSERT INTO node VALUES (1, NULL)", ())
        .await?;
    for id in 2..=CHAIN_LEN {
        conn.execute(
            "INSERT INTO node VALUES (?, ?)",
            (Value::Integer(id), Value::Integer(id - 1)),
        )
        .await?;
    }
    Ok(())
}

async fn scalar(conn: &turso::Connection, sql: &str) -> anyhow::Result<i64> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows.next().await?.expect("one row");
    Ok(row.get::<i64>(0)?)
}

/// The view is populated at CREATE time. Either that statement fails, or the view holds the
/// whole chain — never a silent prefix of it.
#[tokio::test]
async fn recursive_matview_is_complete_or_fails() -> anyhow::Result<()> {
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    seed_chain(&conn).await?;

    match conn.execute(VIEW_SQL, ()).await {
        Ok(_) => {
            let rows = scalar(&conn, "SELECT count(*) FROM chain").await?;
            let max_depth = scalar(&conn, "SELECT max(depth) FROM chain").await?;
            assert_eq!(
                rows, CHAIN_LEN,
                "CREATE MATERIALIZED VIEW reported success but the view is truncated \
                 ({rows} of {CHAIN_LEN} rows, max depth {max_depth})"
            );
            assert_eq!(max_depth, CHAIN_LEN - 1, "view is missing the deepest rows");
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("did not converge"),
                "expected a non-convergence error, got: {msg}"
            );
            // The failed statement must not leave a half-built view behind.
            let leftover = conn.query("SELECT count(*) FROM chain", ()).await;
            if let Ok(mut rows) = leftover {
                if let Some(row) = rows.next().await? {
                    assert_eq!(
                        row.get::<i64>(0)?,
                        0,
                        "failed CREATE MATERIALIZED VIEW left truncated rows behind"
                    );
                }
            }
        }
    }
    Ok(())
}

/// A guard breach during *incremental maintenance* (rather than initial population) must not
/// leave the view half-updated. The view is built over a chain short enough to converge, then a
/// single statement extends the chain far enough to breach the guard: either the maintenance
/// succeeds in full, or it fails and the view still reads exactly its pre-statement contents.
#[tokio::test]
async fn recursive_matview_maintenance_failure_leaves_view_unchanged() -> anyhow::Result<()> {
    const SHALLOW: i64 = 50;

    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    conn.execute(
        "CREATE TABLE node (id INTEGER PRIMARY KEY, parent INTEGER)",
        (),
    )
    .await?;
    conn.execute("INSERT INTO node VALUES (1, NULL)", ())
        .await?;
    for id in 2..=SHALLOW {
        conn.execute(
            "INSERT INTO node VALUES (?, ?)",
            (Value::Integer(id), Value::Integer(id - 1)),
        )
        .await?;
    }
    conn.execute(VIEW_SQL, ()).await?;
    assert_eq!(scalar(&conn, "SELECT count(*) FROM chain").await?, SHALLOW);

    // One statement that deepens the chain by far more than a small guard allows.
    let values: Vec<String> = (SHALLOW + 1..=SHALLOW + 200)
        .map(|id| format!("({id}, {})", id - 1))
        .collect();
    let extend = format!("INSERT INTO node VALUES {}", values.join(", "));

    match conn.execute(&extend, ()).await {
        Ok(_) => {
            assert_eq!(
                scalar(&conn, "SELECT count(*) FROM chain").await?,
                SHALLOW + 200,
                "maintenance reported success but the view is truncated"
            );
        }
        Err(e) => {
            assert!(
                e.to_string().contains("did not converge"),
                "expected a non-convergence error, got: {e}"
            );
            assert_eq!(
                scalar(&conn, "SELECT count(*) FROM chain").await?,
                SHALLOW,
                "failed maintenance left the view partially updated"
            );
            assert_eq!(
                scalar(&conn, "SELECT max(depth) FROM chain").await?,
                SHALLOW - 1,
                "failed maintenance left the view partially updated"
            );
            assert_eq!(
                scalar(&conn, "SELECT count(*) FROM node").await?,
                SHALLOW,
                "the failing statement was not rolled back"
            );
        }
    }
    Ok(())
}

/// Same invariant across a reopen: whatever ends up on disk must not be a truncated answer
/// that a later session reads back as authoritative.
#[tokio::test]
async fn recursive_matview_is_complete_or_absent_after_reopen() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("ivm-recursive-nonconvergence.db");
    let db_path = db_path.to_str().unwrap();

    let created = {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        seed_chain(&conn).await?;
        let created = conn.execute(VIEW_SQL, ()).await.is_ok();
        drop(conn);
        drop(db);
        created
    };

    if !created {
        return Ok(());
    }

    let db = turso::Builder::new_local(db_path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    let rows = scalar(&conn, "SELECT count(*) FROM chain").await?;
    assert_eq!(
        rows, CHAIN_LEN,
        "reopened matview persists a truncated answer ({rows} of {CHAIN_LEN} rows)"
    );
    Ok(())
}
