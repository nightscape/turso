//! Several computed expressions in one matview WHERE.
//!
//! The projection rewrite carries a computed conjunct in a temp column and rewrites the
//! predicate to read that column. With one temp column for the whole predicate, a second
//! computed conjunct is pointed at the first one's value, and the two comparisons become
//! a test no row satisfies — the view CREATEs and answers empty for good.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use std::sync::Arc;
use turso_core::Connection;

fn ids(rows: Vec<Vec<Value>>) -> Vec<i64> {
    let mut out: Vec<i64> = rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("expected integer, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn seed(conn: &Arc<Connection>) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(
        conn,
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
    );
    limbo_exec_rows(
        conn,
        "INSERT INTO block VALUES \
         (1,'{\"task_state\":\"TODO\",\"prio\":\"high\"}'), \
         (2,'{\"task_state\":\"TODO\",\"prio\":\"low\"}'), \
         (3,'{\"task_state\":\"DONE\",\"prio\":\"high\"}'), \
         (4,'{\"task_state\":\"TODO\",\"prio\":\"high\"}')",
    );
    limbo_exec_rows(conn, "INSERT INTO block_requires VALUES (4,1)");
}

#[track_caller]
fn assert_matches(conn: &Arc<Connection>, view: &str, select: &str, stage: &str) {
    assert_eq!(
        ids(limbo_exec_rows(conn, &format!("SELECT id FROM {view}"))),
        ids(limbo_exec_rows(conn, select)),
        "matview disagrees with a fresh recompute after {stage}"
    );
}

const TWO_COMPUTED: &str = "SELECT b.id FROM block b \
     WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
       AND json_extract(b.properties, '$.prio') = 'high'";

/// Two different computed expressions, no subquery anywhere.
#[test]
fn two_computed_conjuncts_maintain() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW hi AS {TWO_COMPUTED}"),
    );
    assert_matches(
        &conn,
        "hi",
        TWO_COMPUTED,
        "materializing over existing rows",
    );

    limbo_exec_rows(
        &conn,
        "UPDATE block SET properties = '{\"task_state\":\"TODO\",\"prio\":\"high\"}' WHERE id = 2",
    );
    assert_matches(&conn, "hi", TWO_COMPUTED, "a row satisfying both conjuncts");

    limbo_exec_rows(
        &conn,
        "UPDATE block SET properties = '{\"task_state\":\"DONE\",\"prio\":\"high\"}' WHERE id = 1",
    );
    assert_matches(&conn, "hi", TWO_COMPUTED, "the first conjunct flipping");

    limbo_exec_rows(
        &conn,
        "INSERT INTO block VALUES (5,'{\"task_state\":\"TODO\",\"prio\":\"high\"}')",
    );
    assert_matches(&conn, "hi", TWO_COMPUTED, "inserting a matching row");
}

const THREE_COMPUTED: &str = "SELECT b.id FROM block b \
     WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
       AND json_extract(b.properties, '$.prio') = 'high' \
       AND length(b.properties) > 10";

#[test]
fn three_computed_conjuncts_maintain() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW hi3 AS {THREE_COMPUTED}"),
    );
    assert_matches(
        &conn,
        "hi3",
        THREE_COMPUTED,
        "materializing over existing rows",
    );
}

const MIXED: &str = "SELECT b.id FROM block b \
     WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
       AND json_extract(b.properties, '$.prio') = 'high' \
       AND NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)";

/// Two computed conjuncts beside an indicator: the temp columns and the indicator column
/// have to coexist in one predicate.
#[test]
fn two_computed_conjuncts_beside_an_indicator_maintain() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(&conn, &format!("CREATE MATERIALIZED VIEW mixed AS {MIXED}"));
    assert_matches(&conn, "mixed", MIXED, "materializing over existing rows");

    limbo_exec_rows(&conn, "DELETE FROM block_requires WHERE block_id = 4");
    assert_matches(&conn, "mixed", MIXED, "freeing a blocked row");

    limbo_exec_rows(
        &conn,
        "UPDATE block SET properties = '{\"task_state\":\"TODO\",\"prio\":\"high\"}' WHERE id = 2",
    );
    assert_matches(&conn, "mixed", MIXED, "a computed conjunct flipping");

    limbo_exec_rows(&conn, "INSERT INTO block_requires VALUES (1,3)");
    assert_matches(&conn, "mixed", MIXED, "blocking a matching row");
}

const REPEATED: &str = "SELECT b.id FROM block b \
     WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
       AND json_extract(b.properties, '$.task_state') <> 'DONE'";

/// The same expression twice is the case that already worked, because both conjuncts
/// wanted the same value.
#[test]
fn a_repeated_computed_expression_still_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW rep AS {REPEATED}"),
    );
    assert_matches(&conn, "rep", REPEATED, "materializing over existing rows");

    limbo_exec_rows(
        &conn,
        "UPDATE block SET properties = '{\"task_state\":\"DONE\",\"prio\":\"low\"}' WHERE id = 1",
    );
    assert_matches(&conn, "rep", REPEATED, "a row leaving the view");
}
