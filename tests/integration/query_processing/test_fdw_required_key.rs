//! Foreign tables whose driver *requires* a qualifier.
//!
//! An API-backed source often cannot enumerate its rows at all — a message API
//! needs a session id before it will return anything. Such a driver declares
//! `KeyColumn::required`, and `best_index` rejects any plan that would scan it
//! unqualified.
//!
//! This is why a view's mirror is scoped to that view's predicate rather than
//! shadowing the whole foreign table: for these drivers a table-wide mirror is
//! not merely wasteful, it is impossible.

use crate::common::{self, ExecRows, TempDatabase};
use std::sync::Arc;
use turso_core::foreign::{
    ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint,
};
use turso_core::{Connection, Value};
use turso_ext::ConstraintOp;

/// `msg(session_id, uuid, body)` where `session_id` must be qualified with `=`.
#[derive(Debug)]
struct RequiredKeyFdw {
    key_columns: Vec<KeyColumn>,
}

impl RequiredKeyFdw {
    fn new() -> Self {
        Self {
            key_columns: vec![
                KeyColumn::new("session_id", 0, vec![ConstraintOp::Eq]).required(),
            ],
        }
    }
}

impl ForeignDataWrapper for RequiredKeyFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &self.key_columns
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        Some(&[1])
    }

    fn schema_sql(&self) -> String {
        "CREATE TABLE msg_req(session_id TEXT, uuid TEXT, body TEXT)".to_string()
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(RequiredKeyCursor {
            rows: Vec::new(),
            index: 0,
        }))
    }
}

/// Returns two rows for whichever session was asked for, and nothing otherwise.
struct RequiredKeyCursor {
    rows: Vec<Vec<Value>>,
    index: usize,
}

impl ForeignCursor for RequiredKeyCursor {
    fn filter(&mut self, constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        let session = constraints
            .iter()
            .find(|c| c.column_index == 0 && c.op == ConstraintOp::Eq)
            .map(|c| c.value.clone());
        self.rows = match session {
            Some(session) => vec![
                vec![
                    session.clone(),
                    Value::build_text("u1"),
                    Value::build_text("hello"),
                ],
                vec![session, Value::build_text("u2"), Value::build_text("there")],
            ],
            None => Vec::new(),
        };
        self.index = 0;
        Ok(!self.rows.is_empty())
    }

    fn next(&mut self) -> turso_core::Result<bool> {
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> turso_core::Result<Value> {
        Ok(self.rows[self.index]
            .get(idx)
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

/// Baseline: a qualified query satisfies the required key and returns rows.
#[turso_macros::test(views)]
fn test_required_key_query_with_qualifier(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", Arc::new(RequiredKeyFdw::new()))?;

    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, body FROM msg_req WHERE session_id = 's1'");
    assert_eq!(rows.len(), 2, "qualified scan should return the session's rows");
    Ok(())
}

/// The point of the spike: an UNqualified scan must be refused, not silently
/// turned into a full scan. This is what makes a table-wide mirror impossible.
#[turso_macros::test(views)]
fn test_required_key_rejects_unqualified_scan(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", Arc::new(RequiredKeyFdw::new()))?;

    let result = common::run_query(&tmp_db, &conn, "SELECT uuid FROM msg_req");
    assert!(
        result.is_err(),
        "an unqualified scan of a required-key foreign table must be refused"
    );
    Ok(())
}

/// A view carrying the qualifier is fine, and gets a mirror.
#[turso_macros::test(views)]
fn test_required_key_matview_with_qualifier(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", Arc::new(RequiredKeyFdw::new()))?;

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_req AS \
         SELECT uuid, body FROM msg_req WHERE session_id = 's1'",
    )?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_req");
    assert_eq!(rows.len(), 2);
    Ok(())
}

/// A view WITHOUT the qualifier must fail loudly at CREATE rather than
/// producing an empty or partial view.
#[turso_macros::test(views)]
fn test_required_key_matview_without_qualifier_is_refused(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", Arc::new(RequiredKeyFdw::new()))?;

    let result = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_req_bad AS SELECT uuid, body FROM msg_req",
    );
    assert!(
        result.is_err(),
        "a view that cannot qualify a required key must be refused at CREATE"
    );
    Ok(())
}
