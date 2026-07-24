//! Reproducer: IVM self-join with WHERE filters on both aliases returns empty
//!
//! Bug: A materialized view over a self-join returns empty results when the
//! WHERE clause filters on columns from BOTH aliases of the same table.
//! The same query executed directly returns correct results.
//!
//! Minimal pattern:
//!   CREATE MATERIALIZED VIEW v AS
//!   SELECT a.val, b.val FROM t AS a JOIN t AS b ON a.id = b.id
//!   WHERE a.kind = 'x' AND b.kind = 'y'
//!
//! Context: This blocks GQL (ISO/IEC 39075) graph queries over an EAV schema,
//! where accessing multiple properties of the same node requires self-joining
//! the property table with different key filters per alias.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;

/// Minimal reproducer: self-join matview with WHERE on both aliases
#[turso_macros::test(views)]
fn ivm_selfjoin_where_on_both_aliases_returns_empty(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    limbo_exec_rows(
        &conn,
        "CREATE TABLE props (entity_id INTEGER NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL)",
    );

    limbo_exec_rows(&conn, "INSERT INTO props VALUES (1, 'name', 'Alice')");
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (1, 'role', 'Engineer')");
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (2, 'name', 'Bob')");
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (2, 'role', 'Designer')");

    let query = "SELECT a.value AS name, b.value AS role \
                 FROM props AS a JOIN props AS b ON a.entity_id = b.entity_id \
                 WHERE a.key = 'name' AND b.key = 'role'";

    let direct = limbo_exec_rows(&conn, query);
    assert_eq!(
        direct.len(),
        2,
        "Direct query should return 2 rows, got: {direct:?}"
    );

    limbo_exec_rows(&conn, &format!("CREATE MATERIALIZED VIEW pivot AS {query}"));

    let matview = limbo_exec_rows(&conn, "SELECT name, role FROM pivot ORDER BY name");

    let expected = vec![
        vec![
            Value::Text("Alice".to_string()),
            Value::Text("Engineer".to_string()),
        ],
        vec![
            Value::Text("Bob".to_string()),
            Value::Text("Designer".to_string()),
        ],
    ];

    assert_eq!(
        matview, expected,
        "Matview should match direct query. Got {matview:?}, expected {expected:?}"
    );
    Ok(())
}

/// Self-join matview with WHERE on only ONE alias also has issues:
/// IVM returns only 2 rows instead of 4 and conflates the two aliases.
/// Direct query: Alice/Alice, Alice/Engineer, Bob/Bob, Bob/Designer (4 rows)
/// IVM returns:  Alice/Alice, Bob/Bob (2 rows — b.value copies a.value)
#[turso_macros::test(views)]
fn ivm_selfjoin_where_on_single_alias(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    limbo_exec_rows(
        &conn,
        "CREATE TABLE props (entity_id INTEGER NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL)",
    );
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (1, 'name', 'Alice')");
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (1, 'role', 'Engineer')");
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (2, 'name', 'Bob')");
    limbo_exec_rows(&conn, "INSERT INTO props VALUES (2, 'role', 'Designer')");

    let query = "SELECT a.value AS name, b.value AS v2 \
                 FROM props AS a JOIN props AS b ON a.entity_id = b.entity_id \
                 WHERE a.key = 'name'";

    let direct = limbo_exec_rows(&conn, query);
    assert_eq!(
        direct.len(),
        4,
        "Direct query should return 4 rows (cross-product per entity), got: {direct:?}"
    );

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW control AS {query}"),
    );

    let matview = limbo_exec_rows(&conn, "SELECT name, v2 FROM control ORDER BY name, v2");

    // Expected: each 'name' row joins with BOTH props of that entity
    let expected = vec![
        vec![
            Value::Text("Alice".to_string()),
            Value::Text("Alice".to_string()),
        ],
        vec![
            Value::Text("Alice".to_string()),
            Value::Text("Engineer".to_string()),
        ],
        vec![
            Value::Text("Bob".to_string()),
            Value::Text("Bob".to_string()),
        ],
        vec![
            Value::Text("Bob".to_string()),
            Value::Text("Designer".to_string()),
        ],
    ];

    assert_eq!(
        matview, expected,
        "Matview should match direct query. Got {matview:?}, expected {expected:?}"
    );
    Ok(())
}

/// Variant: self-join with integer column filter on both aliases
#[turso_macros::test(views)]
fn ivm_selfjoin_where_integer_keys_on_both_aliases(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    limbo_exec_rows(
        &conn,
        "CREATE TABLE kv (entity_id INTEGER NOT NULL, key_id INTEGER NOT NULL, value TEXT NOT NULL)",
    );
    limbo_exec_rows(&conn, "INSERT INTO kv VALUES (1, 1, 'Alice')");
    limbo_exec_rows(&conn, "INSERT INTO kv VALUES (1, 2, 'Engineer')");
    limbo_exec_rows(&conn, "INSERT INTO kv VALUES (2, 1, 'Bob')");
    limbo_exec_rows(&conn, "INSERT INTO kv VALUES (2, 2, 'Designer')");

    let query = "SELECT a.value AS name, b.value AS role \
                 FROM kv AS a JOIN kv AS b ON a.entity_id = b.entity_id \
                 WHERE a.key_id = 1 AND b.key_id = 2";

    let direct = limbo_exec_rows(&conn, query);
    assert_eq!(direct.len(), 2, "Direct query: {direct:?}");

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW int_pivot AS {query}"),
    );

    let matview = limbo_exec_rows(&conn, "SELECT name, role FROM int_pivot ORDER BY name");

    let expected = vec![
        vec![
            Value::Text("Alice".to_string()),
            Value::Text("Engineer".to_string()),
        ],
        vec![
            Value::Text("Bob".to_string()),
            Value::Text("Designer".to_string()),
        ],
    ];

    assert_eq!(matview, expected, "Got {matview:?}, expected {expected:?}");
    Ok(())
}

/// Full GQL-like query: self-join with additional table joins
#[turso_macros::test(views)]
fn ivm_selfjoin_gql_eav_pattern(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    limbo_exec_rows(&conn, "CREATE TABLE nodes (id INTEGER PRIMARY KEY)");
    limbo_exec_rows(
        &conn,
        "CREATE TABLE node_labels (node_id INTEGER NOT NULL, label TEXT NOT NULL)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE node_props_text (node_id INTEGER NOT NULL, key_id INTEGER NOT NULL, value TEXT NOT NULL)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE property_keys (id INTEGER PRIMARY KEY, key TEXT NOT NULL)",
    );

    limbo_exec_rows(&conn, "INSERT INTO nodes VALUES (1), (2)");
    limbo_exec_rows(
        &conn,
        "INSERT INTO node_labels VALUES (1, 'Person'), (2, 'Person')",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO property_keys VALUES (1, 'name'), (2, 'role')",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO node_props_text VALUES (1, 1, 'Alice'), (1, 2, 'Engineer')",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO node_props_text VALUES (2, 1, 'Bob'), (2, 2, 'Designer')",
    );

    // GQL-compiled SQL: MATCH (n:Person) RETURN n.name AS name, n.role AS role
    let query = "\
        SELECT p0.value AS name, p1.value AS role \
        FROM nodes AS n \
        JOIN node_labels AS nl ON nl.node_id = n.id \
        JOIN node_props_text AS p0 ON p0.node_id = n.id \
        JOIN property_keys AS pk0 ON p0.key_id = pk0.id \
        JOIN node_props_text AS p1 ON p1.node_id = n.id \
        JOIN property_keys AS pk1 ON p1.key_id = pk1.id \
        WHERE nl.label = 'Person' AND pk0.key = 'name' AND pk1.key = 'role'";

    let direct = limbo_exec_rows(&conn, query);
    assert_eq!(direct.len(), 2, "Direct GQL query: {direct:?}");

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW gql_view AS {query}"),
    );

    let matview = limbo_exec_rows(&conn, "SELECT name, role FROM gql_view ORDER BY name");

    let expected = vec![
        vec![
            Value::Text("Alice".to_string()),
            Value::Text("Engineer".to_string()),
        ],
        vec![
            Value::Text("Bob".to_string()),
            Value::Text("Designer".to_string()),
        ],
    ];

    assert_eq!(matview, expected, "Got {matview:?}, expected {expected:?}");
    Ok(())
}
