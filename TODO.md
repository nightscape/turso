# Simulator Test Findings - 2026-01-09

## Summary

Differential simulator tests discovered multiple bugs across different categories. All bugs involve fault injection (REOPEN_DATABASE, DISCONNECT) and many involve CDC (Change Data Capture).

---

## Bug Categories

### 1. Results Mismatch ~~(HIGH PRIORITY)~~ **FIXED**

~~Turso returns different data than SQLite for identical queries.~~

| Seed | Shrunk Size | Status |
|------|-------------|--------|
| 1 | 220 statements | **FIXED** |
| 20 | 41 statements | **FIXED** |
| 30 | 29 statements | **FIXED** |

#### Root Cause (Identified 2026-01-09)

The `NotifyCdcChange` instruction in `core/vdbe/execute.rs` was returning `InsnFunctionStepResult::Done` instead of `InsnFunctionStepResult::Step`. This caused the VDBE to halt execution immediately after the first row was updated when CDC was enabled, instead of continuing to the `Next` instruction to process remaining rows.

#### Fix Applied

Modified `op_notify_cdc_change()` in `core/vdbe/execute.rs` to return `Step` (and increment `state.pc`) instead of `Done`:

```rust
pub fn op_notify_cdc_change(...) -> Result<InsnFunctionStepResult> {
    // ... notification logic ...

    state.pc += 1;
    Ok(InsnFunctionStepResult::Step)  // Was: Ok(InsnFunctionStepResult::Done)
}
```

All early return paths were also fixed to use `Step` instead of `Done`.

#### Verification

```bash
# All three seeds now pass
cargo run -p limbo_sim --release -- -l 1 --differential
cargo run -p limbo_sim --release -- -l 20 --differential
cargo run -p limbo_sim --release -- -l 30 --differential
```

---

### 2. Busy Errors (MEDIUM PRIORITY)

Transaction locking/concurrency issues with multiple connections.

| Seed | Shrunk Size | Bugbase Path |
|------|-------------|--------------|
| 2 | N/A (shrink failed) | `.bugbase/2/` |
| 3 | 37 statements | `.bugbase/3/` |
| 4 | N/A (shrink failed) | `.bugbase/4/` |

#### Pattern from Seed 3 Reproducer

```sql
-- Multiple connections starting transactions with faults
-- FAULT 'REOPEN_DATABASE'; -- 7
BEGIN IMMEDIATE; -- 4
-- FAULT 'REOPEN_DATABASE'; -- 4
BEGIN IMMEDIATE; -- 7
-- FAULT 'DISCONNECT'; -- 7
BEGIN IMMEDIATE; -- 7
COMMIT; -- 7
BEGIN IMMEDIATE; -- 5
-- FAULT 'REOPEN_DATABASE'; -- 5
-- ... more interleaved transactions across connections 0-9
```

#### Analysis Hints

- Connections 0-9 are used, each with `BEGIN IMMEDIATE` transactions
- Faults (REOPEN_DATABASE, DISCONNECT) cause connections to drop mid-transaction
- The "Busy" error suggests lock contention isn't being resolved properly after faults
- WAL mode locking may not be cleaning up properly when connections are forcibly closed

#### Investigation Steps

1. Check WAL lock cleanup in `core/storage/wal.rs` when connection drops
2. Verify `BEGIN IMMEDIATE` properly acquires write lock
3. Check if disconnected connections release their locks
4. Look at `PRAGMA busy_timeout` handling

---

### 3. Integrity Check Failures ~~(HIGH PRIORITY)~~ **FIXED**

~~Storage corruption - pages marked as "never used" but should be in use.~~

| Seed | Error | Status |
|------|-------|--------|
| 5 | "Page 9: never used" | **FIXED** |
| 10 | "Page 13: never used" | **FIXED** |

#### Root Cause (Identified 2026-01-09)

The integrity check ran during uncommitted write transactions, seeing an inflated `database_size` that included pages allocated for uncommitted DDL (e.g., CREATE TABLE) without the corresponding schema updates. The integrity check then flagged these pages as "never used" because they weren't referenced by any B-tree roots in the stale schema.

**The actual database was never corrupted** - SQLite's integrity_check passed on the persisted files. This was a false positive from the simulator's integrity check timing.

#### Fix Applied

Modified `limbo_integrity_check()` in `simulator/runner/execution.rs` to skip integrity checks when the connection is in a write transaction:

```rust
fn limbo_integrity_check(conn: &Arc<Connection>) -> Result<()> {
    // Skip integrity check if connection is in a write transaction.
    // The integrity check would see uncommitted page allocations (inflated db_size)
    // but the schema doesn't include uncommitted tables yet, causing false positives.
    if conn.is_in_write_transaction() {
        tracing::debug!("Skipping integrity check: connection is in write transaction");
        return Ok(());
    }
    // ... rest of function
}
```

Also added `is_in_write_transaction()` public method to `Connection` in `core/lib.rs`.

#### Verification

```bash
# Both seeds now pass integrity check (fail with different "Busy" error, see section 2)
cargo run -p limbo_sim --release -- -l 5 --differential
cargo run -p limbo_sim --release -- -l 10 --differential
```

---

## Existing Bugbase

The `.bugbase/` directory contains **60+ entries** from prior testing. Many appear to be related to:

- Matview (materialized view) operations
- CDC (Change Data Capture)
- Concurrency/locking

Run `cargo run -p limbo_sim --release -- list` to see all tracked bugs.

---

## Commands Reference

```bash
# Run differential tests with a specific seed
cargo run -p limbo_sim --release -- -s SEED --differential -n 300

# Reload and verify a bug
cargo run -p limbo_sim --release -- -l SEED --differential

# Run with specific profile
cargo run -p limbo_sim --release -- --profile matview_cdc -s SEED

# List all bugs
cargo run -p limbo_sim --release -- list

# Keep database files for inspection
cargo run -p limbo_sim --release -- -s SEED --differential --keep-files
```

---

## Priority Order

1. ~~**Integrity Check Failures** (Seeds 5, 10)~~ - **FIXED** (was false positive, not actual corruption)
2. ~~**Results Mismatch** (Seeds 1, 20, 30)~~ - **FIXED** (NotifyCdcChange returned Done instead of Step)
3. **Busy Errors** (Seeds 2, 3, 4, 5, 10) - Concurrency issues (seeds 5, 10 now show this after integrity fix)
