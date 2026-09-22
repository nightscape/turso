# IVM Concurrency Bug: B-tree Overflow Cell Ordering Violation

## Summary

A panic occurs when two concurrent write operations both trigger IVM (Incremental View Maintenance) on the same materialized view. The B-tree's overflow cell ordering invariant is violated, causing an assertion failure in debug builds.

## Error Message

```
thread 'tokio-runtime-worker' panicked at core/storage/btree.rs:7410:17:
multiple overflow cells can only occur when a parent overflows during balancing
as divider cells are inserted into it. those cells should always be in-order and sequential
```

## Root Cause

The assertion at `btree.rs:7410` checks that when adding overflow cells to a page, they must be sequential:

```rust
if let Some(overflow_cell) = page.overflow_cells.last() {
    turso_assert!(overflow_cell.index + 1 == cell_idx, "...");
}
```

When two concurrent IVM operations insert into the same B-tree page:
1. Thread A inserts at cell_idx=N, page overflows, creates overflow cell at index N
2. Thread B inserts at cell_idx=M (where M != N+1), tries to create overflow cell
3. Assertion fails because M != N+1

## Reproduction Scenario

From the Holon application logs (`/tmp/flutter.log`):

```
# Two concurrent operations visible in trace spans:

# Thread 1: Main IVM operation
90374: normal_step:apply_view_deltas:insert
90376: cell_idx=7
90377: cell_count=7

# Thread 2: Another INSERT triggering IVM (interleaved)
90242: ffi.stream_forwarding:normal_step:Transaction { tx_mode: Write }
90248: ffi.stream_forwarding:normal_step:begin_read_tx

# Panic occurs
90420: PANIC: multiple overflow cells...
```

Both threads are modifying **page 38** of the `events_view_block` materialized view simultaneously.

## Why This Bug Was Not Detected

### 1. Known unaudited thread safety

**The developers are aware this is a problem!** Multiple IVM-related structs have explicit safety comments:

```rust
// SAFETY: This needs to be audited for thread safety.
// See: https://github.com/tursodatabase/turso/issues/1552
unsafe impl Send for DbspCircuit {}
unsafe impl Sync for DbspCircuit {}
```

This pattern appears in:
- `incremental/view.rs:46` - `PopulateState`
- `incremental/view.rs:157` - `AllViewsTxState`
- `incremental/view.rs:245` - `IncrementalView`
- `incremental/compiler.rs:443` - `DbspNode`
- `incremental/compiler.rs:515` - `DbspCircuit`
- `incremental/operator.rs:229` - `IncrementalOperator` trait

**GitHub Issue**: https://github.com/tursodatabase/turso/issues/1552

**Issue #1552 confirms**: "We currently don't support multi-threading at all."
- Classified as a bug, assigned to milestone 0.5
- Related issues:
  - #1804: Segmentation fault crash
  - #1382: "Invalid page type" error during stress testing
  - #2260: Proposal to enforce "only one Statement executing per Connection"

### 2. Debug-only assertion
The check is wrapped in `#[cfg(debug_assertions)]`:
```rust
#[cfg(debug_assertions)]
{
    if let Some(overflow_cell) = page.overflow_cells.last() {
        turso_assert!(overflow_cell.index + 1 == cell_idx, "...");
    }
}
```
In release builds, the corruption would occur silently, potentially causing data corruption or crashes later.

### 3. No concurrent IVM tests
Looking at `core/incremental/view.rs` tests (50+ tests), ALL are single-threaded unit tests:
- `test_extract_single_table`
- `test_tables_from_union`
- `test_sql_for_populate_*`
- etc.

**No tests spawn multiple threads or tokio tasks to test concurrent IVM.**

### 4. Rare timing window
The bug requires:
- Two concurrent write transactions
- Both affecting the same materialized view
- Both causing the same B-tree page to overflow
- Specific timing where both are in `apply_view_deltas` simultaneously

### 5. Application-level serialization
Many applications serialize writes at the application level, preventing this race condition from manifesting.

### 6. `Rc<RefCell<...>>` in multi-threaded context
`AllViewsTxState` uses `Rc<RefCell<HashMap<...>>>` which is NOT thread-safe:
```rust
pub struct AllViewsTxState {
    states: Rc<RefCell<HashMap<String, Arc<ViewTransactionState>>>>,
}
```
This is then marked with `unsafe impl Send/Sync` - a clear sign that thread safety was deferred.

## Affected Code Paths

1. **Entry point**: `vdbe/mod.rs:1125` - `apply_view_deltas()`
2. **IVM insert**: `incremental/compiler.rs:636` - `commit()` which calls cursor inserts
3. **B-tree insert**: `storage/btree.rs:7406` - `_insert_into_cell()`
4. **Assertion**: `storage/btree.rs:7410`

## Investigation Questions

1. **Is the Pager shared between connections?**
   - If yes, are page buffers properly isolated?
   - The `PageContent` struct contains `overflow_cells: Vec<OverflowCell>` which appears to be per-page, not per-transaction.

2. **Is there a write lock during IVM?**
   - Looking at `apply_view_deltas`, it doesn't appear to hold any exclusive lock during the B-tree modifications.

3. **How does SQLite handle this?**
   - SQLite uses WAL (Write-Ahead Logging) with a single writer at a time.
   - The `SQLITE_BUSY` mechanism prevents concurrent writers.

4. **Transaction isolation during IVM**
   - When does IVM run? During commit?
   - Is the write transaction still active during IVM?
   - Can another transaction start IVM while one is in progress?

## Proposed Fixes

### Option 1: Serialize IVM at Connection Level (Minimal Change)

Add a mutex around the entire `apply_view_deltas` operation:

```rust
// In Connection or Pager
ivm_lock: Mutex<()>,

fn apply_view_deltas(...) {
    let _guard = self.ivm_lock.lock();
    // ... existing logic
}
```

**Pros**: Simple, targeted fix
**Cons**: Reduces concurrency for all IVM operations

### Option 2: Per-Page Locking During IVM

Lock individual pages during modification:

```rust
fn _insert_into_cell(page: &mut PageContent, ...) {
    let _guard = page.write_lock();
    // ... existing logic
}
```

**Pros**: Finer-grained locking, better concurrency
**Cons**: More complex, potential for deadlocks

### Option 3: Copy-on-Write Page Buffers for IVM

Each IVM operation works on a private copy of pages:

```rust
struct IvmTransaction {
    private_pages: HashMap<PageId, PageContent>,
}
```

**Pros**: Full isolation, no locking needed during operation
**Cons**: Memory overhead, merge complexity

### Option 4: Single Writer Enforcement (SQLite-style)

Enforce that only one write transaction can be active at a time:

```rust
fn begin_write_tx(&self) -> Result<WriteTransaction> {
    let _guard = self.write_lock.try_lock()
        .map_err(|_| Error::Busy)?;
    // ...
}
```

**Pros**: Matches SQLite semantics, simple to reason about
**Cons**: Limits concurrency

## Recommended Fix

**Option 1 (Serialize IVM)** is recommended as the immediate fix because:
1. It's the smallest change with lowest risk
2. IVM is already a heavy operation; serializing it won't significantly impact performance
3. It matches the expected semantics (IVM should see a consistent view of the database)

For a longer-term solution, **Option 4** should be considered to match SQLite's concurrency model.

## Test Case to Add

```rust
#[tokio::test]
async fn test_concurrent_ivm_inserts() {
    // Setup: Create a materialized view
    let db = setup_test_db().await;
    db.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, data TEXT)").await;
    db.execute("CREATE MATERIALIZED VIEW events_view AS SELECT * FROM events").await;

    // Spawn multiple concurrent insert tasks
    let handles: Vec<_> = (0..10).map(|i| {
        let db = db.clone();
        tokio::spawn(async move {
            for j in 0..100 {
                db.execute(&format!(
                    "INSERT INTO events (id, data) VALUES ({}, 'test')",
                    i * 100 + j
                )).await.unwrap();
            }
        })
    }).collect();

    // Wait for all to complete - should not panic
    for handle in handles {
        handle.await.unwrap();
    }

    // Verify data integrity
    let count: i64 = db.query_one("SELECT COUNT(*) FROM events_view").await;
    assert_eq!(count, 1000);
}
```

## Related Files

- `core/storage/btree.rs:7406-7415` - `_insert_into_cell()` with the assertion
- `core/vdbe/mod.rs:1125` - `apply_view_deltas()` implementation
- `core/incremental/compiler.rs:636` - `commit()` which calls IVM inserts
- `core/lib.rs:815` - `view_transaction_states` per-connection state

## References

- Holon bug report: Observed in Flutter app when CDC triggers concurrent INSERTs
- Log file: `/tmp/flutter.log` lines 90200-90450
- Panic location: `btree.rs:7410`
