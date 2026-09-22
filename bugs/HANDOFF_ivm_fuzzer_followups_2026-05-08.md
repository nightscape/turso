# Phased hand-off: IVM bugs surfaced while fixing focus_roots null-filter (2026-05-08)

Context: while fixing the compound `IS [NOT] NULL` filter bug
(`bugs/holon_focus_roots_null_filter_2026-05-08.md`, fixed in commit
`aff40a84`), the differential fuzzer caught several other matview bugs.
Each is independent and worth its own commit.

The fuzzer was enhanced to generate compound NULL-check ANDs in
`testing/differential-oracle/sql_gen_prop/view.rs:FilteredColumns`
(`max_kind = if has_second { 7 } else { 3 }`). That enhancement is what
exposes most of these. Bugs 3, 4 also reproduce with the enhancement
reverted to `max_kind = 3`.

Run convention everywhere below:

```
cargo build --bin tursodb --bin differential_fuzzer
timeout 600 cargo run -q --bin differential_fuzzer -- --matview -g sql-gen-prop -n 200 [--seed N]
```

The failing case lands in `simulator-output/test.sql`. Reduce it to a
minimal `.sqltest` like `testing/runner/tests/ivm-null-where-filter.sqltest`
before reading source.

---

## Phase 1 — `confident_eloff`: compound IS NOT NULL + IS NULL drops a row across DML (highest priority)

Most aligned with the just-shipped fix and the easiest to land while
context is fresh.

- **Seed**: `12371730315876910877`. Matview at line 204 of
  `simulator-output/test.sql`:
  `CREATE MATERIALIZED VIEW confident_eloff AS SELECT amiable_denslow, focused_okeefe FROM upbeat_daniels WHERE amiable_denslow IS NOT NULL AND focused_okeefe IS NULL;`
- **Symptom**: at the `CREATE MATERIALIZED VIEW` statement the matview is
  correct (2 rows). After subsequent statements
  (`UPDATE upbeat_daniels SET focused_okeefe = ...` without WHERE,
  `DELETE FROM upbeat_daniels WHERE focused_okeefe = focused_okeefe`,
  `INSERT INTO upbeat_daniels` with NULL focused_okeefe), Turso ends with
  1 row, SQLite ends with 2.
- **Likely root cause area**: DBSP delta consolidation for compound
  predicates of the form `<col1> IS NOT NULL AND <col2> IS NULL`
  (one positive, one negative null check) when the source table is
  fully wiped (`DELETE FROM t`) and refilled. Could be related to the
  delta-ordering fix in `apply_view_deltas` (see MEMORY.md "IVM Matview
  UPDATE Not Propagated").
- **Reproducer scaffold**:
  ```sql
  CREATE TABLE t (a INTEGER, b INTEGER);
  INSERT INTO t VALUES (1, NULL), (2, NULL);
  CREATE MATERIALIZED VIEW v AS SELECT a, b FROM t WHERE a IS NOT NULL AND b IS NULL;
  -- v has 2 rows; both should survive these:
  UPDATE t SET b = b;          -- no-op
  DELETE FROM t WHERE b = b;   -- false for NULL b, deletes 0 rows
  -- assert v still has 2 rows
  ```
  This minimal version may not reproduce — the seed uses INSERT OR
  REPLACE, ON CONFLICT DO UPDATE, and a mass UPDATE without WHERE.
  Reduce by bisecting the statement list.
- **Skip if scope creep**: if the reduced reproducer points at
  INSERT OR REPLACE or chained matviews, file a separate bug report and
  hand back.

## Phase 2 — Aggregate matview drops duplicates that should remain (matview multiplicity)

- **Seed (representative)**: run 1 of the post-fix fuzzer batch, matview
  `sensible_mark`. SQLite returned ~18 rows; Turso returned 3.
  Several rows like `(Real(7.10e25), Null)` appeared 12 times in SQLite
  output and 1 time in Turso.
- **Likely shape**: `SELECT col, agg(*) FROM t GROUP BY col` where the
  group column is the same value across many rows but the *aggregate*
  output happens to repeat. Or `SELECT DISTINCT` at the matview level
  silently de-duplicating non-distinct rows. Differs from regular SQLite
  semantics where rows from a non-DISTINCT, non-GROUP-BY SELECT appear
  per source row.
- **Where to look**: `core/incremental/aggregate_operator.rs`,
  `core/incremental/view.rs:merge_delta`. Check whether the matview
  treats `SELECT col1, col2 FROM t WHERE …` as a multiset or set when
  identical rows are emitted from different source rows. The btree
  storage keys by rowid, so duplicates *should* survive — unless the
  consolidation step collapses identical rows.
- **Verification path**: write a minimal sqltest with 2+ source rows
  that produce identical projected rows; assert matview row count
  equals source row count; compare to a non-materialized equivalent.

## Phase 3 — Aggregate scalar divergence: SUM/AVG returning 0.0 instead of huge value (pre-existing, baseline)

- **Seed (representative)**: baseline run 3 (with my fuzzer changes
  reverted), matview `iz_crvd4cq_s_ub__dcpps3b0_w_wu`. For
  `Integer(-6995753789405549070)`, Turso returned `Real(0.0)` and
  `Real(0.0)`; SQLite returned `Real(1.2554e46)` and
  `Real(1.5361e-256)`.
- **Likely shape**: matview projects an aggregate (likely `SUM`)
  over Real values where the inputs include very large or very small
  magnitudes. Turso may be initialising the accumulator to `Integer(0)`
  and short-circuiting before promoting to `Real`, or applying integer
  overflow semantics to a Real input.
- **Where to look**: `core/incremental/aggregate_operator.rs` —
  accumulator types, NULL handling, type promotion when adding Real
  values to an Integer accumulator. Compare to the regular VDBE
  aggregate path in `core/vdbe/execute.rs` which is known-correct.
- **Pre-existing**: reproduces without my fuzzer changes, so this is a
  long-standing aggregate bug. Lower urgency unless production hits it.

## Phase 4 — Numeric type ordering: Real(1.0) vs Integer(1) sort differently

- **Seed (representative)**: run 2 of post-fix batch, matview
  `q_i_w_0_h1_t_mbg__4h__p`. Same logical row content, but rows ordered
  Real-then-Integer in Turso vs Integer-then-Real in SQLite for keys
  that compare equal numerically.
- **Likely shape**: matview ORDER BY (or btree key ordering) with mixed
  Integer/Real values. SQLite's numeric type-affinity rule says
  `Integer(1) == Real(1.0)` so ordering is determined by the second key
  or by insertion order. Turso may be using byte-level value comparison.
- **Where to look**: `core/types.rs` Value comparison; matview btree
  key encoding. The fix from MEMORY ("INSERT OR REPLACE Index Key /
  NOT NULL DEFAULT Mismatch") shows recent activity around index key
  encoding; this might be related.
- **Lower urgency**: ordering-only divergence rarely breaks
  applications, but it does break the differential oracle, so worth
  fixing to clear the fuzzer noise.

## Phase 5 — Parser: ORDER BY positional reference rejected (not really IVM)

- **Seed (representative)**: run 3 of post-fix batch.
  `Parse error: 1st ORDER BY term out of range - should be between 1 and N`.
  The SELECT has an ORDER BY expression that *evaluates* to an integer
  larger than the column count; SQLite tolerates this.
- **Likely shape**: SQLite only treats integer literals in ORDER BY as
  positional references; Turso may be evaluating non-literal expressions
  as positional references too.
- **Where to look**: `parser/src/parser.rs` ORDER BY handling, or the
  translator phase that resolves positional references in
  `core/translate/select.rs`.
- **Not IVM**: punt to the parser/translate folks — log as a separate
  bug and skip unless trivial.

---

## Suggested execution order

1. **Phase 1 first** (~half day). Fresh on the recent fix, related code,
   high-confidence reproducer with the new fuzzer enhancement. Likely
   the same shape of bug we just shipped — a delta/consolidation gap
   for the new compound-NULL-check predicates.
2. **Phase 2** if Phase 1 lands cleanly (~half day). Same code area
   (DBSP merge / consolidate). Possibly the same root cause.
3. **Phases 3-5** as separate, smaller tickets. Each is independent and
   would be picked up by whoever owns aggregate / numeric / parser work.

For each phase, follow `.claude/skills/ivm-bug-investigation/SKILL.md`:
reproduce → failing sqltest → fuzzer gap analysis → RCA → fix → verify.
The fuzzer enhancement is already in place; keep it.
