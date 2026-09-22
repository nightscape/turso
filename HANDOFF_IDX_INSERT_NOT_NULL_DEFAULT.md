# INSERT OR REPLACE: Index Key Uses NULL Instead of NOT NULL DEFAULT

## Status: ROOT CAUSE FOUND, NOT YET FIXED

## Minimal Reproducer

```sql
CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER, c INTEGER NOT NULL DEFAULT 0, d INTEGER CHECK (d > 0));
CREATE UNIQUE INDEX idx ON t (b DESC, c, d DESC);
INSERT OR REPLACE INTO t (a, b, c, d) VALUES (1, NULL, 1892229699219097912 + -NULL, 100);
PRAGMA integrity_check;
-- Returns: row 1 missing from index idx
```

Run:
```bash
echo "CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER, c INTEGER NOT NULL DEFAULT 0, d INTEGER CHECK (d > 0));
CREATE UNIQUE INDEX idx ON t (b DESC, c, d DESC);
INSERT OR REPLACE INTO t (a, b, c, d) VALUES (1, NULL, 1892229699219097912 + -NULL, 100);
PRAGMA integrity_check;" | ./target/debug/tursodb -q
```

Does NOT require matviews, batch transactions, or any special flags. Plain SQL triggers it.

### Requirements to trigger
- A table with a NOT NULL DEFAULT column
- A secondary UNIQUE index that includes that column
- INSERT OR REPLACE where the NOT NULL column evaluates to NULL at runtime (e.g. `expr + NULL`)
- The NULL triggers the NOT NULL REPLACE resolution which substitutes the DEFAULT

## Root Cause

Bytecode generation ordering bug in `core/translate/insert.rs`.

In `emit_simple_insert()` (~line 640), the call order is:
1. `emit_preflight_constraint_checks()` (line ~715) - copies column registers to index scratch registers, emits NoConflict + IdxInsert
2. `emit_notnulls()` (line ~724) - applies NOT NULL default substitution (NotNull + Integer/String8)

The index key Copy instructions execute BEFORE the NOT NULL default fix. The index entry gets NULL, the table row gets the default value.

### Bytecode trace (from EXPLAIN)

```
37    Copy               5     20    0     r[20]=r[5]     ← copies NULL to index key register
...
51    MakeRecord         19    4     27    for idx         ← index record has c=NULL
52    IdxInsert          0     27    19                    ← inserts with NULL key
53    NotNull            5     55    0     r[5]!=NULL → 55 ← NOW checks NOT NULL
54    Integer            0     5     0     r[5]=0          ← NOW sets default
...
56    MakeRecord         3     4     7                     ← table record has c=0
57    Insert             1     7     2     t               ← table row has c=0
```

**Index entry**: `[NULL, NULL, 100, 1]`
**Table row**: `c = 0`
**Mismatch**: integrity_check fails, subsequent DELETE with IdxDelete fails.

### SQLite comparison

SQLite applies NOT NULL defaults BEFORE index key preparation. The Copy to index scratch registers happens after the default value is already in the column register.

## Fix approach

Move `emit_notnulls()` BEFORE `emit_preflight_constraint_checks()` in `emit_simple_insert()`. This matches SQLite's bytecode ordering.

Key file: `core/translate/insert.rs`

Key functions:
| Function | Line | Role |
|----------|------|------|
| `emit_simple_insert()` | ~640 | Main INSERT entry point, controls ordering |
| `emit_preflight_constraint_checks()` | ~2624 | Copies index columns and emits IdxInsert |
| `emit_notnulls()` | ~1389 | NOT NULL default substitution |
| `emit_index_column_value_for_insert()` | ~2965 | Copy from column reg to index scratch reg |

## How this was found

The differential fuzzer with `--batch-probability 0.3 --matview --seed 500` hit this bug. Batch transactions and matviews were red herrings during investigation — they just happened to create the right data conditions (a runtime-NULL expression for a NOT NULL DEFAULT column with a UNIQUE index). The actual bug is in the INSERT bytecode emitter and requires none of those features.

## Test ideas

1. Minimal reproducer above (sqltest)
2. INSERT OR REPLACE with various NOT NULL DEFAULT types (INTEGER, TEXT, REAL)
3. Plain INSERT with NOT NULL DEFAULT + UNIQUE index (not just OR REPLACE)
4. UPDATE that sets a NOT NULL DEFAULT column to NULL-expression
5. Multi-column UNIQUE index where the NOT NULL column is not the first column
