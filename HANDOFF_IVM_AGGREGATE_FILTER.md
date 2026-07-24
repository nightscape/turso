# Handoff: IVM `FILTER (WHERE ...)` for aggregates

## Problem

`CREATE MATERIALIZED VIEW` silently drops the SQL-standard `FILTER
(WHERE ...)` clause on aggregate calls. From
`core/translate/logical.rs:2200-2239`:

```rust
ast::Expr::FunctionCall {
    name, distinctness, args, filter_over, ..  // <-- filter_over IGNORED
} => {
    if filter_over.over_clause.is_some() { ... }
    // ...builds AggregateFunction without consulting filter_over.filter_clause
}
```

The aggregate function builds without the filter, so

```sql
json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL)
```

is treated identically to

```sql
json_group_array(t.tag)
```

The non-IVM query path **does** support FILTER — `planner.rs:240` and
`:282`/`:318`/`:375` thread `filter_over.filter_clause` into
`add_aggregate_if_not_exists`. The IVM compiler is the only layer that
drops it.

## Why this matters

This is the **third** blocker for holon's `block`-as-matview architecture.
With LEFT JOIN landing (`HANDOFF_IVM_LEFT_JOIN.md`), holon can now write:

```sql
CREATE MATERIALIZED VIEW block AS
  SELECT b.id, json_group_array(t.tag) AS tags
  FROM block_raw b LEFT JOIN block_tags t ON t.block_id = b.id
  GROUP BY b.id;
```

A block with zero tags appears as expected, but `tags = '[null]'` instead
of `'[]'`. The synthetic NULL row injected by LEFT JOIN feeds into
`json_group_array`, which faithfully serializes the NULL.

The user's hard requirement is `'[]'`, not `'[null]'`. The standard SQL
mechanism for this is:

```sql
json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL) AS tags
```

For an unmatched parent, the LEFT JOIN injects `(b.*, NULL)`. The
FILTER predicate fails (`NULL IS NOT NULL` is false), so no value enters
the aggregate. With zero values, `json_group_array` returns `'[]'`.

This is also the right primitive for many other holon-shape queries
where the aggregate should ignore NULLs that arise from the LEFT JOIN.

## SQL semantics (reference)

```
SELECT k, AGG(expr) FILTER (WHERE p) FROM t GROUP BY k
```

is exactly equivalent to

```
SELECT k, AGG(CASE WHEN p THEN expr END) FROM t GROUP BY k
```

— except that `AGG(NULL)` semantics differ across aggregates:

| Aggregate                  | Behavior with NULL input |
|----------------------------|--------------------------|
| `count(expr)`              | skips NULL (returns count of non-NULL) |
| `count(*)`                 | counts row regardless of NULL columns |
| `sum`/`avg`                | skips NULL |
| `min`/`max`                | skips NULL |
| `group_concat`             | skips NULL |
| **`json_group_array`**     | **includes NULL** as JSON `null` (this is the gotcha) |

So for `json_group_array` specifically, `FILTER (WHERE x IS NOT NULL)`
and the `CASE WHEN` rewrite both work — but rewriting `CASE WHEN p THEN
expr END` for `json_group_array` doesn't help if you want `[]`, because
the synthetic NULL still becomes `null` in the array. **FILTER skips the
row entirely; CASE WHEN substitutes NULL.**

The implementation must do FILTER's "skip the row entirely" semantics,
not CASE WHEN's "substitute NULL."

## DBSP theory (no obstacle)

FILTER is a per-row predicate evaluated **before** the aggregate
absorbs the row. In DBSP:

```
γ_AGG_FILTER(p)(L) = γ_AGG(σ_p(L))
```

i.e., aggregate-with-filter is identical to aggregate over a filtered
input. The incremental rule is:

```
δ(γ_AGG_FILTER(p)(L)) = δ(γ_AGG(σ_p(L)))
                     = γ_AGG_INCR(σ_p(δL))
```

where `γ_AGG_INCR` is the incremental aggregate already implemented.
**Implementation strategy:** don't build a new operator; just gate
per-row state updates on `p`.

There is one subtlety: when the filter expression references a column
that gets deleted-then-reinserted with different non-filter columns,
the per-row weight semantics still work because the predicate is
deterministic in the row's values — same row → same predicate result.

## Proposed implementation

### Path of least resistance: gate the existing aggregate state machine

Today `AggregateFunction` (`core/incremental/aggregate_operator.rs:108-124`)
is a flat enum keyed only on column index:

```rust
pub enum AggregateFunction {
    Count,
    CountDistinct(usize),
    Sum(usize),
    SumDistinct(usize),
    Avg(usize),
    AvgDistinct(usize),
    Min(usize),
    Max(usize),
    GroupConcat { col: usize, separator: String },
    GroupConcatDistinct { col: usize, separator: String },
    JsonGroupArray(usize),
    JsonGroupArrayDistinct(usize),
}
```

Add an optional pre-compiled filter expression alongside each variant —
or, less invasively, **alongside** the aggregate at the operator level:

```rust
pub struct AggregateOperator {
    // existing fields...
    aggregate_filters: Vec<Option<FilterPredicate>>,  // one per aggregate
}
```

`FilterPredicate` already exists at
`core/incremental/operator.rs:FilterPredicate` (used by the existing
`FilterOperator`). It implements `evaluate(values: &[Value]) -> bool`.

In `AggregateState::apply_delta` at `aggregate_operator.rs:1290`, the
per-row update path becomes:

```rust
fn apply_delta(...)
    let mut effective_count_delta = 0;
    for (agg_idx, agg) in aggregates.iter().enumerate() {
        let predicate_passes = match &filters[agg_idx] {
            Some(p) => p.evaluate(values),
            None    => true,  // no filter = always include
        };
        if !predicate_passes { continue; }
        // existing per-aggregate update logic
    }
}
```

**Critical**: `count` is updated globally at the top of `apply_delta`
today (`self.count += weight as i64;`). With FILTER, `count` becomes
per-aggregate (because each aggregate may have a different filter). The
group's "did this group exist?" tracking must change:

- A group exists if **any** non-FILTER COUNT/SUM/... has count > 0
  — OR — if any per-aggregate effective count is > 0.
- The simplest fix: always maintain a "row count" for the group
  independent of any aggregate's FILTER, and use that for "group
  exists" detection. Aggregates with FILTER maintain their own
  internal effective counts.

Or, more pragmatically: **the GROUP BY clause exists at the row level,
not the aggregate level.** A group-by row is contributed by `(group_key,
weight)` from input. The group exists if the aggregate's input has any
row with that group key, regardless of FILTER. FILTER only affects the
*value* the aggregate computes — `[]` for filtered-out values is the
correct answer, and the row still appears.

So the implementation:
1. Keep `self.count` as the overall row count (group existence).
2. For each aggregate with FILTER, maintain a separate per-aggregate
   effective count/sum/multiset (whatever the aggregate type needs).
3. When computing the aggregate's output value, use the per-aggregate
   state.

For `JsonGroupArray`: the multiset of values is already maintained
per-aggregate. Just gate the multiset insert on the filter.

For `Count` (no DISTINCT, no column): if FILTER is present, gate the
increment. If not, use `self.count` (the global row count).

For `Min`/`Max`: gate the candidate insertion.

### Where the filter expression comes from

Trace the AST → IVM logical → IVM physical pipeline:

1. **AST** (`parser/src/ast.rs:506`): `FunctionCall.filter_over.filter_clause`
   exists today.

2. **IVM logical layer** (`core/translate/logical.rs:2200-2239`): drop is here.
   Extend `LogicalExpr::AggregateFunction` from

   ```rust
   AggregateFunction { fun, args, distinct }
   ```

   to

   ```rust
   AggregateFunction { fun, args, distinct, filter: Option<Box<LogicalExpr>> }
   ```

   The constructor at line 2218 changes to:

   ```rust
   let filter = filter_over.filter_clause.as_ref()
       .map(|e| self.build_expr(e, _schema))
       .transpose()?
       .map(Box::new);
   Ok(LogicalExpr::AggregateFunction { fun: agg_fun, args: arg_exprs, distinct, filter })
   ```

   Update every match site for `LogicalExpr::AggregateFunction` (grep for
   it) — `..` patterns help most callers ignore the new field. The two
   that have to change are the IVM compiler's `LogicalPlan::Aggregate`
   arm and `preprocess_aggregate_expressions` (more on the latter
   below).

3. **`preprocess_aggregate_expressions`**
   (`core/translate/logical.rs:1519-1660+`): this function lifts complex
   aggregate args into a pre-projection. The FILTER expression also
   needs to lift any complex sub-expressions referenced in the filter
   into the pre-projection schema. If FILTER is just `col IS NOT NULL`
   (the common case), no lifting is needed because `col` is already in
   the pre-projection from the aggregate's arg. For more complex
   FILTERs (`expr1 + expr2 > 5`), pre-project them.

   **Simplification for v1:** restrict FILTER to expressions referencing
   only columns that are already in the pre-projection. Reject anything
   else with a clear error. The holon use case (`tag IS NOT NULL`)
   satisfies this trivially.

4. **IVM physical compiler** (`core/incremental/compiler.rs:2257-2470`):
   the loop that builds `aggregate_functions: Vec<AggregateFunction>` from
   `agg.aggr_expr`. For each `LogicalExpr::AggregateFunction`, also
   compile the filter expression (using the existing `compile_filter_predicate`
   at `compiler.rs:1914`), and store it alongside the aggregate.

5. **Operator construction**
   (`core/incremental/aggregate_operator.rs:AggregateOperator::new`):
   accept the `aggregate_filters: Vec<Option<FilterPredicate>>` parameter.

### State persistence

`AggregateState::to_blob` / `from_blob` (`aggregate_operator.rs:1223+`)
persist the aggregate state across DB reopens. The filter expression
itself is **NOT** part of state — it's reconstructed from the matview's
SQL on reopen (which already happens — that's how `AggregateOperator`
gets reconstructed). So no changes to the blob format are needed.

The per-aggregate effective counts/multisets ARE state and must be
persisted. The existing serialization is per-`AggregateFunction` variant
(`to_value_vector` / `from_value_vector` at `aggregate_operator.rs:860`,
`1093`). Plumbing FILTER's effect through these may or may not require
schema changes depending on which design (separate per-aggregate counts
vs. gating the existing fields) is chosen.

**Recommendation:** for v1, gate the existing fields. The semantics
"this aggregate sees only filter-passing rows" composes with the
existing per-aggregate state without requiring new fields, *as long as*
a row that fails the filter contributes 0 to that aggregate's state.
The `count` global field stays meaningful as "rows in this group" — and
a group with all FILTER-failing rows still exists (count > 0) but its
aggregates compute over zero values.

### Implementation order

1. **Phase 1** (~1 hr): write 4 failing sqltests in
   `testing/runner/tests/ivm-aggregate-filter.sqltest`:
   - `filter-create` — populated table, FILTER excludes some rows.
   - `filter-incremental-insert` — INSERT a FILTER-passing row.
   - `filter-incremental-delete` — DELETE a FILTER-passing row.
   - `filter-with-left-join-empty-bucket` — the holon shape; expect `[]`.

2. **Phase 2** (~1 hr): add `filter` to `LogicalExpr::AggregateFunction`,
   update all match sites. Compile cleanly.

3. **Phase 3** (~30 min): thread filter through `preprocess_aggregate_expressions`.
   For v1, reject FILTER expressions that aren't simple column-reference
   predicates already covered by the pre-projection.

4. **Phase 4** (~2 hr): add `aggregate_filters` to `AggregateOperator`,
   gate per-row state updates in `apply_delta` and equivalent paths
   (`extract_min_max_deltas`, `extract_distinct_transitions`,
   the GroupConcat / JsonGroupArray multiset writers).

5. **Phase 5** (~1 hr): cross-session restore — verify FILTER state
   round-trips through `to_blob`/`from_blob`. Should be automatic if
   Phase 4 only gates state writes (no new state was added).

6. **Phase 6** (~30 min): the `[]` vs `[null]` lockin — flip test #11 in
   `testing/runner/tests/ivm-left-join.sqltest` and the
   `array-agg-left-join` test in `ivm-array-aggregation.sqltest` from
   `'[null]'` to `'[]'` once a FILTER variant exists.

Total estimated scope: **~6 hours focused**, comparable to the
array-aggregation handoff or the LEFT-JOIN one.

## Tests to add

In `testing/runner/tests/ivm-aggregate-filter.sqltest`:

```
@database :memory:
@skip-file-if mvcc "materialized views not supported in MVCC mode"
@requires materialized_views "requires materialized view support"
```

### Core cases

1. **`filter-create-skips-rows`**
   ```sql
   CREATE TABLE t (g INT, v INT);
   INSERT INTO t VALUES (1, 10), (1, 20), (1, 5), (2, 100);
   CREATE MATERIALIZED VIEW mv AS
     SELECT g, sum(v) FILTER (WHERE v >= 10) FROM t GROUP BY g;
   SELECT g, sum FROM mv ORDER BY g;
   -- expect: 1|30, 2|100
   ```

2. **`filter-incremental-insert-passes`** — INSERT a row that passes the filter; aggregate updates.

3. **`filter-incremental-insert-fails`** — INSERT a row that fails; aggregate unchanged.

4. **`filter-incremental-delete-passes`** — DELETE a previously-passing row; aggregate updates.

5. **`filter-incremental-delete-fails`** — DELETE a row that never passed; aggregate unchanged.

6. **`filter-update-pivots-predicate`** — UPDATE a row from filter-passing to filter-failing; aggregate retracts.

7. **`filter-with-distinct`**
   ```sql
   sum(DISTINCT v) FILTER (WHERE v >= 10)
   ```

### Composition with LEFT JOIN (the holon shape)

8. **`filter-left-join-empty-bucket`**
   ```sql
   CREATE TABLE p (id INT PRIMARY KEY);
   CREATE TABLE j (id INT, tag TEXT);
   INSERT INTO p VALUES (1),(2);
   INSERT INTO j VALUES (1,'a');
   CREATE MATERIALIZED VIEW mv AS
     SELECT p.id, json_group_array(j.tag) FILTER (WHERE j.tag IS NOT NULL) AS tags
     FROM p LEFT JOIN j ON j.id = p.id GROUP BY p.id;
   SELECT id, tags FROM mv ORDER BY id;
   -- expect: 1|["a"], 2|[]
   ```

9. **`filter-left-join-add-tag`** — adding a junction row flips `tags` from `[]` to `["x"]`.

10. **`filter-left-join-remove-last-tag`** — removing the only matching junction row flips `tags` from `["x"]` to `[]`.

### Edge cases

11. **`filter-references-column-not-in-aggregate`**
    ```sql
    sum(price) FILTER (WHERE category = 'food')
    ```
    The pre-projection must include `category` even though only `price`
    is the aggregate arg. If we restrict v1 to "filter only references
    columns already in pre-projection," reject with a clear error.

12. **`filter-on-count-star`**
    ```sql
    count(*) FILTER (WHERE active = 1)
    ```
    `COUNT(*)` has no arg but FILTER references a column.

13. **`filter-rejected-with-window`** — FILTER+OVER is a window-function
    construct already rejected for IVM. Sentinel.

### Cross-session

14. **`tests/integration/query_processing/test_ivm_aggregate_filter.rs`**
    — populate a FILTER matview, close DB, reopen. INSERT a passing row
    and a failing row. Verify aggregate updates only for the passing one.

## Reused primitives (do not re-implement)

| Need | Reuse |
|------|-------|
| Compile a per-row predicate from a `LogicalExpr` | `Self::compile_filter_predicate` at `compiler.rs:1914` |
| Evaluate predicate against row values | `FilterPredicate::evaluate` (already used by `FilterOperator`) |
| Aggregate state machine | `AggregateOperator` + `AggregateState` (`aggregate_operator.rs`) |
| Pre-projection lifting for complex aggregate args | `preprocess_aggregate_expressions` at `logical.rs:1519` |
| Cross-session blob serialization | `AggregateState::to_blob` / `from_blob` (`aggregate_operator.rs:1223+`) |
| Non-IVM FILTER handling (for cross-checking semantics) | `core/translate/planner.rs:240` and friends |

## Hypotheses to validate (cheap, do during impl)

| #   | Hypothesis | How to validate |
|-----|------------|-----------------|
| H1  | `FilterPredicate::evaluate` works against the per-row values used by `apply_delta` (same column-index convention) | Read `compile_filter_predicate` and trace the value layout it expects. |
| H2  | The pre-projection lifts FILTER columns automatically when they're already aggregate args | True for `json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL)`. False if FILTER references a different column — explicit lift required. |
| H3  | `AggregateState::to_blob` / `from_blob` survives the addition of FILTER without format changes | Likely yes if FILTER only gates state updates. Verify by writing test #14 and reopening. |
| H4  | The non-IVM planner's FILTER handling is correct as a reference (we should match it) | Read `planner.rs:240` and the bytecode emitter for FILTER (`vdbe/`). The IVM and non-IVM should produce the same matview content. |
| H5  | LEFT JOIN's `MatchCounterOperator` produces synthetic NULL rows that cleanly fail `IS NOT NULL` predicates | Yes — `MatchCounterOperator::null_pad` emits `Value::Null` for right-side columns, and `IS NOT NULL` returns false. Test #8 cross-validates. |
| H6  | `count(*) FILTER (WHERE p)` works without special-casing `Count`'s "no column" semantics | Verify by test #12. The filter evaluates against the row's values; `count` ignores `args` but the filter doesn't. |

## Known bug classes to watch for

From `MEMORY.md` and the LEFT JOIN handoff, these are the failure modes
this work is most likely to hit:

1. **Per-aggregate state divergence on UPDATE.** UPDATE produces
   `(old_row, -1)` then `(new_row, +1)`. If the old row passed the filter
   and the new row doesn't (or vice versa), the aggregate state must
   correctly account for the asymmetry. The HashMap-batching pattern
   used by `MinMaxDeltas` and the LEFT JOIN match counter
   (`r_count_deltas`) is the right reference.

2. **DISTINCT + FILTER interaction.** `sum(DISTINCT v) FILTER (WHERE p)`
   means "for each distinct v that has at least one row passing p, add
   v to the sum once." The `distinct_transitions` machinery in
   `apply_delta` (lines 1296+) must gate distinct-value membership on
   the filter, not just the aggregate's contribution.

3. **Cross-session "group exists" state.** If a group's only rows fail
   FILTER, does the group still appear in the matview? Per SQL semantics:
   yes. `count(*) FILTER (WHERE p)` returns 0 (or `count(p_passing)`
   could return NULL — check SQLite for the exact behavior). Lock down
   with test #12.

4. **`json_group_array` with all-filtered-out values returns `[]`, not
   NULL.** This is the load-bearing answer for holon. Verify via
   diff against SQLite (regular VIEW with same query).

5. **FILTER expression referencing a JOIN-introduced nullable column.**
   The post-LEFT-JOIN schema marks right-side columns nullable. The
   filter expression `t.tag IS NOT NULL` should compile cleanly against
   that schema. If `compile_filter_predicate` chokes on nullable inputs,
   we have a bug. Test #8 covers it.

## Out of scope (separate handoffs)

- **`FILTER (WHERE ...)` with subqueries.** SQL allows
  `FILTER (WHERE x IN (SELECT ...))`. IVM doesn't support correlated
  subqueries in matview bodies generally; FILTER inherits this
  restriction. Reject for now.
- **`FILTER (WHERE ...)` over window functions** (`FILTER OVER (...)`).
  Window functions are independently unsupported in IVM matviews
  (`logical.rs:2208` already rejects).
- **Complex FILTER expressions referencing columns not in the
  pre-projection.** Restrict v1 to filters that only reference columns
  already lifted into the aggregate's pre-projection input. Document
  the restriction.

## Cross-repo coordination

- Holon-side: this is the **third blocker** for the `block`-as-matview
  architecture (after array-aggregation and LEFT JOIN). With this and
  LEFT JOIN landed, the architecture unblocks fully — `block.tags`
  finally returns `'[]'` for tagless blocks instead of `'[null]'`.
- Holon's PBT round-trip (their task #2) is the integration test for
  this work landing successfully.

## File pointers

### Modify
- `core/translate/logical.rs` — `ColumnInfo` (no change), `LogicalExpr::AggregateFunction` (add `filter` field), `Self::build_expr` for `FunctionCall` at line 2200, `preprocess_aggregate_expressions` at line 1519. Grep for every match arm on `LogicalExpr::AggregateFunction` to ensure exhaustive coverage (use `..` where the new field can be ignored).
- `core/incremental/compiler.rs` — `LogicalPlan::Aggregate` arm at line 2257, where `aggregate_functions` is built. Compile each aggregate's filter via `Self::compile_filter_predicate` and pass alongside.
- `core/incremental/aggregate_operator.rs` — `AggregateOperator::new` signature (add `aggregate_filters: Vec<Option<FilterPredicate>>`). `AggregateState::apply_delta` at line 1290 (gate per-aggregate updates). `extract_min_max_deltas` at line 2007 (gate). `extract_distinct_transitions` (gate). GroupConcat / JsonGroupArray multiset writers (gate).

### Create
- `testing/runner/tests/ivm-aggregate-filter.sqltest`
- `tests/integration/query_processing/test_ivm_aggregate_filter.rs`

### Read for context (no changes needed)
- `core/translate/planner.rs:240+` — non-IVM reference
- `core/incremental/operator.rs::FilterPredicate` — the predicate type
- `parser/src/ast.rs:506, 2139` — AST shape
- `HANDOFF_IVM_LEFT_JOIN.md` — the architectural neighbor; the `[null]`
  vs `[]` discussion lives there

## Suggested implementation order

1. Read `core/translate/logical.rs` end-to-end, focused on
   `LogicalExpr::AggregateFunction` construction and consumers. Map
   every match site that needs `..` or an explicit `filter: None`.
2. Read `core/incremental/aggregate_operator.rs::apply_delta` and the
   per-aggregate update paths end-to-end. Plan exactly which predicates
   to gate.
3. Write the failing sqltests (Phase 1, 4 cases).
4. Add `filter` to `LogicalExpr::AggregateFunction` (Phase 2).
5. Wire compiler to compile and propagate the filter (Phase 3).
6. Implement gating in the operator (Phase 4).
7. Cross-session test (Phase 5).
8. Flip the `[null]` lockin tests (Phase 6).
9. Run holon PBT — the real integration test.

## Estimated scope

Comparable to the array-aggregation handoff or the LEFT-JOIN handoff —
**~6 hours focused** once the test surface is locked down. The DBSP
theory is trivial (FILTER ≡ pre-aggregate σ); the work is in the
plumbing. The risk is in the per-aggregate state divergence cases
(UPDATE pivoting predicate, DISTINCT + FILTER) which the HashMap-batching
pattern from the existing aggregates and the LEFT JOIN match counter
already shows how to handle.
