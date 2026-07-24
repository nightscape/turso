# Bug Analysis: Negative Weight Persisted to Materialized View BTree

## Summary

When committing deltas to a materialized view's BTree storage, rows with negative weights can be incorrectly persisted when the key doesn't already exist in the BTree. This violates the invariant that committed BTree storage should only contain rows with positive weights.

## Error Observed

```
Failed to query matview for initial data: Query error: Internal error: Invalid data in materialized view: expected a positive weight, found -1
```

## Root Cause

The bug is in `core/incremental/compiler.rs` in the `WriteRowView::write_row` function (lines 69-107):

```rust
WriteRowView::GetRecord => {
    let res = return_if_io!(cursor.seek(key.clone(), SeekOp::GE { eq_only: true }));
    if !matches!(res, SeekResult::Found) {
        // BUG: When key is NOT found and weight is negative,
        // this inserts a row with negative weight!
        *self = WriteRowView::Insert {
            final_weight: weight,  // <-- No check if weight <= 0
        };
    } else {
        // ... when key IS found, correctly handles negative weights:
        let final_weight = existing_weight + weight;
        if final_weight <= 0 {
            *self = WriteRowView::Delete  // Correct: deletes instead of inserting negative
        } else {
            *self = WriteRowView::Insert { final_weight }
        }
    }
}
```

### The Logic Error

1. **When key EXISTS**: The code correctly computes `final_weight = existing_weight + weight`. If `final_weight <= 0`, it deletes the row.

2. **When key DOES NOT EXIST**: The code directly sets `final_weight = weight` without checking if it's positive. If `weight` is negative (e.g., -1 for a deletion), a row with `weight = -1` gets inserted.

## When This Bug Manifests

This bug occurs in recursive CTEs (like `blocks_with_paths`) under specific conditions:

1. A recursive CTE computes intermediate results during fixed-point iteration
2. Some rows are computed in early iterations and then "retracted" (deleted) in later iterations
3. During commit, the delta contains these retractions with `weight = -1`
4. If the retracted row was never actually committed to the BTree (e.g., it was an intermediate result), the BTree seek returns `NotFound`
5. The buggy code then inserts a row with `weight = -1`

### Specific Scenario: `blocks_with_paths` Materialized View

The `blocks_with_paths` view uses a recursive CTE:
```sql
WITH RECURSIVE paths AS (
    -- Base case: root blocks
    SELECT id, ..., '/' || id as path FROM blocks WHERE parent_id LIKE 'holon-doc://%'

    UNION ALL

    -- Recursive case: build path from parent
    SELECT b.id, ..., p.path || '/' || b.id as path
    FROM blocks b
    INNER JOIN paths p ON b.parent_id = p.id
)
SELECT * FROM paths;
```

During incremental updates to this view:
1. A block modification triggers delta computation
2. The recursive operator iterates to fixed-point
3. Some intermediate paths may be computed and then retracted
4. If a retraction delta reaches `commit()` for a row that was never persisted, the bug triggers

## Why Existing Tests Didn't Catch This

1. **No unit tests for `WriteRowView::write_row`**: The function has no dedicated test coverage.

2. **Integration tests always commit deltas with existing rows**: Most tests follow the pattern:
   - Insert rows (positive weights)
   - Commit to BTree
   - Delete rows (negative weights applied to existing rows)
   - Final weights are correctly computed

3. **Recursive CTE tests don't test incremental updates on empty views**: Tests either:
   - Populate the view from scratch (all positive weights)
   - Test incremental updates where rows already exist

4. **The cursor validation is at read time, not write time**: The error `expected a positive weight, found -1` is in `cursor.rs:164-167` when *reading* from the BTree, not when writing. This means:
   - The bad data gets written successfully
   - The error only surfaces later when reading

## Proposed Fix

```rust
WriteRowView::GetRecord => {
    let res = return_if_io!(cursor.seek(key.clone(), SeekOp::GE { eq_only: true }));
    if !matches!(res, SeekResult::Found) {
        // Key not found - only insert if weight is positive.
        // A negative weight for a non-existent key is a no-op
        // (can't delete what doesn't exist).
        if weight <= 0 {
            *self = WriteRowView::Done;
        } else {
            *self = WriteRowView::Insert {
                final_weight: weight,
            };
        }
    } else {
        // ... existing logic unchanged
    }
}
```

## Test Cases to Add

1. **Unit test for `WriteRowView::write_row` with negative weight on non-existent key**:
   ```rust
   #[test]
   fn test_write_row_negative_weight_key_not_found() {
       // Setup: empty BTree
       // Action: write_row with key=42, weight=-1
       // Expected: No row inserted, operation completes successfully
   }
   ```

2. **Integration test for recursive CTE with intermediate retractions**:
   ```rust
   #[test]
   fn test_recursive_cte_commit_with_intermediate_retractions() {
       // Setup: Create recursive CTE matview
       // Action: Insert data that causes intermediate rows to be computed and then retracted
       // Expected: Only final rows with positive weights in BTree
   }
   ```

3. **Property-based test**:
   ```rust
   proptest! {
       #[test]
       fn test_no_negative_weights_in_committed_btree(deltas: Vec<(i64, isize)>) {
           // For any sequence of deltas committed to a BTree,
           // all rows in the final BTree should have positive weights
       }
   }
   ```

## Severity

**High** - This bug causes runtime errors that crash the application when querying materialized views. The error occurs on read, meaning corrupted data persists until the database is rebuilt.

## Related Files

- `core/incremental/compiler.rs` - Contains the buggy `WriteRowView::write_row`
- `core/incremental/cursor.rs` - Contains the validation that detects the corruption
- `core/incremental/recursive_operator.rs` - Recursive CTE implementation that can produce the problematic deltas
- `core/incremental/view.rs` - View commit orchestration

## Reproducibility

The bug was discovered in the Holon PKM application when:
1. Loading a layout that includes the `blocks_with_paths` materialized view
2. The view is created with `CREATE MATERIALIZED VIEW IF NOT EXISTS`
3. Subsequent incremental updates to the blocks table trigger the bug
