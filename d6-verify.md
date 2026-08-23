# D6 CountColumn — independent verification

**VERDICT: REFUTED.**

Four of the claim's five clauses hold and I reproduced every gate myself. The
persistence clause does not hold across the version boundary: a materialized
view created by a **pre-fix** build reads back a wrong `count(x)` under this
build, and the first write after the upgrade moves the count to a value that
matches neither the old nor the new semantics. The lane report does not
disclose this, and §7 "Deferred" lists only the SUM/AVG gaps.

## STEP 0 — tree identity

```
core/incremental/aggregate_operator.rs:111:const AGG_FUNC_COUNT_COLUMN: i64 = 12;
sqlite/conformance/turso-sqltests/matview_aggregate_null_column.sqltest  EXISTS
```
Correct tree.

## The defect

`AggregateState::from_value_vector` (core/incremental/aggregate_operator.rs:993)
reads each aggregate's function code **out of the persisted blob**
(`AggregateFunction::from_values`, line 1027). A blob written before this change
carries `AGG_FUNC_COUNT` for `count(x)`, so it deserializes as
`AggregateFunction::Count` and leaves `column_counts` empty. `apply_delta`
(line 1348) then uses the **compiler's** list, which now says
`CountColumn(col_idx)`, so the per-column counter starts from 0 and only counts
rows written after the upgrade.

### Reproduction

Old binary: `/Users/martin/Workspaces/bigdata/turso/target/debug/tursodb`
(base build, `strings | grep -c "COUNT(col"` = 0).
New binary: this workspace's `target/debug/tursodb` (= 3).

```sql
-- session 1
CREATE TABLE t(id INTEGER PRIMARY KEY, x INTEGER);
INSERT INTO t VALUES (1,NULL),(2,5),(3,7);
CREATE MATERIALIZED VIEW v AS SELECT count(x) AS cx, count(*) AS n FROM t;
-- session 2
SELECT cx,n FROM v;
INSERT INTO t VALUES (4,9);
SELECT cx,n FROM v;
```

| run | first `cx,n` | after `INSERT (4,9)` |
|---|---|---|
| sqlite3 (truth) | `2,3` | `3,4` |
| CONTROL A: OLD creates, OLD reopens | `3,3` | `4,4` (self-consistent old semantics) |
| CONTROL B: NEW creates, NEW reopens | `2,3` | `3,4` (correct) |
| **OLD creates, NEW reopens** | **`3,3`** | **`1,4`** |

The count **decreases from 3 to 1 on an INSERT**. Both controls are clean, so
this is the cross-version blob read, not base-revision drift. Silent wrong
results, no error raised — the failure mode "Correctness paramount" in
CLAUDE.md exists to catch.

## What I confirmed

1. **`jj diff --stat` — exactly the 5 claimed files.** No extras.
2. **SUM/AVG/FILTER untouched.** `jj diff --git` on aggregate_operator.rs has
   **no removed line** mentioning `AGG_FUNC`/`sum`/`avg`/`filter`; the only
   hits are three added `AGG_FUNC_COUNT_COLUMN` lines. No existing `AGG_FUNC_*`
   value renumbered. All four `AggregateFunction` match sites gained explicit
   arms (no wildcard swallowed the new variant).
3. **`cargo test count_column_reopen`** (run from `tests/`, log
   `/tmp/d6v-reopen-full.log`):
   `test result: ok. 2 passed; 0 failed` — both named tests listed by name.
4. **turso-sqltests** (log `/tmp/d6v-sqltest.log`, runner invoked directly, not
   via make): `1666 passed, 3 failed, 341 skipped`. The 3 are the pre-existing
   `attach-write-cdc-{insert,delete,update}` / "no such table: aux.t1". All five
   new tests PASS by name.
5. **`cargo test -p turso_core incremental`** (log `/tmp/d6v-incr.log`):
   `262 passed; 0 failed; 2 ignored`.

## Adversarial probes — all matched sqlite3 (`/tmp/d6probe/`)

Same scripts run against `sqlite3` (plain view) and `tursodb --experimental-views`
(materialized view); count columns identical in every case.

- **all-NULL → has-value → all-NULL by UPDATE, both directions, per group**, then
  DELETE of the only non-NULL row (`p1_updates`) — identical.
- **`count(*)`, `count(x)`, `count(DISTINCT x)` in ONE matview** plus delete of a
  duplicate value and NULL-ing a row (`p2_mixed`) — identical (`5|3|2`,
  `4|2|2`, `4|1|1`).
- **duplicate `count(x), count(x)`** (`p3_dup`) — `2|2`, correct; my
  double-increment hypothesis (`apply_delta` has no dedup mask for
  `CountColumn`, unlike `processed_counts` for the distinct aggregates) is
  refuted in practice, the planner collapses the identical aggregates.
- **`count(b.x)` across a JOIN with GROUP BY** (`p4_join`) — identical;
  `input_schema.find_column` index lines up with the delta row layout.
- **TEXT column incl. empty string** (`p5_text`) — identical.
- **reopen with `count(*)`+`count(x)`+`count(DISTINCT x)`+`sum(x)` in one blob**,
  then INSERT and DELETE (`p6`) — identical to sqlite3 at all four checkpoints.

## Minor, not defects

- `AggregateFunction::from_sql_function` has **no callers** anywhere in the repo
  (`grep -rn from_sql_function` hits only its own definition at
  aggregate_operator.rs:444 and the lane report). The `AggFunc::Count0` /
  `AggFunc::Count` split added there is dead code, and §1's list of "eleven
  touched sites" counts it as if it were live. The real path is
  compiler.rs:2788-2819.
- `agg_col_idx` mapping `CountColumn(_) => None` (compiler.rs:3046) is sound as
  argued: `FILTER` on `Count`/`Count0` is rejected earlier by
  `always_unsupported` (compiler.rs:2769), so `CountColumn` can never carry one.

---

# Delta re-verification (cross-version guard)

**DELTA VERDICT: CONFIRMED.** The defect above is fixed, the fix is generic
rather than written around `CountColumn`, and the disclosed residual is exactly
what the lane describes.

Diff is now 7 files + this report (`operator.rs` and `persistence.rs` joined the
original 5).

## 1. The repro no longer corrupts — it refuses loudly

Same two binaries as before. `/tmp/d6probe/mig2.db`, created by the OLD binary
(`cx=3`), then opened by the rebuilt binary (`target/debug/tursodb`, 03:20,
`strings | grep -c "REFRESH MATERIALIZED VIEW"` = 2):

```
--stale read--     cx,n = 3,3          (no error)
--now the write--  INSERT INTO t VALUES (4,9);
Runtime error: Internal error: Persisted aggregate state was written for a
different set of aggregates (stored [Count, Count], view expects
[CountColumn(1), Count]). Rebuild the view with REFRESH MATERIALIZED VIEW.
--after write--    cx,n = 3,3          (unchanged)
```

The old `1,4` is gone. The message names the recovery.

**The refused write is rolled back cleanly**, which the lane did not claim and I
checked separately: `SELECT count(*), count(x) FROM t` after the failure returns
`3, 2` — the base table did not keep row 4, so base data and view do not diverge.

### REFRESH recovers fully

```
REFRESH MATERIALIZED VIEW v;
cx,n = 2,3     <- sqlite3 truth for the original 3 rows
INSERT (5,11)  -> 3,4     correct
INSERT (6,NULL)-> 3,5     correct, NULL still skipped
```

## 2. Residual is stale, not corrupt — accurate

Pre-refresh `SELECT cx,n FROM v` returns the stale `3,3` with no error, and
returns the same `3,3` after the refused write. Matview reads come from the
output btree, which the guard never touches; only maintenance reads the state
blob. The lane's stale-read/refused-write split is exactly right.

## 3. No un-guarded deserialization path survives

Both entry points now require the list — there is no defaulted or old-signature
variant anywhere:

- `AggregateState::from_value_vector(values, expected: &[AggregateFunction])` (aggregate_operator.rs:1001)
- `AggregateState::from_blob(blob, expected: &[AggregateFunction])` (aggregate_operator.rs:1317, delegates to the above at :1360)

Every caller (`grep -rn "from_value_vector\|from_blob\|read_record" core/`):

- `persistence.rs:97` — production, inside `ReadRecord::read_record`, which
  itself gained `expected` and returns `Result`. Its single caller
  `aggregate_operator.rs:774` passes `&operator.aggregates`.
- `operator.rs:388` — `get_current_state_from_btree`, a **test helper**: it sits
  inside `#[cfg(test)] mod tests` (opened at operator.rs:286) and all 33 callers
  are test functions. Its `eprintln!`-and-skip became `panic!`, which is
  therefore test-only and does not add a production panic.
- the 5 new unit tests.

The guard itself (`if persisted != expected`, aggregate_operator.rs:1291) is a
whole-list `PartialEq` over the enum, not a `CountColumn` special case.

## 4. Gates

- `cargo test -p turso_core incremental` (`/tmp/d6v2-incr.log`):
  **267 passed; 0 failed; 2 ignored** — the claimed number. All five new tests
  listed by name as `ok`, plus the rewritten
  `operator::tests::test_aggregate_serialization_with_different_column_indices`.
- `cargo test count_column_reopen` from `tests/` (`/tmp/d6v2-reopen.log`):
  **2 passed; 0 failed**.
- turso-sqltests (`/tmp/d6v2-sqltest.log`): **1666 passed, 3 failed, 341 skipped**
  — the same 3 pre-existing `attach-write-cdc-*`; all 5 new tests PASS by name.
  Unchanged from the pre-delta run, so the guard on every state read costs no
  regression.

## 5. Swapped-column blobs are caught

Covered at two granularities, both passing:

- `state_written_for_a_different_column_is_rejected` — same length, same
  function, `CountColumn(1)` blob read as `CountColumn(2)`: rejected.
- `operator::tests::test_aggregate_serialization_with_different_column_indices`
  — end-to-end through the btree: state written for `SUM(col1)/MIN(col3)` read
  back as `SUM(col3)/MIN(col1)`. This test previously **asserted the silent
  restart** (`SUM(col3)` = 4 from new data only); the lane inverted it to
  `expect_err` and asserts the message contains `REFRESH MATERIALIZED VIEW`.
  That inversion is the right call — the old expectation encoded the same
  silent-wrong-answer class as the bug I filed.

Also rejected: a longer list, and a different function over the same column
(`Sum(1)` blob read as `Avg(1)`) — so the guard is not `CountColumn`-shaped.

## 6. No regression on the same-version path

All six probes from the original pass re-run against the rebuilt binary produce
**byte-identical output** to the pre-delta run (`diff /tmp/d6probe/turso.log`
vs `/tmp/d6v2-probes.log` — only the section headers differ), and the p6 reopen
still matches sqlite3 at all four checkpoints.
