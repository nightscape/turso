//! IVM commit cost: writes into a base table that one materialized view reads.
//!
//! The view writer cursor is created once per delta row, so anything the
//! commit does per cursor shows up here.
//!
//! Run with: cargo bench --bench matview_write_benchmark
//! Env: MVWRITE_BULK (default 10000), MVWRITE_SINGLES (default 1000).

use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;
use turso_core::{Connection, Database, DatabaseOpts, OpenFlags, PlatformIO};

#[cfg(not(target_family = "wasm"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |v| {
        v.parse()
            .unwrap_or_else(|e| panic!("{name}={v} is not a usize: {e}"))
    })
}

fn exec(conn: &Arc<Connection>, sql: &str) {
    conn.prepare_execute_batch(sql).unwrap();
}

fn fresh(dir: &TempDir, name: &str, with_view: bool) -> Arc<Connection> {
    #[allow(clippy::arc_with_non_send_sync)]
    let io = Arc::new(PlatformIO::new().unwrap());
    let db = Database::open_file_with_flags(
        io,
        dir.path().join(name).to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(turso_core::dialect::SqliteDialect),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    exec(&conn, "PRAGMA synchronous = OFF");
    exec(
        &conn,
        "CREATE TABLE src (id INTEGER PRIMARY KEY, uri TEXT, type TEXT, title TEXT)",
    );
    if with_view {
        exec(
            &conn,
            "CREATE MATERIALIZED VIEW mv AS SELECT id, uri, type, title FROM src",
        );
    }
    conn
}

fn row(i: usize) -> String {
    format!(
        "({i}, 'entity-{i:08}', 't{}', 'title of entity {i}')",
        i % 5
    )
}

/// One transaction holding `n` inserts.
fn bulk_ms(conn: &Arc<Connection>, base: usize, n: usize) -> f64 {
    let mut batch = String::from("BEGIN;");
    for i in base..base + n {
        batch.push_str(&format!("INSERT INTO src VALUES {};", row(i)));
    }
    batch.push_str("COMMIT;");
    let t = Instant::now();
    exec(conn, &batch);
    t.elapsed().as_secs_f64() * 1000.0
}

/// `n` autocommit inserts, so `n` separate IVM commits.
fn singles_ms(conn: &Arc<Connection>, base: usize, n: usize) -> f64 {
    let stmts: Vec<String> = (base..base + n)
        .map(|i| format!("INSERT INTO src VALUES {}", row(i)))
        .collect();
    let t = Instant::now();
    for s in &stmts {
        exec(conn, s);
    }
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let bulk = env_usize("MVWRITE_BULK", 10_000);
    let singles = env_usize("MVWRITE_SINGLES", 1_000);
    let dir = TempDir::new().unwrap();

    // With the matview: the IVM commit path runs.
    let mv = fresh(&dir, "with_view.db", true);
    let mv_bulk = bulk_ms(&mv, 0, bulk);
    let mv_singles = singles_ms(&mv, 1_000_000, singles);

    // Without it: the plain write cost, as a scale reference.
    let plain = fresh(&dir, "no_view.db", false);
    let plain_bulk = bulk_ms(&plain, 0, bulk);
    let plain_singles = singles_ms(&plain, 1_000_000, singles);

    println!(
        "MVWRITE bulk={bulk} singles={singles} \
         mv_bulk_ms={mv_bulk:.1} plain_bulk_ms={plain_bulk:.1} \
         mv_singles_ms={mv_singles:.1} plain_singles_ms={plain_singles:.1} \
         bulk_overhead={:.2}x singles_overhead={:.2}x",
        mv_bulk / plain_bulk,
        mv_singles / plain_singles
    );
}
