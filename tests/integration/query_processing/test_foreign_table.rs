use crate::common::{self, ExecRows, TempDatabase};
use std::sync::{Arc, Mutex};
use turso_core::foreign::{ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint};
use turso_core::{Connection, Value};
use turso_ext::ConstraintOp;

#[derive(Debug)]
struct MemFdw {
    schema_sql: String,
    key_columns: Vec<KeyColumn>,
    rows: Vec<Vec<Value>>,
    seen_constraints: Arc<Mutex<Vec<(u32, Value)>>>,
}

impl MemFdw {
    fn people() -> Arc<Self> {
        Arc::new(Self {
            schema_sql: "CREATE TABLE people(id INTEGER, name TEXT)".to_string(),
            key_columns: vec![KeyColumn::new("name", 1, vec![ConstraintOp::Eq])],
            rows: vec![
                vec![Value::from_i64(1), Value::build_text("alice")],
                vec![Value::from_i64(2), Value::build_text("bob")],
                vec![Value::from_i64(3), Value::build_text("carol")],
            ],
            seen_constraints: Arc::default(),
        })
    }
}

impl ForeignDataWrapper for MemFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &self.key_columns
    }

    fn schema_sql(&self) -> String {
        self.schema_sql.clone()
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(MemCursor {
            rows: self.rows.clone(),
            index: 0,
            seen_constraints: self.seen_constraints.clone(),
        }))
    }
}

struct MemCursor {
    rows: Vec<Vec<Value>>,
    index: usize,
    seen_constraints: Arc<Mutex<Vec<(u32, Value)>>>,
}

impl ForeignCursor for MemCursor {
    fn filter(&mut self, constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        self.index = 0;
        for c in constraints {
            self.seen_constraints
                .lock()
                .unwrap()
                .push((c.column_index, c.value.clone()));
            self.rows
                .retain(|row| row[c.column_index as usize] == c.value);
        }
        Ok(!self.rows.is_empty())
    }

    fn next(&mut self) -> turso_core::Result<bool> {
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> turso_core::Result<Value> {
        Ok(self.rows[self.index][idx].clone())
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

#[turso_macros::test]
fn test_foreign_table_select(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("people", MemFdw::people())?;

    let rows: Vec<(i64, String)> = conn.exec_rows("SELECT id, name FROM people ORDER BY id");
    assert_eq!(
        rows,
        vec![
            (1, "alice".to_string()),
            (2, "bob".to_string()),
            (3, "carol".to_string())
        ]
    );
    Ok(())
}

#[turso_macros::test]
fn test_foreign_table_where_reaches_filter(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let fdw = MemFdw::people();
    let seen = fdw.seen_constraints.clone();
    conn.register_foreign_table("people", fdw)?;

    let rows: Vec<(i64,)> = conn.exec_rows("SELECT id FROM people WHERE name = 'bob'");
    assert_eq!(rows, vec![(2,)]);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(1, Value::build_text("bob"))],
        "the equality on a key column must reach the driver"
    );
    Ok(())
}

#[turso_macros::test]
fn test_foreign_table_join_with_local_table(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("people", MemFdw::people())?;
    conn.execute("CREATE TABLE scores(person_id INTEGER, score INTEGER)")?;
    conn.execute("INSERT INTO scores VALUES (1, 10), (3, 30)")?;

    let rows: Vec<(String, i64)> = conn.exec_rows(
        "SELECT p.name, s.score FROM people p JOIN scores s ON s.person_id = p.id ORDER BY s.score",
    );
    assert_eq!(
        rows,
        vec![("alice".to_string(), 10), ("carol".to_string(), 30)]
    );
    Ok(())
}

#[turso_macros::test]
fn test_foreign_table_visible_to_second_connection(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("people", MemFdw::people())?;
    let other = tmp_db.connect_limbo();

    let rows: Vec<(i64,)> = other.exec_rows("SELECT count(*) FROM people");
    assert_eq!(rows, vec![(3,)]);
    Ok(())
}

#[turso_macros::test]
fn test_foreign_table_survives_schema_change_on_other_connection(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("people", MemFdw::people())?;
    let other = tmp_db.connect_limbo();
    other.execute("CREATE TABLE unrelated(x)")?;

    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM people");
    assert_eq!(rows, vec![(3,)]);
    let rows: Vec<(i64,)> = other.exec_rows("SELECT count(*) FROM people");
    assert_eq!(rows, vec![(3,)]);
    Ok(())
}

#[derive(Debug)]
struct RequiredKeyFdw {
    key_columns: Vec<KeyColumn>,
}

impl ForeignDataWrapper for RequiredKeyFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &self.key_columns
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
        Ok(self.rows[self.index][idx].clone())
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

fn required_key_fdw() -> Arc<RequiredKeyFdw> {
    Arc::new(RequiredKeyFdw {
        key_columns: vec![KeyColumn::new("session_id", 0, vec![ConstraintOp::Eq]).required()],
    })
}

#[turso_macros::test]
fn test_required_key_query_with_qualifier(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", required_key_fdw())?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM msg_req WHERE session_id = 's1'");
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[turso_macros::test]
fn test_required_key_rejects_unqualified_scan(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", required_key_fdw())?;

    let result = common::run_query(&tmp_db, &conn, "SELECT uuid FROM msg_req");
    assert!(result.is_err());
    Ok(())
}

#[turso_macros::test]
fn test_required_key_satisfied_by_join_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.register_foreign_table("msg_req", required_key_fdw())?;
    conn.execute("CREATE TABLE sessions(id TEXT)")?;
    conn.execute("INSERT INTO sessions VALUES ('s1'), ('s2')")?;

    let rows: Vec<(String, String)> = conn.exec_rows(
        "SELECT s.id, m.uuid FROM sessions s JOIN msg_req m ON m.session_id = s.id ORDER BY s.id, m.uuid",
    );
    assert_eq!(rows.len(), 4);
    Ok(())
}

/// A cache-filling driver: the scan itself writes the rows it fetched into a
/// local table on the connection that runs the SELECT.
#[derive(Debug)]
struct WriteThroughFdw;

impl ForeignDataWrapper for WriteThroughFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }

    fn schema_sql(&self) -> String {
        "CREATE TABLE remote(id INTEGER, v INTEGER)".to_string()
    }

    fn open_cursor(&self, conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(WriteThroughCursor {
            conn,
            rows: Vec::new(),
            index: 0,
        }))
    }
}

struct WriteThroughCursor {
    conn: Arc<Connection>,
    rows: Vec<(i64, i64)>,
    index: usize,
}

impl ForeignCursor for WriteThroughCursor {
    fn filter(&mut self, _constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        self.rows = vec![(1, 100), (2, 200)];
        self.index = 0;
        for (id, v) in &self.rows {
            self.conn
                .execute(format!("INSERT OR REPLACE INTO cache VALUES ({id}, {v})"))?;
        }
        Ok(true)
    }

    fn next(&mut self) -> turso_core::Result<bool> {
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> turso_core::Result<Value> {
        let (id, v) = self.rows[self.index];
        Ok(Value::from_i64(if idx == 0 { id } else { v }))
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

#[turso_macros::test(views)]
fn test_write_through_insert_during_scan_autocommit(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE cache(id INTEGER PRIMARY KEY, v INTEGER)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW cache_sum AS SELECT count(*) AS n, sum(v) AS s FROM cache",
    )?;
    conn.register_foreign_table("remote", Arc::new(WriteThroughFdw))?;

    let scanned: Vec<(i64, i64)> = conn.exec_rows("SELECT id, v FROM remote ORDER BY id");
    assert_eq!(scanned, vec![(1, 100), (2, 200)]);

    let cached: Vec<(i64, i64)> = conn.exec_rows("SELECT id, v FROM cache ORDER BY id");
    assert_eq!(cached, vec![(1, 100), (2, 200)]);
    let view: Vec<(i64, i64)> = conn.exec_rows("SELECT n, s FROM cache_sum");
    assert_eq!(view, vec![(2, 300)]);

    let _: Vec<(i64, i64)> = conn.exec_rows("SELECT id, v FROM remote");
    let view: Vec<(i64, i64)> = conn.exec_rows("SELECT n, s FROM cache_sum");
    assert_eq!(
        view,
        vec![(2, 300)],
        "replacing the same rows keeps the view"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn test_write_through_insert_during_scan_in_transaction(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE cache(id INTEGER PRIMARY KEY, v INTEGER)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW cache_sum AS SELECT count(*) AS n, sum(v) AS s FROM cache",
    )?;
    conn.register_foreign_table("remote", Arc::new(WriteThroughFdw))?;

    conn.execute("BEGIN")?;
    let scanned: Vec<(i64, i64)> = conn.exec_rows("SELECT id, v FROM remote ORDER BY id");
    assert_eq!(scanned, vec![(1, 100), (2, 200)]);
    conn.execute("COMMIT")?;

    let view: Vec<(i64, i64)> = conn.exec_rows("SELECT n, s FROM cache_sum");
    assert_eq!(view, vec![(2, 300)]);
    let cached: Vec<(i64, i64)> = conn.exec_rows("SELECT id, v FROM cache ORDER BY id");
    assert_eq!(cached, vec![(1, 100), (2, 200)]);
    Ok(())
}
