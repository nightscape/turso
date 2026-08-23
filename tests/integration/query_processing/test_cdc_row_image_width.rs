//! Measurement of the row-image width delivered to `set_change_callback`.
//!
//! Each test prints one `CDCM` line per delivered event and asserts nothing
//! about widths, so every number reaches the log.

use std::sync::{Arc, Mutex};

use turso_core::types::RelationChangeEvent;

use crate::common::TempDatabase;

struct Recorded {
    relation: String,
    kind: &'static str,
    columns: Vec<String>,
    parsed: Option<Vec<String>>,
}

#[derive(Clone)]
struct Recorder(Arc<Mutex<Vec<Recorded>>>);

impl Recorder {
    fn install(conn: &Arc<turso_core::Connection>) -> Self {
        let recorder = Recorder(Arc::new(Mutex::new(Vec::new())));
        let sink = recorder.clone();
        conn.set_change_callback(move |event: &RelationChangeEvent| {
            let mut log = sink.0.lock().unwrap();
            for change in &event.changes {
                log.push(Recorded {
                    relation: event.relation_name.clone(),
                    kind: match change.change {
                        turso_core::DatabaseChangeType::Insert { .. } => "Insert",
                        turso_core::DatabaseChangeType::Update { .. } => "Update",
                        turso_core::DatabaseChangeType::Delete { .. } => "Delete",
                    },
                    columns: event.columns.clone(),
                    parsed: change
                        .parse_record()
                        .map(|values| values.iter().map(|v| format!("{v:?}")).collect()),
                });
            }
        });
        recorder
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn dump(&self, scenario: &str) {
        let log = self.0.lock().unwrap();
        if log.is_empty() {
            eprintln!("CDCM {scenario} | NO EVENTS");
        }
        for rec in log.iter() {
            let parsed_len = match &rec.parsed {
                Some(values) => values.len().to_string(),
                None => "None".to_string(),
            };
            let values = match &rec.parsed {
                Some(values) => format!("[{}]", values.join(", ")),
                None => "None".to_string(),
            };
            eprintln!(
                "CDCM {scenario} | rel={} | kind={} | columns.len={} | parsed.len={} | columns=[{}] | values={}",
                rec.relation,
                rec.kind,
                rec.columns.len(),
                parsed_len,
                rec.columns.join(", "),
                values,
            );
        }
    }
}

const CREATE_T: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT, c INTEGER)";

/// Only CDC v1 emits `NotifyCdcChange`, and the pragma always requests
/// `CDC_VERSION_CURRENT`. Pinning the version row first is the one route to the
/// table producer.
fn enable_cdc_v1(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE turso_cdc (change_id INTEGER PRIMARY KEY AUTOINCREMENT, change_time INTEGER, change_type INTEGER, table_name TEXT, id, before BLOB, after BLOB, updates BLOB)",
    )?;
    conn.execute(
        "CREATE TABLE turso_cdc_version (table_name TEXT PRIMARY KEY, version TEXT NOT NULL)",
    )?;
    conn.execute("INSERT INTO turso_cdc_version (table_name, version) VALUES ('turso_cdc', 'v1')")?;
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_v1_table(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute(CREATE_T)?;
    enable_cdc_v1(&conn)?;

    let recorder = Recorder::install(&conn);

    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;
    recorder.dump("S1v1");
    recorder.clear();

    conn.execute("UPDATE t SET b = 'z' WHERE id = 1")?;
    recorder.dump("S2v1");
    recorder.clear();

    conn.execute("UPDATE t SET a = 'p', c = 2 WHERE id = 1")?;
    recorder.dump("S3v1");
    recorder.clear();

    conn.execute("UPDATE t SET b = b WHERE id = 1")?;
    recorder.dump("S8v1");
    recorder.clear();

    conn.execute("DELETE FROM t WHERE id = 1")?;
    recorder.dump("S4v1");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_v1_with_matviews(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute(CREATE_T)?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, a, b, c FROM t")?;
    conn.execute("CREATE MATERIALIZED VIEW w AS SELECT id, b FROM t")?;
    enable_cdc_v1(&conn)?;

    let recorder = Recorder::install(&conn);

    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;
    recorder.dump("S5v1-insert");
    recorder.clear();

    conn.execute("UPDATE t SET b = 'z' WHERE id = 1")?;
    recorder.dump("S5v1-update-1col");
    recorder.clear();

    conn.execute("UPDATE t SET a = 'q' WHERE id = 1")?;
    recorder.dump("S7v1-update-unprojected");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_table_insert(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;

    let recorder = Recorder::install(&conn);
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;
    recorder.dump("S1");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_table_update_one_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;

    let recorder = Recorder::install(&conn);
    conn.execute("UPDATE t SET b = 'z' WHERE id = 1")?;
    recorder.dump("S2");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_table_update_two_columns(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;

    let recorder = Recorder::install(&conn);
    conn.execute("UPDATE t SET a = 'p', c = 2 WHERE id = 1")?;
    recorder.dump("S3");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_table_delete(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;

    let recorder = Recorder::install(&conn);
    conn.execute("DELETE FROM t WHERE id = 1")?;
    recorder.dump("S4");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_full_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, a, b, c FROM t")?;

    let recorder = Recorder::install(&conn);

    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;
    recorder.dump("S5-insert");
    recorder.clear();

    conn.execute("UPDATE t SET b = 'z' WHERE id = 1")?;
    recorder.dump("S5-update-1col");
    recorder.clear();

    conn.execute("UPDATE t SET a = 'p', c = 2 WHERE id = 1")?;
    recorder.dump("S5-update-2col");
    recorder.clear();

    conn.execute("DELETE FROM t WHERE id = 1")?;
    recorder.dump("S5-delete");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_projecting_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;
    conn.execute("CREATE MATERIALIZED VIEW w AS SELECT id, b FROM t")?;
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;

    let recorder = Recorder::install(&conn);

    conn.execute("UPDATE t SET b = 'z' WHERE id = 1")?;
    recorder.dump("S6");
    recorder.clear();

    conn.execute("UPDATE t SET a = 'q' WHERE id = 1")?;
    recorder.dump("S7");

    Ok(())
}

#[turso_macros::test(views)]
fn test_cdc_row_image_width_self_assigning_update(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA unstable_capture_data_changes_conn('full')")?;
    conn.execute(CREATE_T)?;
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y', 1)")?;

    let recorder = Recorder::install(&conn);
    conn.execute("UPDATE t SET b = b WHERE id = 1")?;
    recorder.dump("S8");

    Ok(())
}
