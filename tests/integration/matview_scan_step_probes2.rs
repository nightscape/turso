//! Second round of adversarial probes for the matview cursor's step path,
//! aimed at the pager save/restore the IVM view writer cursor now triggers.
//!
//! Same contract as `matview_scan_step_probes`: a plain mirror table driven
//! through the identical interleaving is the oracle.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use std::sync::Arc;
use turso_core::vdbe::StepResult;
use turso_core::Connection;

fn ints(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

fn seed(conn: &Arc<Connection>, n: i64) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE src (id INTEGER PRIMARY KEY, grp INTEGER, body TEXT)",
    );
    limbo_exec_rows(
        conn,
        "CREATE TABLE mirror (id INTEGER PRIMARY KEY, grp INTEGER, body TEXT)",
    );
    limbo_exec_rows(conn, "BEGIN");
    for id in 1..=n {
        limbo_exec_rows(
            conn,
            &format!("INSERT INTO src VALUES ({id}, {}, 'body-{id}')", id % 7),
        );
        limbo_exec_rows(
            conn,
            &format!("INSERT INTO mirror VALUES ({id}, {}, 'body-{id}')", id % 7),
        );
    }
    limbo_exec_rows(conn, "COMMIT");
    limbo_exec_rows(
        conn,
        "CREATE MATERIALIZED VIEW mv AS SELECT id, grp, body FROM src",
    );
}

fn db() -> TempDatabase {
    TempDatabase::builder()
        .with_views(true)
        .with_db_name("probe2.db")
        .build()
}

fn next_row(stmt: &mut turso_core::Statement) -> Option<i64> {
    loop {
        match stmt.step().unwrap() {
            StepResult::IO => stmt._io().step().unwrap(),
            StepResult::Yield => continue,
            StepResult::Row => {
                let v = stmt.row().unwrap().get_values().next().unwrap().clone();
                return Some(match v {
                    turso_core::Value::Numeric(turso_core::Numeric::Integer(i)) => i,
                    other => panic!("unexpected {other:?}"),
                });
            }
            StepResult::Done => return None,
            other => panic!("unexpected step result {other:?}"),
        }
    }
}

/// Drain `sql`, running `mid` once after `pause_after` rows.
fn scan_with_interleaved(
    conn: &Arc<Connection>,
    sql: &str,
    pause_after: usize,
    mid: impl FnOnce(),
) -> Vec<i64> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out = Vec::new();
    let mut mid = Some(mid);
    while let Some(id) = next_row(&mut stmt) {
        out.push(id);
        if out.len() == pause_after {
            if let Some(f) = mid.take() {
                f();
            }
        }
    }
    out
}

/// Run the same scan + interleaved write over `mv` and over `mirror`.
fn differential(
    n: i64,
    select: &str,
    pause_after: usize,
    write: impl Fn(&Arc<Connection>, &str),
) -> (Vec<i64>, Vec<i64>) {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, n);
    let mv = scan_with_interleaved(&conn, &select.replace("{t}", "mv"), pause_after, || {
        write(&conn, "src")
    });

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, n);
    let table = scan_with_interleaved(
        &conn2,
        &select.replace("{t}", "mirror"),
        pause_after,
        || write(&conn2, "mirror"),
    );
    (mv, table)
}

// ------------------------------------------------- reverse scan + restore

/// A reverse matview scan paused on a row that a same-connection DELETE then
/// removes. The scans order by `rowid`, so both sides run `Last`/`Prev` on a
/// live b-tree cursor: `ORDER BY id` on the matview goes through a sorter
/// (its `id` is no rowid alias), which reads every row before the first one
/// is returned and so never sees the DELETE.
const REVERSE_SCAN: &str = "SELECT rowid FROM {t} ORDER BY rowid DESC";

#[test]
fn reverse_scan_delete_at_cursor_mid_scan_matches_table() {
    let (mv, table) = differential(10, REVERSE_SCAN, 3, |c, tbl| {
        limbo_exec_rows(c, &format!("DELETE FROM {tbl} WHERE id = 8"));
    });
    assert_eq!(mv, table, "reverse scan, cursor row deleted");
}

#[test]
fn reverse_scan_delete_range_at_cursor_mid_scan_matches_table() {
    let (mv, table) = differential(10, REVERSE_SCAN, 3, |c, tbl| {
        limbo_exec_rows(c, &format!("DELETE FROM {tbl} WHERE id BETWEEN 5 AND 8"));
    });
    assert_eq!(mv, table, "reverse scan, cursor row and below deleted");
}

#[test]
fn reverse_scan_delete_at_cursor_large_matches_table() {
    let (mv, table) = differential(3000, REVERSE_SCAN, 100, |c, tbl| {
        limbo_exec_rows(
            c,
            &format!("DELETE FROM {tbl} WHERE id BETWEEN 1500 AND 2901"),
        );
    });
    assert_eq!(mv, table, "reverse scan at scale");
}

// ------------------------------------------------- splits and merges

/// A large same-connection INSERT below and around the paused cursor, so the
/// cursor's leaf page splits under it.
#[test]
fn same_conn_insert_splits_cursor_page_mid_scan_matches_table() {
    let (mv, table) = differential(40, "SELECT id FROM {t}", 5, |c, tbl| {
        // 2000 ids wedged between the existing 5 and 6 so the cursor's leaf
        // page must split many times.
        limbo_exec_rows(c, "BEGIN");
        for k in 0..2000 {
            let id = 1000 + k;
            limbo_exec_rows(
                c,
                &format!("INSERT INTO {tbl} VALUES ({id}, 1, 'padpadpadpadpadpadpadpadpad-{id}')"),
            );
        }
        limbo_exec_rows(c, "COMMIT");
    });
    assert_eq!(mv, table, "insert-split under a paused cursor");
}

/// A same-connection DELETE of almost every row, so the paused cursor's own
/// leaf page is merged away.
#[test]
fn same_conn_delete_merges_cursor_page_mid_scan_matches_table() {
    let (mv, table) = differential(3000, "SELECT id FROM {t}", 20, |c, tbl| {
        limbo_exec_rows(c, &format!("DELETE FROM {tbl} WHERE id > 15"));
    });
    assert_eq!(mv, table, "page merge under a paused cursor");
}

/// Every row removed while the scan is paused: the view b-tree goes empty.
#[test]
fn same_conn_delete_all_mid_scan_matches_table() {
    let (mv, table) = differential(500, "SELECT id FROM {t}", 10, |c, tbl| {
        limbo_exec_rows(c, &format!("DELETE FROM {tbl}"));
    });
    assert_eq!(mv, table, "whole view deleted under a paused cursor");
}

/// Repeated writes between single rows of the same scan.
#[test]
fn write_between_every_row_matches_table() {
    let run = |table: &str| -> Vec<i64> {
        let t = db();
        let conn = t.connect_limbo();
        seed(&conn, 60);
        let src = if table == "mv" { "src" } else { "mirror" };
        let mut stmt = conn.prepare(&format!("SELECT id FROM {table}")).unwrap();
        let mut out = Vec::new();
        while let Some(id) = next_row(&mut stmt) {
            out.push(id);
            // Delete the row just returned, then the one after it.
            limbo_exec_rows(&conn, &format!("DELETE FROM {src} WHERE id = {id}"));
            limbo_exec_rows(&conn, &format!("DELETE FROM {src} WHERE id = {}", id + 1));
        }
        out
    };
    assert_eq!(run("mv"), run("mirror"), "write between every row");
}

/// The cursor's row is replaced by a row with the same rowid (an UPDATE), so
/// the commit stages a retraction followed by an insertion.
#[test]
fn same_conn_update_cursor_row_mid_scan_matches_table() {
    let (mv, table) = differential(10, "SELECT id FROM {t}", 3, |c, tbl| {
        limbo_exec_rows(
            c,
            &format!("UPDATE {tbl} SET body = 'changed-and-much-much-longer' WHERE id = 3"),
        );
    });
    assert_eq!(mv, table, "cursor row updated in place");
}

// ------------------------------------------------- two paused cursors

/// Two scans of the same view are both paused, then a same-connection DELETE
/// removes the row under each of them; both resume.
#[test]
fn two_paused_cursors_delete_at_both_matches_table() {
    let run = |table: &str| -> (Vec<i64>, Vec<i64>) {
        let t = db();
        let conn = t.connect_limbo();
        seed(&conn, 30);
        let src = if table == "mv" { "src" } else { "mirror" };
        let mut a = conn.prepare(&format!("SELECT id FROM {table}")).unwrap();
        let mut b = conn
            .prepare(&format!("SELECT id FROM {table} WHERE id >= 20"))
            .unwrap();
        let mut out_a = Vec::new();
        let mut out_b = Vec::new();
        for _ in 0..3 {
            out_a.push(next_row(&mut a).unwrap());
        }
        for _ in 0..2 {
            out_b.push(next_row(&mut b).unwrap());
        }
        // Row 3 is under cursor a, row 21 under cursor b.
        limbo_exec_rows(&conn, &format!("DELETE FROM {src} WHERE id IN (3, 21)"));
        while let Some(id) = next_row(&mut a) {
            out_a.push(id);
        }
        while let Some(id) = next_row(&mut b) {
            out_b.push(id);
        }
        (out_a, out_b)
    };
    let (mv_a, mv_b) = run("mv");
    let (tb_a, tb_b) = run("mirror");
    assert_eq!(mv_a, tb_a, "first paused cursor");
    assert_eq!(mv_b, tb_b, "second paused cursor");
}

/// Two paused cursors, and the write happens while BOTH sit on the same row.
#[test]
fn two_paused_cursors_same_row_delete_matches_table() {
    let run = |table: &str| -> (Vec<i64>, Vec<i64>) {
        let t = db();
        let conn = t.connect_limbo();
        seed(&conn, 30);
        let src = if table == "mv" { "src" } else { "mirror" };
        let mut a = conn.prepare(&format!("SELECT id FROM {table}")).unwrap();
        let mut b = conn.prepare(&format!("SELECT id FROM {table}")).unwrap();
        let mut out_a = Vec::new();
        let mut out_b = Vec::new();
        for _ in 0..4 {
            out_a.push(next_row(&mut a).unwrap());
            out_b.push(next_row(&mut b).unwrap());
        }
        limbo_exec_rows(
            &conn,
            &format!("DELETE FROM {src} WHERE id BETWEEN 4 AND 9"),
        );
        while let Some(id) = next_row(&mut a) {
            out_a.push(id);
        }
        while let Some(id) = next_row(&mut b) {
            out_b.push(id);
        }
        (out_a, out_b)
    };
    let (mv_a, mv_b) = run("mv");
    let (tb_a, tb_b) = run("mirror");
    assert_eq!(mv_a, tb_a, "cursor a");
    assert_eq!(mv_b, tb_b, "cursor b");
}

/// A chained matview: the commit cascade writes mv and then mv2, while a scan
/// of mv is paused.
#[test]
fn chained_matview_write_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 400);
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW mv2 AS SELECT id, grp FROM mv WHERE grp = 3",
    );
    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 10, || {
        limbo_exec_rows(&conn, "DELETE FROM src WHERE id BETWEEN 10 AND 200");
    });

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 400);
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 10, || {
        limbo_exec_rows(&conn2, "DELETE FROM mirror WHERE id BETWEEN 10 AND 200");
    });
    assert_eq!(mv, table, "paused mv scan during a chained-view cascade");

    assert_eq!(
        ints(limbo_exec_rows(&conn, "SELECT id FROM mv2")),
        ints(limbo_exec_rows(
            &conn2,
            "SELECT id FROM mirror WHERE grp = 3"
        )),
        "mv2 contents after the cascade"
    );
}
