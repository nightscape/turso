use turso::core::Value as CoreValue;
use turso::{Builder, Connection, Value};

fn tag(args: &[CoreValue]) -> Result<CoreValue, String> {
    Ok(CoreValue::build_text(format!("{}:{}", args[0], args[1])))
}

fn builder(path: &str) -> Builder {
    Builder::new_local(path)
        .experimental_materialized_views(true)
        .with_deterministic_scalar_function("t_tag", 2, tag)
}

async fn view_rows(conn: &Connection) -> Vec<(i64, String)> {
    let mut rows = conn
        .query("SELECT id, tagged FROM v ORDER BY id", ())
        .await
        .unwrap();
    let mut result = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        let (Value::Integer(id), Value::Text(tagged)) =
            (row.get_value(0).unwrap(), row.get_value(1).unwrap())
        else {
            panic!("unexpected row types");
        };
        result.push((id, tagged));
    }
    result
}

#[tokio::test]
async fn materialized_view_calls_registered_function_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let path = path.to_str().unwrap();

    {
        let db = builder(path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b TEXT)", ())
            .await
            .unwrap();
        conn.execute(
            "CREATE MATERIALIZED VIEW v AS SELECT id, t_tag(a, b) AS tagged FROM t",
            (),
        )
        .await
        .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'x', 'y')", ())
            .await
            .unwrap();
        assert_eq!(view_rows(&conn).await, vec![(1, "x:y".to_string())]);
    }

    let db = builder(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'p', 'q')", ())
        .await
        .unwrap();
    assert_eq!(
        view_rows(&conn).await,
        vec![(1, "x:y".to_string()), (2, "p:q".to_string())]
    );
}

#[tokio::test]
async fn query_calls_registered_function() {
    let db = builder(":memory:").build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn.query("SELECT t_tag('x', 'y')", ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get_value(0).unwrap(), Value::Text("x:y".to_string()));
}

#[tokio::test]
async fn open_refuses_a_builtin_function_name() {
    let err = Builder::new_local(":memory:")
        .with_deterministic_scalar_function("upper", 1, tag)
        .build()
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("scalar function 'upper' has the name of a built-in function"),
        "{err}"
    );
}
