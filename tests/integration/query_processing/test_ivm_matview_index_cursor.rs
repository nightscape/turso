//! Cursor identity: an indexed materialized view must read exactly like a
//! rowid table carrying the same rows and the same index.
//!
//! A `Cursor::MaterializedView` is a WRAPPER. It keeps its own position
//! (`current_row`) and merges an uncommitted in-transaction overlay that is
//! not in the btree at all (`core/incremental/cursor.rs`). It is only correct
//! when driven through its own methods. Several VDBE opcodes reach past it
//! with `Cursor::as_btree_mut()` (`core/types.rs:3309`), which hands out the
//! INNER btree cursor — positioning it without ever updating `current_row`.
//!
//! Before an index existed on a matview the planner never produced those
//! opcode sequences against one, so the divergence was unreachable.
//!
//! Every test here is a differential against `t`, a rowid table holding the
//! same rows with the same index.

use std::sync::Arc;

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

const ROWS: [(&str, &str); 6] = [
    ("b0", "TODO"),
    ("b1", "TODO"),
    ("b2", "TODO"),
    ("b3", "TODO"),
    ("b4", "TODO"),
    ("b8", "DONE"),
];

fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE TABLE t (rid INTEGER PRIMARY KEY, id TEXT, st TEXT)")?;
    conn.execute(
        "CREATE TABLE req (block_id TEXT, required_id TEXT, \
         PRIMARY KEY (block_id, required_id))",
    )?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    for (i, (id, st)) in ROWS.iter().enumerate() {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('{id}', '{st}')"
        ))?;
        conn.execute(&format!(
            "INSERT INTO t (rid, id, st) VALUES ({}, '{id}', '{st}')",
            i + 1
        ))?;
    }
    conn.execute("INSERT INTO req (block_id, required_id) VALUES ('b0', 'b8')")?;
    conn.execute("CREATE INDEX idx_v_id ON v(id)")?;
    conn.execute("CREATE INDEX idx_t_id ON t(id)")?;
    Ok(())
}

/// The matview arm and the rowid-table arm must return the same rows. Rowids
/// themselves differ between the two relations, so a template may mark a
/// projected rowid with `@RID@`; both arms then only have to agree on whether
/// it is NULL, which is what every defect here is about.
fn assert_arms_agree(conn: &Arc<turso_core::Connection>, what: &str, template: &str) {
    let view = limbo_exec_rows(conn, &template.replace("@B@", "v"));
    let table = limbo_exec_rows(conn, &template.replace("@B@", "t"));
    assert_eq!(
        view,
        table,
        "{what}: indexed matview disagrees with a rowid table holding the same \
         rows and the same index\n  query: {}",
        template.replace("@B@", "v")
    );
    assert!(!view.is_empty(), "{what}: the fixture must return rows");
}

/// DEFECT A — `MaterializedViewCursor::rowid()` returns `current_row`'s rowid
/// and consults no null flag, so the unmatched side of a LEFT JOIN reports the
/// last successfully seeked rowid instead of NULL.
#[turso_macros::test(views)]
fn left_join_miss_nulls_the_matview_rowid(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    // `bl.rowid IS NULL` normalises away the rowid VALUE, which legitimately
    // differs between the two relations; only NULL-ness must agree.
    assert_arms_agree(
        &conn,
        "DEFECT A: rowid on a LEFT JOIN miss",
        "SELECT b.id, bl.st, bl.rowid IS NULL FROM @B@ b \
         LEFT JOIN req br ON br.block_id = b.id \
         LEFT JOIN @B@ bl ON bl.id = br.required_id ORDER BY b.id",
    );

    let view = limbo_exec_rows(
        &conn,
        "SELECT b.id, bl.rowid FROM v b \
         LEFT JOIN req br ON br.block_id = b.id \
         LEFT JOIN v bl ON bl.id = br.required_id ORDER BY b.id",
    );
    for row in &view {
        let Value::Text(id) = &row[0] else {
            panic!("unexpected row {row:?}")
        };
        if id == "b0" {
            continue;
        }
        assert_eq!(
            row[1],
            Value::Null,
            "{id}: an unmatched LEFT JOIN must give a NULL rowid; a number here \
             is the last seeked row's rowid leaking out of the matview cursor"
        );
    }
    Ok(())
}

/// DEFECT B — `op_row_id`'s deferred-seek path positions the cursor with
/// `as_btree_mut()`, bypassing the wrapper, so `current_row` is never set AND
/// the pending `DeferredSeek` is consumed. A MATCHED row then loses every
/// projected value, not just the rowid.
#[turso_macros::test(views)]
fn projecting_rowid_first_keeps_the_matched_row(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    assert_arms_agree(
        &conn,
        "DEFECT B: rowid projected before a column",
        "SELECT b.id, bl.rowid IS NULL, bl.st FROM @B@ b \
         LEFT JOIN req br ON br.block_id = b.id \
         LEFT JOIN @B@ bl ON bl.id = br.required_id ORDER BY b.id",
    );

    // The matched row must still carry its value when rowid is read first.
    let hit = limbo_exec_rows(
        &conn,
        "SELECT bl.rowid IS NOT NULL, bl.st FROM v b \
         JOIN req br ON br.block_id = b.id \
         JOIN v bl ON bl.id = br.required_id WHERE b.id = 'b0'",
    );
    assert_eq!(
        hit,
        vec![vec![Value::Integer(1), Value::Text("DONE".into())]],
        "reading rowid before a column must not blank the matched row"
    );

    // Same row, opposite projection order: both orders must agree.
    let reversed = limbo_exec_rows(
        &conn,
        "SELECT bl.st, bl.rowid IS NOT NULL FROM v b \
         JOIN req br ON br.block_id = b.id \
         JOIN v bl ON bl.id = br.required_id WHERE b.id = 'b0'",
    );
    assert_eq!(
        vec![vec![hit[0][1].clone(), hit[0][0].clone()]],
        reversed,
        "projection ORDER must not change the values a matview index read returns"
    );
    Ok(())
}

/// IVM maintenance builds its own index cursors
/// (`DbspCircuit::new_index_cursor`), never through `OpenRead`, so writing to
/// an indexed matview's base inside a transaction — and committing — has to
/// keep working, index included.
#[turso_macros::test(views)]
fn writes_to_an_indexed_matview_still_work_inside_a_transaction(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    for i in 10..20 {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('c{i}', 'TODO')"
        ))?;
    }
    conn.execute("UPDATE t_raw SET st = 'DONE' WHERE id = 'b1'")?;
    conn.execute("DELETE FROM t_raw WHERE id = 'b2'")?;
    conn.execute("COMMIT")?;

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT st FROM v WHERE id = 'b1'"),
        vec![vec![Value::Text("DONE".into())]],
    );
    assert!(limbo_exec_rows(&conn, "SELECT id FROM v WHERE id = 'b2'").is_empty());
    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT count(*) FROM v WHERE id >= 'c10' AND id < 'c99'"
        ),
        vec![vec![Value::Integer(10)]],
    );
    // The index must agree with a forced scan after all that.
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM v WHERE id >= ''"),
        limbo_exec_rows(&conn, "SELECT count(*) FROM v WHERE +id >= ''"),
    );
    Ok(())
}

/// A CHAINED matview reading an indexed one. Maintaining and reading `w` must
/// not open an index cursor on `v`.
#[turso_macros::test(views)]
fn chained_matview_over_an_indexed_view_is_unaffected(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("CREATE MATERIALIZED VIEW w AS SELECT id, st FROM v WHERE st = 'TODO'")?;

    let before = limbo_exec_rows(&conn, "SELECT count(*) FROM w");
    assert_eq!(before, vec![vec![Value::Integer(5)]]);

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'TODO')")?;
    // Reading the CHAINED view inside the tx must work — it reads its own
    // btree plus its own overlay, never v's index.
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM w"),
        vec![vec![Value::Integer(6)]],
        "a chained view must see the uncommitted row and must not be refused"
    );
    conn.execute("COMMIT")?;

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM w"),
        vec![vec![Value::Integer(6)]],
    );
    Ok(())
}

/// An indexed read inside the writing transaction sees the uncommitted row.
#[turso_macros::test(views)]
fn uncommitted_rows_are_visible_through_the_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b5', 'NEW')")?;
    conn.execute("INSERT INTO t (rid, id, st) VALUES (99, 'b5', 'NEW')")?;
    assert_arms_agree(
        &conn,
        "count inside a write transaction",
        "SELECT count(*) FROM @B@",
    );
    assert_arms_agree(
        &conn,
        "point read of the uncommitted row",
        "SELECT id, st FROM @B@ WHERE id = 'b5'",
    );
    conn.execute("ROLLBACK")?;
    Ok(())
}

/// A backwards scan: `op_prev` has no matview arm, so it walks the inner btree
/// directly — skipping the overlay and leaving `current_row` behind.
#[turso_macros::test(views)]
fn reverse_scan_matches_the_control(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    for (what, sql) in [
        ("plain DESC", "SELECT id, st FROM @B@ ORDER BY id DESC"),
        (
            "DESC with a range on the indexed column",
            "SELECT id, st FROM @B@ WHERE id >= 'b1' ORDER BY id DESC",
        ),
        ("max via reverse scan", "SELECT max(id) FROM @B@"),
        (
            "DESC with LIMIT",
            "SELECT id FROM @B@ ORDER BY id DESC LIMIT 3",
        ),
    ] {
        assert_arms_agree(&conn, what, sql);
    }
    Ok(())
}
