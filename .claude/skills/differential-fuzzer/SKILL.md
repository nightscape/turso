---
name: differential-fuzzer
description: Information about the differential fuzzer tool, how to run it and use it catch bugs in Turso. Always load this skill when running this tool
---

# Differential Fuzzer

Always load [Debugging skill for reference](../debugging/)

The differential fuzzer compares Turso results against SQLite for generated SQL statements to find correctness bugs.

## Location

`testing/differential-oracle/fuzzer/`

## Running the Fuzzer

### Single Run

```bash
# Basic run (100 statements, random seed)
cargo run --bin differential_fuzzer

# With specific seed for reproducibility
cargo run --bin differential_fuzzer -- --seed 12345

# More statements with verbose output
cargo run --bin differential_fuzzer -- -n 1000 --verbose

# Keep database files after run (for debugging)
cargo run --bin differential_fuzzer -- --seed 12345 --keep-files

# All options
cargo run --bin differential_fuzzer -- \
  --seed <SEED>           # Deterministic seed
  -n <NUM>                # Number of statements (default: 100)
  -t <NUM>                # Number of tables (default: 2)
  -c <NUM>                # Columns per table (default: 5)
  --verbose               # Print each SQL statement
  --keep-files            # Persist .db files to disk
```

### Continuous Fuzzing (Loop Mode)

```bash
# Run forever with random seeds
cargo run --bin differential_fuzzer -- loop

# Run 50 iterations
cargo run --bin differential_fuzzer -- loop 50
```

### Docker Runner (CI/Production)

```bash
# Build and run from repo root
docker build -f testing/differential-oracle/fuzzer/docker-runner/Dockerfile -t fuzzer .
docker run -e GITHUB_TOKEN=xxx -e SLACK_WEBHOOK_URL=xxx fuzzer
```

Environment variables for docker-runner:
- `TIME_LIMIT_MINUTES` - Total runtime (default: 1440 = 24h)
- `PER_RUN_TIMEOUT_SECONDS` - Per-run timeout (default: 1200 = 20min)
- `NUM_STATEMENTS` - Statements per run (default: 1000)
- `LOG_TO_STDOUT` - Print fuzzer output (default: false)
- `GITHUB_TOKEN` - For auto-filing issues
- `SLACK_WEBHOOK_URL` - For notifications

## Output Files

All output goes to `simulator-output/` directory:

| File | Description |
|------|-------------|
| `test.sql` | All executed SQL statements. Failed statements prefixed with `-- FAILED:`, errors with `-- ERROR:` |
| `schema.json` | Database schema at end of run (or at failure) |
| `test.db` | Turso database file (only with `--keep-files`) |
| `test-sqlite.db` | SQLite database file (only with `--keep-files`) |

## Reproducing Errors

Always follow these steps

1. **Find the seed** in the error output:
   ```
   INFO: Starting differential_fuzzer with config: SimConfig { seed: 12345, ... }
   ```

2. **Re-run with that seed**:
   ```bash
   cargo run --bin differential_fuzzer -- --seed 12345 --verbose --keep-files
   ```

3. **Check output files**:
   - `simulator-output/test.sql` - Find the failing statement (look for `-- FAILED:`)
   - `simulator-output/schema.json` - Check table structure at failure time

4. **Create a minimal reproducer**
   - Create reproducer in `.sqltest` or in `.rs` always load [Debugging skill for reference](../debugging/)

5. **Compare behavior manually**:
   If needed try to compare the behaviour and produce a report in the end.
   Always write to a tmp file first with Edit tool to test the sql and then pass it to the binaries.
   ```bash
   # Run failing SQL against SQLite
   sqlite3 :memory: < simulator-output/test.sql

   # Run against tursodb CLI
   tursodb :memory: < simulator-output/test.sql
   ```

## Understanding Failures

### Oracle Failure Types

1. **Row set mismatch** - Turso returned different rows than SQLite
2. **Turso errored but SQLite succeeded** - Turso rejected valid SQL
3. **SQLite errored but Turso succeeded** - Turso accepted invalid SQL
4. **Schema mismatch** - Tables/columns differ after DDL

### Warning (non-fatal)

- **Unordered LIMIT mismatch** - LIMIT without ORDER BY may return different valid rows

## Key Source Files

| File | Purpose |
|------|---------|
| `main.rs` | CLI parsing, entry point |
| `runner.rs` | Main simulation loop, executes statements on both DBs |
| `oracle.rs` | Compares Turso vs SQLite results |
| `schema.rs` | Introspects schema from both databases |
| `memory/` | In-memory IO for deterministic simulation |

## Materialized View Fuzzing

The `--matview` flag enables fuzzing of Turso's IVM (Incremental View Maintenance) by comparing materialized views against SQLite's regular views.

### How It Works

The core trick: both databases get the same view, but Turso gets a **materialized** one (maintained incrementally via DBSP) while SQLite gets a **regular** one (recomputed on every query). Then `SELECT * FROM view_name` on both should return identical rows — any divergence is a real IVM bug.

**Split execution**: When a `CREATE MATERIALIZED VIEW v AS SELECT ...` is generated:
- Turso receives: `CREATE MATERIALIZED VIEW v AS SELECT ...`
- SQLite receives: `CREATE VIEW v AS SELECT ...`

For `DROP VIEW`, both databases receive the same SQL (no `MATERIALIZED` keyword needed).

### Running

```bash
# Matview fuzzing requires the proptest generator
cargo run --bin differential_fuzzer -- --matview -g sql-gen-prop --seed 12345 -n 200 --verbose

# Soak test
cargo run --bin differential_fuzzer -- loop 20 --matview -g sql-gen-prop

# Check what matview SQL was generated
grep -i "MATERIALIZED" simulator-output/test.sql
grep "^-- SQLITE:" simulator-output/test.sql
```

The `--matview` flag:
- Enables `DatabaseOpts::with_views(true)` on the Turso database
- Sets `create_materialized_view_weight: 3` and `drop_materialized_view_weight: 1` in the proptest profile
- Only works with `-g sql-gen-prop` (the sql_gen backend doesn't generate matview DDL)

### Schema Divergence Handling

A matview in Turso creates a **table** in `sqlite_master` (type='table'), while a regular view in SQLite creates a **view** (type='view'). Since the schema introspector only queries `type='table'`, the matview table appears in Turso's schema but not SQLite's.

The runner tracks matview names in a `HashSet<String>` and filters them from Turso's table set before schema comparison.

### Generated View Queries

The generator produces 4 kinds of view SELECT:
- **Star**: `SELECT * FROM t`
- **Filtered columns**: `SELECT col1, col2 FROM t WHERE col IS NOT NULL`
- **Aggregate**: `SELECT col, COUNT(*) AS cnt FROM t GROUP BY col`
- **JOIN**: `SELECT t1.col, t2.col FROM t1 JOIN t2 ON t1.pk = t2.fk` (requires 2+ tables)

JOIN views are the most interesting for finding IVM bugs involving multiple base tables.

### Output Format

Matview DDL appears in `simulator-output/test.sql` with paired comments:
```sql
CREATE MATERIALIZED VIEW v AS SELECT ...;
-- SQLITE: CREATE VIEW v AS SELECT ...;
```

### Key Files for Matview Fuzzing

| File | What it does |
|------|-------------|
| `sql_gen_prop/view.rs` | `create_materialized_view()`, `drop_materialized_view_for_schema()`, 4 SELECT kinds |
| `sql_gen_prop/schema.rs` | `materialized_views` field, `materialized_view_names()` accessor |
| `sql_gen_prop/statement.rs` | `CreateMaterializedView` / `DropMaterializedView` enum variants |
| `fuzzer/generate.rs` | `is_matview_ddl` + `sqlite_sql` on `GeneratedStatement`, matview-aware schema conversion |
| `fuzzer/runner.rs` | Split execution logic, matview name tracking, schema comparison filtering |

### Limitations / Future Work

- DML (INSERT/UPDATE/DELETE) on base tables automatically exercises IVM through the existing oracle — whenever a SELECT hits a matview, both databases return their version and the oracle compares
- Explicit `SELECT * FROM matview_name` statements are not yet generated (the matview is not added to the schema's queryable sources). This would increase coverage
- Only the proptest backend supports matview generation; the sql_gen backend ignores the `--matview` flag

## Tracing

Set `RUST_LOG` for more detailed output:

```bash
RUST_LOG=debug cargo run --bin differential_fuzzer -- --seed 12345
```
