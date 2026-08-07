//! Reproducer: materialized-view change callbacks are UNLABELED and PRE-COMMIT.
//!
//! Two independent defects of the same emission site
//! (`core/vdbe/mod.rs::apply_view_deltas`, the `ViewDeltaCommitState::Processing`
//! arm):
//!
//! 1. **Unlabeled.** One commit that changes N materialized views fires N
//!    separate `RelationChangeEvent`s. Nothing on those events says they belong
//!    to the same commit, so a downstream consumer cannot tell "one commit's
//!    fan-out" from "several independent commits" — it can only guess from
//!    arrival timing, which is a scheduler race.
//!
//! 2. **Pre-commit.** `apply_view_deltas` runs at the TOP of `commit_txn`,
//!    before `commit_txn_wal`/`commit_txn_mvcc`. The event type's own doc
//!    comment admits it: "Callbacks fire BEFORE the transaction commits …
//!    Changes may still be rolled back". A transaction that rolls back AFTER
//!    dispatch therefore delivers deltas for writes that never happened, and
//!    nothing retracts them.
//!
//! ## How the labeling test goes red today and green after the fix
//!
//! `UnlabeledCommitEnvelope` below supplies a fallback `commit_id()` that
//! returns `0` ("unlabeled"). Rust resolves an INHERENT method before a trait
//! method at the same autoderef step, so as soon as the fix adds
//! `impl RelationChangeEvent { pub fn commit_id(&self) -> u64 }` the fallback is
//! shadowed and the test reads the real value — with NO edit to this file.
//! A public `commit_id` FIELD alone is not enough; the inherent accessor is
//! part of the contract.

use std::sync::{Arc, Mutex};
use turso::{Builder, RelationChangeEvent};

/// Fallback that makes "no commit identity on the wire" observable at runtime
/// instead of as a compile error. Shadowed by the inherent accessor the fix adds.
trait UnlabeledCommitEnvelope {
    fn commit_id(&self) -> u64 {
        0
    }
}
impl UnlabeledCommitEnvelope for RelationChangeEvent {}

#[derive(Debug, Clone)]
struct Seen {
    relation: String,
    commit_id: u64,
    change_count: usize,
}

fn recorder() -> (
    Arc<Mutex<Vec<Seen>>>,
    impl Fn(&RelationChangeEvent) + Send + Sync,
) {
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let sink = seen.clone();
    let cb = move |event: &RelationChangeEvent| {
        sink.lock().unwrap().push(Seen {
            relation: event.relation_name.clone(),
            commit_id: event.commit_id(),
            change_count: event.changes.len(),
        });
    };
    (seen, cb)
}

/// (a) Two views of ONE commit arrive with no shared identity.
///
/// RED today: both events carry commit_id 0.
/// GREEN after the fix: both carry the same non-zero commit id.
#[tokio::test]
async fn two_views_of_one_commit_share_a_commit_id() {
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

    let (seen, cb) = recorder();
    let _id = conn.set_change_callback(cb).unwrap();

    // ONE commit (autocommit statement) touching BOTH views.
    conn.execute("INSERT INTO t VALUES (1, 'hello')", ())
        .await
        .unwrap();

    let events = seen.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        2,
        "one commit touching two views should deliver both views' deltas, got {events:?}"
    );
    assert!(
        events.iter().all(|e| e.change_count > 0),
        "each delivered event must carry changes, got {events:?}"
    );

    let ids: Vec<u64> = events.iter().map(|e| e.commit_id).collect();
    assert!(
        ids.iter().all(|id| *id != 0),
        "every CDC event must carry a non-zero commit identity so a consumer can \
         group one commit's fan-out deterministically; got {ids:?} for {:?}",
        events.iter().map(|e| &e.relation).collect::<Vec<_>>()
    );
    assert_eq!(
        ids[0], ids[1],
        "both views changed in the SAME commit, so their events must share one \
         commit id; got {ids:?}"
    );
}

/// (b1) An explicitly rolled-back transaction still delivers deltas.
///
/// RED today if callbacks fire for statements inside the transaction.
/// GREEN after the fix: emission is post-commit, so a rollback delivers nothing.
#[tokio::test]
async fn an_explicitly_rolled_back_transaction_delivers_nothing() {
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

    let (seen, cb) = recorder();
    let _id = conn.set_change_callback(cb).unwrap();

    conn.execute("BEGIN", ()).await.unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'phantom')", ())
        .await
        .unwrap();
    conn.execute("ROLLBACK", ()).await.unwrap();

    let events = seen.lock().unwrap().clone();
    assert!(
        events.is_empty(),
        "a rolled-back transaction must deliver NO CDC events — these are phantom \
         deltas for writes that never happened: {events:?}"
    );
}

/// (b2) A commit that FAILS at commit time (deferred FK violation) still
/// delivers deltas, because `apply_view_deltas` runs before the commit does.
#[tokio::test]
async fn a_commit_that_fails_at_commit_time_delivers_nothing() {
    let db = Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();

    conn.execute("PRAGMA foreign_keys = ON", ()).await.unwrap();
    conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", ())
        .await
        .unwrap();
    conn.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER \
         REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "CREATE MATERIALIZED VIEW v_child AS SELECT id, parent_id FROM child",
        (),
    )
    .await
    .unwrap();

    let (seen, cb) = recorder();
    let _id = conn.set_change_callback(cb).unwrap();

    conn.execute("BEGIN", ()).await.unwrap();
    conn.execute("INSERT INTO child VALUES (1, 999)", ())
        .await
        .unwrap();
    let commit = conn.execute("COMMIT", ()).await;
    assert!(
        commit.is_err(),
        "the deferred FK to a non-existent parent must make COMMIT fail; \
         got {commit:?} — if this passes the reproducer's premise is gone"
    );

    let events = seen.lock().unwrap().clone();
    assert!(
        events.is_empty(),
        "a transaction whose COMMIT failed must deliver NO CDC events: {events:?}"
    );
}

/// Backward-compatibility pin: the fix must not change the fan-out shape or the
/// payload — one event per changed relation, with its columns and its changes.
/// Only the envelope grows.
#[tokio::test]
async fn per_relation_fan_out_and_payload_are_unchanged() {
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

    let seen = Arc::new(Mutex::new(Vec::<(String, Vec<String>, usize)>::new()));
    let sink = seen.clone();
    let _id = conn
        .set_change_callback(move |event: &RelationChangeEvent| {
            sink.lock().unwrap().push((
                event.relation_name.clone(),
                event.columns.clone(),
                event.changes.len(),
            ));
        })
        .unwrap();

    conn.execute("INSERT INTO t VALUES (1, 'hello')", ())
        .await
        .unwrap();

    let events = seen.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, "v_all");
    assert_eq!(events[0].1, vec!["id", "v"]);
    assert_eq!(events[0].2, 1);
}

/// (b3) The callback observes a change that is NOT YET DURABLE.
///
/// File-backed DB: the WAL grows only when the commit actually writes its
/// frames. Sampling the WAL size from inside the callback and again after
/// `execute(..).await` returns shows whether emission precedes durability.
///
/// RED today: the callback runs before the commit's frames reach the WAL.
/// GREEN after the fix: emission is post-commit, so the sizes match.
#[tokio::test]
async fn the_callback_runs_after_the_commit_reaches_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("repro.db");
    let wal_path = dir.path().join("repro.db-wal");

    let db = Builder::new_local(db_path.to_str().unwrap())
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

    let at_callback = Arc::new(Mutex::new(Vec::<u64>::new()));
    let sink = at_callback.clone();
    let wal_for_cb = wal_path.clone();
    let _id = conn
        .set_change_callback(move |_e: &RelationChangeEvent| {
            let len = std::fs::metadata(&wal_for_cb).map(|m| m.len()).unwrap_or(0);
            sink.lock().unwrap().push(len);
        })
        .unwrap();

    conn.execute("INSERT INTO t VALUES (1, 'durable?')", ())
        .await
        .unwrap();
    let after_return = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);

    let sampled = at_callback.lock().unwrap().clone();
    assert_eq!(
        sampled.len(),
        1,
        "expected exactly one callback: {sampled:?}"
    );
    assert_eq!(
        sampled[0], after_return,
        "the CDC callback fired while the commit's frames were NOT yet in the WAL \
         (wal at callback = {} bytes, wal after the write returned = {after_return} \
         bytes) — the consumer was told about data that is not durable and can \
         still be rolled back",
        sampled[0]
    );
}
