//! An index on a materialized view, read inside an open transaction.
//!
//! A matview's rows written inside a transaction live in the view's
//! `ViewTransactionState`, not in its btree, and the index is written at the
//! same commit point as the btree. So the contract every test below states is:
//! **inside the transaction, an index-driven read of the view returns exactly
//! what a scan of the same view returns.** The left query lets the planner use
//! the index, the right reads the view `NOT INDEXED`.
//!
//! Comparing the view against ITSELF is what makes a failure unambiguous: the
//! difference can only come from the index.
//!
//! Chained views are out of scope — upstream matviews reading other matviews
//! do not populate at all.

use std::sync::Arc;

use rusqlite::types::Value;

use super::matview_index_oracle::assert_reads_no_view_index;
use crate::common::{limbo_exec_rows, TempDatabase};

fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE TABLE req (block_id TEXT, required_id TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_id ON v(id)")?;
    for i in 0..6 {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('b{i}', '{}')",
            if i % 2 == 0 { "TODO" } else { "DONE" }
        ))?;
    }
    conn.execute("INSERT INTO req (block_id, required_id) VALUES ('b0', 'b1')")?;
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
         SAME view in the SAME transaction\n  indexed: {indexed}\n  scanned: {scanned}"
    );
}

/// Reads of every shape, inside a transaction that inserted one row.
#[turso_macros::test(views)]
fn in_transaction_insert_is_visible_through_the_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;

    assert_index_matches_scan(
        &conn,
        "point read of the uncommitted row",
        "SELECT id, st FROM v WHERE id = 'b9'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b9'",
    );
    assert_index_matches_scan(
        &conn,
        "count",
        "SELECT count(*) FROM v",
        "SELECT count(*) FROM v NOT INDEXED",
    );
    assert_index_matches_scan(
        &conn,
        "range over the indexed column",
        "SELECT id FROM v WHERE id >= 'b3' ORDER BY id",
        "SELECT id FROM v NOT INDEXED WHERE id >= 'b3' ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "ORDER BY the indexed column",
        "SELECT id, st FROM v ORDER BY id",
        "SELECT id, st FROM v NOT INDEXED ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "LEFT JOIN whose inner side is the indexed view",
        "SELECT r.block_id, x.st FROM req r LEFT JOIN v x ON x.id = r.required_id \
         ORDER BY r.block_id",
        "SELECT r.block_id, x.st FROM req r LEFT JOIN v x NOT INDEXED ON x.id = r.required_id \
         ORDER BY r.block_id",
    );

    // Without this the equality above could hold vacuously, with both sides
    // blind to the uncommitted row.
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM v NOT INDEXED"),
        vec![vec![Value::Integer(7)]],
        "fixture: the scan must see the uncommitted row"
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// A DELETE and an UPDATE that moves a row across the indexed column: the
/// overlay must carry retractions, not only insertions.
#[turso_macros::test(views)]
fn in_transaction_delete_and_key_move_are_visible(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b2'")?;
    assert_index_matches_scan(
        &conn,
        "deleted row must be gone from the index too",
        "SELECT count(*) FROM v WHERE id = 'b2'",
        "SELECT count(*) FROM v NOT INDEXED WHERE id = 'b2'",
    );
    assert_index_matches_scan(
        &conn,
        "count after the delete",
        "SELECT count(*) FROM v",
        "SELECT count(*) FROM v NOT INDEXED",
    );

    conn.execute("UPDATE t_raw SET id = 'z5' WHERE id = 'b5'")?;
    assert_index_matches_scan(
        &conn,
        "old key after a key move",
        "SELECT count(*) FROM v WHERE id = 'b5'",
        "SELECT count(*) FROM v NOT INDEXED WHERE id = 'b5'",
    );
    assert_index_matches_scan(
        &conn,
        "new key after a key move",
        "SELECT id, st FROM v WHERE id = 'z5'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'z5'",
    );
    assert_index_matches_scan(
        &conn,
        "full ordered read after delete + key move",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// ROLLBACK must restore the pre-transaction answer, through the index.
#[turso_macros::test(views)]
fn rollback_restores_the_indexed_answer(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    let before = limbo_exec_rows(&conn, "SELECT id FROM v ORDER BY id");

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b0'")?;
    assert_index_matches_scan(
        &conn,
        "mid-transaction",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    conn.execute("ROLLBACK")?;

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM v ORDER BY id"),
        before,
        "after ROLLBACK the indexed read must return the pre-transaction answer"
    );
    assert_index_matches_scan(
        &conn,
        "after ROLLBACK",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    Ok(())
}

/// COMMIT must leave the index agreeing with the view it was merged against.
#[turso_macros::test(views)]
fn commit_leaves_the_index_agreeing_with_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b0'")?;
    conn.execute("UPDATE t_raw SET id = 'z5' WHERE id = 'b5'")?;
    conn.execute("COMMIT")?;

    assert_index_matches_scan(
        &conn,
        "after COMMIT",
        "SELECT id, st FROM v ORDER BY id",
        "SELECT id, st FROM v NOT INDEXED ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "point read of the committed row",
        "SELECT id, st FROM v WHERE id = 'b9'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'b9'",
    );
    assert_index_matches_scan(
        &conn,
        "point read of the committed deletion",
        "SELECT count(*) FROM v WHERE id = 'b0'",
        "SELECT count(*) FROM v NOT INDEXED WHERE id = 'b0'",
    );
    Ok(())
}

/// The overlay is this connection's. A second connection must see none of it,
/// through the index or otherwise, until COMMIT.
#[turso_macros::test(views)]
fn a_second_connection_does_not_see_the_overlay(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let writer = tmp_db.connect_limbo();
    setup(&writer)?;
    let reader = tmp_db.connect_limbo();
    let before = limbo_exec_rows(&reader, "SELECT count(*) FROM v");

    writer.execute("BEGIN")?;
    writer.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    assert_eq!(
        limbo_exec_rows(&reader, "SELECT count(*) FROM v"),
        before,
        "the reader must not see the writer's uncommitted row through the index"
    );
    assert_index_matches_scan(
        &reader,
        "reader during the writer's transaction",
        "SELECT count(*) FROM v",
        "SELECT count(*) FROM v NOT INDEXED",
    );
    writer.execute("COMMIT")?;

    assert_index_matches_scan(
        &reader,
        "reader after the writer commits",
        "SELECT count(*) FROM v",
        "SELECT count(*) FROM v NOT INDEXED",
    );
    Ok(())
}

/// Descending reads walk the merge backwards, from the last entry.
#[turso_macros::test(views)]
fn descending_reads_see_the_overlay(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b4'")?;

    assert_index_matches_scan(
        &conn,
        "descending full read",
        "SELECT id, st FROM v ORDER BY id DESC",
        "SELECT id, st FROM v NOT INDEXED ORDER BY id DESC",
    );
    assert_index_matches_scan(
        &conn,
        "descending range",
        "SELECT id FROM v WHERE id <= 'b3' ORDER BY id DESC",
        "SELECT id FROM v NOT INDEXED WHERE id <= 'b3' ORDER BY id DESC",
    );
    assert_index_matches_scan(
        &conn,
        "strict descending range",
        "SELECT id FROM v WHERE id < 'b9' ORDER BY id DESC",
        "SELECT id FROM v NOT INDEXED WHERE id < 'b9' ORDER BY id DESC",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// A two-column key: the leading column alone is a prefix seek, and entries
/// that share it are ordered by the second column and then the rowid.
#[turso_macros::test(views)]
fn a_two_column_index_sees_the_overlay(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_st_id ON v(st, id)")?;
    for i in 0..6 {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('b{i}', '{}')",
            if i % 2 == 0 { "TODO" } else { "DONE" }
        ))?;
    }

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'TODO')")?;
    conn.execute("UPDATE t_raw SET st = 'DONE' WHERE id = 'b0'")?;

    assert_index_matches_scan(
        &conn,
        "prefix seek on the leading column",
        "SELECT id, st FROM v WHERE st = 'TODO' ORDER BY id",
        "SELECT id, st FROM v NOT INDEXED WHERE st = 'TODO' ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "full key seek",
        "SELECT id, st FROM v WHERE st = 'TODO' AND id = 'b9'",
        "SELECT id, st FROM v NOT INDEXED WHERE st = 'TODO' AND id = 'b9'",
    );
    assert_index_matches_scan(
        &conn,
        "ordered by the whole key",
        "SELECT st, id FROM v ORDER BY st, id",
        "SELECT st, id FROM v NOT INDEXED ORDER BY st, id",
    );
    assert_index_matches_scan(
        &conn,
        "descending on the whole key",
        "SELECT st, id FROM v ORDER BY st DESC, id DESC",
        "SELECT st, id FROM v NOT INDEXED ORDER BY st DESC, id DESC",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// A DESC key column: the overlay must sort under the index's own
/// comparator, not by value order.
#[turso_macros::test(views)]
fn a_descending_key_column_sees_the_overlay(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_id_desc ON v(id DESC)")?;
    for i in 0..6 {
        conn.execute(&format!("INSERT INTO t_raw (id, st) VALUES ('b{i}', 'X')"))?;
    }

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('a1', 'NEW')")?;

    assert_index_matches_scan(
        &conn,
        "descending key, natural order",
        "SELECT id FROM v ORDER BY id DESC",
        "SELECT id FROM v NOT INDEXED ORDER BY id DESC",
    );
    assert_index_matches_scan(
        &conn,
        "descending key, reverse order",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "descending key, point read",
        "SELECT id, st FROM v WHERE id = 'a1'",
        "SELECT id, st FROM v NOT INDEXED WHERE id = 'a1'",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}
/// The overlay retracts the largest key, then every key: the merge must be
/// able to end on a tombstone and to run out of entries entirely.
#[turso_macros::test(views)]
fn deleting_the_largest_key_then_every_row(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b5'")?;
    assert_index_matches_scan(
        &conn,
        "DESC LIMIT 1 after the largest key was deleted",
        "SELECT id FROM v ORDER BY id DESC LIMIT 1",
        "SELECT id FROM v NOT INDEXED ORDER BY id DESC LIMIT 1",
    );

    conn.execute("DELETE FROM t_raw")?;
    assert_index_matches_scan(
        &conn,
        "every row deleted",
        "SELECT id FROM v ORDER BY id",
        "SELECT id FROM v NOT INDEXED ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "count of the emptied view",
        "SELECT count(*) FROM v",
        "SELECT count(*) FROM v NOT INDEXED",
    );

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM v NOT INDEXED"),
        vec![vec![Value::Integer(0)]],
        "fixture: the scan must see the emptied view"
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// Two rows sharing a key value order by the trailing rowid, so the merge
/// must compare it and not stop at the key.
#[turso_macros::test(views)]
fn two_rows_that_collide_on_the_key_value(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id INTEGER PRIMARY KEY, k TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_k ON v(k)")?;
    conn.execute("INSERT INTO t_raw VALUES (1, 'a'), (2, 'b'), (3, 'c')")?;

    conn.execute("BEGIN")?;
    conn.execute("UPDATE t_raw SET k = 'a' WHERE id = 2")?;
    assert_index_matches_scan(
        &conn,
        "point read of the shared key value",
        "SELECT id, k FROM v WHERE k = 'a' ORDER BY id",
        "SELECT id, k FROM v NOT INDEXED WHERE k = 'a' ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "full ordered read across the collision",
        "SELECT id, k FROM v ORDER BY k, id",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k, id",
    );
    conn.execute("COMMIT")?;
    assert_index_matches_scan(
        &conn,
        "the collision after commit",
        "SELECT id, k FROM v WHERE k = 'a' ORDER BY id",
        "SELECT id, k FROM v NOT INDEXED WHERE k = 'a' ORDER BY id",
    );

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM v NOT INDEXED WHERE k = 'a'"),
        vec![vec![Value::Integer(2)]],
        "fixture: two rows must share the key"
    );
    Ok(())
}

/// NULL sorts before every value in an index key, on both sides of the merge.
#[turso_macros::test(views)]
fn null_key_values_added_and_removed_in_the_transaction(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id INTEGER PRIMARY KEY, k TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_k ON v(k)")?;
    conn.execute("INSERT INTO t_raw VALUES (1, NULL), (2, 'b'), (3, NULL), (4, 'd')")?;
    assert_index_matches_scan(
        &conn,
        "committed NULL keys",
        "SELECT id, k FROM v ORDER BY k, id",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k, id",
    );

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw VALUES (5, NULL)")?;
    conn.execute("UPDATE t_raw SET k = NULL WHERE id = 2")?;
    conn.execute("UPDATE t_raw SET k = 'z' WHERE id = 3")?;
    assert_index_matches_scan(
        &conn,
        "NULL keys added and removed",
        "SELECT id, k FROM v ORDER BY k, id",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k, id",
    );
    assert_index_matches_scan(
        &conn,
        "IS NULL point read",
        "SELECT id FROM v WHERE k IS NULL ORDER BY id",
        "SELECT id FROM v NOT INDEXED WHERE k IS NULL ORDER BY id",
    );
    assert_index_matches_scan(
        &conn,
        "count with NULL keys",
        "SELECT count(*) FROM v",
        "SELECT count(*) FROM v NOT INDEXED",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// An INTEGER key orders numerically, so the overlay cannot be sorted by the
/// serialised bytes.
#[turso_macros::test(views)]
fn integer_keys_stay_in_numeric_order_with_an_overlay(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id INTEGER PRIMARY KEY, n INTEGER)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, n FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_n ON v(n)")?;
    conn.execute("INSERT INTO t_raw VALUES (1, 10), (2, 2), (3, 30)")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw VALUES (4, 7)")?;
    conn.execute("UPDATE t_raw SET n = 1 WHERE id = 3")?;
    assert_index_matches_scan(
        &conn,
        "ordered read",
        "SELECT id, n FROM v ORDER BY n, id",
        "SELECT id, n FROM v NOT INDEXED ORDER BY n, id",
    );
    assert_index_matches_scan(
        &conn,
        "range over the integer key",
        "SELECT id FROM v WHERE n >= 7 ORDER BY n",
        "SELECT id FROM v NOT INDEXED WHERE n >= 7 ORDER BY n",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// A DESC key whose overlay adds beyond both ends of the committed btree and
/// retracts from the middle.
#[turso_macros::test(views)]
fn a_descending_key_with_the_overlay_at_both_ends(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id INTEGER PRIMARY KEY, k TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t_raw")?;
    conn.execute("CREATE INDEX idx_v_k ON v(k DESC)")?;
    conn.execute("INSERT INTO t_raw VALUES (1, 'a'), (2, 'c'), (3, 'e')")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw VALUES (4, 'z'), (5, 'A')")?;
    conn.execute("DELETE FROM t_raw WHERE id = 3")?;
    assert_index_matches_scan(
        &conn,
        "ascending read of a DESC key",
        "SELECT id, k FROM v ORDER BY k",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k",
    );
    assert_index_matches_scan(
        &conn,
        "descending read of a DESC key",
        "SELECT id, k FROM v ORDER BY k DESC",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k DESC",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// A row moving across the view's own WHERE predicate enters the overlay as
/// an insertion or a retraction with no write to the indexed column.
#[turso_macros::test(views)]
fn rows_moving_across_a_filtered_views_predicate(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id INTEGER PRIMARY KEY, k TEXT, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t_raw WHERE st = 'TODO'")?;
    conn.execute("CREATE INDEX idx_v_k ON v(k)")?;
    conn.execute("INSERT INTO t_raw VALUES (1, 'a', 'TODO'), (2, 'b', 'TODO'), (3, 'c', 'DONE')")?;
    assert_index_matches_scan(
        &conn,
        "committed filtered view",
        "SELECT id, k FROM v ORDER BY k",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k",
    );

    conn.execute("BEGIN")?;
    conn.execute("UPDATE t_raw SET st = 'DONE' WHERE id = 1")?;
    assert_index_matches_scan(
        &conn,
        "a row moved out of the filter",
        "SELECT id, k FROM v ORDER BY k",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k",
    );
    assert_index_matches_scan(
        &conn,
        "point read of the row that left the filter",
        "SELECT count(*) FROM v WHERE k = 'a'",
        "SELECT count(*) FROM v NOT INDEXED WHERE k = 'a'",
    );

    conn.execute("UPDATE t_raw SET st = 'TODO' WHERE id = 3")?;
    assert_index_matches_scan(
        &conn,
        "a row moved into the filter",
        "SELECT id, k FROM v ORDER BY k",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k",
    );
    conn.execute("COMMIT")?;
    assert_index_matches_scan(
        &conn,
        "both filter moves after commit",
        "SELECT id, k FROM v ORDER BY k",
        "SELECT id, k FROM v NOT INDEXED ORDER BY k",
    );

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM v NOT INDEXED ORDER BY k"),
        vec![vec![Value::Integer(2)], vec![Value::Integer(3)]],
        "fixture: the filter moves must have taken effect"
    );
    Ok(())
}

/// An aggregate view: one input row changes a group's value in place, so the
/// overlay carries a retraction and an insertion of the same key.
#[turso_macros::test(views)]
fn an_aggregate_view_indexed_on_its_grouping_key(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id INTEGER PRIMARY KEY, g TEXT, n INTEGER)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT g, sum(n) AS s FROM t_raw GROUP BY g")?;
    conn.execute("CREATE INDEX idx_v_g ON v(g)")?;
    conn.execute("INSERT INTO t_raw VALUES (1,'a',1),(2,'b',2),(3,'a',3)")?;
    assert_index_matches_scan(
        &conn,
        "committed aggregate view",
        "SELECT g, s FROM v ORDER BY g",
        "SELECT g, s FROM v NOT INDEXED ORDER BY g",
    );

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw VALUES (4,'a',10),(5,'c',5)")?;
    conn.execute("DELETE FROM t_raw WHERE id = 2")?;
    assert_index_matches_scan(
        &conn,
        "aggregate view with an overlay",
        "SELECT g, s FROM v ORDER BY g",
        "SELECT g, s FROM v NOT INDEXED ORDER BY g",
    );
    assert_index_matches_scan(
        &conn,
        "point read of a changed group",
        "SELECT g, s FROM v WHERE g = 'a'",
        "SELECT g, s FROM v NOT INDEXED WHERE g = 'a'",
    );
    conn.execute("COMMIT")?;
    assert_index_matches_scan(
        &conn,
        "aggregate view after commit",
        "SELECT g, s FROM v ORDER BY g",
        "SELECT g, s FROM v NOT INDEXED ORDER BY g",
    );
    Ok(())
}
