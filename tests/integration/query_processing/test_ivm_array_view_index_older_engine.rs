//! A database the engine before the type-order change wrote, with indexes on a
//! materialized view's INTEGER[] and TEXT[] columns, stays writable and
//! consistent. The statements that wrote it are in the `.sql` sibling of the
//! fixture.

use crate::common::{try_limbo_exec_rows, TempDatabase};
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};

/// Writes to `a` (the source table of `am`, whose `ints` and `words` columns
/// `am_ints` and `am_words` index): insert, change either array, or delete.
fn older_engine_array_index_ops() -> impl Strategy<Value = Vec<String>> {
    let ints = prop::collection::vec(-200i64..200, 0..4).prop_map(|v| {
        format!(
            "'{{{}}}'",
            v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        )
    });
    // A user can write the word `nan` or `inf` (any case), but the engine refuses it as a
    // non-finite number; the `{NAn}` example in the test pins that refusal until the parse is fixed.
    let word = "[a-zA-Z]{1,3}".prop_filter("not a number", |w| w.parse::<f64>().is_err());
    let words = prop::collection::vec(word, 0..4).prop_map(|v| format!("'{{{}}}'", v.join(",")));
    let op = (0u8..4, 1i64..12, ints, words).prop_map(|(kind, id, ints, words)| match kind {
        0 => format!("INSERT OR REPLACE INTO a VALUES ({id}, {ints}, {words}, 'x{id}')"),
        1 => format!("UPDATE a SET ints = {ints} WHERE id = {id}"),
        2 => format!("UPDATE a SET words = {words} WHERE id = {id}"),
        _ => format!("DELETE FROM a WHERE id = {id}"),
    });
    prop::collection::vec(op, 0..8)
}

fn rows(
    conn: &std::sync::Arc<turso_core::Connection>,
    db: &TempDatabase,
    sql: &str,
) -> Result<Vec<String>, String> {
    let mut rows: Vec<String> = try_limbo_exec_rows(db, conn, sql)
        .map_err(|e| format!("`{sql}`: {e}"))?
        .into_iter()
        .map(|row| format!("{row:?}"))
        .collect();
    rows.sort();
    Ok(rows)
}

/// A copy of the fixture; the `TempDir` must outlive the database.
fn older_engine_db() -> (tempfile::TempDir, TempDatabase) {
    let fixture = format!(
        "{}/integration/query_processing/test_ivm_key_seek_fixture_array_view_index.db",
        env!("CARGO_MANIFEST_DIR")
    );
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("older-engine.db");
    std::fs::copy(&fixture, &path).unwrap();
    let db = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .with_opts(turso_core::DatabaseOpts::new().with_custom_types(true))
        .build();
    (dir, db)
}

fn writes_keep_the_view_indexes_consistent(ops: &[String]) -> Result<(), String> {
    let (_dir, db) = older_engine_db();
    let conn = db.connect_limbo();
    for sql in ops {
        conn.execute(sql).map_err(|e| format!("`{sql}`: {e}"))?;
    }
    views_match_their_select(&conn, &db, ops)
}

fn views_match_their_select(
    conn: &std::sync::Arc<turso_core::Connection>,
    db: &TempDatabase,
    ops: &[String],
) -> Result<(), String> {
    let integrity = rows(conn, db, "PRAGMA integrity_check")?;
    if integrity != [r#"[Text("ok")]"#] {
        return Err(format!("after {ops:?}: integrity_check {integrity:?}"));
    }
    let (view, select) = (
        rows(conn, db, "SELECT id, label FROM am")?,
        rows(conn, db, "SELECT id, label FROM a")?,
    );
    if view != select {
        return Err(format!(
            "after {ops:?}: am {view:?} != its SELECT {select:?}"
        ));
    }
    Ok(())
}

#[test]
fn an_older_engines_indexes_on_array_view_columns_stay_consistent_under_writes() {
    let (_dir, db) = older_engine_db();
    let conn = db.connect_limbo();
    let refused = vec!["INSERT OR REPLACE INTO a VALUES (1, '{}', '{NAn}', 'x1')".to_string()];
    let err = conn.execute(&refused[0]).unwrap_err().to_string();
    assert!(
        err.contains("a.words"),
        "the refusal must name the column: {err}"
    );
    views_match_their_select(&conn, &db, &refused).unwrap();

    let fixed: Vec<Vec<String>> = vec![
        vec![],
        vec!["DELETE FROM a WHERE id = 1".to_string()],
        vec![
            "UPDATE a SET ints = '{-7}' WHERE id = 6".to_string(),
            "UPDATE a SET words = '{A}' WHERE id = 3".to_string(),
        ],
    ];
    for ops in &fixed {
        writes_keep_the_view_indexes_consistent(ops).unwrap();
    }
    let mut runner = TestRunner::new(RunnerConfig {
        cases: std::env::var("PROPTEST_CASES").map_or(64, |n| n.parse().unwrap()),
        failure_persistence: None,
        ..RunnerConfig::default()
    });
    runner
        .run(&older_engine_array_index_ops(), |ops| {
            writes_keep_the_view_indexes_consistent(&ops).map_err(TestCaseError::fail)
        })
        .unwrap();
}
