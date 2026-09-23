//! Secondary indexes on a materialized view.
//!
//! A matview's btree is an ordinary rowid table, so an index on it is an
//! ordinary secondary index — except that the view's rows are written when the
//! view's delta is applied at commit, so the index entries are written there
//! too. The contract every test below states is the same: **an index-driven
//! read of the view returns exactly what a scan of the same view returns.**
//! The left query lets the planner use the index, the right reads the view
//! `NOT INDEXED`.

use std::sync::Arc;

use rusqlite::types::Value;

use super::matview_index_oracle::assert_reads_no_view_index;
use crate::common::{limbo_exec_rows, TempDatabase};

fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_id ON v(id)")?;
    for i in 0..6 {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('b{i}', '{}')",
            if i % 2 == 0 { "TODO" } else { "DONE" }
        ))?;
    }
    Ok(())
}

fn assert_index_matches_scan(
    conn: &Arc<turso_core::Connection>,
    what: &str,
    indexed: &str,
    scanned: &str,
) {
    assert_reads_no_view_index(conn, scanned);
    let via_index = limbo_exec_rows(conn, indexed);
    let via_scan = limbo_exec_rows(conn, scanned);
    assert_eq!(
        via_index, via_scan,
        "{what}: an index-driven read of the view disagrees with a scan of the \
         SAME view\n  indexed: {indexed}\n  scanned: {scanned}"
    );
}

/// `CREATE INDEX` on a populated view backfills from the view's rows, the same
/// scan an index on any rowid table is built from.
#[turso_macros::test(views)]
fn create_index_backfills_a_populated_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    for i in 0..6 {
        conn.execute(&format!("INSERT INTO t_raw (id, st) VALUES ('b{i}', 'X')"))?;
    }
    conn.execute("CREATE INDEX idx_v_id ON v(id)")?;

    assert_index_matches_scan(
        &conn,
        "backfilled point read",
        "SELECT id, st FROM v WHERE id = 'b3'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b3'",
    );
    assert_index_matches_scan(
        &conn,
        "backfilled ordered read",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    Ok(())
}

/// Inserts, deletes and a key move after the index exists. Each one commits,
/// so this is the index at rest, not the transaction overlay.
#[turso_macros::test(views)]
fn committed_writes_keep_the_index_in_step(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    assert_index_matches_scan(
        &conn,
        "inserted row",
        "SELECT id, st FROM v WHERE id = 'b9'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b9'",
    );

    conn.execute("DELETE FROM t_raw WHERE id = 'b2'")?;
    assert_index_matches_scan(
        &conn,
        "deleted row",
        "SELECT count(*) FROM v WHERE id = 'b2'",
        "SELECT count(*) FROM v NOT INDEXED WHERE id = 'b2'",
    );

    conn.execute("UPDATE t_raw SET id = 'z5' WHERE id = 'b5'")?;
    assert_index_matches_scan(
        &conn,
        "old key of a key move",
        "SELECT count(*) FROM v WHERE id = 'b5'",
        "SELECT count(*) FROM v NOT INDEXED WHERE id = 'b5'",
    );
    assert_index_matches_scan(
        &conn,
        "new key of a key move",
        "SELECT id, st FROM v WHERE id = 'z5'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'z5'",
    );
    assert_index_matches_scan(
        &conn,
        "full ordered read",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    Ok(())
}

/// A value change that does not touch the key still rewrites the view row, so
/// the index entry must survive it rather than be retracted.
#[turso_macros::test(views)]
fn a_non_key_update_leaves_the_index_entry_in_place(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("UPDATE t_raw SET st = 'MOVED' WHERE id = 'b4'")?;
    assert_index_matches_scan(
        &conn,
        "row whose non-key column changed",
        "SELECT id, st FROM v WHERE id = 'b4'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b4'",
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT st FROM v WHERE id = 'b4'"),
        vec![vec![Value::Text("MOVED".to_string())]]
    );
    Ok(())
}

/// The index and its schema row survive a reopen, and the reopened index is
/// still maintained. This is the load-order case: the index row is read from
/// sqlite_schema before the view it belongs to is registered as a table.
#[turso_macros::test(views)]
fn the_index_survives_a_reopen_and_stays_maintained(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let path = tmp_db.path.clone();
    {
        let conn = tmp_db.connect_limbo();
        setup(&conn)?;
    }
    // The open `Database` would otherwise be reused, with its loaded schema.
    drop(tmp_db);

    let reopened = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .build();
    let conn = reopened.connect_limbo();

    assert_index_matches_scan(
        &conn,
        "after reopen",
        "SELECT id, st FROM v WHERE id = 'b3'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b3'",
    );
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    assert_index_matches_scan(
        &conn,
        "write after reopen",
        "SELECT id, st FROM v WHERE id = 'b9'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b9'",
    );
    Ok(())
}

/// `DROP VIEW` takes the index's btree and its sqlite_schema row with it.
/// A surviving row would point at a freed root page and break the next open.
#[turso_macros::test(views)]
fn drop_view_removes_its_indexes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let path = tmp_db.path.clone();
    {
        let conn = tmp_db.connect_limbo();
        setup(&conn)?;
        conn.execute("DROP VIEW v")?;
        assert_eq!(
            limbo_exec_rows(
                &conn,
                "SELECT count(*) FROM sqlite_schema WHERE name = 'idx_v_id'"
            ),
            vec![vec![Value::Integer(0)]],
            "DROP VIEW must delete the index's sqlite_schema row"
        );
    }
    drop(tmp_db);

    let reopened = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .build();
    let conn = reopened.connect_limbo();
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM t_raw"),
        vec![vec![Value::Integer(6)]]
    );
    assert_eq!(
        limbo_exec_rows(&conn, "PRAGMA integrity_check"),
        vec![vec![Value::Text("ok".to_string())]]
    );
    Ok(())
}

/// The same writes as `committed_writes_keep_the_index_in_step`, in one
/// explicit transaction: the index is written when COMMIT applies the delta.
#[turso_macros::test(views)]
fn writes_in_a_committed_transaction_reach_the_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b2'")?;
    conn.execute("UPDATE t_raw SET id = 'z5' WHERE id = 'b5'")?;
    conn.execute("UPDATE t_raw SET st = 'MOVED' WHERE id = 'b4'")?;
    conn.execute("COMMIT")?;

    for key in ["b9", "b2", "b5", "z5", "b4"] {
        assert_index_matches_scan(
            &conn,
            &format!("key {key} after COMMIT"),
            &format!("SELECT id, st FROM v WHERE id = '{key}'"),
            &format!("SELECT id, st FROM v NOT INDEXED WHERE id = '{key}'"),
        );
    }
    assert_index_matches_scan(
        &conn,
        "full ordered read after COMMIT",
        "SELECT id, st FROM v ORDER BY id",
        "SELECT id, st FROM v NOT INDEXED ORDER BY id",
    );
    Ok(())
}

#[turso_macros::test(views)]
fn a_rolled_back_transaction_leaves_the_index_untouched(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    let before = limbo_exec_rows(&conn, "SELECT id, st FROM v ORDER BY id");

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b2'")?;
    conn.execute("UPDATE t_raw SET id = 'z5' WHERE id = 'b5'")?;
    conn.execute("ROLLBACK")?;

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id, st FROM v ORDER BY id"),
        before
    );
    for key in ["b9", "b2", "b5", "z5"] {
        assert_index_matches_scan(
            &conn,
            &format!("key {key} after ROLLBACK"),
            &format!("SELECT id, st FROM v WHERE id = '{key}'"),
            &format!("SELECT id, st FROM v NOT INDEXED WHERE id = '{key}'"),
        );
    }

    conn.execute("INSERT INTO t_raw (id, st) VALUES ('c1', 'AFTER')")?;
    assert_index_matches_scan(
        &conn,
        "write after ROLLBACK",
        "SELECT id, st FROM v ORDER BY id",
        "SELECT id, st FROM v NOT INDEXED ORDER BY id",
    );
    Ok(())
}

/// An aggregate view rewrites a group's row at the same rowid whenever the
/// group changes, so an index on the aggregate column moves with every write.
#[turso_macros::test(views)]
fn an_index_on_an_aggregate_column_follows_the_group(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT st, max(id) AS top FROM t_raw GROUP BY st")?;
    conn.execute("CREATE INDEX idx_v_top ON v(top)")?;
    for (id, st) in [("a", "X"), ("b", "X"), ("c", "Y")] {
        conn.execute(&format!("INSERT INTO t_raw VALUES ('{id}', '{st}')"))?;
    }
    conn.execute("UPDATE t_raw SET st = 'Y' WHERE id = 'b'")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'c'")?;
    conn.execute("INSERT INTO t_raw VALUES ('d', 'Z')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'd'")?;

    for top in ["a", "b", "c", "d"] {
        assert_index_matches_scan(
            &conn,
            &format!("groups whose top is {top}"),
            &format!("SELECT st, top FROM v WHERE top = '{top}'"),
            &format!("SELECT st, top FROM v NOT INDEXED WHERE top = '{top}'"),
        );
    }
    assert_index_matches_scan(
        &conn,
        "full ordered read",
        "SELECT st, top FROM v ORDER BY top",
        "SELECT st, top FROM v NOT INDEXED ORDER BY top",
    );
    Ok(())
}

/// REFRESH rebuilds the view's rows from scratch, and its indexes with them.
/// `randomblob` makes every rebuilt row differ from the one it replaces.
#[turso_macros::test(views)]
fn refresh_rebuilds_the_index_with_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, hex(randomblob(8)) AS h FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_h ON v(h)")?;
    for i in 0..4 {
        conn.execute(&format!("INSERT INTO t_raw (id, st) VALUES ('b{i}', 'X')"))?;
    }
    conn.execute("REFRESH MATERIALIZED VIEW v")?;

    assert_index_matches_scan(
        &conn,
        "ordered read after REFRESH",
        "SELECT id, h FROM v ORDER BY h",
        "SELECT id, h FROM v NOT INDEXED ORDER BY h",
    );
    Ok(())
}

/// The delta applier maintains a plain, total, non-unique key over the view's
/// output columns; every other shape is refused with the reason, before any
/// index btree exists.
#[test]
fn unsupported_index_shapes_are_refused() {
    let tmp_db = TempDatabase::builder()
        .with_opts(turso_core::DatabaseOpts::new().with_index_method(true))
        .with_views(true)
        .build();
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, st TEXT, n INTEGER)")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st, n FROM t")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v_ord AS SELECT id, st FROM t ORDER BY 2")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v_lim AS SELECT id, st FROM t ORDER BY 2 LIMIT 2")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('a', 'x', 1), ('b', 'x', 2), ('c', 'y', 3)")
        .unwrap();

    let cases = [
        ("CREATE INDEX i_ord ON v_ord(id)", "with ORDER BY"),
        ("CREATE INDEX i_lim ON v_lim(id)", "with LIMIT"),
        ("CREATE UNIQUE INDEX i_uniq ON v(id)", "UNIQUE index"),
        ("CREATE INDEX i_expr ON v(lower(st))", "expression index"),
        ("CREATE INDEX i_part ON v(id) WHERE n > 1", "partial index"),
        (
            "CREATE INDEX i_fts ON v USING fts (st)",
            "index method on materialized view",
        ),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(sql, reason)| match conn.execute(sql) {
            Ok(()) => Some(format!("accepted: {sql}")),
            Err(e) if e.to_string().contains(reason) => None,
            Err(e) => Some(format!("{sql}: error does not say `{reason}`: {e}")),
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));

    let rows = limbo_exec_rows(
        &conn,
        "SELECT count(*) FROM sqlite_schema WHERE type = 'index' AND tbl_name LIKE 'v%'",
    );
    assert_eq!(
        rows,
        vec![vec![Value::Integer(0)]],
        "a refused index left a schema row"
    );
}

/// A materialized view's columns and name are its definition's; ALTER TABLE
/// on it would leave its circuit, its state table and its index out of step
/// with its btree. Each form is refused, and the view stays maintained and
/// openable.
#[test]
fn alter_table_on_an_indexed_view_is_refused() {
    const REFUSED: &str = "cannot alter materialized view";
    // (statement, the view's name if the statement is accepted, the refusal)
    let forms = [
        ("ALTER TABLE v RENAME TO v2", "v2", REFUSED),
        ("ALTER TABLE v RENAME COLUMN st TO s2", "v", REFUSED),
        ("ALTER TABLE v RENAME COLUMN k TO k2", "v", REFUSED),
        ("ALTER TABLE v ADD COLUMN x TEXT", "v", REFUSED),
        ("ALTER TABLE v DROP COLUMN k", "v", REFUSED),
        (
            "ALTER TABLE t RENAME TO t2",
            "v",
            "dependent materialized view",
        ),
    ];
    let mut wrong = Vec::new();
    for (alter, renamed, reason) in forms {
        let outcome = std::panic::catch_unwind(|| alter_an_indexed_view(alter, renamed, reason));
        match outcome {
            Ok(found) => wrong.extend(found),
            Err(payload) => wrong.push(format!(
                "{alter}: panicked: {}",
                payload
                    .downcast_ref::<String>()
                    .map_or("<non-string payload>", String::as_str)
            )),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

fn alter_an_indexed_view(alter: &str, renamed: &str, reason: &str) -> Vec<String> {
    let mut wrong = Vec::new();
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let path = tmp_db.path.clone();
    {
        let conn = tmp_db.connect_limbo();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, st TEXT, k INTEGER)")
            .unwrap();
        conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st, k FROM t")
            .unwrap();
        conn.execute("CREATE INDEX vi ON v(st)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2)")
            .unwrap();
        let view = match conn.execute(alter) {
            Ok(()) => {
                wrong.push(format!("accepted: {alter}"));
                renamed
            }
            Err(e) if e.to_string().contains(reason) => "v",
            Err(e) => {
                wrong.push(format!("{alter}: error does not say `{reason}`: {e}"));
                "v"
            }
        };
        conn.execute("INSERT INTO t VALUES (3, 'c', 3)").unwrap();
        conn.execute("UPDATE t SET st = 'z' WHERE id = 1").unwrap();
        let indexed = limbo_exec_rows(&conn, &format!("SELECT id, st FROM {view} ORDER BY st"));
        let scanned = limbo_exec_rows(
            &conn,
            &format!("SELECT id, st FROM {view} NOT INDEXED ORDER BY st"),
        );
        if indexed != scanned {
            wrong.push(format!("{alter}: index {indexed:?} != scan {scanned:?}"));
        }
    }
    drop(tmp_db);
    let reopened = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .build();
    let conn = reopened.connect_limbo();
    let integrity = limbo_exec_rows(&conn, "PRAGMA integrity_check");
    if integrity != vec![vec![Value::Text("ok".into())]] {
        wrong.push(format!(
            "{alter}: after reopen, integrity_check {integrity:?}"
        ));
    }
    wrong
}
