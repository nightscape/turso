//! Differential check of materialized views whose WHERE mixes AND, OR and NOT
//! over EXISTS subqueries and plain predicates.
//!
//! The predicates follow what Holon generates: a JSON property test, tag
//! membership, an edge to a required block (optionally with a property test on
//! it) and an EXISTS that reads no outer column. After every write the views
//! must hold the same rows, with the same duplicates, as a fresh run of their
//! SELECT.

use crate::common::{limbo_exec_rows, limbo_exec_rows_fallible, TempDatabase};
use proptest::prelude::*;
use std::sync::Arc;
use turso_core::Connection;

const STATES: [&str; 2] = ["TODO", "DONE"];
const TAGS: [&str; 2] = ["agent", "urgent"];
const BLOCK_IDS: std::ops::Range<i64> = 1..7;

#[derive(Debug, Clone)]
enum Predicate {
    PropEq(usize),
    Membership(usize),
    EdgeExists(Option<usize>),
    Uncorrelated(usize),
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
}

impl Predicate {
    fn sql(&self) -> String {
        match self {
            Predicate::PropEq(state) => format!(
                "json_extract(b.properties, '$.task_state') = '{}'",
                STATES[*state]
            ),
            Predicate::Membership(tag) => format!(
                "EXISTS (SELECT 1 FROM block_tags bt WHERE bt.block_id = b.id AND bt.tag = '{}')",
                TAGS[*tag]
            ),
            Predicate::EdgeExists(state) => {
                let filter = state
                    .map(|state| {
                        format!(
                            " AND json_extract(r.properties, '$.task_state') = '{}'",
                            STATES[state]
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "EXISTS (SELECT 1 FROM block_requires bl JOIN block r ON r.id = bl.required_id \
                     WHERE bl.block_id = b.id{filter})"
                )
            }
            Predicate::Uncorrelated(tag) => format!(
                "EXISTS (SELECT 1 FROM block_tags ut WHERE ut.tag = '{}')",
                TAGS[*tag]
            ),
            Predicate::And(left, right) => format!("({} AND {})", left.sql(), right.sql()),
            Predicate::Or(left, right) => format!("({} OR {})", left.sql(), right.sql()),
            Predicate::Not(inner) => format!("NOT ({})", inner.sql()),
        }
    }
}

fn predicate() -> impl Strategy<Value = Predicate> {
    let leaf = prop_oneof![
        (0..STATES.len()).prop_map(Predicate::PropEq),
        (0..TAGS.len()).prop_map(Predicate::Membership),
        proptest::option::of(0..STATES.len()).prop_map(Predicate::EdgeExists),
        (0..TAGS.len()).prop_map(Predicate::Uncorrelated),
    ];
    leaf.prop_recursive(3, 8, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(left, right)| Predicate::And(Box::new(left), Box::new(right))),
            (inner.clone(), inner.clone())
                .prop_map(|(left, right)| Predicate::Or(Box::new(left), Box::new(right))),
            inner.prop_map(|inner| Predicate::Not(Box::new(inner))),
        ]
    })
}

#[derive(Debug, Clone)]
enum Write {
    InsertBlock { id: i64, state: Option<usize> },
    DeleteBlock { id: i64 },
    SetState { id: i64, state: Option<usize> },
    InsertTag { block_id: i64, tag: usize },
    DeleteTags { block_id: i64 },
    InsertRequire { block_id: i64, required_id: i64 },
    DeleteRequires { block_id: i64 },
}

impl Write {
    fn sql(&self) -> String {
        match self {
            Write::InsertBlock { id, state } => format!(
                "INSERT INTO block SELECT {id}, {} WHERE NOT EXISTS (SELECT 1 FROM block WHERE id = {id})",
                properties_sql(*state)
            ),
            Write::DeleteBlock { id } => format!("DELETE FROM block WHERE id = {id}"),
            Write::SetState { id, state } => format!(
                "UPDATE block SET properties = {} WHERE id = {id}",
                properties_sql(*state)
            ),
            Write::InsertTag { block_id, tag } => format!(
                "INSERT INTO block_tags VALUES ({block_id}, '{}')",
                TAGS[*tag]
            ),
            Write::DeleteTags { block_id } => {
                format!("DELETE FROM block_tags WHERE block_id = {block_id}")
            }
            Write::InsertRequire {
                block_id,
                required_id,
            } => format!("INSERT INTO block_requires VALUES ({block_id}, {required_id})"),
            Write::DeleteRequires { block_id } => {
                format!("DELETE FROM block_requires WHERE block_id = {block_id}")
            }
        }
    }
}

fn properties_sql(state: Option<usize>) -> String {
    match state {
        Some(state) => format!("'{{\"task_state\":\"{}\"}}'", STATES[state]),
        None => "'{}'".to_string(),
    }
}

fn write() -> impl Strategy<Value = Write> {
    let id = BLOCK_IDS;
    let state = proptest::option::of(0..STATES.len());
    prop_oneof![
        (id.clone(), state.clone()).prop_map(|(id, state)| Write::InsertBlock { id, state }),
        id.clone().prop_map(|id| Write::DeleteBlock { id }),
        (id.clone(), state).prop_map(|(id, state)| Write::SetState { id, state }),
        (id.clone(), 0..TAGS.len()).prop_map(|(block_id, tag)| Write::InsertTag { block_id, tag }),
        id.clone()
            .prop_map(|block_id| Write::DeleteTags { block_id }),
        (id.clone(), id.clone()).prop_map(|(block_id, required_id)| Write::InsertRequire {
            block_id,
            required_id
        }),
        id.prop_map(|block_id| Write::DeleteRequires { block_id }),
    ]
}

#[derive(Debug, Clone)]
enum Step {
    Single(Write),
    Transaction { writes: Vec<Write>, commit: bool },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => write().prop_map(Step::Single),
        1 => (proptest::collection::vec(write(), 1..4), any::<bool>())
            .prop_map(|(writes, commit)| Step::Transaction { writes, commit }),
    ]
}

fn rows_sorted(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut rows: Vec<String> = limbo_exec_rows(conn, sql)
        .into_iter()
        .map(|row| format!("{row:?}"))
        .collect();
    rows.sort();
    rows
}

fn views(predicate: &Predicate) -> [(&'static str, String); 2] {
    let predicate = predicate.sql();
    [
        (
            "v_ids",
            format!("SELECT b.id FROM block b WHERE {predicate}"),
        ),
        (
            "v_states",
            format!(
                "SELECT json_extract(b.properties, '$.task_state') AS s FROM block b WHERE {predicate}"
            ),
        ),
    ]
}

fn assert_views_match(conn: &Arc<Connection>, views: &[(&str, String)], after: &str) {
    for (name, select) in views {
        assert_eq!(
            rows_sorted(conn, &format!("SELECT * FROM {name}")),
            rows_sorted(conn, select),
            "{name} disagrees with a fresh run of its SELECT after {after}\n{select}"
        );
    }
}

fn run_case(predicate: &Predicate, steps: &[Step]) {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    for ddl in [
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
        "CREATE TABLE block_tags (block_id INTEGER, tag TEXT)",
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
        "INSERT INTO block VALUES (1,'{\"task_state\":\"TODO\"}'),(2,'{\"task_state\":\"DONE\"}'),(3,'{}')",
        "INSERT INTO block_tags VALUES (1,'agent'),(1,'agent'),(2,'urgent')",
        "INSERT INTO block_requires VALUES (1,2),(2,3),(3,1)",
    ] {
        limbo_exec_rows(&conn, ddl);
    }
    let views = views(predicate);
    for (name, select) in &views {
        limbo_exec_rows_fallible(
            &db,
            &conn,
            &format!("CREATE MATERIALIZED VIEW {name} AS {select}"),
        )
        .unwrap_or_else(|err| panic!("CREATE failed for {select}: {err}"));
    }
    assert_views_match(&conn, &views, "materializing");

    for step in steps {
        match step {
            Step::Single(write) => {
                limbo_exec_rows(&conn, &write.sql());
                assert_views_match(&conn, &views, &write.sql());
            }
            Step::Transaction { writes, commit } => {
                limbo_exec_rows(&conn, "BEGIN");
                for write in writes {
                    limbo_exec_rows(&conn, &write.sql());
                    assert_views_match(&conn, &views, &write.sql());
                }
                limbo_exec_rows(&conn, if *commit { "COMMIT" } else { "ROLLBACK" });
                assert_views_match(&conn, &views, if *commit { "COMMIT" } else { "ROLLBACK" });
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn exists_predicates_match_a_fresh_run(
        predicate in predicate(),
        steps in proptest::collection::vec(step(), 1..12),
    ) {
        run_case(&predicate, &steps);
    }
}
