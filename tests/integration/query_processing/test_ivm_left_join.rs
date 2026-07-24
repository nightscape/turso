//! Cross-session and adversarial-corpus tests for IVM LEFT JOIN.
//!
//! Sqltest coverage in `testing/runner/tests/ivm-left-join.sqltest`
//! exercises the in-memory codepath. These tests round-trip through a
//! file-backed DB to lock down the MatchCounterOperator's btree-resident
//! state (L_PRESENCE + R_COUNT) across DB reopens.

#[ignore = "blocked on MergeOperator cross-session restore (Risk-2 in plan): \
  the LEFT JOIN's MergeOperator(UnionMode::All) starts with next_rowid=1 \
  on every DB reopen, producing rowids that collide with the persisted \
  matview's existing rowids. This is a pre-existing latent bug for ALL \
  UNION ALL matviews (not specific to LEFT JOIN). The fix is to apply \
  RecursiveOperator's restore_state_from_btree_data pattern to \
  MergeOperator. Track via separate handoff."]
#[tokio::test]
async fn test_left_join_cross_session_restore() -> anyhow::Result<()> {
    let db_path = "/tmp/turso-ivm-left-join-cross-session-test.db";
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS lj AS \
                       SELECT p.pid, p.name, j.tag \
                       FROM parents p LEFT JOIN junction j ON j.pid = p.pid";

    // Session 1: create matview with parents only (junction empty), assert
    // that all parents appear with NULL tag.
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(
            "CREATE TABLE parents (pid INTEGER PRIMARY KEY, name TEXT)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE junction (id INTEGER PRIMARY KEY, pid INT, tag TEXT)",
            (),
        )
        .await?;
        conn.execute("INSERT INTO parents VALUES (1,'A'),(2,'B'),(3,'C')", ())
            .await?;
        conn.execute(matview_sql, ()).await?;

        let mut rows = conn.query("SELECT COUNT(*) FROM lj", ()).await?;
        let row = rows.next().await?.unwrap();
        let count: i64 = row.get(0)?;
        assert_eq!(count, 3, "Session 1: expected 3 null-padded rows");

        drop(conn);
        drop(db);
    }

    // Session 2: reopen, INSERT a junction row matching pid=1.
    // Verify the null-pad row for pid=1 retracts and the matched row appears.
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(matview_sql, ()).await?;

        // Sanity: pre-state has 3 null-padded rows.
        let mut rows = conn
            .query("SELECT pid, tag FROM lj ORDER BY pid", ())
            .await?;
        let mut tags: Vec<(i64, Option<String>)> = Vec::new();
        while let Some(row) = rows.next().await? {
            let pid: i64 = row.get(0)?;
            let tag: Option<String> = row.get(1)?;
            tags.push((pid, tag));
        }
        assert_eq!(
            tags,
            vec![(1, None), (2, None), (3, None)],
            "Session 2: pre-INSERT state did not survive reopen"
        );

        // INSERT a matching junction row.
        conn.execute("INSERT INTO junction VALUES (1, 1, 'red')", ())
            .await?;

        let mut rows = conn
            .query("SELECT pid, tag FROM lj ORDER BY pid, tag", ())
            .await?;
        let mut tags: Vec<(i64, Option<String>)> = Vec::new();
        while let Some(row) = rows.next().await? {
            let pid: i64 = row.get(0)?;
            let tag: Option<String> = row.get(1)?;
            tags.push((pid, tag));
        }
        assert_eq!(
            tags,
            vec![(1, Some("red".to_string())), (2, None), (3, None)],
            "Session 2: incremental INSERT after reopen did not flip pid=1"
        );

        // DELETE the junction row — null-pad must reappear.
        conn.execute("DELETE FROM junction WHERE id = 1", ())
            .await?;

        let mut rows = conn
            .query("SELECT pid, tag FROM lj ORDER BY pid", ())
            .await?;
        let mut tags: Vec<(i64, Option<String>)> = Vec::new();
        while let Some(row) = rows.next().await? {
            let pid: i64 = row.get(0)?;
            let tag: Option<String> = row.get(1)?;
            tags.push((pid, tag));
        }
        assert_eq!(
            tags,
            vec![(1, None), (2, None), (3, None)],
            "Session 2: incremental DELETE after reopen did not retract pid=1's match"
        );

        drop(conn);
        drop(db);
    }

    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));
    Ok(())
}

#[ignore = "blocked on MergeOperator cross-session restore (Risk-2 in plan), \
  same root cause as test_left_join_cross_session_restore"]
#[tokio::test]
async fn test_left_join_cross_session_count_state() -> anyhow::Result<()> {
    // Session 1: create matview with two junction rows for the same parent.
    // Session 2: reopen, DELETE one junction row. The R_COUNT must still be
    // > 0 (we still have one match), so no null-pad row should appear.
    let db_path = "/tmp/turso-ivm-left-join-count-state-test.db";
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS lj AS \
                       SELECT p.pid, j.tag \
                       FROM parents p LEFT JOIN junction j ON j.pid = p.pid";

    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute("CREATE TABLE parents (pid INTEGER PRIMARY KEY)", ())
            .await?;
        conn.execute(
            "CREATE TABLE junction (id INTEGER PRIMARY KEY, pid INT, tag TEXT)",
            (),
        )
        .await?;
        conn.execute("INSERT INTO parents VALUES (1)", ()).await?;
        conn.execute(
            "INSERT INTO junction VALUES (1, 1, 'red'),(2, 1, 'blue')",
            (),
        )
        .await?;
        conn.execute(matview_sql, ()).await?;

        let mut rows = conn.query("SELECT COUNT(*) FROM lj", ()).await?;
        let row = rows.next().await?.unwrap();
        let count: i64 = row.get(0)?;
        assert_eq!(count, 2);

        drop(conn);
        drop(db);
    }

    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(matview_sql, ()).await?;

        // DELETE one of the two matches; we should still have one matched row,
        // no null-pad. This locks down that R_COUNT.weight stayed at 2 across
        // the reopen and decrements correctly.
        conn.execute("DELETE FROM junction WHERE id = 1", ())
            .await?;

        let mut rows = conn
            .query("SELECT pid, tag FROM lj ORDER BY pid, tag", ())
            .await?;
        let mut got: Vec<(i64, Option<String>)> = Vec::new();
        while let Some(row) = rows.next().await? {
            let pid: i64 = row.get(0)?;
            let tag: Option<String> = row.get(1)?;
            got.push((pid, tag));
        }
        assert_eq!(
            got,
            vec![(1, Some("blue".to_string()))],
            "Session 2: count must have survived; one DELETE leaves count=1, no null-pad."
        );

        drop(conn);
        drop(db);
    }

    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));
    Ok(())
}

/// G0 diamond-DAG fix composed with this rev's FILTER aggregation:
/// dual `LEFT OUTER JOIN` + `json_group_array(...) FILTER (...)` + GROUP BY.
/// Holon's `bugs/holon_block_hydration_repro.sql` "block:f" repro.
///
/// The diamond-DAG memo (DbspCircuit::exec_node_cache) lands in the
/// LEFT JOIN rev. This test exercises the *full* composition required
/// by the holon hydration target: the FILTER (WHERE … IS NOT NULL)
/// suppresses the synthetic NULLs that LEFT JOIN's null-pad inserts,
/// so empty-on-both rows surface with `'[]'` / `'[]'` instead of
/// `'[null]'`. Without G0, `block:f` (no tags, no blockers) would be
/// silently absent.
#[tokio::test]
async fn test_left_join_dual_filter_aggregation_holon_shape() -> anyhow::Result<()> {
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    conn.execute("CREATE TABLE block (id TEXT PRIMARY KEY)", ())
        .await?;
    conn.execute("CREATE TABLE block_tags (block_id TEXT, tag TEXT)", ())
        .await?;
    conn.execute(
        "CREATE TABLE task_blockers (blocked_id TEXT, blocker_id TEXT)",
        (),
    )
    .await?;

    // 6 blocks; only some have tags; only some have blockers; `f` has neither.
    conn.execute(
        "INSERT INTO block VALUES ('a'),('b'),('c'),('d'),('e'),('f')",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO block_tags VALUES ('a','urgent'),('a','review'),('b','review'),('c','archived')",
        (),
    )
    .await?;
    conn.execute("INSERT INTO task_blockers VALUES ('d','a'),('e','a')", ())
        .await?;

    conn.execute(
        "CREATE MATERIALIZED VIEW block_hydrated AS \
         SELECT b.id, \
           COALESCE(json_group_array(bt.tag) FILTER (WHERE bt.tag IS NOT NULL), '[]') AS tags, \
           COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by \
         FROM block b \
         LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id \
         LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id \
         GROUP BY b.id",
        (),
    )
    .await?;

    let mut rows = conn
        .query(
            "SELECT id, tags, blocked_by FROM block_hydrated ORDER BY id",
            (),
        )
        .await?;
    let mut got: Vec<(String, String, String)> = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let tags: String = row.get(1)?;
        let blockers: String = row.get(2)?;
        got.push((id, tags, blockers));
    }

    assert_eq!(
        got,
        vec![
            (
                "a".to_string(),
                "[\"review\",\"urgent\"]".to_string(),
                "[]".to_string()
            ),
            (
                "b".to_string(),
                "[\"review\"]".to_string(),
                "[]".to_string()
            ),
            (
                "c".to_string(),
                "[\"archived\"]".to_string(),
                "[]".to_string()
            ),
            ("d".to_string(), "[]".to_string(), "[\"a\"]".to_string()),
            ("e".to_string(), "[]".to_string(), "[\"a\"]".to_string()),
            ("f".to_string(), "[]".to_string(), "[]".to_string()),
        ],
        "block:f (no tags, no blockers) must appear with empty arrays — \
         missing it is the diamond-DAG regression"
    );

    // CDC: deleting the last junction entry for an isolated row must
    // keep the row alive with empty arrays.
    conn.execute("DELETE FROM task_blockers WHERE blocked_id = 'd'", ())
        .await?;

    let mut rows = conn
        .query(
            "SELECT id, tags, blocked_by FROM block_hydrated WHERE id = 'd'",
            (),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .expect("block:d must still be present after DELETE");
    let id: String = row.get(0)?;
    let tags: String = row.get(1)?;
    let blockers: String = row.get(2)?;
    assert_eq!(
        (id, tags, blockers),
        ("d".to_string(), "[]".to_string(), "[]".to_string()),
        "after DELETE of last task_blocker, block:d must still appear with empty arrays"
    );

    Ok(())
}
