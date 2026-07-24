//! Reproducer for holon BugFunnel row 90 (dogfood #4, 2026-07-12):
//! the `block` IVM matview grows DUPLICATE rows after an app restart over an
//! existing DB, while the base table stays unique.
//!
//! This mirrors holon's CURRENT *chained* matview shape (not the old single
//! multi-JOIN+GROUP BY that `test_ivm_left_join_aggregate_duplicate` covers):
//!   agg  = SELECT block_id AS source_id, json_group_array(tag) AS vals
//!          FROM block_tags GROUP BY block_id            (AGGREGATE matview)
//!   block= SELECT b.*, COALESCE(agg.vals,'[]') AS tags
//!          FROM block_raw b LEFT OUTER JOIN agg ON agg.source_id = b.id
//!                                                  (JOIN matview on the agg)
//!
//! Sequence (exactly what holon's boot re-seed emits on the 2nd launch):
//!   session 1: create tables + matviews, insert one block + its 'Page' tag,
//!              verify the block matview has exactly 1 row, then CLOSE the DB
//!              (persisting the backing btrees + DBSP state).
//!   session 2: REOPEN, then in ONE transaction replay the idempotent re-seed
//!              UPDATE block_raw SET sort_key=... ;   -- touches a GROUP-BY/left col
//!              DELETE FROM block_tags ;              -- clear the tag
//!              INSERT INTO block_tags ('Page') ;    -- re-add the SAME tag
//!   The net change is zero, yet the `block` matview ends up with 2 identical
//!   rows for the block.

use turso::Builder;

fn fresh(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

async fn open(path: &str) -> anyhow::Result<(turso::Database, turso::Connection)> {
    let db = Builder::new_local(path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    Ok((db, conn))
}

async fn count_block_rows(conn: &turso::Connection, id: &str) -> anyhow::Result<usize> {
    let mut rows = conn
        .query(&format!("SELECT id, sort_key, tags FROM block WHERE id = '{id}'"), ())
        .await?;
    let mut dump = Vec::new();
    while let Some(row) = rows.next().await? {
        let mut cols = Vec::new();
        for i in 0..row.column_count() {
            cols.push(format!("{:?}", row.get_value(i)?));
        }
        dump.push(format!("({})", cols.join(", ")));
    }
    eprintln!("[chained-agg-reopen] block rows for {id}: {dump:?}");
    Ok(dump.len())
}

/// Faithful multi-block variant matching holon's boot-2 journal re-seed:
/// a parent Page (`j`, tag 'Page') plus two children (`j::src`, `j::render`,
/// no tags). On reopen the re-seed, interleaved through the SHARED
/// block_tags_agg, does: UPDATE the parent's sort_key + DELETE/re-INSERT the
/// parent's 'Page' tag, and DELETE each child's whole block_raw row then
/// re-INSERT it (ON CONFLICT UPSERT). block_raw stays unique; the `block`
/// matview must not duplicate any id.
/// MINIMAL isolation: a single block with NO tag (an UNMATCHED / null-padded
/// LEFT JOIN row). On reopen, DELETE its whole block_raw row then re-INSERT it.
/// If this alone duplicates, the defect is purely "retraction of an unmatched
/// LEFT JOIN left-row across a reopen fails to remove the persisted output row".
#[tokio::test]
async fn test_ivm_chained_agg_reopen_unmatched_row_delete_reinsert() -> anyhow::Result<()> {
    let path = "/tmp/turso-ivm-chained-agg-reopen-unmatched.db";
    fresh(path);

    {
        let (db, conn) = open(path).await?;
        setup_multiblock_views(&conn).await?;
        // 's' has NO tag -> unmatched LEFT JOIN row (tags='[]').
        conn.execute(
            "INSERT INTO block_raw (id, parent_id, sort_key, content) VALUES ('s', NULL, '80', 'src')",
            (),
        )
        .await?;
        assert_eq!(count_block_rows(&conn, "s").await?, 1, "session-1 baseline");
        drop(conn);
        drop(db);
    }

    {
        let (_db, conn) = open(path).await?;
        assert_eq!(count_block_rows(&conn, "s").await?, 1, "reopen pre-reseed");
        conn.execute("DELETE FROM block_raw WHERE id = 's'", ()).await?;
        conn.execute(
            "INSERT INTO block_raw (id, parent_id, sort_key, content) VALUES ('s', NULL, '80', 'src')",
            (),
        )
        .await?;
        assert_eq!(
            count_block_rows(&conn, "s").await?,
            1,
            "unmatched-row DELETE+reINSERT across reopen must NOT duplicate"
        );
    }
    Ok(())
}

async fn setup_multiblock_views(conn: &turso::Connection) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE block_raw (id TEXT PRIMARY KEY, parent_id TEXT, sort_key TEXT, content TEXT)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE block_tags (block_id TEXT, tag TEXT, PRIMARY KEY (block_id, tag))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block_tags_agg AS \
         SELECT block_id AS source_id, json_group_array(tag) AS vals \
         FROM block_tags GROUP BY block_id",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block AS \
         SELECT b.id, b.parent_id, b.sort_key, b.content, \
                COALESCE(block_tags_agg.vals, '[]') AS tags \
         FROM block_raw b \
         LEFT OUTER JOIN block_tags_agg ON block_tags_agg.source_id = b.id",
        (),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_ivm_chained_agg_reopen_multiblock_reseed_no_duplicate() -> anyhow::Result<()> {
    let path = "/tmp/turso-ivm-chained-agg-reopen-multiblock.db";
    fresh(path);

    // Session 1: seed parent + two children, parent tagged 'Page'.
    {
        let (db, conn) = open(path).await?;
        setup_multiblock_views(&conn).await?;
        conn.execute(
            "INSERT INTO block_raw (id, parent_id, sort_key, content) VALUES \
             ('j', NULL, '80', 'Journals'), \
             ('j::src', 'j', '8080', 'src'), \
             ('j::render', 'j', '8180', 'render')",
            (),
        )
        .await?;
        conn.execute("INSERT INTO block_tags (block_id, tag) VALUES ('j', 'Page')", ())
            .await?;
        let mut rows = conn.query("SELECT COUNT(*) FROM block", ()).await?;
        let c: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(c, 3, "session-1 baseline: 3 blocks");
        drop(conn);
        drop(db);
    }

    // Session 2: reopen, replay the boot re-seed (interleaved, per holon trace).
    {
        let (_db, conn) = open(path).await?;

        // Children: whole-row DELETE then re-INSERT (ON CONFLICT UPSERT), each
        // its own autocommit statement — exactly the boot-2 order captured.
        conn.execute("DELETE FROM block_raw WHERE id = 'j::src'", ()).await?;
        conn.execute("DELETE FROM block_raw WHERE id = 'j::render'", ()).await?;
        // Parent: sort_key UPDATE + tag DELETE/re-INSERT in one transaction.
        conn.execute("BEGIN", ()).await?;
        conn.execute("UPDATE block_raw SET sort_key = '80' WHERE id = 'j'", ()).await?;
        conn.execute("DELETE FROM block_tags WHERE block_id = 'j'", ()).await?;
        conn.execute("INSERT INTO block_tags (block_id, tag) VALUES ('j', 'Page')", ())
            .await?;
        conn.execute("COMMIT", ()).await?;
        // Children re-INSERT via UPSERT.
        conn.execute(
            "INSERT INTO block_raw (id, parent_id, sort_key, content) VALUES \
             ('j::render', 'j', '8180', 'render') \
             ON CONFLICT(id) DO UPDATE SET sort_key = excluded.sort_key, content = excluded.content",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO block_raw (id, parent_id, sort_key, content) VALUES \
             ('j::src', 'j', '8080', 'src') \
             ON CONFLICT(id) DO UPDATE SET sort_key = excluded.sort_key, content = excluded.content",
            (),
        )
        .await?;

        let mut rows = conn
            .query(
                "SELECT id, COUNT(*) AS c FROM block GROUP BY id HAVING COUNT(*) > 1",
                (),
            )
            .await?;
        let mut dupes = Vec::new();
        while let Some(row) = rows.next().await? {
            let id = match row.get_value(0)? {
                turso::Value::Text(t) => t,
                v => format!("{v:?}"),
            };
            let c = match row.get_value(1)? {
                turso::Value::Integer(i) => i,
                _ => -1,
            };
            dupes.push((id, c));
        }
        assert!(
            dupes.is_empty(),
            "multi-block reopen re-seed produced DUPLICATE block matview rows: {dupes:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_ivm_chained_agg_reopen_reseed_no_duplicate() -> anyhow::Result<()> {
    let path = "/tmp/turso-ivm-chained-agg-reopen-reseed.db";
    fresh(path);

    // ── Session 1 ────────────────────────────────────────────────────────
    {
        let (db, conn) = open(path).await?;
        conn.execute(
            "CREATE TABLE block_raw (id TEXT PRIMARY KEY, sort_key TEXT, content TEXT)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE block_tags (block_id TEXT, tag TEXT, PRIMARY KEY (block_id, tag))",
            (),
        )
        .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW block_tags_agg AS \
             SELECT block_id AS source_id, json_group_array(tag) AS vals \
             FROM block_tags GROUP BY block_id",
            (),
        )
        .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW block AS \
             SELECT b.id, b.sort_key, b.content, \
                    COALESCE(block_tags_agg.vals, '[]') AS tags \
             FROM block_raw b \
             LEFT OUTER JOIN block_tags_agg ON block_tags_agg.source_id = b.id",
            (),
        )
        .await?;

        conn.execute(
            "INSERT INTO block_raw (id, sort_key, content) VALUES ('j', '80', 'Journals')",
            (),
        )
        .await?;
        conn.execute("INSERT INTO block_tags (block_id, tag) VALUES ('j', 'Page')", ())
            .await?;

        assert_eq!(count_block_rows(&conn, "j").await?, 1, "session-1 baseline");
        drop(conn);
        drop(db);
    }

    // ── Session 2: reopen + idempotent re-seed in one transaction ────────
    {
        let (_db, conn) = open(path).await?;
        assert_eq!(
            count_block_rows(&conn, "j").await?,
            1,
            "reopen (pre-reseed) must still be a single row"
        );

        conn.execute("BEGIN", ()).await?;
        conn.execute("UPDATE block_raw SET sort_key = '81' WHERE id = 'j'", ())
            .await?;
        conn.execute("DELETE FROM block_tags WHERE block_id = 'j'", ())
            .await?;
        conn.execute("INSERT INTO block_tags (block_id, tag) VALUES ('j', 'Page')", ())
            .await?;
        conn.execute("COMMIT", ()).await?;

        assert_eq!(
            count_block_rows(&conn, "j").await?,
            1,
            "after reopen + idempotent re-seed the block matview must NOT duplicate"
        );
    }
    Ok(())
}
