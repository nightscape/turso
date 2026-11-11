//! A database the engine before the type-order change wrote, with indexes on a
//! materialized view's INTEGER[] and TEXT[] columns, stays writable and
//! consistent. The statements that wrote it are in the `.sql` sibling of the
//! fixture.

use crate::common::{try_limbo_exec_rows, TempDatabase};
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};
use rusqlite::types::Value;
use std::collections::BTreeMap;

/// Writes to `a` (the source table of `am`, whose `ints` and `words` columns
/// `am_ints` and `am_words` index): insert, change either array, or delete.
fn older_engine_array_index_ops() -> impl Strategy<Value = Vec<String>> {
    let ints = prop::collection::vec(-200i64..200, 0..4).prop_map(|v| {
        format!(
            "'{{{}}}'",
            v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        )
    });
    let word = prop_oneof![
        "[a-zA-Z]{1,3}".boxed(),
        prop::sample::select(vec![
            "nan", "NAn", "inf", "-inf", "Infinity", "007", "1.50", "1e2"
        ])
        .prop_map(str::to_string)
        .boxed(),
    ];
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

/// The `words` text per id of the rows the fixture's `.sql` sibling inserts.
fn fixture_words() -> BTreeMap<i64, String> {
    [
        (1, "{b,a}"),
        (2, "{a}"),
        (3, "{ab}"),
        (4, "{Z}"),
        (5, "{a,b,c}"),
        (6, "{}"),
        (7, "{zz}"),
        (8, "{ab,b}"),
    ]
    .into_iter()
    .map(|(id, words)| (id, words.to_string()))
    .collect()
}

/// The `words` per id after the statements of `older_engine_array_index_ops`.
fn model_words(ops: &[String]) -> BTreeMap<i64, String> {
    let mut words = fixture_words();
    for sql in ops {
        let quoted = |from: usize| {
            let start = from + sql[from..].find('\'').unwrap() + 1;
            let end = start + sql[start..].find('\'').unwrap();
            (sql[start..end].to_string(), end + 1)
        };
        let id = |marker: &str| -> i64 {
            let rest = &sql[sql.find(marker).unwrap() + marker.len()..];
            rest[..rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len())]
                .parse()
                .unwrap()
        };
        if sql.starts_with("INSERT OR REPLACE") {
            let (_, after_ints) = quoted(0);
            words.insert(id("VALUES ("), quoted(after_ints).0);
        } else if sql.starts_with("UPDATE a SET words") {
            if let Some(w) = words.get_mut(&id("WHERE id = ")) {
                *w = quoted(0).0;
            }
        } else if sql.starts_with("DELETE") {
            words.remove(&id("WHERE id = "));
        }
    }
    words
}

fn words_of(
    conn: &std::sync::Arc<turso_core::Connection>,
    db: &TempDatabase,
    table: &str,
) -> Result<BTreeMap<i64, String>, String> {
    let sql = format!("SELECT id, words FROM {table}");
    try_limbo_exec_rows(db, conn, &sql)
        .map_err(|e| format!("`{sql}`: {e}"))?
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::Integer(id), Value::Text(words)] => Ok((*id, words.clone())),
            other => Err(format!("`{sql}`: unexpected row {other:?}")),
        })
        .collect()
}

fn writes_keep_the_view_indexes_consistent(ops: &[String]) -> Result<(), String> {
    let (_dir, db) = older_engine_db();
    let conn = db.connect_limbo();
    for sql in ops {
        conn.execute(sql).map_err(|e| format!("`{sql}`: {e}"))?;
    }
    views_match_their_select(&conn, &db, ops)?;
    let want = model_words(ops);
    let got = words_of(&conn, &db, "a")?;
    if got != want {
        return Err(format!(
            "after {ops:?}: words of a {got:?}, expected {want:?}"
        ));
    }
    // The view reads its array columns back as their stored record, so it is
    // compared with the table by value.
    let same_words = rows(
        &conn,
        &db,
        "SELECT a.id FROM a JOIN am ON a.id = am.id WHERE a.words = am.words",
    )?;
    let ids = rows(&conn, &db, "SELECT id FROM a")?;
    if same_words != ids {
        return Err(format!(
            "after {ops:?}: am holds the words of a for ids {same_words:?}, a has {ids:?}"
        ));
    }
    Ok(())
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
    let fixed: Vec<Vec<String>> = vec![
        vec!["INSERT OR REPLACE INTO a VALUES (1, '{}', '{NAn}', 'x1')".to_string()],
        vec![
            "INSERT OR REPLACE INTO a VALUES (9, '{1}', '{007,1.50,1e2,-inf,Infinity,inf,nan}', 'x9')"
                .to_string(),
            "UPDATE a SET words = '{NAn,007}' WHERE id = 2".to_string(),
        ],
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
