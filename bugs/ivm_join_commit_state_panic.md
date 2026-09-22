# Bug: BTree cursor corruption in IVM with chained materialized views

## Summary

When chained materialized views (a matview that JOINs with another matview) exist alongside other matviews with CDC callbacks active, inserts into base tables cause BTree cursor corruption during `JoinOperator::commit`. The cursor's `PageStack` has `current_page=-1` with an empty `loaded_pages` array.

## Reproducer

```bash
cargo test --test integration_tests test_ivm_join_cursor_corruption_chained_watch_views -- --nocapture
```

**File**: `tests/integration/query_processing/test_ivm_join_cursor_corruption.rs`

Panics consistently. Verified on macOS Darwin 25.3.0, local build from main.

## Stack Trace

```
core::panicking::panic_fmt
turso_core::storage::btree::PageStack::top          (btree.rs:6684)
turso_core::storage::btree::BTreeCursor::insert_into_page
<BTreeCursor as CursorTrait>::insert
turso_core::incremental::persistence::WriteRow::write_row  (persistence.rs:298)
<JoinOperator as IncrementalOperator>::commit        (join_operator.rs:759)
DbspNode::process_node                              (compiler.rs:476)
DbspCircuit::execute_node                           (compiler.rs:891)  ← 4 levels deep
DbspCircuit::execute_node                           (compiler.rs:872)
DbspCircuit::execute_node                           (compiler.rs:993)
DbspCircuit::execute_node                           (compiler.rs:872)
DbspCircuit::run_circuit                            (compiler.rs:586)
DbspCircuit::commit                                 (compiler.rs:680)
IncrementalView::merge_delta                        (view.rs:1622)
Program::apply_view_deltas                          (vdbe/mod.rs:1465)
Program::commit_txn                                 (vdbe/mod.rs:1544)
```

## Required conditions

The bug requires **all** of these:

1. **Chained matview dependencies**: A matview that JOINs with another matview (e.g., `watch_view_main` JOINs with `current_focus`)
2. **Multiple other matviews**: A recursive CTE matview (`blocks_with_paths`) and a filter matview (`events_view_block`) competing for pager/cursor state
3. **CDC callbacks active**
4. **Sequential inserts** triggering IVM cascades across all views

### Schema structure (3 dependency levels):

```
Level 0: blocks, events, navigation_cursor, navigation_history
Level 1: blocks_with_paths (recursive CTE on blocks)
         events_view_block (filter on events)
         current_focus (JOIN on navigation tables)
Level 2: watch_view_main (blocks INNER JOIN current_focus)
         watch_view_sidebar (blocks INNER JOIN current_focus)
```

### NOT required (verified by passing tests):
- Multiple level-1 matviews without chaining — even with 200+ inserts
- Deeply nested hierarchies
- Interleaved inserts across different tables
- Re-entrant DB access from CDC callbacks

## Analysis

During `apply_view_deltas`, BFS collects all transitively dependent views and processes them sequentially. For chained views, `execute_node` recurses 4 levels deep through the DBSP circuit graph. During this deep traversal, a BTree cursor's page stack is cleared but not re-pushed before the next `WriteRow::write_row` uses it.

### Secondary issue: `Invalid` state loop

`JoinOperator::commit()` uses `mem::replace(&mut self.commit_state, JoinCommitState::Invalid)` as a sentinel. If commit panics (as above), the state stays `Invalid`. The `return_and_restore_if_io!` macro restores on IO/Error but **not on panic**. All subsequent IVM updates to this operator immediately panic at the `Invalid` match arm — creating an infinite panic loop. In the Holon app, this produces 136 panics per startup.

## Potential fixes

1. **Fix cursor lifecycle**: Ensure BTree cursors are properly re-initialized per node in the cascading `execute_node` calls, not shared/reused across cascade levels
2. **Reset on panic**: After catching a panic, reset `commit_state` to `Idle` (mitigates the infinite loop, doesn't fix root cause)
3. **Replace sentinel pattern**: Use `Option<JoinCommitState>` with `take()` instead of `mem::replace` with `Invalid`
