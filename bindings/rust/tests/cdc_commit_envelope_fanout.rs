//! The commit envelope must make one commit's fan-out self-delimiting.
//!
//! A consumer groups events by `commit_id` and knows the group is complete once
//! it holds `commit_len` of them. That only works if `commit_len` counts the
//! events actually DELIVERED. The emission site skips a view whose output delta
//! is empty, so counting the views it considered instead of the events it sent
//! would leave every consumer waiting forever for a member that never arrives.

use std::sync::{Arc, Mutex};
use turso::{Builder, RelationChangeEvent};

#[derive(Debug, Clone)]
struct Seen {
    relation: String,
    commit_id: u64,
    commit_index: u32,
    commit_len: u32,
}

#[tokio::test]
async fn a_commits_fan_out_is_self_delimiting() {
    let db = Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();

    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .await
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v_all AS SELECT id, v FROM t", ())
        .await
        .unwrap();
    conn.execute(
        "CREATE MATERIALIZED VIEW v_upper AS SELECT id, upper(v) AS v FROM t",
        (),
    )
    .await
    .unwrap();
    // Third view: the inserted row does not match, so its output delta stays
    // empty and it delivers no event.
    conn.execute(
        "CREATE MATERIALIZED VIEW v_none AS SELECT id, v FROM t WHERE v = 'nothing matches this'",
        (),
    )
    .await
    .unwrap();

    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let sink = seen.clone();
    let _id = conn
        .set_change_callback(move |event: &RelationChangeEvent| {
            sink.lock().unwrap().push(Seen {
                relation: event.relation_name.clone(),
                commit_id: event.commit_id(),
                commit_index: event.commit_index,
                commit_len: event.commit_len,
            });
        })
        .unwrap();

    conn.execute("INSERT INTO t VALUES (1, 'hello')", ())
        .await
        .unwrap();

    let events = seen.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        2,
        "only the two views whose output actually changed may deliver an event: {events:?}"
    );
    assert!(
        events.iter().all(|e| e.relation != "v_none"),
        "the view with an empty output delta must not deliver an event: {events:?}"
    );

    let commit_id = events[0].commit_id;
    assert_ne!(commit_id, 0, "a delivered event must carry a commit id");
    assert!(
        events.iter().all(|e| e.commit_id == commit_id),
        "all events of one commit must share its commit id: {events:?}"
    );

    assert!(
        events.iter().all(|e| e.commit_len == events.len() as u32),
        "commit_len must equal the number of events actually delivered ({}), \
         not the number of views considered (3): {events:?}",
        events.len()
    );

    let mut indices: Vec<u32> = events.iter().map(|e| e.commit_index).collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec![0, 1],
        "commit_index values must be exactly 0..commit_len: {events:?}"
    );
}
