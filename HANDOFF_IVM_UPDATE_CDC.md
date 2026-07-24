# Handoff: IVM Matview UPDATE Not Propagated / CDC Zero Changes

## Problem Statement

After `UPDATE` on a base table with a dependent materialized view, the matview
shows stale data and the CDC callback receives zero changes. Reported by holon
PKMS app where the frontend shows stale data after edits.

**Severity**: High. Any UPDATE on a table with a matview silently drops the
change from the matview. INSERTs and DELETEs work. Only UPDATEs are affected.

## Reproduction

Fails ~40-60% of the time in auto-commit mode. 100% correct inside BEGIN/COMMIT.

```sql
CREATE TABLE t (id TEXT PRIMARY KEY, val TEXT);
INSERT INTO t VALUES ('a', 'old_a');
INSERT INTO t VALUES ('b', 'old_b');
CREATE MATERIALIZED VIEW mv AS SELECT id, val FROM t;
UPDATE t SET val = 'new_b' WHERE id = 'b';
SELECT val FROM mv WHERE id = 'b';
-- Expected: new_b
-- Actual (flaky): old_b
```

Test files:
- `testing/runner/tests/ivm-cdc-update.sqltest` (sqltest format, ~50% failure)
- `tests/integration/query_processing/test_ivm_cdc_update.rs` (Rust, ~30% failure)

Run: `cargo run -p test-runner -- run testing/runner/tests/ivm-cdc-update.sqltest`

## What Was Found

### Bug 1: Spurious `view_transaction_states` Entry (FIXED in `ovl`)

**Root cause**: Opening a matview cursor for a read-only SELECT
(`core/vdbe/execute.rs:1072`) called `view_transaction_states.get_or_create()`.
This created a spurious entry. When the read transaction committed,
`apply_view_deltas` found this non-empty entry, processed the view with empty
deltas, and `set_output_delta(empty_delta)` overwrote the correct output_delta
from the prior write transaction's commit.

**Fix applied**:
1. `execute.rs:1072`: Changed `get_or_create()` to `get().unwrap_or_else(||
   Arc::new(ViewTransactionState::new()))` so read-only cursor opens don't
   register in the global map.
2. `mod.rs:1492-1502`: Added empty delta skip in `apply_view_deltas` Processing
   state — if all `table_deltas` have empty `.changes`, skip the view.

**Validation**: Confirmed via tracing that the second spurious
`apply_view_deltas` call no longer occurs.

### Bug 2: Delta Ordering in CommitState::UpdateView (FIXED)

This was the remaining bug causing ~40% failure rate.

**Root cause**: `CommitState::UpdateView` in `core/incremental/compiler.rs`
iterates `delta.changes` in arbitrary order (HashMap iteration order from
`consolidate()`). For an UPDATE, the delta has two entries for the same rowid:
a delete(old_values, weight=-1) and an insert(new_values, weight=+1).

When the insert is processed BEFORE the delete:
1. Insert(new_b, +1): Seeks rowid X, finds existing (old_b, weight=1).
   Computes final_weight = 1+1 = 2. Inserts (new_b, weight=2).
2. Delete(old_b, -1): Seeks rowid X, finds existing (new_b, weight=2).
   Computes final_weight = 2-1 = 1. Inserts (old_b, weight=1) — using the
   DELETE delta's values, not the existing record's values.

Result: matview row reverts to old_b.

When the delete is processed FIRST (correct order):
1. Delete(old_b, -1): Seeks rowid X, finds (old_b, weight=1). final_weight=0.
   Deletes row.
2. Insert(new_b, +1): Seeks rowid X, not found. Inserts (new_b, weight=1).

Result: matview row correctly shows new_b.

**Fix**: After `delta.consolidate()`, sort delta changes so that for any given
rowid, deletes (weight < 0) are processed before inserts (weight > 0):
`delta.changes.sort_by(|(a, aw), (b, bw)| a.rowid.cmp(&b.rowid).then(aw.cmp(bw)));`

**Why BEGIN/COMMIT worked**: In explicit transactions, `apply_view_deltas` runs
at COMMIT time. The page cache retains the btree page across statements within
the transaction, so the ordering issue was masked — the btree page was always
read from cache with correct data. In auto-commit mode, each statement commits
separately, and the flaky HashMap iteration order determined pass/fail.

## Commit Flow (for reference)

The auto-commit path in the Halt handler (`execute.rs:2241-2254`):

```
1. end_statement(ReleaseSavepoint)     — pops statement savepoint
2. commit_txn()
   2a. apply_view_deltas()             — writes matview btree, adds dirty pages
   2b. commit_txn_wal()
       → commit_dirty_pages()          — writes dirty pages to WAL frames
       → wal.end_write_tx()            — releases write lock
       → wal.end_read_tx()             — releases read lock
3. (next statement) begin_read_tx()    — acquires read lock
   → clear_page_cache(false)           — evicts all cached pages
   → subsequent reads go through WAL   — *** misses matview frame? ***
```

## Resolution

Both bugs are now fixed. The WAL visibility hypothesis was a red herring —
the WAL correctly stores and retrieves matview frames. The actual issue was
non-deterministic delta processing order in the DBSP commit path.

## Files Changed

| File | Change | Status |
|------|--------|--------|
| `core/vdbe/execute.rs:1072` | `get_or_create` → `get` for matview cursor | Fixed |
| `core/vdbe/mod.rs:1492` | Skip empty deltas in `apply_view_deltas` | Fixed |
| `testing/runner/tests/ivm-cdc-update.sqltest` | Failing test (sqltest) | New |
| `tests/integration/query_processing/test_ivm_cdc_update.rs` | Failing test (Rust) | New |
| `tests/integration/query_processing/mod.rs` | Register test module | New |
| `core/incremental/compiler.rs` | Sort delta changes: deletes before inserts per rowid | Fixed |

## What NOT to Investigate

- The NoopCheck optimization in `op_insert` (`execute.rs:7699`) is NOT the
  cause. Tested by completely emptying the body — same failure rate.
- The `ApplyViewChange` delta emission is correct (confirmed via tracing).
- The DBSP circuit processing is correct (`merge_delta` returns 2 changes).
- The matview btree write completes (dirty page IS in the set).
- Test isolation issues were a red herring from the in-memory test reusing a
  global database — fixed by using `TempDatabase`.
