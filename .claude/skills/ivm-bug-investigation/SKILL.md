---
name: ivm-bug-investigation
description: Investigate IVM/DBSP/MatView bug reports in Turso. This skill should be used when a materialized view produces wrong results, hangs, or crashes. It covers reproduction, test creation, fuzzer gap analysis, root cause analysis, and fix verification.
---

# IVM Bug Investigation

Always load [Testing skill](../testing/) and [Differential Fuzzer skill](../differential-fuzzer/) for reference.

Structured workflow for investigating bugs in Turso's Incremental View Maintenance (IVM/DBSP) system.

## Core Principle: Reproduce Before Reading

**Do not read source code until you have a reproducer or have exhausted reproduction attempts.** Bug reports contain enough information to build reproducers. Reading source code before reproducing is a procrastination pattern that wastes tokens and delays progress. The reproduction attempt itself reveals what you actually need to understand — failed attempts tell you what conditions are missing, which is far more targeted than a broad code survey.

The only exception: if you have tried 3+ reproduction variations and none trigger the bug, then read the specific code path mentioned in the bug report to understand what conditions are needed.

## When to Use

- Materialized view returns wrong row count or wrong values
- `CREATE MATERIALIZED VIEW` hangs or crashes
- CDC (change data capture) propagation produces incorrect deltas
- Chained matview (matview reading from another matview) misbehaves
- Recursive CTE matview produces incomplete results

## Task Structure

When starting an investigation, create tasks with explicit dependencies using `TaskCreate` and `TaskUpdate` (addBlockedBy/addBlocks). The dependency graph enforces this ordering:

```
1. Reproduce (no dependencies)
2. Write failing test (blocked by: Reproduce)
3. Fuzzer gap analysis & reproduction — enhance fuzzer and confirm it catches bug (blocked by: Write failing test)
4. Root cause analysis (blocked by: Fuzzer gap analysis & reproduction)
5. Implement fix (blocked by: Root cause analysis)
6. Verify all tests pass with fix (blocked by: Implement fix)
7. Update memory (blocked by: Verify all tests pass)
```

Tests come first: never start root cause analysis without a failing test that captures the bug. The test is the specification — it defines what "fixed" means.

Fuzzer before RCA: enhance the fuzzer and confirm it detects the bug on the broken code. This serves two purposes: (1) you know the fuzzer actually catches this bug class, not just passes because it never generates the right pattern, and (2) the fuzzer may reveal additional circumstances where the bug triggers, giving valuable input to root cause analysis.

## Task 1: Reproduce

**CRITICAL: DO NOT read source code during reproduction.** Build a reproducer from the bug report alone. The bug report contains the schema, trigger sequence, and symptoms — that is enough. Reading source code at this stage is a procrastination trap that delays the actual reproduction attempt by hundreds of lines of reading for zero benefit.

Only read source code if you have exhausted all reproduction attempts (3+ variations tried and failed) and genuinely cannot construct a reproducer without understanding internals.

### Build and verify CLI works

```bash
cargo build
echo "SELECT 1+1;" | timeout 10 ./target/debug/tursodb -q 2>/dev/null
```

### Create minimal reproducer SQL

Write the smallest possible SQL that triggers the bug. Use `tursodb` CLI with `--experimental-views` flag. **Start immediately** — translate the bug report's schema and trigger sequence into SQL and run it:

```bash
cat <<'SQL' | timeout 30 ./target/debug/tursodb --experimental-views -q 2>/dev/null
CREATE MATERIALIZED VIEW test_mv AS <suspect_query>;
SELECT COUNT(*) FROM test_mv;
SELECT * FROM test_mv ORDER BY <col>;
SQL
```

If the bug involves multiple matviews, rapid inserts, or specific data patterns — replicate those conditions directly from the bug report. Don't simplify prematurely; start with the reported scenario and minimize later.

### Iterate on reproduction if first attempt doesn't trigger

If the bug doesn't trigger on the first try:
1. Increase data volume (more rows, deeper recursion)
2. Add more matview types (recursive CTE + JOIN + filtered — as reported)
3. Interleave DDL and DML (CREATE MATERIALIZED VIEW between INSERTs)
4. Try file-backed database instead of `:memory:` if the bug involves page pressure

Only after 3+ failed attempts with different variations should you consider reading source code to understand what conditions are needed.

### Compare with known-working variants

Test variations to isolate the broken path:
- Regular query (no matview) — does the SQL itself work?
- `UNION` vs `UNION ALL` — does deduplication mode matter?
- Simple table source vs matview source — does chaining matter?
- Small vs large recursion depth — does iteration count matter?

Document which variants work and which don't. This narrows the root cause significantly.

## Task 2: Write Failing Test (blocked by: Reproduce)

### Choose test location

Add to `testing/runner/tests/ivm-chained-matview.sqltest` for matview-specific tests, or create a new `.sqltest` file if the bug class is distinct.

### Write the test

```
test descriptive-name {
    <setup DDL/DML>
    <CREATE MATERIALIZED VIEW ...>
    <verification SELECT>
}
expect {
    <expected output>
}
```

Each test gets its own `:memory:` database. To share state between queries, put them in the same test block.

### Run the test and confirm it fails

```bash
cargo run -p test-runner -- run testing/runner/tests/ivm-chained-matview.sqltest 2>&1 | tee /tmp/sqltest.txt
```

The test **must fail** at this point. If it passes, the test doesn't capture the bug — go back and refine.

## Task 3: Fuzzer Gap Analysis & Reproduction (blocked by: Write failing test)

**The fuzzer must reproduce 98% of bugs we encounter.** Only in very rare cases where testing a bug would massively complicate the fuzzer to a point where the downsides far outweigh the additional coverage can you skip — and even then, you MUST ask the user for permission. Never skip at your own judgement. You have a bias for trying to get code to work which is a hindrance in the long run where code quality and test coverage pay off far more.

Run the fuzzer against the **broken code** to establish that the fuzzer can catch this bug class. This must happen before the fix — otherwise you can't distinguish "fuzzer passes because fix works" from "fuzzer never generates the right pattern".

### Check current fuzzer coverage

```bash
cargo run --bin differential_fuzzer -- --matview -g sql-gen-prop -n 200 --verbose 2>&1 | tee /tmp/fuzzer.txt
grep -i "<relevant_pattern>" simulator-output/test.sql
```

### Identify why the fuzzer missed it

Common reasons:
- The SQL pattern isn't generated (e.g., no counter-style recursive CTEs)
- Random data doesn't exercise the code path (e.g., no deep tree structures)
- The oracle comparison doesn't cover the specific failure mode
- NULL values aren't generated for nullable columns

### Enhance the fuzzer

View generation lives in `testing/differential-oracle/sql_gen_prop/view.rs`. The `ViewSelectKind` enum lists generated patterns.

To add a new pattern:
1. Add variant to `ViewSelectKind` enum
2. Add to the `kinds` vec in `create_view_inner()` (with prerequisites if needed)
3. Add match arm generating the SQL and output columns
4. Build and run: `cargo run --bin differential_fuzzer -- --matview -g sql-gen-prop -n 200`

### Confirm fuzzer catches the bug on broken code

Run the enhanced fuzzer on the **current broken code** and confirm it detects the bug. This is the critical validation step — if the fuzzer doesn't catch the bug before the fix, you have no evidence it will catch regressions after. The fuzzer may also reveal additional trigger conditions that inform root cause analysis.

If the fuzzer does not catch the bug after enhancement, iterate on the enhancement until it does. Do not proceed to RCA until the fuzzer reproduces the bug.

## Task 4: Root Cause Analysis (blocked by: Fuzzer gap analysis)

Use insights from the failing test AND fuzzer results to guide analysis.

### Key IVM files

| File | What to look for |
|------|-----------------|
| `core/incremental/recursive_operator.rs` | Fixed-point iteration, `normalize_delta`, `filter_new_rows`, rowid assignment |
| `core/incremental/compiler.rs` | Circuit compilation, `ProcessingRecursive` state machine, delay feedback loop |
| `core/incremental/view.rs` | `PopulateState` state machine, `ExecutingRecursiveCircuit`, btree write logic |
| `core/translate/logical.rs` | `RecursiveCTE` struct, `union_all` flag, max iterations |

### Common root causes

- **Rowid collisions**: UNION ALL mode doesn't assign unique rowids → btree overwrites (rows keyed by rowid)
- **Delay feedback**: Recursive step doesn't receive correct delta from previous iteration
- **Base data clearing**: `input_data.clear_base()` after iteration 1 breaks joins that need base table data
- **One-shot vs persistent**: `is_one_shot()` check affects whether base data is preserved for JoinOperator
- **Negative weights**: CDC delta weights go negative, causing phantom deletions

### Hypothesis-driven debugging

1. State hypotheses sorted by probability
2. For each hypothesis, design a validation (e.g., test UNION vs UNION ALL, add tracing, check rowids)
3. Validate highest-probability hypotheses first
4. Use `perplexity-ask` MCP tool for quick ~80% reliable background research

## Task 5: Implement Fix (blocked by: Root cause analysis)

Keep fixes minimal and targeted. Common fix patterns:
- Assign unique rowids in `normalize_delta` for UNION ALL mode
- Preserve base data across iterations when joins need it
- Fix delta weight accounting in CDC propagation

## Task 6: Verify All Tests Pass (blocked by: Implement fix)

```bash
# Failing test from Task 2 now passes
cargo run -p test-runner -- run testing/runner/tests/ivm-chained-matview.sqltest 2>&1 | tee /tmp/sqltest.txt

# Existing recursive tests pass
cargo test -p turso_core -- recursive

# Enhanced fuzzer passes (multiple runs for confidence)
for i in $(seq 1 5); do
  cargo run -q --bin differential_fuzzer -- --matview -g sql-gen-prop -n 200
done

# Full test suite (check for regressions)
make -C testing/runner run-rust
```

If any test fails, go back to Task 5. Do not proceed until everything passes.

### Known pre-existing failures

These test failures are pre-existing and unrelated to IVM recursive CTE fixes:
- `matview-rollback-*` tests (rollback handling)
- `attach-write-cdc-*` tests (attached database CDC)

## Task 7: Update Memory (blocked by: Verify all tests pass)

Save the bug pattern, root cause, and fix to `MEMORY.md` for future reference.

## Important Information
- Always `tee` the output of commands to a file before filtering.
