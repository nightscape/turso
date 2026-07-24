# Task: Support compound equi-join conditions in IVM materialized views

## Problem

Materialized views fail when the underlying SELECT uses JOIN conditions with multiple equality predicates connected by AND. This blocks a key use case: graph query languages (GQL/Cypher) compiled to SQL over an Entity-Attribute-Value schema.

**Error:** `"Only simple column references are supported in join conditions for incremental views"`

**Example SQL that fails:**

```sql
CREATE MATERIALIZED VIEW person_names AS
SELECT COALESCE(_npt.value, _npi.value) AS "name"
FROM nodes AS _v0
JOIN node_labels AS _nl ON _nl.node_id = _v0.id
JOIN property_keys AS _pk ON _pk.key = 'name'
LEFT JOIN node_props_text AS _npt ON _npt.node_id = _v0.id AND _npt.key_id = _pk.id
LEFT JOIN node_props_int  AS _npi ON _npi.node_id = _v0.id AND _npi.key_id = _pk.id
WHERE _nl.label = 'Person'
```

This is a standard EAV (Entity-Attribute-Value) pattern where property tables require two-column joins (`node_id` + `key_id`). The `AND` in the ON clause gets split into separate equijoin pairs by `extract_equijoin_conditions`, but then compilation fails.

## Root cause analysis

There are **two independent blockers**. Both must be fixed.

### Blocker 1: Compound ON conditions produce non-Column LogicalExpr

**File:** `core/translate/logical.rs:1270-1277`

`extract_equijoin_conditions` splits `ON a.x = b.y AND a.z = c.w` into pairs by recursing on AND. For each `Equals` arm, it calls:
```rust
let left_expr = self.build_expr(lhs, left_schema)?;
let right_expr = self.build_expr(rhs, right_schema)?;
equijoins.push((left_expr, right_expr));
```

The problem: `build_expr(lhs, left_schema)` resolves columns against `left_schema` only. In `_npt.node_id = _v0.id`, `_npt` is the right-side table being joined, so `_npt.node_id` can't be found in `left_schema`. `build_expr` may produce a `LogicalExpr::BinaryExpr` or error out rather than a `LogicalExpr::Column`.

Then in the DBSP compiler (`core/incremental/compiler.rs:1670`), the pattern match requires both expressions to be `LogicalExpr::Column`:
```rust
if let (LogicalExpr::Column(first_col), LogicalExpr::Column(second_col)) = (left_expr, right_expr) {
    // ok
} else {
    return Err(...)  // <-- THIS IS THE ERROR WE HIT
}
```

**Fix:** In `extract_equijoin_conditions`, when processing an equality condition, build both sides against a *combined* schema (like the `_` fallback arm already does at line 1299). Then determine which column belongs to left vs right based on table qualifiers, not which schema the expression was built against. `resolve_join_columns` (compiler.rs:1200-1251) already handles this left/right disambiguation — the issue is that the expressions arrive in the wrong form.

### Blocker 2: LEFT OUTER JOIN not supported in IVM

**File:** `core/incremental/join_operator.rs:386-390`

```rust
JoinType::Left => {
    return Err(LimboError::ParseError(
        "LEFT OUTER JOIN is not yet supported in incremental views".to_string(),
    ))
}
```

The EAV SQL uses `LEFT JOIN` for property lookups because a node may not have every property type. This is fundamental to the EAV pattern — you can't use INNER JOIN without losing nodes that have, say, a text property but no int property.

**Fix:** Implement LEFT JOIN in the DBSP join operator. The incremental formula changes from:

- **INNER:** `δ(R ⋈ S) = (δR ⋈ S) ∪ (R ⋈ δS) ∪ (δR ⋈ δS)`
- **LEFT:** `δ(R ⟕ S) = (δR ⟕ S) ∪ (R ⟕ δS) ∪ (δR ⟕ δS)` — but unmatched left rows emit NULLs for right columns, and insertions into S may "upgrade" previously-null rows (requiring a retraction + insertion)

The `JoinOperator` already has `left_key_indices: Vec<usize>` and `right_key_indices: Vec<usize>` as vectors, so composite key support is already baked in at the execution level. The work is in the join semantics, not the key extraction.

## Files to modify

| File | What to change |
|---|---|
| `core/translate/logical.rs:1270-1277` | In `extract_equijoin_conditions`, build both sides of `=` against a combined schema, then split into left/right columns based on table qualifiers |
| `core/incremental/compiler.rs:1670-1686` | The pattern match and `resolve_join_columns` should already work once the logical planner produces correct `LogicalExpr::Column` pairs — verify this |
| `core/incremental/join_operator.rs:386-390` | Remove the LEFT JOIN error, implement left-join semantics with NULL emission for unmatched left rows |
| `core/incremental/join_operator.rs` (process method) | Handle delta processing for LEFT JOIN: track which left keys have matches, emit NULL-padded rows for unmatched, handle match/unmatch transitions on deltas |

## Suggested implementation order

1. **Fix Blocker 1 first** (compound ON conditions) — this is the simpler change and unblocks INNER JOINs with composite keys
2. **Then fix Blocker 2** (LEFT JOIN) — this is more involved but well-understood in DBSP literature

## Verification

Test from the `gql-to-sql` repo:
```bash
cargo test -p graph-executor --test turso_compat -- --include-ignored 2>&1 | tee /tmp/turso-test.log
```

The two `#[ignore]`d tests (`test_matview_match_all`, `test_matview_with_where`) should pass once both blockers are fixed.

Minimal standalone repro (no gql-to-sql dependency needed):
```rust
#[tokio::test]
async fn test_compound_join_matview() {
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build().await.unwrap();
    let conn = db.connect().unwrap();

    conn.execute_batch("
        CREATE TABLE items (id INTEGER PRIMARY KEY);
        CREATE TABLE attrs (item_id INTEGER, key TEXT, value TEXT,
                           PRIMARY KEY (item_id, key));
        INSERT INTO items VALUES (1);
        INSERT INTO attrs VALUES (1, 'name', 'Alice');
    ").await.unwrap();

    // This fails today — compound ON condition
    conn.execute(
        "CREATE MATERIALIZED VIEW v AS
         SELECT a.value FROM items i
         JOIN attrs a ON a.item_id = i.id AND a.key = 'name'",
        ()
    ).await.unwrap();

    let mut rows = conn.query("SELECT * FROM v", ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get_value(0).unwrap(), turso::Value::Text("Alice".to_string()));
}
```
