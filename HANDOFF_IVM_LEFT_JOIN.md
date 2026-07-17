# Handoff: IVM LEFT JOIN in Materialized Views

## Problem

`CREATE MATERIALIZED VIEW` rejects `LEFT JOIN`, `RIGHT JOIN`, `FULL OUTER
JOIN`, and `CROSS JOIN` at view-creation time. From
`core/incremental/join_operator.rs:396-413`:

```rust
JoinType::Left  => return Err(...
    "LEFT OUTER JOIN is not yet supported in incremental views" ...),
JoinType::Right => return Err(...
    "RIGHT OUTER JOIN is not yet supported in incremental views" ...),
JoinType::Full  => return Err(...
    "FULL OUTER JOIN is not yet supported in incremental views" ...),
JoinType::Cross => return Err(...
    "CROSS JOIN is not yet supported in incremental views" ...),
JoinType::Inner => {}  // supported
```

Inner JOIN works (bilinear in Z-set algebra). The outer joins are an
engineering gap, **not** a DBSP/theoretical one — Materialize, Feldera,
and differential-dataflow all ship LEFT JOIN in production DBSP-family
systems.

## Why this matters

This is the second half of holon's `block`-as-matview unblock. The first
half — array aggregation in IVM matviews — landed via
`HANDOFF_IVM_ARRAY_STRING_AGGREGATION.md`. Holon's target architecture
is:

```sql
CREATE MATERIALIZED VIEW block AS
  SELECT b.*,
         json_group_array(t.tag) AS tags,
         json_group_array(d.blocker_id) AS blocked_by
  FROM block_raw b
  LEFT JOIN block_tags t ON t.block_id = b.id
  LEFT JOIN task_blockers d ON d.task_id = b.id
  GROUP BY b.id;
```

A block with zero tags must still appear in the output (with `[]` or
`[null]` depending on `FILTER` semantics). Without LEFT JOIN, the
matview drops blocks that have no junction-table rows — which is the
exact failure mode the spike was trying to fix.

## DBSP theory (no fundamental obstacle)

LEFT JOIN is **inner-join ⊎ null-padded antijoin**:

```
L ⟕ R  =  (L ⋈ R)  ⊎  { (l, NULL_R) : l ∈ L ∧ ¬∃r ∈ R. matches(l, r) }
```

Inner join is bilinear:
`(L+δL) ⋈ (R+δR) = L⋈R + δL⋈R + L⋈δR + δL⋈δR`

The "no matching r" condition (antijoin) is **not** bilinear — a single
right-side change can flip a left row between matched/unmatched. DBSP
handles this with a per-left-row **match count** kept in side state,
plus a "null-pad" operator that emits `(l, NULL)` exactly when count
crosses 0:

| Event                              | Side-state action       | Output delta                    |
| ---------------------------------- | ----------------------- | ------------------------------- |
| Insert `(l, r)` matching           | count[l] += 1           | emit `(l, r)`; if count flipped 0→1, retract `(l, NULL)` |
| Delete `(l, r)`                    | count[l] -= 1           | retract `(l, r)`; if count flipped 1→0, emit `(l, NULL)` |
| Insert l with no current match     | count[l] = 0            | emit `(l, NULL)`                |
| Delete l                           | count[l] removed        | retract whatever was emitted for l |

The shape is **structurally identical** to Turso's existing MIN/MAX
operator (per-key weighted index, retraction-on-zero-crossing) — see
`core/incremental/aggregate_operator.rs` for the pattern (`MinMaxDeltas`
type at line 337, `RecomputeMinMax` state machine at line 2013).

Reference: Budiu et al., *DBSP: Automatic Incremental View Maintenance
for Rich Query Languages*, VLDB 2023 — §5 covers outer joins formally.

## Proposed implementation

### Operator graph

For a query `L LEFT JOIN R ON cond GROUP BY ...`, build:

```
                        ┌──────────────────┐
   L (delta) ──────────►│   InnerJoin      │──── matched rows ──┐
                        │   L ⋈ R          │                    │
   R (delta) ──────────►│                  │                    │
                        └──────────────────┘                    ▼
                                                         ┌─────────────┐
                                                         │             │
                        ┌──────────────────┐             │   UNION     │── output
   L (delta) ──────────►│  MatchCounter    │── unmatched │   ALL       │
                        │  per-l count of  │── (l, NULL) │             │
   R (delta) ──────────►│  matching r rows │── rows      │             │
                        └──────────────────┘             └─────────────┘
```

The **MatchCounter** is the new operator. It maintains:

```rust
// Per left-row hash, the number of matching right-rows.
// Stored in btree using a NEW storage type code (e.g. AGG_TYPE_MATCH_COUNT = 0b11).
// Key: (operator_id, l_join_key_hash, l_row_hash) → match_count: i64
```

When the count for a given `l_row_hash` transitions:
- `0 → positive`: emit retract `(l_values, NULL_padding)` to output delta
- `positive → 0`: emit insert `(l_values, NULL_padding)` to output delta

### State storage

Reuse the existing 2-bit storage-type encoding in
`aggregate_operator.rs:74-77`:

```rust
pub const AGG_TYPE_REGULAR: u8 = 0b00;
pub const AGG_TYPE_MINMAX:  u8 = 0b01;
pub const AGG_TYPE_DISTINCT:u8 = 0b10;
// 0b11 is currently unused — claim it for join match counts.
```

The btree key shape: `(storage_id, l_join_key_hash, l_row_hash) →
{l_values_blob, count}`. Same `Hash128` machinery used by
`generate_storage_id` / `generate_group_hash` — see
`aggregate_operator.rs` and `dbsp.rs`.

`l_join_key_hash` lets the operator find "all left rows with this join
key" efficiently when a right-side delta arrives. Without it, every
right-side delta would scan all left-row counts.

### Where LEFT JOIN gets compiled

The current rejection is in `core/incremental/join_operator.rs:396`. The
compiler entry point for joins is in `core/incremental/compiler.rs`
under the `LogicalPlan::Join` arm at ~line 2348 (`Inner` join is
constructed there today). New work:

1. Map `LogicalJoinType::Left` to a new `JoinType::Left` variant in the
   IVM `JoinType` enum (currently in `operator.rs`, imported by
   `compiler.rs:14`).
2. In `JoinOperator::new` or a new constructor, allocate a side-state
   storage_id for the match counter.
3. Implement the new `MatchCounterOperator` as a sibling to the existing
   `JoinOperator`, with its own `IncrementalOperator` impl.
4. In the compiler, emit the operator graph above (or fold the
   match-count tracking into `JoinOperator` itself behind a
   `join_type: Left` switch — pick whichever is cleaner).
5. Output column schema for the LEFT JOIN: left columns + nullable
   right columns. The schema-construction lives in `view.rs` /
   `extract_view_columns` — verify how nullability is currently
   represented when wiring the new arm.

### Output-row construction

Two outputs interleave via UNION ALL:
- **Matched output**: `l_columns ++ r_columns` for each `(l,r)` join match. Inner-join output, no change from today.
- **Null-padded output**: `l_columns ++ [NULL; r_columns.len()]` for left rows whose match count is 0.

The number of NULLs needed is the right side's column-count, available
from the right input's `LogicalSchema` at compile time — store it on the
operator.

### Antijoin reuse

Once the match-counter mechanism exists, **antijoin** (`L WHERE NOT
EXISTS (...)`) and **`NOT IN` subquery** become natural follow-ups —
they're the same primitive minus the null-padded-output step. Worth
designing the match-counter as a general operator, not LEFT-JOIN-only.

## Tests to add

Pattern after the array-aggregation handoff:
`testing/runner/tests/ivm-array-aggregation.sqltest`. New file:
`testing/runner/tests/ivm-left-join.sqltest`. Header:

```
@database :memory:
@skip-file-if mvcc "materialized views not supported in MVCC mode"
@requires materialized_views "requires materialized view support"
```

Test cases (each as `test name { ... } expect { ... }`):

### Core LEFT JOIN cases

1. **`left-join-create-empty-junction`** — parent rows, junction empty.
   Each parent appears with NULL in junction columns.
2. **`left-join-create-mixed`** — some parents matched, some not.
3. **`left-join-incremental-add-match`** — INSERT a junction row that
   creates a first match for a parent. The parent's NULL row should
   retract; matched row should appear.
4. **`left-join-incremental-remove-last-match`** — DELETE the only
   junction row for a parent. Matched row retracts; NULL-padded row
   reappears.
5. **`left-join-incremental-second-match`** — INSERT a second junction
   row for an already-matched parent. Output: two matched rows, no
   NULL row.
6. **`left-join-multiple-matches-then-delete-one`** — DELETE one of N
   matches. Match count stays positive; no NULL row appears.
7. **`left-join-parent-delete-cascade`** — DELETE a parent row that has
   matches. All matched rows retract.
8. **`left-join-parent-delete-no-matches`** — DELETE a parent that had
   no matches. The NULL-padded row retracts.
9. **`left-join-update-pivots-match`** — UPDATE the join-key column on
   a junction row, moving its match from parent A to parent B.
   Should retract A's matched row, possibly emit A's NULL row, and
   possibly retract B's NULL row, emit B's matched row.
10. **`left-join-batched-tx`** — multiple INSERTs in BEGIN/COMMIT
    converge to the right end-state. Regression guard against
    consolidation bugs (see `MEMORY.md` IVM Recursive CTE Negative
    Weight Bug for the class of issue).

### Composition with array aggregation (the holon use case)

11. **`left-join-with-json-group-array-empty-junction`** — parent +
    empty junction + `json_group_array(j.tag)` GROUP BY parent.
    Expect: each parent appears with `[null]`. (Or `[]` if `FILTER
    (WHERE tag IS NOT NULL)` is in scope — see follow-ups below.)
12. **`left-join-with-json-group-array-mixed`** — same but with some
    matches.
13. **`left-join-with-json-group-array-add-tag`** — add a tag for a
    previously-tagless parent. Array changes from `[null]` to `[tag]`.
14. **`left-join-with-json-group-array-remove-last-tag`** — opposite:
    array changes from `[tag]` to `[null]`.
15. **`left-join-with-group-concat-skips-nulls`** — GROUP_CONCAT with
    LEFT JOIN over an empty junction. Since GROUP_CONCAT skips NULLs
    (per the array-agg work), the output should be NULL for tagless
    parents.

### Cross-session restore

In a Rust integration test
`tests/integration/query_processing/test_ivm_left_join.rs`:

16. Populate a LEFT JOIN matview, close DB, reopen, verify match
    counts survived the restore. INSERT a junction row, verify the
    NULL-pad row retracts. DELETE it, verify it reappears.

### Edge cases

17. **`right-join-still-rejected`** — locked-down sentinel until RIGHT
    JOIN ships:
    ```
    expect error { RIGHT OUTER JOIN is not yet supported }
    ```
18. **`full-join-still-rejected`** — same for FULL JOIN.
19. **`cross-join-still-rejected`** — same for CROSS JOIN.

(Drop these sentinels as those joins land. Without them, the rejection
path silently regressing isn't caught by anything.)

## Reused primitives (do not re-implement)

| Need | Reuse |
|------|-------|
| Per-key incremental state w/ retraction | Pattern in `core/incremental/aggregate_operator.rs` MIN/MAX (`MinMaxDeltas`, `RecomputeMinMax`) |
| 2-bit storage-type encoding | `aggregate_operator.rs:74-77` — claim `0b11` |
| 128-bit content hashing for keys | `Hash128` (used throughout `aggregate_operator.rs`) |
| Btree-backed cursor/seek/insert | `DbspStateCursors`, `WriteRow`, `ReadRecord` in `persistence.rs` |
| Async-I/O state machine pattern | `EvalState`, `return_if_io!`, `return_and_restore_if_io!` |
| Cross-session state restore | Pattern in `MEMORY.md` "IVM Cross-Session Negative Weight Bug" — `restore_recursive_operators_if_needed` |
| Topological ordering of dependent matviews | `apply_view_deltas` in `core/vdbe/mod.rs`, see `MEMORY.md` "IVM Matview-on-Matview Propagation Bug" |

## Hypotheses to validate (cheap, do during impl)

| #   | Hypothesis                                                                                                    | How to validate                                                                                          |
| --- | ------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| H1  | Inner join's `JoinOperator::eval` already returns deltas in a shape that can be UNION ALL'd with a sibling op | Read `JoinOperator::eval` and `commit` in `join_operator.rs` end-to-end                                  |
| H2  | The schema layer can express "left columns + nullable right columns" without a NULL-affinity hack             | Read `extract_view_columns` in `core/util.rs` and `ViewColumnSchema` in `view.rs`                        |
| H3  | The compiler's `LogicalPlan::Join` arm has access to both children's column counts at the right point         | Read `compile` in `compiler.rs` around the `Join` arm (~L2348)                                           |
| H4  | The LEFT JOIN match-counter doesn't suffer from the same pre-projection lifting that the array-agg work hit   | The pre-projection in `logical.rs:1567+` was for aggregate args. JOIN ON conditions are on a different code path — verify |
| H5  | `JoinType` in `core/incremental/operator.rs` already has a `Left` variant the compiler refuses to use, OR has only `Inner` | Read `enum JoinType` in `operator.rs`                                                                    |

## Known bug classes to watch for

The IVM codebase has a documented history of bug patterns this work
will hit. From `MEMORY.md`:

1. **State machine I/O re-entry**: every state must have at most ONE
   `return_if_io!` that yields. Multi-yield states cause duplicate
   side effects (see "IVM State Index Duplicate Entry Bug",
   "IVM PageStack Crash — Temp Cursors Recreated on I/O Re-entry").
   The match-counter operator's btree update path must follow this.
2. **Delta consolidation ordering**: deletes (weight<0) must process
   before inserts (weight>0) per rowid (see "IVM Matview UPDATE Not
   Propagated — CDC Zero Changes"). For the match counter, an UPDATE
   pivoting a join key produces both delete and insert for the same
   logical right row — they must be processed in delete-then-insert
   order, otherwise the count goes 1→0→1 and a spurious NULL-pad
   toggle appears.
3. **Cross-session state restore**: in-memory state is lost on DB
   reopen. The match-counter's per-l-row counts must be reconstructable
   from btree contents on first `execute()`/`commit()` after reopen.
   Pattern: `restore_*_if_needed()` at first call. Without this, the
   match counter starts at 0 after reopen and every left row briefly
   appears as unmatched, then transitions back. Even worse: if the
   restore is partial, NULL-pad rows duplicate.
4. **Matview-on-matview propagation**: if a downstream matview reads
   from a LEFT-JOIN matview, the output deltas must thread through the
   same `apply_view_deltas` topo-sort path that the array-agg work
   exercised. See `MEMORY.md` "IVM Matview-on-Matview Propagation Bug"
   for the existing fix; verify LEFT JOIN's output deltas are
   structurally compatible.
5. **Page-cache pinning**: `MEMORY.md` "IVM Page Cache Eviction Bug"
   describes a class of crashes from clearing pinned pages during
   IVM cascades. Stress-test the LEFT JOIN operator under rapid inserts
   + chained matview + CDC (the exact stress shape that surfaced the
   original page-cache bug).
6. **`extract_view_columns` and `*` expansion**: per `MEMORY.md`
   "Key Locations", `core/util.rs:extract_view_columns()` "must resolve
   `*` against schema for CTEs". For LEFT JOIN, `SELECT *` should
   include nullable right-side columns — verify this resolves correctly.

## Out of scope (separate handoffs)

- **`FILTER (WHERE ...)` clause** for aggregates. Holon will likely want
  `json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL)` to get `[]`
  instead of `[null]` for tagless parents. This is a separate IVM gap;
  check whether it's supported today.
- **RIGHT JOIN, FULL JOIN, CROSS JOIN.** Same operator-graph shape. RIGHT
  JOIN ≡ swap inputs + LEFT JOIN. FULL JOIN ≡ LEFT JOIN ⊎ RIGHT-side
  null-padded antijoin. CROSS JOIN ≡ no-condition inner join (different
  shape — every l × every r — possibly easier).
- **Antijoin / `NOT EXISTS` / `NOT IN`.** Same match-counter primitive,
  no null-padding. Should fall out of the LEFT JOIN work cheaply if the
  match-counter is built as a general operator.
- **Output ordering.** LEFT JOIN preserves no particular order. Like the
  array-agg work, IVM matviews are content-sorted, not insertion-sorted.
  Document this divergence from regular `VIEW` behavior.
- **Optimizer rules for moving predicates through LEFT JOIN.** Standard
  SQL optimizer territory; not blocking the IVM gap.

## Cross-repo coordination

- Holon-side: this is the **second blocker** for the
  `block`-as-matview architecture (their task #3). Once both this and
  `HANDOFF_IVM_ARRAY_STRING_AGGREGATION.md` ship, the architecture
  unblocks fully. The interim production fix (their task #5,
  CacheBlockReader hydration via correlated-subquery view) can be
  deleted at that point.
- Holon's PBT round-trip (their task #2) is the integration test for
  this work landing successfully — once Block instances round-trip
  correctly through a LEFT-JOIN-aggregated matview, the unblock is
  real.

## File pointers (already-read in the array-agg session)

- IVM JOIN rejection: `core/incremental/join_operator.rs:388-413`
- IVM operator graph types: `core/incremental/operator.rs` (`JoinType`,
  `IncrementalOperator` trait, `EvalState`, `DbspStateCursors`)
- IVM compiler dispatch for joins: `core/incremental/compiler.rs`
  `LogicalPlan::Join` arm at ~L2348
- View / matview schema construction: `core/incremental/view.rs:388+`
  (`ReferencedTable`, `populate_state`)
- Per-key state machine reference (MIN/MAX retraction):
  `core/incremental/aggregate_operator.rs:1676` (`extract_min_max_deltas`),
  `:2013-2227` (`RecomputeMinMax`)
- Persistence patterns: `core/incremental/persistence.rs` (`ReadRecord`,
  `WriteRow`)
- `Value::Ord` for hashable keys: `core/types.rs:783`

## Suggested implementation order

1. Read `core/incremental/operator.rs` end-to-end. Understand
   `JoinOperator`, `JoinType`, `IncrementalOperator` trait, `EvalState`.
2. Read `core/incremental/join_operator.rs` end-to-end. Understand how
   inner join's eval/commit is structured. Reproduce mentally:
   `(L+δL) ⋈ (R+δR) = L⋈R + δL⋈R + L⋈δR + δL⋈δR`.
3. Read `core/incremental/compiler.rs:LogicalPlan::Join` arm.
4. Write the failing tests in `testing/runner/tests/ivm-left-join.sqltest`
   (start with cases #1, #3, #4, #11). They should all fail with the
   existing "LEFT OUTER JOIN is not yet supported" error.
5. Implement the match-counter operator + storage type. Smallest
   possible surface: a new file
   `core/incremental/left_join_operator.rs` or extend
   `join_operator.rs`. Use the MIN/MAX state machine as the reference
   pattern.
6. Wire it into the compiler. Map `LogicalJoinType::Left` to the new
   path. Output column schema construction.
7. Run sqltests. Iterate.
8. Add cross-session restore. Run integration test. Iterate.
9. Add the array-agg-composition tests (#11-#15). These are the
   actual holon-unblock tests.
10. Document the engine divergence (lex-sorted output, no insertion
    order) in the matview README.

## Estimated scope

This is a meaningful but bounded change — comparable to the
array-aggregation work or the recursive-CTE-state-restore work, both of
which were ~1-2 day implementations once the test surface was locked
down. The DBSP theory is well-understood; the codebase has the
primitives. The risk is in the I/O-re-entry / state-machine interaction
classes documented in `MEMORY.md`, not in any novel research.
