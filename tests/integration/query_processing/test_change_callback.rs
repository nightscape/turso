use std::sync::{Arc, Mutex};

use turso_core::types::DatabaseChangeType;

use crate::common::TempDatabase;

#[turso_macros::test(views)]
fn a_commit_on_another_connection_reaches_the_callback(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let writer = tmp_db.connect_limbo();
    let listener = tmp_db.connect_limbo();
    writer.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    writer.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;

    let seen: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    listener
        .set_change_callback(move |event| {
            sink.lock()
                .unwrap()
                .push((event.relation_name.clone(), event.changes.len()));
        }).unwrap();

    writer.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")?;

    assert_eq!(*seen.lock().unwrap(), vec![("mv".to_string(), 2)]);
    Ok(())
}

#[turso_macros::test(views)]
fn a_view_update_is_a_delete_of_the_old_row_then_an_insert_of_the_new_row(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 'old')")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    conn.set_change_callback(move |event| {
        for change in &event.changes {
            let kind = match change.change {
                DatabaseChangeType::Delete { .. } => "delete",
                DatabaseChangeType::Update { .. } => "update",
                DatabaseChangeType::Insert { .. } => "insert",
            };
            sink.lock()
                .unwrap()
                .push(format!("{kind} {:?}", change.parse_record().unwrap()));
        }
    })
    .unwrap();

    conn.execute("UPDATE t SET v = 'new' WHERE id = 1")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "got {seen:?}");
    assert!(
        seen[0].starts_with("delete") && seen[0].contains("old"),
        "got {seen:?}"
    );
    assert!(
        seen[1].starts_with("insert") && seen[1].contains("new"),
        "got {seen:?}"
    );
    Ok(())
}
