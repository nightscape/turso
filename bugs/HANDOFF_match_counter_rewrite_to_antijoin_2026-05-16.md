# Handoff: Replace MatchCounterOperator's Phase-Split with Canonical Compositional Antijoin

**Date:** 2026-05-16
**Author:** mauch (with Claude)
**Status:** Backlog (Option 2 from `bugs/holon_json_group_array_multiset_negative_2026-05-15.md` RCA)
**Predecessor fix:** see [[ivm_match_counter_phase_b_phase_c_double_emit]] memory — Option 1 (tactical filter in Phase B) landed 2026-05-16.

## Why this exists

Turso's `MatchCounterOperator` implements the null-padded ("antijoin") half of a LEFT JOIN by running two phases on each commit:

- **Phase B** — `EmitRTransitions::ScanningL` in `core/incremental/match_counter_operator.rs`. For each R-side join key whose `R_COUNT` crosses zero, scan `L_PRESENCE` storage and emit `(±1) × null_pad(l_row)` for every L row at that key.
- **Phase C** — `ProcessLDelta::LRowResolve::ReadingRCount`. For each row in `δL`, compute `pre_is_unmatched = l_pre > 0 ∧ c_pre == 0`, `post_is_unmatched = l_post > 0 ∧ c_post == 0`, emit `post − pre`.

The split is **not** in any IVM literature we could find. The DBSP paper (Budiu et al. 2023, VLDB) and the canonical implementations (Feldera, differential-dataflow, Materialize) all express LEFT JOIN as a **compositional** decomposition

```
LEFT OUTER JOIN(L, R) = INNER(L, R) ∪ NullPad(Antijoin(L, R))
```

and rely on the **general DBSP incrementalization algorithm** (Algorithm 4.6 in the paper) to derive a single-pass circuit. There is no "L-driven" / "R-driven" split — `Antijoin(L, R)` is maintained as a *count-based aggregation* (one `R_COUNT` per key, with `count == 0` ↔ "unmatched") and `δ(NullPad(Antijoin(L, R)))` falls out linearly from the standard delta rules.

The phase split is the **proximate cause** of three of the four MatchCounter bugs in MEMORY.md:

| Bug | Class | Origin |
|---|---|---|
| MatchCounter Uninitialized Eval State (2026-05-05) | I/O re-entry coordination between Phase B and Phase C state machines | Phase split |
| MatchCounter Unconsolidated Input Bug (2026-05-07) | Phase C iterates per-δL-entry without consolidating; redundant UPDATEs drop null-pad rows | Phase split (each phase reads `l_pre` / `c_pre` once and computes post per-entry — no shared snapshot) |
| MatchCounter Phase B + Phase C Double-Emit (2026-05-16) | Both phases independently emit for the same `(l_row, k)` when L and R both transition for that key | **Phase split contract violated:** the comment says Phase B handles `δl == 0`, but Phase B doesn't enforce the filter |

The tactical fix for the third bug filters `δL` rows out of Phase B's scan. It works for the commit-time path, but it patches a symptom of the architectural choice; future joint-delta scenarios will keep finding new coordination cracks.

A fourth, *still-open*, related symptom is documented in [[ivm_match_counter_chained_matview_eval_stale_lpresence]]:
the chained-matview uncommitted-read path runs MatchCounter's `eval` per-statement without persisting `L_PRESENCE`, so Phase B re-reads a stale snapshot on later statements in the same tx. The compositional rewrite likely subsumes that bug as well, because there is no Phase B to be stale — antijoin maintenance becomes "diff `R_COUNT` per key, propagate through `NullPad`."

## What "Option 2" means concretely

Replace `MatchCounterOperator` with two compositional primitives that already exist (or are cheap to add) in the codebase:

```
LEFT JOIN(L, R) compiler subgraph (target):

  L ──┐                                       ┌──► Inner(L,R) ─┐
      ├──► Antijoin(L, R)  ──► NullPad ───────┤                ├──► MergeOperator(UNION ALL)
  R ──┘  (= L ⋈ (count(R per k) ≜ 0))         └────────────────┘
```

Where:

- **`Antijoin`** is a *single* operator (or a 2-step subcircuit of `Aggregate(R) → AntiJoinFilter`) that maintains `R_COUNT(k)` and emits `δ(unmatched L rows)`. It has **one** state shard (`R_COUNT`) and **one** read of `L` per delta — no separate "phase B" scan. The output δ rule:

  ```
  δ(Antijoin(L, R))(l, k) = δL(l) · [c_post(k) == 0]
                          + L_post(l) · ([c_post(k) == 0] − [c_pre(k) == 0])
  ```

  This is exactly Algorithm 4.6 applied to `Antijoin = L ⋈ {k | count(R, k) = 0}`. Note that the two terms partition cleanly by `δL ≠ 0` vs. `δL = 0`, which is what the current Phase C / Phase B *intended* to encode — but the compositional form forces the partition by the formula, not by which loop you're in.

- **`NullPad`** is linear and stateless: `δ(NullPad(δ_antijoin)) = NullPad(δ_antijoin)`. Trivial wrapper.

`AggregateOperator` already exists in this codebase and maintains exactly the `R_COUNT(k)` style of state we need. The hard part of the rewrite is *not* a new aggregate primitive; it's wiring the LEFT JOIN compiler in `core/incremental/compiler.rs` to emit the new subgraph and removing all `L_PRESENCE` persistence.

## Concrete plan

### Phase 0 — Scope & risk assessment (½ day)

- Re-read DBSP paper §4 (algorithm) and §5 (outer joins). Confirm the `R_COUNT` Antijoin formulation matches what we'd build.
- Audit every consumer of `DbspOperator::MatchCounter { … }` in `core/incremental/`:
  - `compiler.rs::add_node` LEFT JOIN expansion (~line 2787)
  - `view.rs` matview persistence — does anything serialize MatchCounter operator IDs? (Spot-check; should be just storage roots.)
  - Any `as_any().downcast_ref::<MatchCounterOperator>()` callers (grep for `MatchCounterOperator` outside its defining module).
- Look at on-disk schema: the `_turso_internal_dbsp_state_v1_…` btree holds `L_PRESENCE` and `R_COUNT` rows keyed by operator-id + storage-type. Confirm Antijoin's `R_COUNT` shares the same encoding (it already should — same as MatchCounter's R_COUNT shard).

### Phase 1 — Build the Antijoin primitive (1–2 days)

New file: `core/incremental/antijoin_operator.rs`. Structure mirrors `JoinOperator` but with a single state shard.

- State on disk: one btree row per `(operator_id, k)` storing `R_COUNT`. Reuse `WriteRow` helpers from `persistence.rs`.
- In-memory state machine:
  - `Idle` → `ProcessDeltas { δL, δR, output, ... }`
  - For each `δR_count(k) = sum of δR weights for join-key k`:
    - read `c_pre(k)` from btree → if `c_pre == 0` and `c_post > 0`, emit retraction for every `l ∈ L_stored` with key `k` (scan `L_PRESENCE`-equivalent — but see below).
    - if `c_pre > 0` and `c_post == 0`, emit insertion for every `l ∈ L_stored`.
  - For each `δL(l)`:
    - emit `δL(l) · [c_post(k) == 0]`.
  - Persist `R_COUNT` updates.
  - **Key invariant:** every state must have at most ONE `return_if_io!` that yields. Use the `sought: bool` pattern from the WriteRow fix.
- For the L-side scan in the R-transition path: this **does** still need an L storage. But here it's a single store of L rows keyed by join-key, with the same delta-skipping invariant as Option 1 — and now it's expressible as `L ⋈ (R_COUNT crosses zero)` directly, not as a parallel phase. Concretely: one btree shard `L_INDEX` keyed by `(operator_id, join_key)` with values = `l_row`. **Or** — and this is the bigger architectural win — drop L_INDEX entirely and rely on the upstream `L` storage (which already exists for `Inner`). The `Inner` and `Antijoin` operators share the same L input and can share its storage. Investigate before committing to a design.

### Phase 2 — Compiler wiring (½ day)

`core/incremental/compiler.rs::DbspGraphBuilder` LEFT JOIN case (~line 2787). Replace the three-operator wiring (Inner + MatchCounter + Merge) with the new wiring:

```rust
LogicalJoinType::Left => {
    let inner_id  = self.add_inner_join(...);
    let aj_id     = self.add_antijoin(...);
    let nullpad_id = self.add_nullpad(aj_id, right_column_count);
    self.add_merge(vec![inner_id, nullpad_id], UnionMode::All { ... })
}
```

(`NullPad` is a one-line stateless map; might be folded into `Antijoin`'s output projection rather than a separate node — cosmetic.)

### Phase 3 — Migration of existing on-disk matviews (½ day)

This is the risk hotspot. Existing matview files on disk have `MatchCounter` state under `L_PRESENCE` and `R_COUNT` storage IDs. Options:

1. **Bump matview format version**, reject older matviews, force rebuild. Cleanest. Acceptable if all known consumers (Holon, internal users) can tolerate a one-time rebuild.
2. **Keep reading old L_PRESENCE for migration**, then drop after first successful tx. More involved; defer unless option 1 is unacceptable.

Confirm with the team before picking. Default to option 1 — every existing IVM bug from MEMORY.md has been "rebuild matview to recover" at some point already, so the cost is bounded.

### Phase 4 — Delete `MatchCounterOperator` (1 hour)

After all callers are gone:
- Delete `core/incremental/match_counter_operator.rs`.
- Delete `DbspOperator::MatchCounter` variant.
- Delete the `delta_l_rows` field added by Option 1 (now redundant — the entire phase split is gone).
- Remove all `MatchCounter*` entries from MEMORY.md → they're now archaeological.

### Phase 5 — Tests + fuzzer (1 day)

- Keep `testing/sqltests/turso-tests/ivm-dual-leftjoin-update-junction-insert.sqltest` (this exact regression must keep passing).
- Add a regression test for the chained-matview uncommitted-read path from `/tmp/holon-minimized.sql`. The Holon repro should now pass against the on-disk `holon.db`.
- Re-run the fuzzer for ≥10 000 statements against the new operator. The existing `WideGroupByDualLeftJoinAggregate` view variant generates the right matview shape; the new operator should be measurably less crash-prone.
- Cross-check: every entry under `MatchCounter*` in MEMORY.md historical bug list — write a regression test for each that exercises the new operator, and confirm it passes. (This is the highest-confidence way to detect a regression in the rewrite.)

## Pointers / reading material

- DBSP paper: <https://www.vldb.org/pvldb/vol16/p1601-budiu.pdf>. §4 (incrementalization algorithm), §5 (outer joins).
- Feldera source: <https://github.com/feldera/feldera> — search for `antijoin` and `outer_join` in `crates/dbsp/src/operator/`.
- differential-dataflow `antijoin`: <https://github.com/TimelyDataflow/differential-dataflow/blob/master/src/operators/join.rs> (look for `antijoin` impl).
- Larson & Zhou 2007, "Efficient Maintenance of Materialized Outer-Join Views" (PSNS algorithm) — also useful but predates DBSP and uses change-tables rather than Z-sets.

## Test files that must continue to pass

- `testing/sqltests/turso-tests/ivm-dual-leftjoin-update-junction-insert.sqltest`
- `tests/integration/query_processing/test_ivm_left_join.rs` (3 tests, 2 ignored — see test_left_join_cross_session_restore note about MergeOperator rowid restore; that bug is orthogonal)
- `tests/integration/query_processing/test_ivm_aggregate_filter.rs`
- All 76 tests in the `cargo test --test integration_tests ivm` filter must remain green
- The `testing/sqltests/turso-tests/ivm-*.sqltest` suite

## Open questions to resolve up-front

1. **L_INDEX vs. share with Inner's L storage** — see Phase 1. Pick before writing code.
2. **On-disk migration** — Phase 3. Likely "bump version, force rebuild" but confirm.
3. **Antijoin-on-eval-without-commit:** the same chained-matview uncommitted-read scenario that exposes [[ivm_match_counter_chained_matview_eval_stale_lpresence]]. Is the antijoin formulation still correct when `eval` doesn't persist `R_COUNT`? Answer should be yes (the formula uses `c_pre + δR_count` without depending on persisted state of L) but verify with a focused test before committing.

## Estimated total effort

3–4 days of focused work, plus careful test-first construction and 1 day buffer for the matview migration path. Recommended only after at least one more user reports a MatchCounter-class bug — until then, Option 1 is sufficient.
