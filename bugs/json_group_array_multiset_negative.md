# Bug: `json_group_array multiset went negative` after MatchCounter fix

## Summary

`AggregateOperator::process_row` panics in `aggregate_operator.rs:1470`:

```
json_group_array multiset went negative for col 3 val Text(...) — delta
consolidation invariant violated
```

The aggregator maintains a `(value -> count)` multiset for each
`json_group_array(...)` column. When a delete delta arrives for a value
the aggregator never inserted, the count goes below zero and the
assertion fires.

## Discovered by

The `test_match_counter_uninitialized_dual_left_cdc_burst` test (now
`#[ignore]`'d). With the MatchCounter I/O re-entry bug fixed, the LEFT
JOIN now emits the *correct* delta sequence to the downstream
aggregator — and this surfaces a previously-masked invariant break in
`json_group_array`.

## Trigger shape

- LEFT OUTER JOIN matview with `json_group_array(...)` aggregation
- File-backed DB
- Burst of `DELETE`s on the R-side junction table that drives the
  per-key R_COUNT across zero (the same trigger that originally fired
  the MatchCounter panic)
- The matview must have rows that toggle between matched and
  null-padded LEFT JOIN states

## Hypothesis

When a key transitions from "has matches" to "has no matches", the
LEFT JOIN emits a delete for the previously-emitted matched row(s) and
inserts a null-padded row. The `json_group_array` aggregator sees a
delete for a tag value (e.g. `'extra'`) that originated from an L row
whose tag column was already at count 0 in the aggregator's multiset.

This is likely a delta-consolidation issue: the aggregator's view of
the multiset diverges from what the upstream LEFT JOIN believes is
in-flight. Could be a mismatch between Phase B (`EmitRTransitions`,
emits null-pads from existing L_PRESENCE rows when R_COUNT crosses
zero) and the aggregator's expectation that those rows already had
their tag values aggregated.

## Reproducer

`tests/integration/query_processing/test_match_counter_uninitialized_repro.rs::
test_match_counter_uninitialized_dual_left_cdc_burst` (currently
`#[ignore]`'d). To trigger, remove the `#[ignore]` attribute and run
that test.

## Suggested investigation path

1. Add `tracing::debug!` to the LEFT JOIN delta emission and the
   aggregator's multiset updates to see exactly which value goes
   negative and what delta caused it.
2. Compare with the equivalent SQLite VIEW result to see what the
   aggregator's expected multiset state should be at that point.
3. Likely either:
   - `MatchCounterOperator` is double-emitting a delete (Phase B + Phase C)
   - Or the aggregator is missing an insert that should have been
     processed earlier.

## Additional confirming repro (2026-05-08, holon real workload)

Captured during `holon-mcp` bootstrap on Turso pin `290fbb4ff` (which
shipped the matview-first-open-cursor fix). Minimised by
`tools/turso-sql-replay`:

- Repro file: `crates/holon/sql/regressions/2026-05-08-block-matview-multiset-negative.sql`
  in the holon repo (147 stmts / 77 KB, deterministic across 3 runs).
- Trigger value in this run: tag `'Page'` (col 17 = `bt.tag`); the
  shape is the dual `LEFT OUTER JOIN` on `block_tags` and
  `block_requires` documented above.
- Replays in pure Turso: `cargo run -p turso-sql-replay -- replay <file>`
  → `panicked at .../aggregate_operator.rs:1470:29: json_group_array
  multiset went negative for col 17 val Text("Page") — delta
  consolidation invariant violated`.
- Holon-side impact: every autocommit txn after the first panic
  cascade-panics in `AggregateOperator::commit` at
  `aggregate_operator.rs:2192` (Invalid state on retry). Caught at the
  `TursoBackend` actor layer (`futures_util::catch_unwind`) — silent
  to callers. Symptom: `cc_*` cache tables (claude-history MCP sync)
  stay empty because every QueryableCache write commits and hits the
  cascade panic.
- Original 5114-stmt slim trace and 7798-stmt full trace also kept
  as `/tmp/cc-block-only.sql` / `/tmp/cc-aggregate-trace.sql` if a
  larger repro shape is useful.

## Resolution (2026-05-08)

### Root cause

`AggregateOperator::eval_internal`'s `EvalState::Init` arm consumed the
upstream delta without consolidating. The upstream LEFT JOIN circuit
emits the standard DBSP three-way sum (`δL⋈R + L⋈δR + δL⋈δR`), which
produces the *same projected row* multiple times with opposing weights
for cells unaffected by the change. Processing those entries one at a
time in `apply_delta` could drive the per-(group, value) multiset below
zero before the matching insert landed, tripping the assertion. The
specific shape of the holon dual-LEFT-JOIN matview made this near-
deterministic; the fuzzer-found seed (`5530919842341186569`) reproduces
it with a much smaller chained-matview `LeftJoinAggregate` on a single
`UPDATE base SET col = NULL`.

### Fix

`core/incremental/aggregate_operator.rs`,
`AggregateOperator::eval_internal::EvalState::Init`: call
`deltas.left.consolidate()` before iterating. `Delta::consolidate`
already exists (`dbsp.rs:246`) and sums weights per `HashableRow` while
dropping zero-net entries — exactly the invariant the assertion was
asserting.

### Detection

Fuzzer was previously blind to this class because matview generation
had no `ViewSelectKind` combining LEFT JOIN with GROUP BY +
multiset-tracked aggregates and never set `Expression::filter: Some(_)`.
Two new variants added in `testing/differential-oracle/sql_gen_prop/view.rs`:

- `LeftJoinAggregate`
- `DualLeftJoinAggregate` (mirrors holon's `block` matview shape)

Both emit `COALESCE(json_array_length(json_group_array(rhs.col) FILTER (WHERE rhs.col IS NOT NULL)), 0)`
to keep the oracle deterministic while still routing every insert/delete
through `json_array_states`. Empirically detects the bug in ~15 % of
500-statement runs on broken code.

### Remaining flake

The 689-statement holon repro
(`/Users/martin/Workspaces/pkm/holon/crates/holon/sql/regressions/2026-05-08-block-matview-multiset-negative.sql`)
still panics intermittently (~2–3 / 5 runs) with file-backed storage even
with the consolidate fix. The panic site shows `entry_before=0` for a
group whose multiset *should* be 1 from the prior commit's INSERT —
i.e. the prior commit's persisted aggregate state did not survive to the
DELETE commit. That is a separate persistence / IO-yield re-entrancy
bug, not a delta-consolidation bug; tracked as a follow-up. The minimal
single-LEFT-JOIN repro and the fuzzer seed both pass cleanly with the
consolidate fix; 0 panics in 20 × 500-statement fuzzer sweeps.

## Related

- Original bug: `match_counter_uninitialized.md` (FIXED in this branch)
- LEFT JOIN matviews use `MatchCounter ⊎ Inner ⨉ MergeOperator(UnionMode::All)`
