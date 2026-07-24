# Phase 2 hand-off: LEFT JOIN multiplicity drop, NOT aggregate dup-drop (2026-05-08)

The original Phase 2 description in
`bugs/HANDOFF_ivm_fuzzer_followups_2026-05-08.md` predicted "aggregate
matview drops duplicates that should remain" with `sensible_mark` as
the seed and a 12×row-collapsing-to-1 pattern. After the Phase 1 fix
landed and ~600 fuzzer seeds were swept, **no aggregate matview was
observed dropping duplicates**. The closest matching bug class is a
LEFT JOIN multiplicity drop on from-scratch population.

## Reproducer (seed-based, not yet minimized)

```bash
cargo build --bin tursodb --bin differential_fuzzer
timeout 60 cargo run -q --bin differential_fuzzer -- --matview \
    -g sql-gen-prop -n 200 --seed 344 --keep-files
```

Failure: `Matview data mismatch in 'honest_editions'`. Turso ends up
with 23 rows of `(1, 1)`; SQLite has 340. Both agree on the unique
rows. Difference is purely the multiplicity of identical projected
rows for a single left key matching many right rows.

The matview at `simulator-output/test.sql` line 270:

```sql
CREATE MATERIALIZED VIEW honest_editions AS
SELECT zestful_nikolau.spectacular_moran AS lcol,
       willing_delesalle.spectacular_moran AS rcol
FROM zestful_nikolau LEFT JOIN willing_delesalle
ON zestful_nikolau.spectacular_moran = willing_delesalle.spectacular_moran;
```

## Why it doesn't reduce trivially

A direct 1×N LEFT JOIN repro (1 left row, 50–300 matching right rows)
returns the correct `N` rows in both Turso and SQLite. The handoff's
shape "GROUP BY col where the aggregate happens to repeat" doesn't
trigger anything either — Turso correctly preserves multiplicity for
identical projections from singleton GROUP BY groups.

The seed-344 trigger appears to need:
- An intermediate CTAS table acting as one of the JOIN sides
  (`willing_delesalle = loving_miroslav × zestful_nikolau`).
- A second matview already in the schema (the seed has
  `lsdd8__01e76` aggregate matview + `g_t_j_9u596_…` UNION ALL
  matview ahead of `honest_editions`).
- ~10 source rows in the left table and ~340 right rows.

Removing any of those drops the bug.

## Suspected root cause (not verified)

The from-scratch JoinOperator path processes left then right tables
one row at a time via `process_one_row` →
`merge_delta` → `circuit.commit`. For LEFT JOIN, the 3-operator
subgraph (Inner Join + MatchCounterOperator + MergeOperator) has a
known consolidation gap (see MEMORY.md "MatchCounter Unconsolidated
Input Bug"). When a single δL row encounters a δR delta containing
many entries with the same join key, the per-entry algorithm in
MatchCounter may record a single match where the multiset semantics
demand `N`. That fits the symptom — Turso emits some `(L, R)` rows
but truncates well below the right-side count.

Best places to look:
- `core/incremental/match_counter_operator.rs` — match accounting on
  initial population vs. incremental.
- `core/incremental/join_operator.rs:process_join_state` —
  `ProcessLeftJoin` / `ProcessRightJoin` arms each call
  `read_next_join_row` once per iteration; check whether
  `last_row_scanned` correctly cycles through *all* matches when the
  storage has multiple entries for the same key.
- `core/incremental/view.rs:process_one_row` — the row-at-a-time
  population path interacts with the join's stored state. Verify the
  state transitions when a single row's circuit step emits multiple
  output rows that would arrive across several `IOResult::IO` yields.

## Next steps

1. **Reduce further**. Try replacing the surrounding fuzzer matviews
   with explicit DDL until the bug reappears with a clean repro.
   The hint that intermediate matviews matter suggests the
   `view_transaction_states` snapshot/rollback flow or the cross-
   matview scheduling in `apply_view_deltas` interacts with the
   stored-state lookups inside the join operator.
2. **Bypass from-scratch**. Create the matview on an empty table,
   then INSERT the rows incrementally; if the multiplicity is correct
   with incremental but wrong from-scratch, the bug is in
   `populate_from_table_inner` or
   `MatchCounterOperator` first-pass behavior.
3. **Then RCA + fix**. Once a clean repro exists, the IVM bug
   investigation methodology applies as in Phase 1.

## Out of scope for this hand-off

- Phase 3 (aggregate scalar divergence, SUM/AVG with extreme Reals)
  is unrelated.
- Phase 4 (numeric type ordering for matview keys) is unrelated.
- Phase 5 (parser ORDER BY positional) is unrelated.

The Phase 1 fix landed cleanly and the original `confident_eloff`
divergence is gone. Phase 2 needed deeper minimization than expected;
hand back so a fresh investigation can pick a clean reproduction
strategy.
