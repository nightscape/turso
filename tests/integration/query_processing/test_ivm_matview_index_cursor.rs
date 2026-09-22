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

/// The C2 refusal message, as Holon must be able to grep for it.
fn assert_refused(
    conn: &Arc<turso_core::Connection>,
    what: &str,
    sql: &str,
    view: &str,
    index: &str,
) {
    let err = conn
        .execute(sql)
        .expect_err(&format!(
            "{what}: expected a refusal, the query succeeded: {sql}"
        ))
        .to_string();
    for needle in [
        index,
        view,
        "cannot be read while this transaction has uncommitted changes to it",
        "commit first",
    ] {
        assert!(
            err.contains(needle),
            "{what}: the refusal must name {needle:?} so Holon can grep it; got: {err}"
        );
    }
}

/// DEFECT C, ruling C2 — the index btree is maintained at COMMIT, so rows
/// written inside the open transaction have no index entry. Reading through
/// the index would skip them silently, so it is REFUSED instead.
///
/// The refusal is narrow: only this connection's overlay, only for the view
/// that was written to.
#[turso_macros::test(views)]
fn indexed_read_is_refused_only_while_that_view_has_uncommitted_changes(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("CREATE TABLE unrelated (x INTEGER)")?;
    conn.execute("CREATE TABLE o_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW o AS SELECT id, st FROM o_raw")?;
    conn.execute("CREATE INDEX idx_o_id ON o(id)")?;
    conn.execute("INSERT INTO o_raw (id, st) VALUES ('z1', 'A')")?;

    let point_read = "SELECT id, st FROM v WHERE id = 'b1'";
    let baseline = limbo_exec_rows(&conn, point_read);
    assert_eq!(baseline.len(), 1, "fixture");

    // (1) An open transaction that has written nothing must not refuse.
    conn.execute("BEGIN")?;
    assert_eq!(
        limbo_exec_rows(&conn, point_read),
        baseline,
        "open tx, no writes"
    );
    conn.execute("COMMIT")?;

    // (2) Writes to an unrelated TABLE must not refuse.
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO unrelated (x) VALUES (1)")?;
    assert_eq!(
        limbo_exec_rows(&conn, point_read),
        baseline,
        "unrelated table write"
    );
    conn.execute("COMMIT")?;

    // (3) A write to ANOTHER matview's base must not refuse this view's index.
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO o_raw (id, st) VALUES ('z2', 'B')")?;
    assert_eq!(
        limbo_exec_rows(&conn, point_read),
        baseline,
        "another matview was written; this view's index must still read"
    );
    conn.execute("COMMIT")?;

    // (4) A write to THIS view's base refuses THIS view's index...
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b5', 'NEW')")?;
    // ...while a plan that does NOT open the index still works and SEES the
    // uncommitted row. (`count(*)` would be refused: the planner serves it
    // from a COVERING INDEX scan. The refusal is exactly "this plan opens a
    // cursor on that index", never "this view is unreadable".)
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM (SELECT * FROM v)"),
        vec![vec![Value::Integer(7)]],
        "a non-index plan must still see the uncommitted row"
    );
    assert_refused(
        &conn,
        "after writing this view's base",
        point_read,
        "v",
        "idx_v_id",
    );
    // The refusal ABORTS the transaction: the write is rolled back with it.
    // That is a cost of the refuse-ruling, so it is pinned here rather than
    // left to be discovered.
    let commit_err = conn
        .execute("COMMIT")
        .expect_err("the refusal must have ended the transaction")
        .to_string();
    assert!(
        commit_err.contains("no transaction is active"),
        "expected the refusal to have aborted the tx, got: {commit_err}"
    );
    assert_eq!(
        limbo_exec_rows(&conn, point_read),
        baseline,
        "after the aborted tx the index reads again"
    );
    assert!(
        limbo_exec_rows(&conn, "SELECT id FROM v WHERE +id = 'b5'").is_empty(),
        "the write of the aborted tx must be gone"
    );

    // (5) The same write, committed without a refused read, then the index
    //     reads again and is up to date.
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b5', 'NEW')")?;
    conn.execute("COMMIT")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id, st FROM v WHERE id = 'b5'"),
        vec![vec![Value::Text("b5".into()), Value::Text("NEW".into())]],
        "after COMMIT the index must be usable and up to date"
    );

    // (6) After an explicit ROLLBACK the index reads again and the write is gone.
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b6', 'TMP')")?;
    conn.execute("ROLLBACK")?;
    assert_eq!(
        limbo_exec_rows(&conn, point_read),
        baseline,
        "after ROLLBACK"
    );
    assert!(
        limbo_exec_rows(&conn, "SELECT id FROM v WHERE id = 'b6'").is_empty(),
        "the rolled-back row must be gone"
    );
    Ok(())
}

/// The OTHER view's own index is the one refused, and only it.
#[turso_macros::test(views)]
fn refusal_names_the_view_that_was_written(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("CREATE TABLE o_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW o AS SELECT id, st FROM o_raw")?;
    conn.execute("CREATE INDEX idx_o_id ON o(id)")?;
    conn.execute("INSERT INTO o_raw (id, st) VALUES ('z1', 'A')")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO o_raw (id, st) VALUES ('z2', 'B')")?;
    assert_refused(
        &conn,
        "the written view's index is refused and named",
        "SELECT id FROM o WHERE id = 'z1'",
        "o",
        "idx_o_id",
    );
    Ok(())
}

/// C2 must not break WRITES. IVM maintenance builds its own index cursors
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
/// not open an index cursor on `v`, or C2 would refuse ordinary work.
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

/// The dependency is TRANSITIVE. `w` reads `v`; a write to `v`'s base stages
/// a delta for `v` only, so a guard that looked at `w`'s own transaction
/// state would let an index on `w` serve stale rows, unrefused. Four arms,
/// exactly the verifier's counterexample.
#[turso_macros::test(views)]
fn chained_view_with_its_own_index_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE MATERIALIZED VIEW w AS SELECT id, st FROM v")?;
    for (id, st) in ROWS {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('{id}', '{st}')"
        ))?;
    }
    // Index on `w` only — `v` is unindexed, so nothing about `v` can refuse.
    conn.execute("CREATE INDEX idx_w_id ON w(id)")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    assert_refused(
        &conn,
        "index on a CHAINED view, base of its upstream written",
        "SELECT count(*) FROM w",
        "w",
        "idx_w_id",
    );
    Ok(())
}

/// Indexes on BOTH levels: each must be refused, and each message must name
/// the index that was actually opened.
#[turso_macros::test(views)]
fn both_levels_indexed_are_both_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE MATERIALIZED VIEW w AS SELECT id, st FROM v")?;
    for (id, st) in ROWS {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('{id}', '{st}')"
        ))?;
    }
    conn.execute("CREATE INDEX idx_v_id ON v(id)")?;
    conn.execute("CREATE INDEX idx_w_id ON w(id)")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    assert_refused(
        &conn,
        "both indexed, reading v",
        "SELECT count(*) FROM v",
        "v",
        "idx_v_id",
    );
    Ok(())
}

/// A three-level chain: the staleness must propagate the whole way up.
#[turso_macros::test(views)]
fn three_level_chain_refuses_at_the_top(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v1 AS SELECT id, st FROM t_raw")?;
    conn.execute("CREATE MATERIALIZED VIEW v2 AS SELECT id, st FROM v1")?;
    conn.execute("CREATE MATERIALIZED VIEW v3 AS SELECT id, st FROM v2")?;
    for (id, st) in ROWS {
        conn.execute(&format!(
            "INSERT INTO t_raw (id, st) VALUES ('{id}', '{st}')"
        ))?;
    }
    conn.execute("CREATE INDEX idx_v3_id ON v3(id)")?;

    let before = limbo_exec_rows(&conn, "SELECT count(*) FROM v3");
    assert_eq!(before, vec![vec![Value::Integer(6)]]);

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    assert_refused(
        &conn,
        "three-level chain, top indexed",
        "SELECT count(*) FROM v3",
        "v3",
        "idx_v3_id",
    );

    // After the abort the index reads again, and a committed write lands.
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b9', 'NEW')")?;
    conn.execute("COMMIT")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM v3"),
        vec![vec![Value::Integer(7)]],
        "after COMMIT the top of the chain must be readable through its index"
    );
    Ok(())
}

/// A write to an UNRELATED chain must not refuse this chain's index — the
/// transitive rule must not become "any open write refuses everything".
#[turso_macros::test(views)]
fn an_unrelated_chain_does_not_refuse(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE a_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE TABLE b_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW a1 AS SELECT id, st FROM a_raw")?;
    conn.execute("CREATE MATERIALIZED VIEW a2 AS SELECT id, st FROM a1")?;
    conn.execute("CREATE MATERIALIZED VIEW b1 AS SELECT id, st FROM b_raw")?;
    conn.execute("INSERT INTO a_raw (id, st) VALUES ('x1', 'A')")?;
    conn.execute("INSERT INTO b_raw (id, st) VALUES ('y1', 'B')")?;
    conn.execute("CREATE INDEX idx_a2_id ON a2(id)")?;
    conn.execute("CREATE INDEX idx_b1_id ON b1(id)")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO b_raw (id, st) VALUES ('y2', 'B')")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM a2"),
        vec![vec![Value::Integer(1)]],
        "the a-chain's index must still read when only the b-chain was written"
    );
    assert_refused(
        &conn,
        "the written chain is the one refused",
        "SELECT count(*) FROM b1",
        "b1",
        "idx_b1_id",
    );
    Ok(())
}

/// The C1 target behaviour, for when the ruling changes: an indexed read
/// inside the writing transaction SUCCEEDS and sees the uncommitted row.
/// Ignored because C2 is the current ruling and refuses it.
#[turso_macros::test(views)]
#[ignore = "C1 target: needs the index maintained against the in-tx overlay; C2 (refuse) is the current ruling"]
fn c1_target_uncommitted_rows_visible_through_the_index(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t_raw (id, st) VALUES ('b5', 'NEW')")?;
    conn.execute("INSERT INTO t (rid, id, st) VALUES (99, 'b5', 'NEW')")?;
    assert_arms_agree(
        &conn,
        "C1: count inside a write transaction",
        "SELECT count(*) FROM @B@",
    );
    assert_arms_agree(
        &conn,
        "C1: point read of the uncommitted row",
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
