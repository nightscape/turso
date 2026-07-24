# Handoff: DBSP zset clone churn in `turso_core::incremental::dbsp`

## Problem

Heap profiling of Holon (downstream consumer, heavy IVM/matview user) shows the DBSP operator graph is the #2 allocation-churn family in the whole application: **~264MB / ~3.4M allocations in a ~6-minute session** whose data volume was only ~1000 rows. Live-at-end for these sites is ~0MB — this is pure transient churn (CPU + allocator pressure), not a leak. The amplification factor (millions of clones for a thousand rows) is the issue: rows appear to be deep-cloned per operator hop per change event.

## Evidence (dhat, rust-heap mode, release build)

Two dominant program points, same family (frames innermost-first, abridged):

```
175.1MB total, 210,457 blocks (≈830B avg), live-at-end 0
  <turso_core::types::Value as ConvertVec>::to_vec
  clone<turso_core::types::Value>
  clone                                  // HashableRow::clone (its Vec<Value>)
  clone<(HashableRow, isize)>
  to_vec<(HashableRow, isize)>           // cloning a whole zset Vec
  clone
  process_node
  execute_node
  execute_node                           // nested operator evaluation
```

```
89.2MB total, 3,156,581 blocks (≈28B avg), live-at-end 0
  <turso_core::types::Value as ConvertVec>::to_vec
  clone<turso_core::types::Value>
  clone<(HashableRow, isize)>
  to_vec<(HashableRow, isize)>
  clone / clone
  process_node
  execute_node
  execute_node
```

Reading the stacks: somewhere in `process_node` (reached via nested `execute_node` calls), an entire `Vec<(HashableRow, isize)>` (a zset / delta batch) is cloned, which deep-clones every `HashableRow`, which deep-clones its `Vec<Value>`, which clones every `Value` (the 28B-avg site is the per-Value/inline-string tail; the 830B-avg site is the row-vector backbone). A separate ~2.8MB *retained* PP with the same `clone<(HashableRow, isize)>` shape sits under operator-state persistence, so a small portion of clones is legitimately for state storage.

Workload that produced this: initial bulk insert of ~1000 `block` rows + ongoing CDC while ~5–10 materialized views over `block` (and views-of-views patterns typical of Holon) were live, plus interactive navigation/scroll. Numbers were stable across two independent runs (264MB and ~180MB in a run with a different interaction mix).

## Hypotheses for the fix (in rough preference order)

1. **Don't clone deltas between operators — move or borrow.** If `process_node` receives a delta the caller no longer needs, take it by value; if the caller does need it, operators that only *read* (filters, projections re-emitting subsets) should iterate `&[(HashableRow, isize)]` and clone only the rows they actually emit. The `to_vec` on the whole tuple-vec suggests at least one operator clones its entire input unconditionally — likely fan-out (one delta feeding multiple downstream operators) cloning per consumer.
2. **`Arc` the row payload inside zsets**: `(Arc<HashableRow>, isize)` or `HashableRow { values: Arc<[Value]>, ... }`. Fan-out and pass-through become refcount bumps. Hash/Eq stay defined on the row content; weight consolidation only touches the `isize`. Serialization to the `__turso_internal_dbsp_state_v1_*` BLOBs happens at the persistence boundary and is unaffected (it needs bytes either way).
3. **Cheaper `Value` clones** as a fallback if zset structure can't change: interning/`Arc`-backed text payloads would shrink the 3.1M-allocation tail, but it treats the symptom — the row-level clone is the amplifier.

## Where to look

- `turso_core::incremental::dbsp` — `HashableRow`, the `(HashableRow, isize)` zset representation, `process_node`, and the `execute_node` recursion (operator graph evaluation). Look specifically for `.clone()` / `.to_vec()` on whole delta vectors at operator boundaries and at fan-out points.
- The retained-state write path (operator state → `__turso_internal_dbsp_state_v1_*`) to distinguish legitimate ownership transfers from redundant copies.

## Repro / measurement inside the Turso repo

- Standalone: create 2–3 materialized views over one table (include a view with a join or aggregation, and ideally a chained/derived view), insert ~1k rows, then apply ~1k single-row updates. Wrap in a dhat-rs harness (`dhat::Profiler::new_heap()` in a test/bench) and assert on `total_blocks` for the clone sites, or just compare before/after profiles in dh_view.
- Cheap proxy metric without dhat: a debug counter on `HashableRow::clone` — the current workload shape gives ~3.4M clones per ~1k logical row-changes; post-fix this should drop by orders of magnitude.
- Downstream validation: Holon's `/turso-sql-replay` tool (`tools/src/turso_sql_replay.rs` in the holon repo) can replay captured SQL traces against a patched turso_core to confirm both correctness (matview consistency checks built in) and the churn reduction under the real query mix.

## Risks / acceptance

- IVM correctness is the hard constraint: weight consolidation, multiset semantics, and operator-state persistence must be bit-identical. Holon has history with matview drift and multiset panics here — run the existing IVM test suite plus a holon-side `general_e2e_pbt` (Full slice) against the patched core before calling it done.
- Accept when: clone count per row-change drops ~100×, dhat shows the `process_node` clone family gone from the top-10 churn list, and no matview-consistency regressions in either repo's suites.

---

## Notes from the Holon side

- The upstream B-tree record-buffer churn (`get_immutable_record_or_create` allocating a fresh `Vec<u8>` per seek — ~741MB in the same session) is a separate, even larger family if you're already in there.
- Holon's with-capacity row-map fix is already applied downstream, so post-fix profiles should isolate DBSP cleanly.
