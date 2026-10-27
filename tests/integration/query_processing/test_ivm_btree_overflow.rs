//! Test for IVM BTree overflow cell assertion
//!
//! Reproduces the "multiple overflow cells can only occur when a parent overflows
//! during balancing" panic in btree.rs during rapid inserts into tables with
//! JOIN materialized views and CDC callbacks.
//!
//! This matches the production scenario: single-level matviews (not chained),
//! with rapid concurrent inserts triggering IVM updates.

use crate::common::TempDatabase;

/// Rapid inserts into a table with a JOIN matview + CDC causes BTree overflow assertion
#[turso_macros::test(views)]
fn test_rapid_inserts_with_join_matview_and_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Schema matching production: navigation tables + blocks
    conn.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT, timestamp TEXT DEFAULT (datetime('now')))")?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER REFERENCES navigation_history(id))")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    // Single JOIN matview on base tables (NOT chained)
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id, nh.timestamp
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Blocks table
    conn.execute("CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, content TEXT DEFAULT '')")?;

    // Set up CDC callback (required trigger for the bug)
    conn.set_change_callback(|event| {
        let _ = event; // just having the callback registered is enough
    })
    .unwrap();

    // Populate navigation so the JOIN matview has data
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-1')")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Rapid inserts into blocks - this triggers IVM updates on any matview
    // that references blocks. With CDC active, the BTree overflow assertion fires.
    for i in 0..200 {
        conn.execute(format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('block-{i}', 'holon-doc://test.org', 'Content {i}')"
        ))?;
    }

    // Also insert child blocks to create deeper trees
    for i in 0..100 {
        conn.execute(format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('child-{i}', 'block-{}', 'Child content {i}')",
            i % 50
        ))?;
    }

    Ok(())
}
