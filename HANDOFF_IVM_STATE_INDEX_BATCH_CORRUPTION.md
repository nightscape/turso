# IVM State Index Corruption Under Batch Transactions

## Status: FIXED + Two Unrelated Bugs Found

The DBSP state index corruption is fixed. The fix also exposed two pre-existing bugs:
1. **Page cache eviction panic** (intermittent, seed 100) — pre-existing, documented in MEMORY.md
2. **Base table IdxDelete failure** (deterministic, seed 500) — separate bug, needs own investigation

## What Was Fixed

All `WriteRow` states in `core/incremental/persistence.rs` that had two `return_if_io!` calls (seek + mutate) were vulnerable to I/O re-entrancy: if the mutate yielded, re-entry re-executed the seek AND mutate, corrupting the index.

**Fix**: Added a `sought: bool` flag to each vulnerable state. The seek sets `sought = true` and returns to the same state. On re-entry with `sought = true`, only the mutate executes. Each loop iteration has at most one `return_if_io!`.

**States fixed**: `Delete`, `DeleteIndex`, `InsertNew`, `InsertIndex`, `UpdateExisting` — all five states that had seek+mutate pairs.

## Remaining Bug 1: Page Cache Eviction Panic (seed 100, intermittent)

```
[PageStack::current] current_page=-1 is negative! stack_depth=0, loaded_pages=[]
```

This is the pre-existing page cache eviction bug documented in MEMORY.md. It happens when `PageCache::clear()` invalidates pinned pages during IVM cascade. The previous fix (preserving pinned pages in `clear()`) apparently doesn't cover all cases under batch transaction load.

**Reproducer**: seed 100 with `--batch-probability 0.3` — fails ~10-15% of runs.

```bash
for i in $(seq 1 20); do
  cargo run -q --bin differential_fuzzer -- \
    --matview -g sql-gen-prop -n 200 --batch-probability 0.3 --seed 100 2>&1 \
    | grep -E "PASSED|FAILED|PageStack"
done
```

## Remaining Bug 2: Base Table IdxDelete Failure (seed 500, deterministic)

```
Corrupt database: IdxDelete: no matching index entry found for key [...]
```

This is NOT a DBSP state table issue — it's the base table's own index (`adaptable_halbrook`) becoming inconsistent. A DELETE on the base table fails because the row exists in the table but its index entry is missing.

**Reproducer**: seed 500 with `--batch-probability 0.3` — fails 100% of runs. Always at statement 111 (`DELETE FROM adaptable_halbrook`).

```bash
cargo run -q --bin differential_fuzzer -- \
  --matview -g sql-gen-prop -n 200 --batch-probability 0.3 --seed 500
```

**Hypothesis**: Batch transactions with matviews trigger IVM processing at commit time. The IVM processing modifies btree cursors/pages as a side effect, which invalidates the base table's index cursor state. When a subsequent DELETE tries to clean up the index entry, the cursor is stale.

## Fuzzer Enhancements (Applied This Session)

Three orthogonal enhancements enabled discovery of these bugs:

1. **Batch transactions** (`--batch-probability <float>`): Wraps 2-10 DML statements in `BEGIN TRANSACTION` / `COMMIT`. Added to `runner.rs`.
2. **Continuous matview verification**: After every DML, compares all matviews between Turso and SQLite. Always on when matviews exist. Added `verify_matviews_match()` to `runner.rs`.
3. **Outer JOIN modifier on RecursiveCte**: The `RecursiveCte` view kind now independently (50% chance) wraps its final SELECT in an external JOIN. Modified in `view.rs`.
4. **`matview_heavy()` profile**: Write-heavy preset with frequent matview creation. Added to `profile.rs`.

## Key Files

| File | What |
|------|------|
| `core/incremental/persistence.rs` | `WriteRow` state machine — `sought` flag fix applied |
| `testing/differential-oracle/fuzzer/runner.rs` | Batch transaction + matview verification (new) |
| `testing/differential-oracle/sql_gen_prop/view.rs` | RecursiveCte outer JOIN modifier (new) |
| `testing/differential-oracle/sql_gen_prop/profile.rs` | `matview_heavy()` profile (new) |
