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
fn assert_view_matches(conn: &Arc<Connection>, view: &str, view_sql: &str, stage: &str) {
    assert_eq!(
        ids(limbo_exec_rows(conn, &format!("SELECT * FROM {view}"))),
        ids(limbo_exec_rows(conn, view_sql)),
        "matview {view} disagrees with a fresh recompute after {stage}"
    );
}

/// A correlation that is not an equality has no hash key to count on.
#[test]
fn non_equality_correlation_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);
    const VIEW_SQL: &str = "SELECT b.id FROM block b \
         WHERE NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id > b.id)";

    limbo_exec_rows(&conn, &format!("CREATE MATERIALIZED VIEW v AS {VIEW_SQL}"));
    assert_view_matches(&conn, "v", VIEW_SQL, "materializing over existing rows");

    limbo_exec_rows(&conn, "INSERT INTO block_requires VALUES (9,1)");
    assert_view_matches(&conn, "v", VIEW_SQL, "adding a requirement above every id");

    limbo_exec_rows(&conn, "DELETE FROM block_requires WHERE block_id = 4");
    assert_view_matches(
        &conn,
        "v",
        VIEW_SQL,
        "removing the highest earlier requirement",
    );

    limbo_exec_rows(&conn, "INSERT INTO block VALUES (10,'j')");
    assert_view_matches(&conn, "v", VIEW_SQL, "inserting an outer row");
}

/// A residual outer reference must bind to the outer table, not to the inner
/// table's column of the same bare name.
#[test]
fn a_residual_outer_reference_maintains_when_names_collide() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE outer_t (id INTEGER PRIMARY KEY, ts INTEGER)",
    );
    limbo_exec_rows(&conn, "CREATE TABLE inner_t (id INTEGER, ts INTEGER)");
    limbo_exec_rows(&conn, "INSERT INTO outer_t VALUES (1,10),(2,20)");
    limbo_exec_rows(&conn, "INSERT INTO inner_t VALUES (1,5),(2,50)");
    const VIEW_SQL: &str = "SELECT o.id FROM outer_t o \
         WHERE NOT EXISTS (SELECT 1 FROM inner_t i WHERE i.id = o.id AND i.ts > o.ts)";

    limbo_exec_rows(&conn, &format!("CREATE MATERIALIZED VIEW v AS {VIEW_SQL}"));
    assert_view_matches(&conn, "v", VIEW_SQL, "materializing over existing rows");

    limbo_exec_rows(&conn, "INSERT INTO inner_t VALUES (1,15)");
    assert_view_matches(
        &conn,
        "v",
        VIEW_SQL,
        "an inner row newer than the outer row",
    );

    limbo_exec_rows(&conn, "UPDATE outer_t SET ts = 60 WHERE id = 2");
    assert_view_matches(
        &conn,
        "v",
        VIEW_SQL,
        "the outer value overtaking the inner row",
    );

    limbo_exec_rows(&conn, "DELETE FROM inner_t WHERE ts = 15");
    assert_view_matches(&conn, "v", VIEW_SQL, "removing the newer inner row");
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

fn seed_holon(conn: &Arc<Connection>) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(conn, "CREATE TABLE block_tags (block_id INTEGER, tag TEXT)");
    limbo_exec_rows(
        conn,
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
    );
    limbo_exec_rows(
        conn,
        "INSERT INTO block VALUES \
         (1,'{\"task_state\":\"TODO\"}'),(2,'{\"task_state\":\"DONE\"}'), \
         (3,'{\"task_state\":\"TODO\"}'),(4,'{\"task_state\":\"DONE\"}'),(5,'{}')",
    );
    limbo_exec_rows(
        conn,
        "INSERT INTO block_tags VALUES (1,'urgent'),(2,'urgent'),(3,'later')",
    );
    limbo_exec_rows(
        conn,
        "INSERT INTO block_requires VALUES (1,2),(3,2),(3,4),(5,1)",
    );
}

fn maintain_through_holon_writes(conn: &Arc<Connection>, view: &str, view_sql: &str) {
    limbo_exec_rows(
        conn,
        &format!("CREATE MATERIALIZED VIEW {view} AS {view_sql}"),
    );
    let check = |stage: &str| assert_view_matches(conn, view, view_sql, stage);
    check("materializing over existing rows");

    limbo_exec_rows(conn, "INSERT INTO block_requires VALUES (1,4)");
    check("a second requirement for one row");
    limbo_exec_rows(conn, "DELETE FROM block_requires WHERE block_id = 3");
    check("removing every requirement of a row");
    limbo_exec_rows(
        conn,
        "UPDATE block SET properties = '{\"task_state\":\"TODO\"}' WHERE id = 2",
    );
    check("a required row changing state");
    limbo_exec_rows(
        conn,
        "INSERT INTO block_tags VALUES (4,'urgent'),(4,'urgent')",
    );
    check("duplicate tag rows");
    limbo_exec_rows(conn, "DELETE FROM block WHERE id = 4");
    check("deleting a required row");
    limbo_exec_rows(
        conn,
        "INSERT INTO block VALUES (6,'{\"task_state\":\"TODO\"}')",
    );
    check("inserting an outer row");
}

/// The subquery reads a JOIN, which the planner cannot turn into a semi join.
#[test]
fn exists_over_a_join_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_holon(&conn);

    maintain_through_holon_writes(
        &conn,
        "blocked_by_open",
        "SELECT b.id FROM block b WHERE EXISTS (SELECT 1 FROM block_requires bl \
         JOIN block r ON r.id = bl.required_id \
         WHERE bl.block_id = b.id AND json_extract(r.properties, '$.task_state') <> 'DONE')",
    );
}

/// `json_extract` in the subquery's WHERE can fail on malformed JSON, so a join
/// would have to skip rows the subquery tests.
#[test]
fn exists_with_a_json_filter_under_or_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_holon(&conn);

    maintain_through_holon_writes(
        &conn,
        "v",
        "SELECT b.id FROM block b WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
         OR EXISTS (SELECT 1 FROM block_requires bl WHERE bl.block_id = b.id \
         AND json_extract(bl.required_id, '$') = 2)",
    );
}

/// An EXISTS inside an EXISTS that correlates to the query directly around it.
#[test]
fn nested_exists_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_holon(&conn);

    maintain_through_holon_writes(
        &conn,
        "v",
        "SELECT b.id FROM block b WHERE EXISTS (SELECT 1 FROM block_requires bl \
         WHERE bl.block_id = b.id AND EXISTS \
         (SELECT 1 FROM block_tags bt WHERE bt.block_id = bl.required_id AND bt.tag = 'urgent'))",
    );
}

/// A nested EXISTS that sits under an OR cannot become a join in the inner query.
#[test]
fn nested_exists_under_or_maintains() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_holon(&conn);

    maintain_through_holon_writes(
        &conn,
        "v",
        "SELECT b.id FROM block b WHERE NOT EXISTS (SELECT 1 FROM block_requires bl \
         WHERE bl.block_id = b.id AND (EXISTS \
         (SELECT 1 FROM block_tags bt WHERE bt.block_id = bl.required_id AND bt.tag = 'urgent') \
         OR bl.required_id = 4))",
    );
}

#[track_caller]
fn assert_refused_naming(db: &TempDatabase, conn: &Arc<Connection>, sql: &str, needle: &str) {
    let err = limbo_exec_rows_fallible(db, conn, sql).expect_err("shape must be refused at DDL");
    let msg = err.to_string();
    assert!(
        msg.contains(needle),
        "error must name the limitation ({needle}), got: {msg}"
    );
}

/// A subquery two levels down that reads the outermost query is not supported.
#[test]
fn a_nested_exists_that_reads_two_levels_out_is_refused() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed_holon(&conn);

    assert_refused_naming(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT b.id FROM block b \
         WHERE EXISTS (SELECT 1 FROM block_requires bl WHERE bl.block_id = b.id AND EXISTS \
         (SELECT 1 FROM block_tags bt WHERE bt.block_id = b.id AND bt.tag = 'urgent'))",
        "directly around",
    );
}
