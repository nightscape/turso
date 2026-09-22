//! Test: Recursive CTE matview from-scratch population inside transactions.
//!
//! Reproduces a bug where creating a recursive CTE matview (from scratch) inside
//! an uncommitted transaction doesn't see the pending writes. Simple (non-recursive)
//! matviews work correctly — the bug is specific to recursive CTE population.
//!
//! The IVM delta path correctly sees uncommitted data, so an existing recursive CTE
//! matview maintained via IVM shows the right rows. But a fresh matview created
//! with the same SQL shows 0 rows — the from-scratch population starts a separate
//! read snapshot that can't see the uncommitted writes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::common::{limbo_exec_rows, TempDatabase};

fn setup_schema(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            parent_id TEXT,
            content TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT 'text',
            properties TEXT,
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
    )?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    Ok(())
}

fn create_recursive_matview(conn: &Arc<turso_core::Connection>, name: &str) -> anyhow::Result<()> {
    conn.execute(&format!(
        "CREATE MATERIALIZED VIEW {name} AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, content_type,
                    properties, created_at, updated_at,
                    '/' || id AS path, id AS root_id
             FROM block
             WHERE parent_id LIKE 'doc:%'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, b.content_type,
                    b.properties, b.created_at, b.updated_at,
                    p.path || '/' || b.id AS path, p.root_id
             FROM block b INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    ))?;
    Ok(())
}

fn count_matview(conn: &Arc<turso_core::Connection>, name: &str) -> i64 {
    let rows = limbo_exec_rows(conn, &format!("SELECT COUNT(*) FROM {name}"));
    match &rows[0][0] {
        rusqlite::types::Value::Integer(n) => *n,
        _ => -1,
    }
}

/// Baseline: auto-commit INSERT populates recursive CTE matview correctly
#[turso_macros::test(views)]
fn test_autocommit_populates_rcte_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    create_recursive_matview(&conn, "block_with_path")?;

    conn.execute("INSERT INTO block (id, parent_id, content) VALUES ('root', 'doc:x', 'Root')")?;
    conn.execute("INSERT INTO block (id, parent_id, content) VALUES ('child', 'root', 'Child')")?;

    assert_eq!(count_matview(&conn, "block_with_path"), 2);
    Ok(())
}

/// Baseline: simple (non-recursive) matview created inside transaction sees uncommitted data
#[turso_macros::test(views)]
fn test_simple_matview_inside_tx_sees_uncommitted(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO block (id, parent_id, content) VALUES ('root', 'doc:x', 'Root')")?;

    // Create a fresh simple matview inside the transaction — should see uncommitted data
    conn.execute("CREATE MATERIALIZED VIEW mv_simple AS SELECT * FROM block")?;
    assert_eq!(
        count_matview(&conn, "mv_simple"),
        1,
        "simple matview created inside transaction should see uncommitted INSERT"
    );

    conn.execute("COMMIT")?;
    Ok(())
}

/// BUG: Recursive CTE matview created inside transaction does NOT see uncommitted data
#[turso_macros::test(views)]
fn test_rcte_matview_inside_tx_sees_uncommitted(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO block (id, parent_id, content) VALUES ('root', 'doc:x', 'Root')")?;

    // Verify the data is visible on this connection
    let base = limbo_exec_rows(&conn, "SELECT COUNT(*) FROM block");
    assert_eq!(
        base[0][0],
        rusqlite::types::Value::Integer(1),
        "block table should have 1 row inside transaction"
    );

    // Create a fresh recursive CTE matview inside the transaction
    create_recursive_matview(&conn, "rcte_check")?;
    assert_eq!(
        count_matview(&conn, "rcte_check"),
        1,
        "recursive CTE matview created inside transaction should see uncommitted INSERT \
         (root has parent_id='doc:x' matching LIKE 'doc:%')"
    );

    conn.execute("COMMIT")?;
    Ok(())
}

/// BUG: IVM-maintained matview has data but fresh recursive CTE matview shows 0
#[turso_macros::test(views)]
fn test_ivm_vs_fresh_rcte_consistency_in_tx(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;

    // Create the IVM-maintained matview first (empty)
    create_recursive_matview(&conn, "block_with_path")?;
    assert_eq!(count_matview(&conn, "block_with_path"), 0);

    conn.execute("BEGIN")?;
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, content, properties, created_at, updated_at)
         VALUES ('block:root', 'doc:test', 'Root', '{\"seq\":0}', 1000, 1000)",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, content, properties, created_at, updated_at)
         VALUES ('block:child1', 'block:root', 'Child 1', '{\"seq\":1}', 1000, 1000)",
    )?;

    // IVM-maintained matview should have 2 rows (root + child via recursion)
    let ivm_count = count_matview(&conn, "block_with_path");
    assert_eq!(
        ivm_count, 2,
        "IVM-maintained matview should have 2 rows inside transaction"
    );

    // Create a fresh matview with the same SQL
    create_recursive_matview(&conn, "_check_bwp")?;
    let fresh_count = count_matview(&conn, "_check_bwp");

    assert_eq!(
        fresh_count, ivm_count,
        "fresh recursive CTE matview should match IVM matview inside transaction: \
         IVM has {} rows but fresh has {} rows",
        ivm_count, fresh_count
    );

    conn.execute("COMMIT")?;
    Ok(())
}

/// After transaction commit, matview counts should be consistent
#[turso_macros::test(views)]
fn test_tx_insert_matview_consistent_after_commit(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    create_recursive_matview(&conn, "block_with_path")?;

    conn.execute("BEGIN")?;
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, content) VALUES ('root', 'doc:x', 'Root')",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, content) VALUES ('child', 'root', 'Child')",
    )?;
    conn.execute("COMMIT")?;

    assert_eq!(
        count_matview(&conn, "block_with_path"),
        2,
        "recursive CTE matview should have 2 rows after transaction commit"
    );

    // Create fresh matview after commit — should also have 2 rows
    create_recursive_matview(&conn, "_check_after_commit")?;
    assert_eq!(
        count_matview(&conn, "_check_after_commit"),
        2,
        "fresh recursive CTE matview created after commit should have 2 rows"
    );

    Ok(())
}

/// CDC should report changes after UPDATE on data inserted via transaction
#[turso_macros::test(views)]
fn test_tx_insert_then_update_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    create_recursive_matview(&conn, "block_with_path")?;

    conn.execute("BEGIN")?;
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, content, properties, created_at, updated_at)
         VALUES ('block:root', 'doc:test', 'Root', '{\"seq\":0}', 1000, 1000)",
    )?;
    conn.execute("COMMIT")?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "block_with_path" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    });

    conn.execute(
        "UPDATE block SET content = 'Updated', updated_at = 2000 WHERE id = 'block:root'",
    )?;

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC should report >0 changes after UPDATE on block inserted via transaction, got {}",
        changes
    );

    Ok(())
}

fn setup_holon_schema(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY, parent_id TEXT, document_id TEXT,
            content TEXT NOT NULL DEFAULT '', content_type TEXT NOT NULL DEFAULT 'text',
            source_language TEXT, source_name TEXT, properties TEXT,
            collapsed INTEGER NOT NULL DEFAULT 0, completed INTEGER NOT NULL DEFAULT 0,
            block_type TEXT NOT NULL DEFAULT 'text',
            created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0,
            _change_origin TEXT
        )",
    )?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    conn.execute("CREATE INDEX idx_block_doc ON block(document_id)")?;

    conn.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT, timestamp TEXT DEFAULT (datetime('now')))")?;
    conn.execute("CREATE INDEX idx_nav_hist ON navigation_history(region)")?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER REFERENCES navigation_history(id))")?;

    conn.execute("CREATE TABLE events (id TEXT PRIMARY KEY, event_type TEXT, aggregate_type TEXT, aggregate_id TEXT, origin TEXT, status TEXT, payload TEXT, trace_id TEXT, command_id TEXT, created_at INTEGER, speculative_id TEXT, rejection_reason TEXT)")?;

    // Chained matviews
    conn.execute("CREATE MATERIALIZED VIEW current_focus AS SELECT nc.region, nh.block_id FROM navigation_cursor nc JOIN navigation_history nh ON nc.history_id = nh.id")?;
    conn.execute("CREATE MATERIALIZED VIEW focus_roots AS SELECT cf.region, cf.block_id, b.id AS root_id FROM current_focus cf JOIN block b ON b.parent_id = cf.block_id UNION ALL SELECT cf.region, cf.block_id, b.id AS root_id FROM current_focus cf JOIN block b ON b.id = cf.block_id")?;

    // Recursive CTE matview
    conn.execute(
        "CREATE MATERIALIZED VIEW block_with_path AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, content_type, source_language, source_name,
                    properties, created_at, updated_at,
                    '/' || id AS path, id AS root_id
             FROM block WHERE parent_id LIKE 'doc:%' OR parent_id LIKE 'sentinel:%'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, b.content_type, b.source_language,
                    b.source_name, b.properties, b.created_at, b.updated_at,
                    p.path || '/' || b.id AS path, p.root_id
             FROM block b INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    )?;

    // Watch views (like production)
    conn.execute("CREATE MATERIALIZED VIEW watch_view_1 AS SELECT id, parent_id, content, content_type, properties FROM block")?;
    conn.execute("CREATE MATERIALIZED VIEW watch_view_2 AS SELECT id, content FROM block WHERE content_type = 'text'")?;
    conn.execute("CREATE MATERIALIZED VIEW events_view AS SELECT * FROM events WHERE event_type LIKE 'block.%'")?;

    // Navigation seed
    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'block:root-layout')",
    )?;
    conn.execute("INSERT INTO navigation_cursor VALUES ('main', 1)")?;

    Ok(())
}

/// IVM should correctly count depth-2 children during multi-INSERT transaction
#[turso_macros::test(views)]
fn test_tx_depth2_child_not_dropped(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_holon_schema(&conn)?;

    // Events transaction (like production)
    conn.execute("BEGIN")?;
    for i in 0..10 {
        conn.execute(&format!(
            "INSERT INTO events VALUES ('evt{i:03}', 'file.created', 'file', 'f{i}', 'org', 'confirmed', '{{}}', NULL, NULL, 1000, NULL, NULL)"
        ))?;
    }
    conn.execute("COMMIT")?;

    // Block inserts in transaction
    conn.execute("BEGIN")?;

    // 1: root (base case, matches parent_id LIKE 'doc:%')
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, document_id, content, content_type, properties, created_at, updated_at)
         VALUES ('block:root-layout', 'doc:test', 'doc:test', 'Holon Layout', 'text', '{\"ID\":\"root-layout\",\"sequence\":0}', 1000, 1000)",
    )?;
    assert_eq!(
        count_matview(&conn, "block_with_path"),
        1,
        "after INSERT 1: root"
    );

    // 2: source child of root (depth 1)
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, document_id, content, content_type, source_language, properties, created_at, updated_at)
         VALUES ('block:root::src::0', 'block:root-layout', 'doc:test', 'MATCH query', 'source', 'holon_gql', '{\"ID\":\"root::src::0\",\"sequence\":1}', 1000, 1000)",
    )?;
    assert_eq!(
        count_matview(&conn, "block_with_path"),
        2,
        "after INSERT 2: src child"
    );

    // 3: render child of root (depth 1)
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, document_id, content, content_type, source_language, properties, created_at, updated_at)
         VALUES ('block:root::render::0', 'block:root-layout', 'doc:test', 'columns()', 'source', 'render', '{\"sequence\":2,\"ID\":\"root::render::0\"}', 1000, 1000)",
    )?;
    assert_eq!(
        count_matview(&conn, "block_with_path"),
        3,
        "after INSERT 3: render child"
    );

    // 4: text child of root (depth 1) — parent of the depth-2 block
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, document_id, content, content_type, properties, created_at, updated_at)
         VALUES ('block:e7fcc60b', 'block:root-layout', 'doc:test', 'Left Sidebar', 'text', '{\"ID\":\"e7fcc60b\",\"sequence\":3,\"collapse_to\":\"drawer\"}', 1000, 1000)",
    )?;
    assert_eq!(
        count_matview(&conn, "block_with_path"),
        4,
        "after INSERT 4: left-sidebar"
    );

    // 5: DEPTH-2 child (child of #4) — this is the potentially missing row
    conn.execute(
        "INSERT OR REPLACE INTO block (id, parent_id, document_id, content, content_type, source_language, properties, created_at, updated_at)
         VALUES ('block:left-sidebar::render::0', 'block:e7fcc60b', 'doc:test', 'list()', 'source', 'render', '{\"sequence\":4,\"ID\":\"left-sidebar::render::0\"}', 1000, 1000)",
    )?;
    assert_eq!(
        count_matview(&conn, "block_with_path"),
        5,
        "after INSERT 5: depth-2 child should be in matview (parent block:e7fcc60b was added in INSERT 4)"
    );

    conn.execute("COMMIT")?;

    // After commit, verify still correct
    assert_eq!(
        count_matview(&conn, "block_with_path"),
        5,
        "after COMMIT: all 5 rows should be in matview"
    );

    Ok(())
}
