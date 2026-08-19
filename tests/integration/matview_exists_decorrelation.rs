//! Correlated `EXISTS` / `NOT EXISTS` in a matview's WHERE.
//!
//! The rewrite turns each subquery into an indicator column computed by a join-shaped
//! operator, so the surrounding predicate is evaluated by the ordinary filter machinery.
//! Shapes the rewrite cannot express must say so at DDL time and name the limitation;
//! the failure this replaces was a view that reported success and answered empty.

use crate::common::{limbo_exec_rows, limbo_exec_rows_fallible, TempDatabase};
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
        "CREATE TABLE block (id INTEGER PRIMARY KEY, tag TEXT)",
    );
    limbo_exec_rows(
        conn,
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
    );
    limbo_exec_rows(
        conn,
        "INSERT INTO block VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')",
    );
    limbo_exec_rows(conn, "INSERT INTO block_requires VALUES (2,1),(4,3)");
}

const UNBLOCKED: &str = "SELECT b.id FROM block b \
     WHERE NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)";

#[track_caller]
fn assert_matches_recompute(conn: &Arc<Connection>, view_sql: &str, stage: &str) {
    assert_eq!(
        ids(limbo_exec_rows(conn, "SELECT id FROM unblocked")),
        ids(limbo_exec_rows(conn, view_sql)),
        "matview disagrees with a fresh recompute after {stage}"
    );
}

#[test]
fn correlated_not_exists_over_a_base_table_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW unblocked AS {UNBLOCKED}"),
    );
    assert_matches_recompute(&conn, UNBLOCKED, "materializing over existing rows");

    limbo_exec_rows(&conn, "INSERT INTO block_requires VALUES (1,3)");
    assert_matches_recompute(&conn, UNBLOCKED, "blocking a previously free row");

    limbo_exec_rows(&conn, "DELETE FROM block_requires WHERE block_id = 2");
    assert_matches_recompute(&conn, UNBLOCKED, "freeing a blocked row");

    limbo_exec_rows(&conn, "INSERT INTO block VALUES (5,'e')");
    assert_matches_recompute(&conn, UNBLOCKED, "inserting an outer row");

    limbo_exec_rows(
        &conn,
        "UPDATE block_requires SET block_id = 5 WHERE block_id = 4",
    );
    assert_matches_recompute(&conn, UNBLOCKED, "moving a requirement between rows");
}

/// The acceptance query's real shape: the subquery reads a materialized view, so the
/// rewrite depends on the view→view dependency edge and on parents refreshing first.
#[test]
fn correlated_not_exists_over_a_matview_source_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, tag TEXT)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE req_raw (block_id INTEGER, required_id INTEGER, live INTEGER)",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO block VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')",
    );
    limbo_exec_rows(&conn, "INSERT INTO req_raw VALUES (2,1,1),(4,3,1),(3,1,0)");
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW block_requires AS \
         SELECT block_id, required_id FROM req_raw WHERE live = 1",
    );

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW unblocked AS {UNBLOCKED}"),
    );
    assert_matches_recompute(&conn, UNBLOCKED, "materializing over a matview source");

    // Reaches the view only through the upstream matview's own delta.
    limbo_exec_rows(&conn, "UPDATE req_raw SET live = 1 WHERE block_id = 3");
    assert_matches_recompute(&conn, UNBLOCKED, "an upstream row entering the matview");

    limbo_exec_rows(&conn, "DELETE FROM req_raw WHERE block_id = 2");
    assert_matches_recompute(&conn, UNBLOCKED, "an upstream row leaving the matview");
}

#[track_caller]
fn assert_refused_naming(db: &TempDatabase, conn: &Arc<Connection>, sql: &str, needle: &str) {
    let err = limbo_exec_rows_fallible(db, conn, sql).expect_err("shape must be refused at DDL");
    let msg = err.to_string();
    assert!(
        msg.contains(needle),
        "error must name the limitation ({needle}), got: {msg}"
    );
    assert!(
        !msg.contains("Cannot convert LogicalExpr"),
        "refusal must be actionable, not a debug dump: {msg}"
    );
}

/// TE-3.a: a correlation that is not an equality has no hash key to count on.
#[test]
fn non_equality_correlation_is_refused() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    assert_refused_naming(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT b.id FROM block b \
         WHERE NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id > b.id)",
        "equality",
    );
}

#[test]
fn uncorrelated_exists_is_refused() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    assert_refused_naming(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT b.id FROM block b \
         WHERE EXISTS (SELECT 1 FROM block_requires br WHERE br.required_id = 1)",
        "correlat",
    );
}

/// A residual outer reference must be refused, not silently bound to the inner table's
/// column of the same bare name.
#[test]
fn a_residual_outer_reference_is_refused_even_when_names_collide() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE outer_t (id INTEGER PRIMARY KEY, ts INTEGER)",
    );
    limbo_exec_rows(&conn, "CREATE TABLE inner_t (id INTEGER, ts INTEGER)");
    limbo_exec_rows(&conn, "INSERT INTO outer_t VALUES (1,10),(2,20)");
    limbo_exec_rows(&conn, "INSERT INTO inner_t VALUES (1,5),(2,50)");

    assert_refused_naming(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT o.id FROM outer_t o \
         WHERE NOT EXISTS (SELECT 1 FROM inner_t i WHERE i.id = o.id AND i.ts > o.ts)",
        "outer",
    );
}

#[test]
fn a_reserved_indicator_column_name_is_refused() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, __exists_1 INTEGER)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
    );

    assert_refused_naming(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT b.id FROM block b \
         WHERE NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)",
        "__exists_",
    );
}

/// A foreign table is kept current through a mirror, and the mirror rewrite reaches only
/// FROM and JOIN clauses. Left unfenced, a subquery over one reads a table that never
/// receives deltas, so the view answers as though it were empty.
#[test]
fn a_foreign_table_cannot_be_a_subquery_source() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    let csv = db.path.parent().unwrap().join("blocked.csv");
    std::fs::write(&csv, "id,name\n1,alice\n3,carol\n").unwrap();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, tag TEXT)",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO block VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')",
    );
    limbo_exec_rows(&conn, "CREATE SERVER csv_srv OPTIONS (driver 'csv')");
    limbo_exec_rows(
        &conn,
        &format!(
            "CREATE FOREIGN TABLE blocked (id TEXT, name TEXT) SERVER csv_srv \
             OPTIONS (path '{}', skip_header 'true')",
            csv.display()
        ),
    );

    assert_refused_naming(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW free AS SELECT b.id FROM block b \
         WHERE NOT EXISTS (SELECT 1 FROM blocked f WHERE f.id = b.id)",
        "foreign table",
    );
}

const NOW_READY: &str = "SELECT b.id FROM block b \
     WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
       AND NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)";

fn seed_now(conn: &Arc<Connection>) {
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
         (1,'{\"task_state\":\"TODO\"}'),(2,'{\"task_state\":\"TODO\"}'), \
         (3,'{\"task_state\":\"DONE\"}'),(4,'{\"task_state\":\"TODO\"}'),(5,'{\"task_state\":\"TODO\"}')",
    );
    limbo_exec_rows(conn, "INSERT INTO block_requires VALUES (2,1),(4,3)");
}

#[track_caller]
fn assert_ready_matches(conn: &Arc<Connection>, stage: &str) {
    assert_eq!(
        ids(limbo_exec_rows(conn, "SELECT id FROM ready")),
        ids(limbo_exec_rows(conn, NOW_READY)),
        "matview disagrees with a fresh recompute after {stage}"
    );
}

/// The readiness query this feature exists for: a computed conjunct beside a correlated
/// NOT EXISTS. It is the shape that originally reported success and answered empty.
#[test]
fn a_computed_conjunct_beside_not_exists_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_now(&conn);

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW ready AS {NOW_READY}"),
    );
    assert_ready_matches(&conn, "materializing over existing rows");

    limbo_exec_rows(&conn, "INSERT INTO block_requires VALUES (1,3)");
    assert_ready_matches(&conn, "blocking a ready row");

    limbo_exec_rows(&conn, "DELETE FROM block_requires WHERE block_id = 2");
    assert_ready_matches(&conn, "freeing a blocked row");

    limbo_exec_rows(
        &conn,
        "UPDATE block SET properties = '{\"task_state\":\"DONE\"}' WHERE id = 5",
    );
    assert_ready_matches(&conn, "the computed conjunct flipping for a row");

    limbo_exec_rows(
        &conn,
        "INSERT INTO block VALUES (6,'{\"task_state\":\"TODO\"}')",
    );
    assert_ready_matches(&conn, "inserting a ready row");
}

/// Same shape with the subquery reading a matview, so the computed conjunct and the
/// indicator have to compose with a delta arriving through an upstream circuit.
#[test]
fn a_computed_conjunct_beside_not_exists_over_a_matview_source_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE req_raw (block_id INTEGER, required_id INTEGER, live INTEGER)",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO block VALUES \
         (1,'{\"task_state\":\"TODO\"}'),(2,'{\"task_state\":\"TODO\"}'), \
         (3,'{\"task_state\":\"DONE\"}'),(4,'{\"task_state\":\"TODO\"}')",
    );
    limbo_exec_rows(&conn, "INSERT INTO req_raw VALUES (2,1,1),(4,3,0)");
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW block_requires AS \
         SELECT block_id, required_id FROM req_raw WHERE live = 1",
    );

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW ready AS {NOW_READY}"),
    );
    assert_ready_matches(&conn, "materializing over a matview source");

    limbo_exec_rows(&conn, "UPDATE req_raw SET live = 1 WHERE block_id = 4");
    assert_ready_matches(&conn, "an upstream row entering the matview");
}

/// A disjunction of two subqueries beside a computed conjunct: three indicator columns
/// feed one predicate, and the OR is evaluated over them.
#[test]
fn a_computed_conjunct_beside_an_or_of_exists_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block_tags (block_id INTEGER, tag TEXT)",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO block VALUES \
         (1,'{\"task_state\":\"TODO\"}'),(2,'{\"task_state\":\"TODO\"}'), \
         (3,'{\"task_state\":\"DONE\"}'),(4,'{\"task_state\":\"TODO\"}')",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO block_tags VALUES (1,'agent'),(2,'human-only')",
    );

    const AGENT_READY: &str = "SELECT b.id FROM block b \
         WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
           AND (EXISTS (SELECT 1 FROM block_tags bt WHERE bt.block_id = b.id AND bt.tag = 'agent') \
                OR NOT EXISTS (SELECT 1 FROM block_tags bt WHERE bt.block_id = b.id AND bt.tag = 'human-only'))";

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW agent_ready AS {AGENT_READY}"),
    );
    let check = |stage: &str| {
        assert_eq!(
            ids(limbo_exec_rows(&conn, "SELECT id FROM agent_ready")),
            ids(limbo_exec_rows(&conn, AGENT_READY)),
            "matview disagrees with a fresh recompute after {stage}"
        );
    };
    check("materializing over existing rows");

    // Both branches true at once, then only the first.
    limbo_exec_rows(&conn, "INSERT INTO block_tags VALUES (2,'agent')");
    check("tagging a human-only row as agent");

    limbo_exec_rows(&conn, "INSERT INTO block_tags VALUES (4,'human-only')");
    check("marking a free row human-only");

    limbo_exec_rows(&conn, "DELETE FROM block_tags WHERE block_id = 1");
    check("removing the agent tag");
}

/// Guards against over-reach: a computed expression with no subquery beside it still
/// compiles and materializes.
#[test]
fn a_computed_expression_alone_still_materializes() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_now(&conn);

    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW todos AS SELECT b.id FROM block b \
         WHERE json_extract(b.properties, '$.task_state') = 'TODO'",
    );
    assert_eq!(
        ids(limbo_exec_rows(&conn, "SELECT id FROM todos")),
        ids(limbo_exec_rows(
            &conn,
            "SELECT b.id FROM block b WHERE json_extract(b.properties, '$.task_state') = 'TODO'"
        ))
    );
}
