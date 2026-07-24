//! Regression test for IVM matview drift on populate (2026-05-13).
//!
//! From-scratch populate of a matview with:
//!   - a 16-column GROUP BY,
//!   - two LEFT JOINs to side tables,
//!   - json_group_array(...) FILTER (WHERE ... IS NOT NULL)
//!
//! dropped the aggregated row for FILTER predicates that matched, returning
//! `tags=[]` for a block that has exactly one `block_tags` entry. The
//! incremental path produced the correct `["Page"]`; only populate drifted.
//!
//! Captured from holon-mcp startup. See
//! `data/ivm_block_matview_drift.sql` for the 164-row dataset.

const DATA_SQL: &str = include_str!("data/ivm_block_matview_drift.sql");

const MATVIEW_DDL: &str = "CREATE MATERIALIZED VIEW block AS \
    SELECT \
        b.id, b.parent_id, b.depth, b.sort_key, b.content, b.content_type, \
        b.source_language, b.source_name, b.properties, b.marks, \
        b.collapsed, b.completed, b.block_type, b.created_at, b.updated_at, \
        b._change_origin, \
        COALESCE(json_group_array(bt.tag) FILTER (WHERE bt.tag IS NOT NULL), '[]') AS tags, \
        COALESCE(json_group_array(br.required_id) FILTER (WHERE br.required_id IS NOT NULL), '[]') AS requires \
    FROM block_raw b \
    LEFT OUTER JOIN block_tags     bt ON bt.block_id = b.id \
    LEFT OUTER JOIN block_requires br ON br.block_id = b.id \
    GROUP BY \
        b.id, b.parent_id, b.depth, b.sort_key, b.content, b.content_type, \
        b.source_language, b.source_name, b.properties, b.marks, \
        b.collapsed, b.completed, b.block_type, b.created_at, b.updated_at, \
        b._change_origin";

const SCHEMA_DDL: &str = "\
    CREATE TABLE block_raw (\
        id TEXT PRIMARY KEY, parent_id TEXT, depth INTEGER NOT NULL DEFAULT 0,\
        sort_key TEXT NOT NULL DEFAULT 'A0', content TEXT NOT NULL DEFAULT '',\
        content_type TEXT NOT NULL DEFAULT 'text', source_language TEXT,\
        source_name TEXT, properties TEXT, marks TEXT,\
        collapsed INTEGER NOT NULL DEFAULT 0, completed INTEGER NOT NULL DEFAULT 0,\
        block_type TEXT NOT NULL DEFAULT 'text',\
        created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0,\
        _change_origin TEXT\
    );\
    CREATE TABLE block_tags (\
        block_id TEXT NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(block_id, tag)\
    );\
    CREATE TABLE block_requires (\
        block_id TEXT NOT NULL, required_id TEXT NOT NULL, PRIMARY KEY(block_id, required_id)\
    );";

const TARGET_ID: &str = "block:e37a1996-06e0-429a-8364-5e83b4599556";

async fn setup_and_query(matview_before_data: bool) -> anyhow::Result<String> {
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    for stmt in SCHEMA_DDL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        conn.execute(stmt, ()).await?;
    }

    if matview_before_data {
        conn.execute(MATVIEW_DDL, ()).await?;
    }

    // Strip comment lines, then split on top-level `;`. Some VALUES contain
    // embedded newlines and `;` inside quoted strings.
    let stripped: String = DATA_SQL
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut start = 0usize;
    let bytes = stripped.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' {
            // Doubled '' inside a string literal is an escaped quote; skip both.
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

    if !matview_before_data {
        conn.execute(MATVIEW_DDL, ()).await?;
    }

    let mut rows = conn
        .query(
            &format!("SELECT tags FROM block WHERE id = '{TARGET_ID}'"),
            (),
        )
        .await?;
    let row = rows.next().await?.expect("target row missing");
    Ok(row.get::<String>(0)?)
}

#[tokio::test]
async fn populate_after_data_returns_tag() -> anyhow::Result<()> {
    let tags = setup_and_query(false).await?;
    assert_eq!(
        tags, r#"["Page"]"#,
        "from-scratch populate dropped the FILTER row's aggregated value"
    );
    Ok(())
}

#[tokio::test]
async fn incremental_maintains_tag() -> anyhow::Result<()> {
    // Sanity: the incremental path (matview created before data load) is
    // correct on the same dataset. If this ever regresses, the populate
    // assertion above no longer pins the bug to populate alone.
    let tags = setup_and_query(true).await?;
    assert_eq!(tags, r#"["Page"]"#);
    Ok(())
}
