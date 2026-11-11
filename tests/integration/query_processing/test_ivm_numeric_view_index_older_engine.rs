//! A database the engine before the type-order change wrote, with an index on
//! a materialized view's numeric(10,2) column, stays writable and consistent.

use crate::common::{try_limbo_exec_rows, TempDatabase};
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};

/// Writes to `d` (the source table of `dm`, which `dm_price` indexes): insert,
/// reprice or delete a row, with prices of either sign.
fn older_engine_numeric_index_ops() -> impl Strategy<Value = Vec<String>> {
    let op = (0u8..3, 1i64..12, -9999i64..10000).prop_map(|(kind, id, cents)| {
        let price = format!(
            "{}{}.{:02}",
            if cents < 0 { "-" } else { "" },
            cents.abs() / 100,
            cents.abs() % 100
        );
        match kind {
            0 => format!("INSERT OR REPLACE INTO d VALUES ({id}, {price}, 'x{id}')"),
            1 => format!("UPDATE d SET price = {price} WHERE id = {id}"),
            _ => format!("DELETE FROM d WHERE id = {id}"),
        }
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

fn writes_keep_the_view_index_consistent(ops: &[String]) -> Result<(), String> {
    let fixture = format!(
        "{}/integration/query_processing/test_ivm_key_seek_fixture_numeric_view_index.db",
        env!("CARGO_MANIFEST_DIR")
    );
    let dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let path = dir.path().join("older-engine.db");
    std::fs::copy(&fixture, &path).map_err(|e| format!("copying {fixture}: {e}"))?;
    let db = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .with_opts(turso_core::DatabaseOpts::new().with_custom_types(true))
        .build();
    let conn = db.connect_limbo();
    for sql in ops {
        conn.execute(sql).map_err(|e| format!("`{sql}`: {e}"))?;
    }
    let integrity = rows(&conn, &db, "PRAGMA integrity_check")?;
    if integrity != [r#"[Text("ok")]"#] {
        return Err(format!("after {ops:?}: integrity_check {integrity:?}"));
    }
    let (view, select) = (
        rows(&conn, &db, "SELECT id, label FROM dm")?,
        rows(&conn, &db, "SELECT id, label FROM d")?,
    );
    if view != select {
        return Err(format!(
            "after {ops:?}: dm {view:?} != its SELECT {select:?}"
        ));
    }
    Ok(())
}

#[test]
fn an_older_engines_index_on_a_numeric_view_column_stays_consistent_under_writes() {
    let fixed: Vec<Vec<String>> = vec![vec![], vec!["DELETE FROM d WHERE id = 2".to_string()]];
    for ops in &fixed {
        writes_keep_the_view_index_consistent(ops).unwrap();
    }
    let mut runner = TestRunner::new(RunnerConfig {
        cases: 64,
        failure_persistence: None,
        ..RunnerConfig::default()
    });
    runner
        .run(&older_engine_numeric_index_ops(), |ops| {
            writes_keep_the_view_index_consistent(&ops).map_err(TestCaseError::fail)
        })
        .unwrap();
}
