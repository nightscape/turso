use turso::Builder;

#[tokio::test]
async fn a_change_callback_on_an_mvcc_database_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mvcc.db");
    let db = Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn
        .query("PRAGMA journal_mode = 'mvcc'", ())
        .await
        .unwrap();
    while rows.next().await.unwrap().is_some() {}

    let error = conn.set_change_callback(|_| {}).unwrap_err();

    assert!(error.to_string().contains("MVCC"), "got {error}");
}
