//! Full scan of a materialized view vs. the same rows in a plain table.
//!
//! Exits non-zero ("red") when the matview scan is more than
//! `MAX_RATIO` times slower than the table scan.
//!
//! Run with: cargo bench --bench matview_scan_benchmark
//! Env: MVSCAN_ROWS (default 100000), MVSCAN_ITERS (default 15).

use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use turso_core::{Connection, Database, DatabaseOpts, OpenFlags, PlatformIO};

#[cfg(not(target_family = "wasm"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const MAX_RATIO: f64 = 3.0;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |v| {
        v.parse()
            .unwrap_or_else(|e| panic!("{name}={v} is not a usize: {e}"))
    })
}

fn exec(conn: &Arc<Connection>, sql: &str) {
    conn.prepare_execute_batch(sql).unwrap();
}

fn scan(conn: &Arc<Connection>, sql: &str) -> Duration {
    let start = Instant::now();
    let mut stmt = conn.query(sql).unwrap().unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    let elapsed = start.elapsed();
    assert!(rows.is_empty(), "the LIKE pattern must match no row");
    elapsed
}

fn median_ms(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

fn main() {
    let rows = env_usize("MVSCAN_ROWS", 100_000);
    let iters = env_usize("MVSCAN_ITERS", 15);

    let temp_dir = TempDir::new().unwrap();
    #[allow(clippy::arc_with_non_send_sync)]
    let io = Arc::new(PlatformIO::new().unwrap());
    let db = Database::open_file_with_flags(
        io,
        temp_dir.path().join("bench.db").to_str().unwrap(),
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
        "CREATE TABLE src (id INTEGER PRIMARY KEY, uri TEXT, type TEXT, title TEXT, body TEXT)",
    );
    let mut batch = String::from("BEGIN;");
    for i in 0..rows {
        batch.push_str(&format!(
            "INSERT INTO src VALUES ({i}, 'entity-{i:08}', 't{}', 'title of entity {i}', 'body text for entity number {i} with some words');",
            i % 5
        ));
        if i % 5000 == 4999 {
            batch.push_str("COMMIT;");
            exec(&conn, &batch);
            batch = String::from("BEGIN;");
        }
    }
    batch.push_str("COMMIT;");
    exec(&conn, &batch);

    let select = "SELECT uri, type, title, title || ' ' || body AS search_text FROM src";
    let t = Instant::now();
    exec(&conn, &format!("CREATE MATERIALIZED VIEW mv AS {select}"));
    eprintln!(
        "create matview: {:.0} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
    exec(
        &conn,
        "CREATE TABLE copy (uri TEXT, type TEXT, title TEXT, search_text TEXT)",
    );
    exec(&conn, &format!("INSERT INTO copy {select}"));

    let query = |from: &str| {
        format!(
            "SELECT uri, type, title FROM {from} WHERE search_text LIKE '%no such text%' LIMIT 50"
        )
    };
    let (table_sql, view_sql) = (query("copy"), query("mv"));
    scan(&conn, &table_sql);
    scan(&conn, &view_sql);

    let (mut table, mut view) = (Vec::new(), Vec::new());
    for _ in 0..iters {
        table.push(scan(&conn, &table_sql));
        view.push(scan(&conn, &view_sql));
    }
    let (table_ms, view_ms) = (median_ms(table), median_ms(view));
    let ratio = view_ms / table_ms;
    let verdict = if ratio > MAX_RATIO { "RED" } else { "GREEN" };
    println!(
        "MVSCAN rows={rows} iters={iters} table_ms={table_ms:.2} matview_ms={view_ms:.2} ratio={ratio:.2} {verdict}"
    );
    if ratio > MAX_RATIO {
        std::process::exit(1);
    }
}
