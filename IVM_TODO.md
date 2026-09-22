# IVM/DBSP Feature Gaps — Phased TODO

Status key: `[ ]` = not started, `[~]` = partial, `[x]` = done

## Phase 1: Planner Fixes (unblock queries that the DBSP engine already handles)

These aren't DBSP limitations — they're bugs/shortcuts in the logical planner that prevent valid queries from reaching the DBSP compiler.

### 1.1 Compound equijoin conditions
`[ ]` **Impact: HIGH** — blocks EAV patterns, any multi-column foreign key

`ON a.node_id = b.node_id AND a.key_id = b.key_id` fails because `extract_equijoin_conditions` (`logical.rs:1270`) builds each side of `=` against the wrong schema. `build_expr(lhs, left_schema)` can't resolve columns that belong to the right table.

The DBSP compiler already handles multiple join key pairs — `compiler.rs:1652` iterates `join.on` and pushes to `left_key_indices`/`right_key_indices` vectors.

**Fix:** In `extract_equijoin_conditions`, build both sides of `=` against a combined schema (the `_` fallback arm at line 1299 already does this). Then classify left vs right by table qualifier. `resolve_join_columns` in `compiler.rs:1200` already handles this disambiguation.

**Files:** `core/translate/logical.rs:1270-1277`

**Test:** See `IVM_COMPOUND_JOIN.md` for standalone repro.

### 1.2 Non-equijoin filter conditions in ON clauses
`[ ]` **Impact: MEDIUM** — blocks `JOIN ... ON a.id = b.id AND a.type > 3`

`compiler.rs:1633` rejects any join with `filter.is_some()`. But many of these filters reference only one side and can be pushed down as a pre-join Filter, or they're cross-side conditions that can be applied as a post-join Filter. Both Filter types already work.

**Fix:** Instead of rejecting `join.filter`, classify each conjunct:
- Single-side predicate → push down as Filter before the join input
- Cross-side predicate → insert as Filter after the join output

**Files:** `core/incremental/compiler.rs:1632-1637`

## Phase 2: Expression Generalization (remove column-reference-only restrictions)

The DBSP operators work on column indices. The restriction to bare column references is artificial — the projection operator already compiles arbitrary expressions via VDBE. The technique for all items below is the same: **insert a synthetic Projection node** before the restricted operator that pre-computes expressions into virtual columns, then point the operator at those column indices.

### 2.1 Expressions in aggregate arguments
`[ ]` **Impact: HIGH** — blocks `SUM(price * qty)`, `COUNT(DISTINCT lower(name))`

`compiler.rs:1480-1585` requires every aggregate argument to be `LogicalExpr::Column`. Any expression hits the error arm.

**Fix:** Before compiling the Aggregate node, scan `aggr_expr` for non-Column arguments. For each, add the expression to a pre-projection and rewrite the aggregate to reference the new column index.

**Files:** `core/incremental/compiler.rs` (Aggregate arm, ~line 1440)

### 2.2 Expressions in GROUP BY
`[ ]` **Impact: MEDIUM** — blocks `GROUP BY date(created_at)`, `GROUP BY a + b`

`compiler.rs:1463` requires `LogicalExpr::Column`.

**Fix:** Same pre-projection technique as 2.1. Can share the same synthetic projection node.

**Files:** `core/incremental/compiler.rs` (Aggregate arm, ~line 1460)

## Phase 3: Missing Aggregate Functions

The `AggFunc` enum already has these variants, and the non-incremental engine supports them. They just need DBSP counterparts.

### 3.1 TOTAL()
`[ ]` **Impact: LOW** — `TOTAL` is `SUM` that returns 0.0 instead of NULL for empty groups

**Fix:** Add `AggregateFunction::Total(usize)` to `aggregate_operator.rs`. Reuse SUM's accumulation logic, override the empty-group case.

**Files:** `core/incremental/aggregate_operator.rs`, `core/incremental/compiler.rs:1580`

### 3.2 GROUP_CONCAT / STRING_AGG
`[ ]` **Impact: MEDIUM** — common for building comma-separated lists

Already in `AggFunc::GroupConcat` and `AggFunc::StringAgg`, but fall through to the `_` error arm at `compiler.rs:1580`.

Incremental maintenance is trickier than numeric aggregates: insertions append, but deletions require rebuilding from stored rows (or maintaining an ordered multiset of contributions). A counted-multiset approach that rebuilds on any delta is correct and simple, just not O(1) per delta.

**Files:** `core/incremental/aggregate_operator.rs`, `core/incremental/compiler.rs:1580`

## Phase 4: Outer Joins

### 4.1 LEFT OUTER JOIN
`[ ]` **Impact: HIGH** — blocks EAV patterns, any optional relationship

`join_operator.rs:386` returns an error. The `JoinOperator` infrastructure (state tables, key extraction, delta processing) is all there for INNER joins.

**DBSP formula:** Same three components as INNER (`δR ⟕ S`, `R ⟕ δS`, `δR ⟕ δS`), but:
- Unmatched left rows emit NULL-padded right columns
- When a right-side insert creates the *first* match for a left key: retract the NULL-padded row, emit the real joined row
- When a right-side delete removes the *last* match for a left key: retract the joined row, emit a NULL-padded row

Requires tracking match counts per left key in state storage.

**Files:** `core/incremental/join_operator.rs`

See `IVM_COMPOUND_JOIN.md` for full analysis.

### 4.2 RIGHT OUTER JOIN
`[ ]` **Impact: LOW** — mirror of LEFT, and most queries can be rewritten to use LEFT

### 4.3 FULL OUTER JOIN
`[ ]` **Impact: LOW** — LEFT + RIGHT combined, rarely used

### 4.4 CROSS JOIN
`[ ]` **Impact: LOW** — Cartesian product with empty key indices. Simple but O(N*M) state, so questionable whether it's worth supporting.

## Phase 5: Non-recursive CTEs

### 5.1 WITH foo AS (...) SELECT ... FROM foo
`[ ]` **Impact: MEDIUM** — readability/reuse feature, queries can be rewritten without CTEs

`compile_plan` hits the catch-all error at `compiler.rs:1824` for `WithCTE`/`CTERef`. Note that recursive CTEs already work via a separate code path.

**Fix option A:** Inline CTEs in the logical plan before DBSP compilation (rewrite `CTERef` → the CTE's subplan). The compiler already calls `plan.inline_ctes()` at line 1241 — verify this handles non-recursive CTEs. If it does, this might already work and the error is dead code.

**Fix option B:** Compile the CTE body as a subplan, memoize the output node ID, and substitute it for `CTERef` references.

**Files:** `core/incremental/compiler.rs:1240-1241`, `core/translate/logical.rs` (inline_ctes)

## Not Planned (fundamentally hard or semantically wrong)

| Feature | Why |
|---|---|
| ORDER BY on matview | Materialized views are sets, not sequences. Ordering is a query-time concern. |
| LIMIT/OFFSET | Ill-defined on an incrementally maintained set — which rows survive when the set changes? |
| Window functions | Require total ordering and per-row partition computation. Incremental maintenance is an open research problem. |
| Subqueries in WHERE | Would need correlated subquery decorrelation into joins first, which is a query optimizer feature. |

## Dependency Graph

```
Phase 1.1 (compound equijoins) ─────┐
Phase 1.2 (join filter pushdown) ────┤
                                     ├──→ Phase 4.1 (LEFT JOIN)
Phase 2.1 (expr in aggregates) ──┐   │         │
Phase 2.2 (expr in GROUP BY) ────┤   │         ├──→ Phase 4.2 (RIGHT JOIN)
                                 │   │         └──→ Phase 4.3 (FULL JOIN)
Phase 3.1 (TOTAL) ──────────────(independent)
Phase 3.2 (GROUP_CONCAT) ───────(independent)
Phase 5.1 (non-recursive CTEs) ─(independent, might already work)
```

Phases 1 and 2 are prerequisites for real-world graph/EAV workloads.
Phase 4.1 (LEFT JOIN) is the other critical piece for EAV patterns.
Everything else is incremental improvement.
