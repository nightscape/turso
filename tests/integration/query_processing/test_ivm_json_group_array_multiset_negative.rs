//! Regression test for the json_group_array multiset-negative panic
//! reproduced from a Holon trace on 2026-05-17.
//!
//! The matview shape (dual LEFT JOIN + `json_group_array(...) FILTER`)
//! is the same as `test_ivm_block_matview_drift`, but this trace's
//! trigger sequence (INSERT block_raw → INSERT junction → UPDATE
//! block_raw → DELETE junction) panics inside `AggregateOperator`'s
//! commit path with:
//!
//! ```text
//! json_group_array multiset went negative for col 17 val Text("Page")
//! — delta consolidation invariant violated
//! ```
//!
//! Introduced by the MatchCounter → Antijoin refactor. Bug is gated
//! on btree state from the preloaded 158-row block_raw dataset; we
//! have not yet found a smaller data set that triggers it, so the
//! test replays the captured statements verbatim.

const REPLAY_SQL: &str = include_str!("data/ivm_json_group_array_multiset_negative.sql");

#[tokio::test]
async fn replay_no_multiset_negative_panic() -> anyhow::Result<()> {
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    // Strip comment lines, then split on top-level `;`. Some VALUES contain
    // embedded newlines and `;` inside quoted strings.
    let stripped: String = REPLAY_SQL
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = stripped.as_bytes();
    let mut start = 0usize;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' {
            if in_string && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_string = !in_string;
        } else if c == b';' && !in_string {
            let s = stripped[start..i].trim();
            if !s.is_empty() {
                conn.execute(s, ()).await?;
            }
            start = i + 1;
        }
        i += 1;
    }
    let tail = stripped[start..].trim();
    if !tail.is_empty() {
        conn.execute(tail, ()).await?;
    }

    // Reaching this point means no panic. As a final sanity check,
    // the target block must show tags=[] (both INSERT and DELETE
    // on the only tag entry ran).
    let mut rows = conn
        .query(
            "SELECT tags FROM block WHERE id = 'block:d09025cc-3748-404e-ad4d-432fcdc194d5'",
            (),
        )
        .await?;
    let row = rows.next().await?.expect("target row missing");
    let tags: String = row.get::<String>(0)?;
    assert_eq!(
        tags, "[]",
        "target block must end with empty tags after INSERT + DELETE"
    );
    Ok(())
}
