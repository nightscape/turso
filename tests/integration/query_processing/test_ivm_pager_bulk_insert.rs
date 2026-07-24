//! Test: Bulk INSERT in transaction with active IVM matview must not panic in pager.
//!
//! Production bug: pager.rs:4632 panics with "attempt to subtract with overflow"
//! during allocate_page → balance_quick → insert during IVM processing of 256+ row
//! transactions on a table with an active materialized view.

use crate::common::TempDatabase;

/// Bulk insert with freelist pressure: delete existing data to create free pages,
/// then bulk insert with IVM. This triggers allocate_page from freelist.
#[turso_macros::test(views)]
fn test_bulk_insert_freelist_pressure_with_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY,
            event_type TEXT NOT NULL,
            aggregate_type TEXT NOT NULL,
            aggregate_id TEXT,
            payload TEXT DEFAULT '{}'
        )",
    )?;

    // Insert and delete data to create freelist entries
    for batch in 0..3 {
        for i in 0..200 {
            let idx = batch * 200 + i;
            conn.execute(&format!(
                "INSERT INTO events VALUES ('tmp-{idx}', 'temp.event', 'temp', 'tmp-{idx}', \
                 '{{\"data\":\"padding to make rows bigger and use more pages {idx}\"}}')"
            ))?;
        }
    }
    // Delete all rows — pages go to freelist
    conn.execute("DELETE FROM events")?;

    // Now create the matview (IVM active)
    conn.execute(
        "CREATE MATERIALIZED VIEW events_view AS
         SELECT * FROM events WHERE aggregate_type = 'directory'",
    )?;

    // Bulk insert with IVM — allocate_page should reuse freelist pages
    conn.execute("BEGIN TRANSACTION")?;
    for i in 0..300 {
        let payload = format!(
            "{{\"change_type\":\"created\",\"origin\":\"sync\",\"data\":{{\"id\":\"dir-{i}\",\
             \"name\":\"Directory item {i}\",\"parent_id\":\"parent-{}\",\"order\":{i},\
             \"description\":\"Description with enough text to cause page splits\"\
             }}}}",
            i / 10
        );
        conn.execute(&format!(
            "INSERT INTO events VALUES ('evt-{i}', 'directory.created', 'directory', 'dir-{i}', '{payload}')"
        ))?;
    }
    conn.execute("COMMIT")?;

    let rows = crate::common::limbo_exec_rows(&conn, "SELECT COUNT(*) FROM events_view");
    let count = match &rows[0][0] {
        rusqlite::types::Value::Integer(n) => *n,
        _ => 0,
    };
    assert_eq!(count, 300, "matview should have all 300 rows");

    Ok(())
}

#[turso_macros::test(views)]
fn test_bulk_insert_with_matview_no_pager_panic(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY,
            event_type TEXT NOT NULL,
            aggregate_type TEXT NOT NULL,
            aggregate_id TEXT,
            payload TEXT DEFAULT '{}'
        )",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW events_view AS
         SELECT * FROM events WHERE aggregate_type = 'directory'",
    )?;

    // Bulk insert in a transaction (256+ rows with large JSON payloads, matching production)
    conn.execute("BEGIN TRANSACTION")?;
    for i in 0..300 {
        let payload = format!(
            "{{\"change_type\":\"created\",\"origin\":\"todoist-sync\",\"data\":{{\"id\":\"dir-{i}\",\
             \"name\":\"Directory item {i} with a longer name for page pressure\",\
             \"parent_id\":\"parent-{}\",\"order\":{i},\"color\":\"blue\",\
             \"description\":\"Some longer description text to make the payload bigger and cause more page splits in the btree during IVM processing\"\
             }}}}",
            i / 10
        );
        conn.execute(&format!(
            "INSERT INTO events VALUES ('evt-{i}', 'directory.created', 'directory', 'dir-{i}', '{payload}')"
        ))?;
    }
    conn.execute("COMMIT")?;

    // Verify matview has the data
    let rows = crate::common::limbo_exec_rows(&conn, "SELECT COUNT(*) FROM events_view");
    let count = match &rows[0][0] {
        rusqlite::types::Value::Integer(n) => *n,
        _ => 0,
    };
    assert_eq!(count, 300, "matview should have all 300 rows");

    // Do a second bulk transaction (production saw repeated failures)
    conn.execute("BEGIN TRANSACTION")?;
    for i in 300..600 {
        let payload = format!(
            "{{\"change_type\":\"created\",\"origin\":\"todoist-sync\",\"data\":{{\"id\":\"dir-{i}\",\
             \"name\":\"Directory item {i} with a longer name for page pressure\",\
             \"parent_id\":\"parent-{}\",\"order\":{i},\"color\":\"red\",\
             \"description\":\"More description text to create additional page pressure and trigger freelist operations during IVM delta processing\"\
             }}}}",
            i / 10
        );
        conn.execute(&format!(
            "INSERT INTO events VALUES ('evt-{i}', 'directory.created', 'directory', 'dir-{i}', '{payload}')"
        ))?;
    }
    conn.execute("COMMIT")?;

    let rows = crate::common::limbo_exec_rows(&conn, "SELECT COUNT(*) FROM events_view");
    let count = match &rows[0][0] {
        rusqlite::types::Value::Integer(n) => *n,
        _ => 0,
    };
    assert_eq!(
        count, 600,
        "matview should have all 600 rows after second batch"
    );

    Ok(())
}

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
        conn.execute(&format!(
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
