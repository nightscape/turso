use turso::core::Value as CoreValue;
use turso::{Builder, Value};

fn tag(args: &[CoreValue]) -> Result<CoreValue, String> {
    Ok(CoreValue::build_text(format!("{}:{}", args[0], args[1])))
}

fn builder(path: &str) -> Builder {
    Builder::new_local(path)
        .experimental_materialized_views(true)
        .with_deterministic_scalar_function("t_tag", 2, tag)
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
