use std::collections::BTreeMap;
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

#[turso_macros::test(views)]
fn creating_a_view_over_existing_rows_reports_nothing(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")?;
    let seen = record_all(&conn);

    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t")?;
    assert!(
        seen.lock().unwrap().is_empty(),
        "got {:?}",
        seen.lock().unwrap()
    );

    conn.execute("INSERT INTO t VALUES (3, 'c')")?;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "got {seen:?}");
    Ok(())
}

#[test]
fn registering_a_change_callback_on_an_mvcc_database_is_refused() {
    let db = TempDatabase::builder().build();
    let conn = db.connect_limbo();
    conn.execute("PRAGMA journal_mode = 'mvcc'").unwrap();

    let error = conn.set_change_callback(|_| {}).unwrap_err();

    assert!(error.to_string().contains("MVCC"), "got {error}");
}

#[test]
fn switching_to_mvcc_while_a_change_callback_is_registered_is_refused() {
    let db = TempDatabase::builder().build();
    let conn = db.connect_limbo();
    conn.set_change_callback(|_| {}).unwrap();

    let error = conn.execute("PRAGMA journal_mode = 'mvcc'").unwrap_err();

    assert!(
        error.to_string().contains("change callbacks"),
        "got {error}"
    );
}

/// Applies every committed event of `view` to a copy of the view, keyed by
/// rowid, so a test can compare the copy with the view.
fn replay_view_changes(
    conn: &Arc<turso_core::Connection>,
    view: &'static str,
) -> Arc<Mutex<BTreeMap<i64, String>>> {
    let image = Arc::new(Mutex::new(view_rows_by_rowid(conn, view)));
    let sink = image.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name != view {
            return;
        }
        let mut image = sink.lock().unwrap();
        for change in &event.changes {
            let row = format!("{:?}", change.parse_record().unwrap());
            match change.change {
                DatabaseChangeType::Insert { .. } => assert!(
                    image.insert(change.id, row).is_none(),
                    "insert of rowid {} that {view} already holds",
                    change.id
                ),
                DatabaseChangeType::Delete { .. } => assert_eq!(
                    image.remove(&change.id),
                    Some(row),
                    "delete of a row {view} does not hold"
                ),
                DatabaseChangeType::Update { .. } => {
                    panic!("a view change is a delete or an insert")
                }
            }
        }
    })
    .unwrap();
    image
}

fn view_rows_by_rowid(conn: &Arc<turso_core::Connection>, view: &str) -> BTreeMap<i64, String> {
    let mut stmt = conn
        .prepare(format!("SELECT rowid, * FROM {view}"))
        .unwrap();
    let mut rows = BTreeMap::new();
    stmt.run_with_row_callback(|row| {
        let values: Vec<turso_core::Value> = row.get_values().cloned().collect();
        let turso_core::Value::Numeric(turso_core::Numeric::Integer(rowid)) = values[0] else {
            panic!("a view rowid is an integer, got {:?}", values[0]);
        };
        rows.insert(rowid, format!("{:?}", &values[1..]));
        Ok(())
    })
    .unwrap();
    rows
}

/// Runs each step, then checks that the events replayed so far rebuild the
/// view. A step is one or more statements; a transaction is one step.
fn assert_events_replay_the_view(
    conn: &Arc<turso_core::Connection>,
    view: &'static str,
    steps: &[&[&str]],
) -> anyhow::Result<()> {
    let image = replay_view_changes(conn, view);
    for step in steps {
        for sql in *step {
            conn.execute(sql)?;
        }
        assert_eq!(
            *image.lock().unwrap(),
            view_rows_by_rowid(conn, view),
            "replayed events of {view} disagree with the view after {step:?}"
        );
    }
    Ok(())
}

#[turso_macros::test(views)]
fn a_row_that_stays_in_a_top_k_window_reports_nothing(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, score INTEGER, note TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c'), (4, 40, 'd')")?;
    conn.execute("CREATE MATERIALIZED VIEW top AS SELECT id, score FROM t ORDER BY score LIMIT 2")?;
    let seen = record_changes(&conn, "top");

    conn.execute("UPDATE t SET note = 'x' WHERE id = 1")?;
    conn.execute("UPDATE t SET score = 15 WHERE id = 1")?;
    conn.execute("UPDATE t SET score = 10 WHERE id = 1")?;
    conn.execute("INSERT INTO t VALUES (5, 50, 'e')")?;
    conn.execute("UPDATE t SET score = 35 WHERE id = 4")?;
    conn.execute("DELETE FROM t WHERE id = 3")?;
    conn.execute("BEGIN")?;
    conn.execute("UPDATE t SET score = 5 WHERE id = 2")?;
    conn.execute("UPDATE t SET score = 20 WHERE id = 2")?;
    conn.execute("COMMIT")?;

    assert_eq!(
        kinds(&seen),
        ["delete", "insert", "delete", "insert"],
        "only the two score changes of row 1 are visible: {:?}",
        seen.lock().unwrap()
    );
    Ok(())
}

#[turso_macros::test(views)]
fn top_k_view_events_replay_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, score INTEGER)")?;
    conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 20)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW top AS SELECT id, score FROM t ORDER BY score DESC LIMIT 3 OFFSET 1",
    )?;
    assert_events_replay_the_view(
        &conn,
        "top",
        &[
            &["INSERT INTO t VALUES (6, 50)"],
            &["UPDATE t SET score = 25 WHERE id = 1"],
            &["DELETE FROM t WHERE id = 4"],
            &["UPDATE t SET score = score + 1"],
            &[
                "BEGIN",
                "INSERT INTO t VALUES (7, 100)",
                "DELETE FROM t WHERE id = 7",
                "UPDATE t SET score = 0 WHERE id = 6",
                "COMMIT",
            ],
            &["REFRESH MATERIALIZED VIEW top"],
            &[
                "BEGIN",
                "INSERT INTO t VALUES (8, 90)",
                "REFRESH MATERIALIZED VIEW top",
                "COMMIT",
            ],
            &["BEGIN", "REFRESH MATERIALIZED VIEW top", "ROLLBACK"],
        ],
    )
}

#[turso_macros::test(views)]
fn json_group_array_view_events_replay_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, v TEXT)")?;
    conn.execute("INSERT INTO t VALUES (1, 'a', 'x'), (2, 'a', 'y'), (3, 'b', 'z')")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv AS \
         SELECT g, json_group_array(v) AS vs, group_concat(v) AS c FROM t GROUP BY g",
    )?;
    assert_events_replay_the_view(
        &conn,
        "mv",
        &[
            &["INSERT INTO t VALUES (4, 'a', 'w')"],
            &["UPDATE t SET v = 'q' WHERE id = 2"],
            &["UPDATE t SET g = 'b' WHERE id = 1"],
            &["DELETE FROM t WHERE g = 'b'"],
            &[
                "BEGIN",
                "INSERT INTO t VALUES (5, 'c', 'k')",
                "UPDATE t SET v = 'm' WHERE id = 5",
                "COMMIT",
            ],
            &["BEGIN", "INSERT INTO t VALUES (6, 'c', 'n')", "ROLLBACK"],
            &["REFRESH MATERIALIZED VIEW mv"],
            &[
                "BEGIN",
                "INSERT INTO t VALUES (7, 'd', 'o')",
                "REFRESH MATERIALIZED VIEW mv",
                "COMMIT",
            ],
            &["BEGIN", "REFRESH MATERIALIZED VIEW mv", "ROLLBACK"],
        ],
    )
}

#[turso_macros::test(views)]
fn recursive_view_events_replay_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE node (id TEXT PRIMARY KEY, pid TEXT)")?;
    conn.execute("INSERT INTO node VALUES ('a', 'root'), ('b', 'a'), ('c', 'b'), ('d', 'a')")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW tree AS \
         WITH RECURSIVE r AS ( \
           SELECT id, 0 AS depth FROM node WHERE pid = 'root' \
           UNION ALL \
           SELECT n.id, r.depth + 1 FROM node n JOIN r ON n.pid = r.id \
         ) SELECT id, depth FROM r",
    )?;
    assert_events_replay_the_view(
        &conn,
        "tree",
        &[
            &["INSERT INTO node VALUES ('e', 'c')"],
            &["UPDATE node SET pid = 'd' WHERE id = 'b'"],
            &["DELETE FROM node WHERE id = 'd'"],
            &["UPDATE node SET pid = 'a' WHERE id = 'b'"],
            &[
                "BEGIN",
                "INSERT INTO node VALUES ('f', 'e')",
                "UPDATE node SET pid = 'root' WHERE id = 'c'",
                "COMMIT",
            ],
            &["REFRESH MATERIALIZED VIEW tree"],
            &[
                "BEGIN",
                "INSERT INTO node VALUES ('g', 'f')",
                "REFRESH MATERIALIZED VIEW tree",
                "COMMIT",
            ],
            &["BEGIN", "REFRESH MATERIALIZED VIEW tree", "ROLLBACK"],
        ],
    )
}
