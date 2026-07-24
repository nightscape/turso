# Bug Analysis: IVM + INSERT with REQUIRE_SEEK Panic

**Date:** 2026-01-05
**Location:** `core/vdbe/execute.rs:6318`
**Severity:** High (causes panic and database lock)

## Summary

A panic occurs in `op_insert` when an INSERT operation has both:
1. Dependent materialized views (requiring old record capture for IVM)
2. The `REQUIRE_SEEK` flag set

The assertion at line 6318 incorrectly assumes these two conditions are mutually exclusive.

## Reproduction

This bug was discovered in the Holon application when executing the navigation `focus` operation:

```sql
-- Step 1: Delete forward history
DELETE FROM navigation_history WHERE region = $region AND id > $current_id;

-- Step 2: Insert new history entry
INSERT INTO navigation_history (region, block_id) VALUES ($region, $block_id);

-- Step 3: Update cursor (this is where the panic occurs)
INSERT OR REPLACE INTO navigation_cursor (region, history_id) VALUES ($region, $new_id);
```

With this materialized view defined:
```sql
CREATE MATERIALIZED VIEW current_focus AS
SELECT nc.region, nh.block_id, nh.timestamp
FROM navigation_cursor nc
JOIN navigation_history nh ON nc.history_id = nh.id;
```

## Error Message

```
PanicException(to capture old record accurately, we must be located at the correct position in the table)
```

Followed by:
```
Query error: database is locked
```

## Technical Analysis

### The Problematic Code

In `core/vdbe/execute.rs`, function `op_insert`, starting at line 6301:

```rust
loop {
    match &state.op_insert_state.sub_state {
        OpInsertSubState::MaybeCaptureRecord => {
            let schema = program.connection.schema.read();
            let dependent_views = schema.get_dependent_materialized_views(table_name);

            // Case 1: No dependent views - skip capture, proceed normally
            if dependent_views.is_empty() || flag.has(InsertFlags::UPDATE_ROWID_CHANGE) {
                if flag.has(InsertFlags::REQUIRE_SEEK) {
                    state.op_insert_state.sub_state = OpInsertSubState::Seek;
                } else {
                    state.op_insert_state.sub_state = OpInsertSubState::Insert;
                }
                continue;
            }

            // Case 2: Has dependent views - BUG: asserts REQUIRE_SEEK is false
            turso_assert!(!flag.has(InsertFlags::REQUIRE_SEEK),
                "to capture old record accurately, we must be located at the correct position in the table");

            // ... capture logic assumes cursor is already positioned ...
        }
        // ...
    }
}
```

### The Logic Flaw

The code handles these cases:
1. **No dependent views + REQUIRE_SEEK** → Seek, then Insert ✓
2. **No dependent views + !REQUIRE_SEEK** → Insert directly ✓
3. **Has dependent views + !REQUIRE_SEEK** → Capture old record, then Insert ✓
4. **Has dependent views + REQUIRE_SEEK** → **PANIC** ✗

Case 4 is valid and should be handled: seek first, then capture the old record, then insert.

### When Does This Occur?

The `REQUIRE_SEEK` flag is set when the VDBE program generator cannot guarantee the cursor is already positioned at the correct location. This happens with:

- `INSERT OR REPLACE` statements (like the one in navigation_cursor)
- Certain INSERT statements where the rowid is computed dynamically
- INSERTs that don't follow a `NewRowid` or `NotExists` instruction

When the table being inserted into has dependent materialized views, the IVM system needs to capture the old record (if any) to compute the delta for view maintenance.

## Why This Bug Wasn't Detected

### 1. Uncommon Combination of Conditions

The bug requires a specific combination:
- A table with a materialized view dependency
- An INSERT operation with `REQUIRE_SEEK` flag
- The two must occur on the same table in the same operation

Most IVM tests use simple INSERT statements (not INSERT OR REPLACE), which typically don't have `REQUIRE_SEEK`.

### 2. Test Coverage Gap

Looking at the test files:
- `turso_ivm_join_test.rs` - Tests JOINs but with simple INSERTs
- `turso_ivm_bug_proptest.rs` - Property tests but may not generate INSERT OR REPLACE with views
- Integration tests focus on correctness, not edge cases of flag combinations

### 3. The Assertion Was Added Defensively

The assertion appears to have been added as a safety check during IVM development, assuming that any INSERT needing old record capture would already have the cursor positioned. This assumption held for the initial use cases but breaks with `INSERT OR REPLACE`.

### 4. Navigation Schema Was Added Later

The `current_focus` materialized view that triggers this bug was likely added after the IVM system was tested and stabilized. The navigation tables (`navigation_cursor`, `navigation_history`) are application-specific and weren't part of the core IVM test suite.

## Suggested Tests to Add

Before implementing the fix, add tests that would have caught this:

### Test 1: INSERT OR REPLACE with Dependent View
```rust
#[test]
fn test_insert_or_replace_with_materialized_view() {
    // Setup
    execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)");
    execute("CREATE MATERIALIZED VIEW v AS SELECT * FROM t WHERE value IS NOT NULL");

    // This should work without panic
    execute("INSERT OR REPLACE INTO t (id, value) VALUES (1, 'first')");
    execute("INSERT OR REPLACE INTO t (id, value) VALUES (1, 'updated')");

    // Verify view is correct
    let result = query("SELECT * FROM v");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["value"], "updated");
}
```

### Test 2: INSERT OR REPLACE on Table in JOIN View
```rust
#[test]
fn test_insert_or_replace_on_join_view_table() {
    execute("CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT)");
    execute("CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, data TEXT)");
    execute("CREATE MATERIALIZED VIEW v AS SELECT a.name, b.data FROM a JOIN b ON a.id = b.a_id");

    // Insert into both tables
    execute("INSERT INTO a VALUES (1, 'Alice')");
    execute("INSERT INTO b VALUES (1, 1, 'data1')");

    // This INSERT OR REPLACE should work
    execute("INSERT OR REPLACE INTO a VALUES (1, 'Alice Updated')");

    // Verify
    let result = query("SELECT * FROM v");
    assert_eq!(result[0]["name"], "Alice Updated");
}
```

### Test 3: Property Test for Flag Combinations
```rust
#[proptest]
fn test_insert_flags_with_views(
    has_view: bool,
    use_replace: bool,
    existing_row: bool,
) {
    // Generate schema with optional view
    // Generate INSERT or INSERT OR REPLACE
    // Should never panic, only return errors for invalid operations
}
```

## Proposed Fix

### Option A: Add New State (Recommended)

Add a new state `SeekThenCapture` that handles the seek-then-capture flow:

```rust
#[derive(Debug, PartialEq)]
pub enum OpInsertSubState {
    MaybeCaptureRecord,
    SeekThenCapture,  // NEW: Seek first when REQUIRE_SEEK + dependent views
    Seek,
    Insert,
    UpdateLastRowid,
    ApplyViewChange,
}

// In op_insert:
OpInsertSubState::MaybeCaptureRecord => {
    let dependent_views = schema.get_dependent_materialized_views(table_name);

    if dependent_views.is_empty() || flag.has(InsertFlags::UPDATE_ROWID_CHANGE) {
        if flag.has(InsertFlags::REQUIRE_SEEK) {
            state.op_insert_state.sub_state = OpInsertSubState::Seek;
        } else {
            state.op_insert_state.sub_state = OpInsertSubState::Insert;
        }
        continue;
    }

    // FIX: Handle the case where we need both seek AND capture
    if flag.has(InsertFlags::REQUIRE_SEEK) {
        state.op_insert_state.sub_state = OpInsertSubState::SeekThenCapture;
        continue;
    }

    // ... existing capture logic for when cursor is already positioned ...
}

OpInsertSubState::SeekThenCapture => {
    // Do the seek first
    let key = state.registers[*key_reg].get_value();
    let cursor = state.get_cursor(*cursor_id);
    let cursor = cursor.as_btree_mut();

    match key {
        Value::Integer(rowid) => {
            let seek_key = SeekKey::TableRowId(*rowid);
            return_if_io!(cursor.seek(seek_key, SeekOp::GE { eq_only: true }));
        }
        _ => {
            // Non-integer key - no old record to capture
            state.op_insert_state.old_record = None;
            state.op_insert_state.sub_state = OpInsertSubState::Insert;
            continue;
        }
    }

    // Now capture the old record (cursor is positioned)
    // ... same capture logic as MaybeCaptureRecord ...

    state.op_insert_state.sub_state = OpInsertSubState::Insert;
}
```

### Option B: Reorder Operations (Simpler but Less Clean)

Modify `MaybeCaptureRecord` to do the seek inline when needed:

```rust
OpInsertSubState::MaybeCaptureRecord => {
    let dependent_views = schema.get_dependent_materialized_views(table_name);

    if dependent_views.is_empty() || flag.has(InsertFlags::UPDATE_ROWID_CHANGE) {
        // ... existing logic ...
    }

    // If REQUIRE_SEEK, do the seek now before capturing
    if flag.has(InsertFlags::REQUIRE_SEEK) {
        let key = state.registers[*key_reg].get_value();
        if let Value::Integer(rowid) = key {
            let cursor = state.get_cursor(*cursor_id);
            let cursor = cursor.as_btree_mut();
            let seek_key = SeekKey::TableRowId(*rowid);
            return_if_io!(cursor.seek(seek_key, SeekOp::GE { eq_only: true }));
        }
        // Clear the flag since we've done the seek
        // Note: This requires making flag mutable or tracking seek-done separately
    }

    // ... existing capture logic ...
}
```

### Recommendation

**Option A** is cleaner because:
1. Maintains clear state machine semantics
2. Handles I/O properly (seek may return IO)
3. Doesn't require mutating the instruction flags
4. Easier to debug and trace

## Impact Assessment

### Tables Affected
Any table that:
1. Has a materialized view dependency (directly or via JOIN)
2. Receives `INSERT OR REPLACE` or similar statements

### Workaround
Until fixed, avoid `INSERT OR REPLACE` on tables with materialized view dependencies. Use explicit `SELECT` + `UPDATE`/`INSERT` instead:

```sql
-- Instead of:
INSERT OR REPLACE INTO navigation_cursor (region, history_id) VALUES ($region, $new_id);

-- Use:
UPDATE navigation_cursor SET history_id = $new_id WHERE region = $region;
-- If no rows updated:
INSERT INTO navigation_cursor (region, history_id) VALUES ($region, $new_id);
```

## Files to Modify

1. `core/vdbe/execute.rs` - Add `SeekThenCapture` state and handling
2. `core/vdbe/execute.rs` - Add tests for the new state
3. Consider adding integration test in `tests/` directory

## References

- VDBE Insert instruction: `core/vdbe/insn.rs:123` (InsertFlags)
- IVM view dependency tracking: `core/schema.rs:375` (get_dependent_materialized_views)
- Related IVM bug reproducers in holon: `examples/turso_ivm_*.rs`
