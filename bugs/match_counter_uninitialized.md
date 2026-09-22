# Bug: `MatchCounterOperator::eval called with Uninitialized state`

## Summary

`MatchCounterOperator::process_match_counter_state` calls
`return_if_io!(read_r_count(...))` (line 484) and
`return_if_io!(read_next_join_row(...))` (line 531) **without first
restoring `*outer`**. The parent `eval_internal` loop replaces `*state`
with `EvalState::Uninitialized` at the top of every iteration via
`mem::replace`. When the inner read suspends on I/O, control returns
all the way up to the caller with `*state` still `Uninitialized`. The
next `commit` cycle re-enters `eval`, the `Uninitialized` arm fires,
and the operator panics at line 378.

## Reproducer

Two artifacts:

- `bugs/match_counter_uninitialized_repro.rs` — Rust integration test.
  Drop into `tests/integration/query_processing/`, register in `mod.rs`,
  run:
  ```
  cargo test --test integration_tests \
      match_counter_uninitialized_repro -- --nocapture
  ```
- `bugs/match_counter_uninitialized_repro.sql` — SQL-only repro of the
  same shape (does **not** trigger the panic on its own — synchronous
  `tursodb` shell never suspends on I/O during eval; included for
  schema reference).

The Rust test panics consistently on `81cef68c` and on the current tip
of `nightscape@holon` (`d24ff84c6` / `2ce119f9e`).

## Stack trace

```
panicked at core/incremental/match_counter_operator.rs:378:21:
MatchCounterOperator::eval called with Uninitialized state
   2: MatchCounterOperator::eval_internal
        match_counter_operator.rs:378:21
   3: <MatchCounterOperator as IncrementalOperator>::eval
        match_counter_operator.rs:687:40
   4: <MatchCounterOperator as IncrementalOperator>::commit
        match_counter_operator.rs:709:30
   5: DbspNode::process_node                 compiler.rs:680:43
   6: DbspCircuit::execute_node              compiler.rs:1520:44
   7-11: DbspCircuit::execute_node           compiler.rs:1499:56  (5 levels)
  12: DbspCircuit::run_circuit               compiler.rs:889:18
  13: DbspCircuit::commit                    compiler.rs:1145:30
  14: IncrementalView::merge_delta           view.rs:1973:55
  15: Program::apply_view_deltas             vdbe/mod.rs:1881:36
  16: Program::commit_txn                    vdbe/mod.rs:1960:20
  17: execute::halt                          execute.rs:3063:14
  18: execute::op_halt                       execute.rs:3170:5
```

## Root cause (proposed)

`process_match_counter_state` (`match_counter_operator.rs:431`) takes
`outer: &mut EvalState`. Some inner arms write `*outer` before
returning IO/Done; **two arms do not**:

```rust
// Line 484
RKeyScan::ReadingRCount { .. } => {
    let c_pre = return_if_io!(read_r_count(r_storage_id, join_key_hash, cursors));
    //          ^^^^^^^^^^^^^^^ if this returns IO, *outer not restored.
    ...
}

// Line 531
RKeyScan::ScanningL { emit_weight, last_l_hash } => {
    let scan = return_if_io!(read_next_join_row(
        lp_storage_id, &key_join, last_l_hash, cursors
    ));
    //         ^^^^^^^^^^^^^^^ same issue.
    ...
}
```

The corresponding `LRowResolve` arms in `ProcessLDelta` likely have the
same shape and need an audit too.

## Suggested fix

Either:

1. Use `return_and_restore_if_io!` instead of `return_if_io!` (mirrors
   what `commit` does at lines 706, 779, 821). Save the in-progress
   inner state into `*outer` before the read.
2. Restructure each arm to write `*outer = MatchCounter(Box::new(...))`
   before any `return_if_io!`.

Option 1 is the smallest diff.

## Downstream impact (holon)

Holon's `block` matview is dual-LEFT JOIN +
`json_group_array(...) FILTER` + GROUP BY (the same shape as the
`test_left_join_dual_filter_aggregation_holon_shape` test at
`tests/integration/query_processing/test_ivm_left_join.rs:227`).
Holon's `TursoBackend::Actor` wraps every command in `catch_unwind` so
the panic doesn't kill the process; it logs:

```
ERROR holon::storage::turso: [TursoBackend::Actor] Caught panic during
  command processing: MatchCounterOperator::eval called with
  Uninitialized state. Actor continues.
WARN turso_core::incremental::match_counter_operator:
  [MatchCounterOperator::commit] Recovering from Invalid state.
  Resetting to Idle.
```

But the IVM state is corrupted by then — downstream PBT assertions see
"missing blocks", `tags = ["[]"]` rows (the literal JSON-encoded empty
array string showing up as a one-element array), and other CDC
divergences. Holon PBT logs ~20 occurrences/run on the SqlOnly variant
and similar on Full.
