# B-tree Overflow Cell Index Bug - Fix Options

## Problem Summary

The B-tree panics during bulk inserts due to confusion between **logical indices** (includes overflow cells) and **physical indices** (only cells on page). The original code assumed at most 1 overflow cell per page; this invariant was loosened but the index math wasn't fully updated.

**Panic location**: `sqlite3_ondisk.rs:705` - `cell_get: idx out of bounds`

**Root cause**: Code uses `saturating_sub(overflow_cells.len())` which subtracts ALL overflow cells, but should only subtract those with index < target.

---

## Option A: Restore Original Invariant (Surgical Fix)

**Philosophy**: The original design was correct - enforce at most 1 overflow cell per page. Simpler math, bounded resource usage, proven design.

### Why This Works

From original code comments:
- "We only need maximum 5 pages to balance 3 pages, because we can guarantee that cells from 3 pages will fit in 5 pages"
- Balance is triggered immediately when overflow occurs (line 2386-2388)
- With bounded pages, divider cell insertion shouldn't cascade overflow

### Tasks

#### A1. Revert MAX_NEW_SIBLING_PAGES_AFTER_BALANCE
**File**: `core/storage/btree.rs:118`
**Current**: `pub const MAX_NEW_SIBLING_PAGES_AFTER_BALANCE: usize = 16;`
**Change to**: `pub const MAX_NEW_SIBLING_PAGES_AFTER_BALANCE: usize = 5;`

Also revert any Vec usage back to bounded arrays if they were changed.

#### A2. Restore Overflow Cell Assertions
**File**: `core/storage/btree.rs`

Add back assertions that were removed (visible in `jj diff --from mlyusxwl`):

```rust
// Around line 2768 (after getting right_pointer)
if matches!(page_type, PageType::IndexInterior) {
    turso_assert!(parent_contents.overflow_cells.len() <= 1,
        "index interior page must have no more than 1 overflow cell");
}

// Around line 2835 (in the sibling loop)
turso_assert!(parent_contents.overflow_cells.len() <= 1,
    "must have at most 1 overflow cell in the parent");
```

#### A3. Revert saturating_sub Back to Direct Subtraction
**File**: `core/storage/btree.rs`

Locations to check (search for `saturating_sub.*overflow_cells`):
- Line 2761-2763: `last_sibling_is_right_pointer` calculation
- Line 2835-2837: `actual_cell_idx` in sibling loop
- Line 2941-2943: Another `actual_cell_idx` calculation
- Line 3050-3052: Cell drop loop

Change from:
```rust
let actual_cell_idx = (first_cell_divider + sibling_pointer)
    .saturating_sub(parent_contents.overflow_cells.len());
```

Back to:
```rust
let actual_cell_idx = first_cell_divider + sibling_pointer
    - parent_contents.overflow_cells.len();
```

#### A4. Remove My Recent Fixes
**File**: `core/storage/btree.rs`

Remove the positional filtering fix at lines 4194-4200:
```rust
// Remove this:
let overflow_cells_before = parent_contents
    .overflow_cells
    .iter()
    .filter(|oc| oc.index < cell_divider_idx)
    .count();
```

The simpler `- overflow_cells.len()` will work correctly with the invariant restored.

#### A5. Investigate Why Invariant Was Broken
Check git/jj history to understand what scenario required more than 1 overflow cell or more than 5 sibling pages. There may be an edge case that needs different handling.

Search for related issues:
```bash
jj log --revisions 'ancestors(@, 50)' -T 'description' | grep -i overflow
```

### Validation

```bash
cargo test --package limbo_core btree 2>&1 | tee test_option_a.log
RUST_LOG=debug cargo test btree_fuzz 2>&1 | tee fuzz_option_a.log
```

---

## Option B: Properly Handle Multiple Overflow Cells

**Philosophy**: Accept that multiple overflow cells can exist and fix all index calculations to handle this correctly.

### Why This Is Harder

- Must audit every place overflow cells affect index calculations
- The math is more complex: must count only overflow cells with index < target
- Easy to miss locations, leading to subtle bugs
- More runtime overhead (filtering/counting)

### Tasks

#### B1. Audit All Overflow Cell Index Calculations

Search for all locations:
```bash
grep -n "overflow_cells" core/storage/btree.rs | grep -v "^[0-9]*:\s*//"
```

Key patterns to find and fix:
- `saturating_sub(.*overflow_cells.len())`
- `- overflow_cells.len()`
- `+ overflow_cells.len()`
- `cell_count() + overflow_cells.len()` (logical count - usually correct)
- `== overflow_cells.len()`

#### B2. Create Helper Function

Add a helper to consistently calculate physical index:

```rust
impl PageContent {
    /// Convert a logical cell index to physical index by subtracting
    /// overflow cells that appear before the target index.
    fn logical_to_physical_idx(&self, logical_idx: usize) -> usize {
        let overflow_before = self.overflow_cells
            .iter()
            .filter(|oc| oc.index < logical_idx)
            .count();
        logical_idx - overflow_before
    }
}
```

#### B3. Fix Known Locations

**Already Fixed** (keep these):
- Lines 4194-4200: `validate_cells_after_balance` positional filter
- Lines 4154-4160: Rightmost pointer logical count check
- Lines 2984-3008: Overflow cell clearing after divider processing

**Need to Fix**:

1. **Line 2761-2763** - `last_sibling_is_right_pointer`:
   ```rust
   // Current (wrong for multiple overflow cells):
   let last_sibling_is_right_pointer = (sibling_pointer + first_cell_divider)
       .saturating_sub(parent_contents.overflow_cells.len())
       == parent_contents.cell_count();

   // Fix: Use positional calculation
   let logical_idx = sibling_pointer + first_cell_divider;
   let physical_idx = parent_contents.logical_to_physical_idx(logical_idx);
   let last_sibling_is_right_pointer = physical_idx == parent_contents.cell_count();
   ```

2. **Line 2835-2837** - `actual_cell_idx` in get right pointer:
   ```rust
   // Current (wrong):
   let actual_cell_idx = (first_cell_divider + sibling_pointer)
       .saturating_sub(parent_contents.overflow_cells.len());

   // Fix:
   let logical_idx = first_cell_divider + sibling_pointer;
   let actual_cell_idx = parent_contents.logical_to_physical_idx(logical_idx);
   ```

3. **Line 2941-2943** - Similar pattern in another branch

4. **Line 3050-3052** - Cell drop loop:
   ```rust
   // Current (wrong):
   let actual_cell_idx = cell_idx.saturating_sub(parent_contents.overflow_cells.len());

   // Fix:
   let actual_cell_idx = parent_contents.logical_to_physical_idx(cell_idx);
   ```

5. **Line 3098** - Another cell drop location

#### B4. Handle Overflow Cell Ordering

Ensure overflow cells are always sorted by index, or handle unsorted case:

```rust
// In _insert_into_cell when pushing overflow cell:
page.overflow_cells.push(OverflowCell { index: cell_idx, payload: ... });
// Consider: page.overflow_cells.sort_by_key(|oc| oc.index);
```

#### B5. Add Debug Assertions

Add assertions to catch any remaining issues:

```rust
#[cfg(debug_assertions)]
fn validate_physical_idx(page: &PageContent, physical_idx: usize, context: &str) {
    assert!(
        physical_idx < page.cell_count(),
        "{}: physical_idx {} >= cell_count {} (overflow_cells={})",
        context, physical_idx, page.cell_count(), page.overflow_cells.len()
    );
}
```

### Validation

```bash
cargo test --package limbo_core btree 2>&1 | tee test_option_b.log
RUST_LOG=debug cargo test btree_fuzz 2>&1 | tee fuzz_option_b.log
```

---

## Recommendation

**Start with Option A** (restore invariant). It's:
- Simpler to implement
- Matches original proven design
- Less risk of missing edge cases
- Better bounded resource usage

If Option A fails because there's a legitimate need for multiple overflow cells, then pursue Option B with the helper function approach.

---

## Current State

**Fixes already applied**:
1. Positional overflow cell counting in `validate_cells_after_balance` (lines 4194-4200)
2. Overflow cell clearing after divider loop (lines 2984-3008)
3. Logical cell count for rightmost pointer check (lines 4154-4160)

**Test results**:
- Integration tests: 19/19 pass
- Fuzz test: Fails at 701/2000 ops on second cycle

**To reproduce failure**:
```bash
RUST_LOG=debug cargo test btree_fuzz 2>&1 | tee fuzz.log
grep -A5 "FAILED\|panic\|assertion" fuzz.log
```
