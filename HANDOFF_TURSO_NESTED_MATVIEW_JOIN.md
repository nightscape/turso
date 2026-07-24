# Handoff: Turso IVM — Nested Materialized View JOINs Hang & Don't Propagate

## Repo
`/Users/martin/Workspaces/bigdata/turso/`

## Summary

Two related bugs in Turso's IVM (Incremental View Materialization) when a materialized view (B) JOINs another materialized view (A) that itself contains a JOIN:

1. **Hang on CREATE**: `CREATE MATERIALIZED VIEW B AS SELECT ... FROM table JOIN A ...` hangs indefinitely when A is a matview with a JOIN
2. **No CDC propagation** (if creation succeeds): Changes to base tables propagate to A but NOT to B

## Minimal Reproducer (pure SQL)

```sql
-- Base tables
CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT);
CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT, block_id TEXT);
CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER);
INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL);

-- MatView A: has a JOIN internally
CREATE MATERIALIZED VIEW current_focus AS
SELECT nc.region, nh.block_id
FROM navigation_cursor nc
JOIN navigation_history nh ON nc.history_id = nh.id;

-- MatView B: JOINs MatView A — THIS HANGS
CREATE MATERIALIZED VIEW watch_view AS
SELECT blocks.id, blocks.content
FROM blocks
INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
WHERE cf.region = 'main';
```

## Rust Reproducer (standalone binary)

File: `holon/examples/turso_ivm_nested_join_repro.rs`

```
cargo run --example turso-ivm-nested-join-repro
```

This uses `turso::Builder::new_local(":memory:")` with `.enable_experimental_views(true)`. The CREATE for `watch_view` hangs.

## Rust Reproducer (async test)

File: `holon/crates/holon/src/storage/turso_ivm_join_test.rs`

```
cargo test -p holon turso_ivm_join_test::test_nested_matview_joins_panic -- --nocapture
```

Confirmed hang: test runs for 60+ seconds without completing. Hangs at the `CREATE MATERIALIZED VIEW watch_view` step (Step 3).

## Related Bug: JoinOperator Invalid State Panic

File: `holon/examples/turso-ivm-joinoperator-invalid-reproducer.rs`

When a matview with a JOIN exists and data is inserted into an _unrelated_ table, `JoinOperator::commit` panics with "Invalid state reached" because `apply_view_deltas` is called for ALL views during commit, including the JOIN view that never received updates.

## Root Cause Hypothesis

Based on the code comments in the reproducers:

1. **Hang**: The DBSP graph setup for matview B needs to resolve matview A's definition. If A contains a JOIN, the graph construction enters a dependency loop or blocks waiting for A's JOIN operator to be ready.

2. **Invalid state panic**: `apply_view_deltas` iterates all views during commit, not just views affected by the changed tables. When a JoinOperator is in `Invalid` state (no deltas received), `commit()` panics instead of no-oping.

## Relevant Source Files

- `core/incremental/join_operator.rs` — `JoinCommitState::Invalid` panic at line ~770
- `core/incremental/operator.rs` — operator commit orchestration
- `core/incremental/compiler.rs` — DBSP graph compilation for matviews
- `core/incremental/view.rs` — matview creation and dependency tracking

## Use Case

We're building a PKM app where:
1. `navigation_cursor` + `navigation_history` track what document the user is viewing
2. `current_focus` matview provides the current focus per UI region (has a JOIN)
3. User queries like `SELECT * FROM blocks JOIN current_focus ...` need to be reactive matviews (via `query_and_watch`)

When the user clicks a document in the sidebar, we update `navigation_cursor`, which should cascade through `current_focus` → the query matview → CDC → Flutter UI update.

## Success Criteria

1. `CREATE MATERIALIZED VIEW B AS ... JOIN A ...` completes without hanging when A is also a matview with a JOIN
2. DML on base tables propagates through A → B (full dependency chain)
3. CDC callbacks fire for B when A's underlying data changes
4. `JoinOperator::commit` no-ops gracefully when the operator received no deltas (instead of panicking on Invalid state)

## Workaround (if not fixing now)

Change `current_focus` from `CREATE MATERIALIZED VIEW` to `CREATE VIEW` (regular view). The downstream matview would then resolve the view definition and track the base tables directly. This avoids matview-on-matview but loses IVM for `current_focus` itself.
