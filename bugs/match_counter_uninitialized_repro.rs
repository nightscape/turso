//! Reproducer for `MatchCounterOperator::eval called with Uninitialized state`
//!
//! Bug: `core/incremental/match_counter_operator.rs:378`
//! Pinned commit when surfaced: `81cef68c` (branch `nightscape@holon`)
//! Source-side fix scope: `process_match_counter_state` calls
//! `return_if_io!(read_r_count(...))` (line 484) and
//! `return_if_io!(read_next_join_row(...))` (line 531) without restoring
//! `*outer` first. When those reads suspend on I/O, `*eval_state` stays
//! `EvalState::Uninitialized` (set by the parent `eval_internal` loop's
//! `mem::replace`), and the next `commit` cycle reaches the panic branch
//! at line 378.
//!
//! How to run (from a Turso checkout root):
//! ```
//! cp bugs/match_counter_uninitialized_repro.rs \
//!    tests/integration/query_processing/test_match_counter_uninitialized_repro.rs
//! # then add `mod test_match_counter_uninitialized_repro;` to
//! # tests/integration/query_processing/mod.rs
//! cargo test --test integration_tests \
//!     match_counter_uninitialized_repro -- --nocapture
//! ```
//!
//! Holon downstream symptom (caught by `TursoBackend::Actor`'s
//! `catch_unwind`, ~20 panics/run):
//! ```
//! ERROR holon::storage::turso: [TursoBackend::Actor] Caught panic during
//!   command processing: MatchCounterOperator::eval called with
//!   Uninitialized state. Actor continues.
//! WARN turso_core::incremental::match_counter_operator:
//!   [MatchCounterOperator::commit] Recovering from Invalid state.
//!   Resetting to Idle.
//! ```
//!
//! Strategy: use a file-backed DB (so btree reads can suspend on real
//! I/O) + many tight CDC cycles on a dual-LEFT JOIN matview that mirrors
//! holon's `block` matview shape. Each separate INSERT/DELETE statement
//! is its own commit, so the matview re-runs MatchCounterOperator per
//! statement. A burst of DELETEs that drive `R_COUNT` across zero is the
//! most reliable trigger because it forces the `ScanningL` phase
//! (the second `return_if_io!` site).

#[tokio::test]
async fn test_match_counter_uninitialized_dual_left_cdc_burst() -> anyhow::Result<()> {
    let db_path = "/tmp/turso-match-counter-uninit-repro.db";
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    let db = turso::Builder::new_local(db_path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    conn.execute("CREATE TABLE block_raw (id TEXT PRIMARY KEY, content TEXT)", ())
        .await?;
    conn.execute(
        "CREATE TABLE block_tags (block_id TEXT, tag TEXT, PRIMARY KEY (block_id, tag))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE task_blockers (blocked_id TEXT, blocker_id TEXT, PRIMARY KEY (blocked_id, blocker_id))",
        (),
    )
    .await?;

    // Dual LEFT JOIN matview — each LEFT JOIN spawns its own
    // MatchCounterOperator. Mirrors holon's `block` matview verbatim
    // (see `crates/holon/sql/schema/block_matview.sql`).
    conn.execute(
        "CREATE MATERIALIZED VIEW block_matview AS \
         SELECT b.id, b.content, \
           COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags, \
           COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by \
         FROM block_raw b \
         LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id \
         LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id \
         GROUP BY b.id, b.content",
        (),
    )
    .await?;

    // Seed: a moderately-sized dataset so the btree pages get pushed out
    // of the buffer pool and reads can suspend.
    for i in 0..200 {
        conn.execute(
            &format!("INSERT INTO block_raw VALUES ('b{i}', 'content for block {i}')"),
            (),
        )
        .await?;
    }
    for i in 0..200 {
        conn.execute(
            &format!("INSERT INTO block_tags VALUES ('b{i}', 'tag-{}')", i % 7),
            (),
        )
        .await?;
        if i % 3 == 0 {
            conn.execute(
                &format!("INSERT INTO block_tags VALUES ('b{i}', 'extra')"),
                (),
            )
            .await?;
        }
    }
    for i in 0..200 {
        if i % 4 == 0 {
            conn.execute(
                &format!(
                    "INSERT INTO task_blockers VALUES ('b{i}', 'b{}')",
                    (i + 1) % 200
                ),
                (),
            )
            .await?;
        }
    }

    // Burst of DELETEs that drive R_COUNT across zero per key — the
    // MatchCounter must enter `ScanningL` (line 531) and re-emit
    // null-padded rows. Each statement is its own commit cycle.
    for i in 0..200 {
        conn.execute(
            &format!("DELETE FROM block_tags WHERE block_id = 'b{i}'"),
            (),
        )
        .await?;
    }
    for i in 0..200 {
        if i % 4 == 0 {
            conn.execute(
                &format!("DELETE FROM task_blockers WHERE blocked_id = 'b{i}'"),
                (),
            )
            .await?;
        }
    }

    // Re-insert + re-delete to keep churning the operator.
    for i in 0..50 {
        conn.execute(
            &format!("INSERT INTO block_tags VALUES ('b{i}', 'page')"),
            (),
        )
        .await?;
        conn.execute(
            &format!("DELETE FROM block_tags WHERE block_id = 'b{i}'"),
            (),
        )
        .await?;
    }

    // Final snapshot — if the bug fired, either a panic propagated up or
    // the matview produced garbage. Without the panic-catch wrapper that
    // holon uses, this test exits non-zero on panic.
    let mut rows = conn
        .query(
            "SELECT id, tags, blocked_by FROM block_matview WHERE id = 'b0'",
            (),
        )
        .await?;
    let row = rows.next().await?.expect("b0 must still be present");
    let id: String = row.get(0)?;
    let tags: String = row.get(1)?;
    let blockers: String = row.get(2)?;
    assert_eq!(id, "b0");
    assert_eq!(
        tags, "[]",
        "after deleting all of b0's tags, tags must be empty array"
    );
    assert_eq!(
        blockers, "[]",
        "after deleting all of b0's blockers, blocked_by must be empty array"
    );

    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));
    Ok(())
}
