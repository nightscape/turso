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
        })
        .unwrap();

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

fn record_changes(
    conn: &turso_core::Connection,
    relation: &'static str,
) -> Arc<Mutex<Vec<String>>> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name != relation {
            return;
        }
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
    seen
}

fn kinds(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|change| change.split(' ').next().unwrap().to_string())
        .collect()
}

#[turso_macros::test(views)]
fn all_deletes_of_a_multi_row_view_update_come_before_its_inserts(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("UPDATE t SET v = v || 'x'")?;

    assert_eq!(kinds(&seen), ["delete", "delete", "insert", "insert"]);
    Ok(())
}

#[turso_macros::test(views)]
fn two_updates_of_one_row_in_a_transaction_are_one_delete_and_one_insert(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 'a')")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("BEGIN")?;
    conn.execute("UPDATE t SET v = 'b'")?;
    conn.execute("UPDATE t SET v = 'c'")?;
    conn.execute("COMMIT")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "got {seen:?}");
    assert!(
        seen[0].starts_with("delete") && seen[0].contains("\"a\""),
        "got {seen:?}"
    );
    assert!(
        seen[1].starts_with("insert") && seen[1].contains("\"c\""),
        "got {seen:?}"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn a_row_inserted_and_deleted_in_one_transaction_reports_nothing(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES (1, 'a')")?;
    conn.execute("DELETE FROM t")?;
    conn.execute("COMMIT")?;

    assert!(seen.lock().unwrap().is_empty());
    Ok(())
}

#[turso_macros::test(views)]
fn a_rolled_back_transaction_reports_no_view_changes_later(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    conn.execute("INSERT INTO t VALUES (3, 'a')")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("BEGIN")?;
    conn.execute("DELETE FROM t WHERE id = 3")?;
    conn.execute("INSERT INTO t VALUES (4, 'b')")?;
    conn.execute("ROLLBACK")?;
    conn.execute("SELECT * FROM mv")?;
    assert!(
        seen.lock().unwrap().is_empty(),
        "got {:?}",
        seen.lock().unwrap()
    );

    conn.execute("INSERT INTO t VALUES (5, 'c')")?;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert!(
        seen[0].contains("'c'") || seen[0].contains('c'),
        "got {seen:?}"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn rollback_to_a_savepoint_removes_its_view_changes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES (1, 'kept')")?;
    conn.execute("SAVEPOINT s")?;
    conn.execute("INSERT INTO t VALUES (2, 'undone')")?;
    conn.execute("ROLLBACK TO s")?;
    conn.execute("COMMIT")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert!(seen[0].contains("kept"), "got {seen:?}");
    Ok(())
}

#[turso_macros::test(views)]
fn a_duplicate_row_that_only_changes_a_view_weight_reports_nothing(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT DISTINCT v FROM t")?;
    conn.execute("INSERT INTO t VALUES (1, 'x')")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("INSERT INTO t VALUES (2, 'x')")?;

    assert!(
        seen.lock().unwrap().is_empty(),
        "got {:?}",
        seen.lock().unwrap()
    );
    Ok(())
}

#[turso_macros::test(views)]
fn a_view_without_the_changed_column_reports_nothing(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, w TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 'a', 'b')")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    let seen = record_changes(&conn, "mv");

    conn.execute("UPDATE t SET w = 'c'")?;

    assert!(
        seen.lock().unwrap().is_empty(),
        "got {:?}",
        seen.lock().unwrap()
    );
    Ok(())
}

const CDC_TABLE: &str = "CREATE TABLE turso_cdc (change_id INTEGER PRIMARY KEY AUTOINCREMENT, change_time INTEGER, change_type INTEGER, table_name TEXT, id, before BLOB, after BLOB, updates BLOB)";

fn enable_cdc(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute(CDC_TABLE)?;
    conn.execute("PRAGMA capture_data_changes_conn('full')")?;
    Ok(())
}

fn record_all(conn: &turso_core::Connection) -> Arc<Mutex<Vec<String>>> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    conn.set_change_callback(move |event| {
        for change in &event.changes {
            let kind = match change.change {
                DatabaseChangeType::Delete { .. } => "delete",
                DatabaseChangeType::Update { .. } => "update",
                DatabaseChangeType::Insert { .. } => "insert",
            };
            sink.lock().unwrap().push(format!(
                "{} {kind} {:?}",
                event.relation_name,
                change.parse_record().unwrap()
            ));
        }
    })
    .unwrap();
    seen
}

#[turso_macros::test(views)]
fn a_table_write_is_reported_while_cdc_is_on_and_internal_tables_are_not(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT v, count(*) AS n FROM t GROUP BY v")?;
    enable_cdc(&conn)?;
    let relations: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = relations.clone();
    conn.set_change_callback(move |event| {
        sink.lock().unwrap().push(event.relation_name.clone());
    })
    .unwrap();

    conn.execute("INSERT INTO t VALUES (1, 'a')")?;

    let mut relations = relations.lock().unwrap().clone();
    relations.sort();
    assert_eq!(relations, ["mv", "t"]);
    Ok(())
}

#[turso_macros::test(views)]
fn a_table_write_is_not_reported_while_cdc_is_off(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    let seen = record_all(&conn);

    conn.execute("INSERT INTO t VALUES (1, 'a')")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert!(seen[0].starts_with("mv "), "got {seen:?}");
    Ok(())
}

#[turso_macros::test(views)]
fn table_changes_are_net_per_row_and_an_update_is_one_update(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    enable_cdc(&conn)?;
    let seen = record_all(&conn);

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES (1, 'a')")?;
    conn.execute("UPDATE t SET v = 'b'")?;
    conn.execute("COMMIT")?;
    conn.execute("UPDATE t SET v = 'c'")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "got {seen:?}");
    assert!(
        seen[0].starts_with("t insert") && seen[0].contains("\"b\""),
        "got {seen:?}"
    );
    assert!(
        seen[1].starts_with("t update") && seen[1].contains("\"c\""),
        "got {seen:?}"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn a_statement_that_fails_inside_a_transaction_reports_none_of_its_rows(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    enable_cdc(&conn)?;
    let seen = record_all(&conn);

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES (1, 'kept')")?;
    assert!(conn
        .execute("INSERT INTO t VALUES (2, 'undone'), (1, 'duplicate')")
        .is_err());
    conn.execute("COMMIT")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert!(seen[0].contains("kept"), "got {seen:?}");
    Ok(())
}

#[turso_macros::test(views)]
fn a_rolled_back_savepoint_removes_its_table_changes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    enable_cdc(&conn)?;
    let seen = record_all(&conn);

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES (1, 'kept')")?;
    conn.execute("SAVEPOINT s")?;
    conn.execute("INSERT INTO t VALUES (2, 'undone')")?;
    conn.execute("ROLLBACK TO s")?;
    conn.execute("COMMIT")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    Ok(())
}

#[turso_macros::test(views)]
fn a_rolled_back_transaction_reports_no_table_changes_later(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    enable_cdc(&conn)?;
    conn.execute("INSERT INTO t VALUES (3, 'a')")?;
    let seen = record_all(&conn);

    conn.execute("BEGIN")?;
    conn.execute("DELETE FROM t WHERE id = 3")?;
    conn.execute("INSERT INTO t VALUES (4, 'b')")?;
    conn.execute("ROLLBACK")?;
    conn.execute("SELECT * FROM t")?;
    assert!(
        seen.lock().unwrap().is_empty(),
        "got {:?}",
        seen.lock().unwrap()
    );

    conn.execute("INSERT INTO t VALUES (5, 'c')")?;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert!(seen[0].starts_with("t insert"), "got {seen:?}");
    Ok(())
}

#[turso_macros::test(views)]
fn a_temp_table_write_is_reported_with_the_temp_prefix(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    enable_cdc(&conn)?;
    conn.execute("CREATE TEMP TABLE tt (a)")?;
    let seen = record_all(&conn);

    conn.execute("INSERT INTO tt VALUES (7)")?;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    assert!(seen[0].starts_with("temp.tt insert"), "got {seen:?}");
    Ok(())
}
