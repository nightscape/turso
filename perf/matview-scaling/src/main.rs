use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;
use turso_core::{Database, DatabaseOpts, Numeric, OpenFlags, PlatformIO, Value};

fn setup_db(temp_dir: &TempDir) -> Arc<Database> {
    let db_path = temp_dir.path().join("bench.db");
    #[allow(clippy::arc_with_non_send_sync)]
    let io = Arc::new(PlatformIO::new().unwrap());
    let opts = DatabaseOpts::new().with_views(true);
    let db = Database::open_file_with_flags(
        io,
        db_path.to_str().unwrap(),
        OpenFlags::default(),
        opts,
        None,
    )
    .unwrap();
    let conn = db.connect().unwrap();
    let mut stmt = conn.query("PRAGMA synchronous = OFF").unwrap().unwrap();
    stmt.run_ignore_rows().unwrap();
    db
}

fn exec(db: &Arc<Database>, sql: &str) {
    let conn = db.connect().unwrap();
    conn.prepare_execute_batch(sql).unwrap();
}

fn exec_conn(conn: &Arc<turso_core::Connection>, sql: &str) {
    conn.prepare_execute_batch(sql).unwrap();
}

fn query_count(conn: &Arc<turso_core::Connection>, sql: &str) -> i64 {
    let mut stmt = conn.query(sql).unwrap().unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    match &rows[0][0] {
        Value::Numeric(Numeric::Integer(n)) => *n,
        other => panic!("Expected integer, got {:?}", other),
    }
}

fn generate_flat_inserts(n: usize) -> String {
    // Simple linear chain: row i has parent i-1
    let mut sql = String::new();
    for chunk_start in (0..n).step_by(500) {
        let chunk_end = (chunk_start + 500).min(n);
        let values: Vec<String> = (chunk_start..chunk_end)
            .map(|i| {
                if i == 0 {
                    format!("('n{i}', NULL, 'root')")
                } else {
                    format!("('n{i}', 'n{}', 'node-{i}')", i - 1)
                }
            })
            .collect();
        sql.push_str(&format!("INSERT INTO items VALUES {};\n", values.join(", ")));
    }
    sql
}

fn generate_tree(n: usize) -> String {
    let bf = (n as f64).powf(0.25).ceil() as usize;
    let mut inserts = Vec::new();
    let mut count = 0;

    // Level 0: roots
    let mut roots = Vec::new();
    for i in 0..bf {
        if count >= n { break; }
        inserts.push(format!("('r{i}', NULL, 'root-{i}')"));
        roots.push(format!("r{i}"));
        count += 1;
    }
    // Level 1
    let mut level1 = Vec::new();
    for root in &roots {
        for j in 0..bf {
            if count >= n { break; }
            let id = format!("{root}-c{j}");
            inserts.push(format!("('{id}', '{root}', 'child-{j}')"));
            level1.push(id);
            count += 1;
        }
    }
    // Level 2
    let mut level2 = Vec::new();
    for parent in &level1 {
        for j in 0..bf {
            if count >= n { break; }
            let id = format!("{parent}-c{j}");
            inserts.push(format!("('{id}', '{parent}', 'gc-{j}')"));
            level2.push(id);
            count += 1;
        }
    }
    // Level 3
    for parent in &level2 {
        for j in 0..bf {
            if count >= n { break; }
            let id = format!("{parent}-c{j}");
            inserts.push(format!("('{id}', '{parent}', 'leaf-{j}')"));
            count += 1;
        }
    }

    let mut sql = String::new();
    for chunk in inserts.chunks(500) {
        sql.push_str(&format!("INSERT INTO items VALUES {};\n", chunk.join(", ")));
    }
    sql
}

const RECURSIVE_MATVIEW_SQL: &str = "CREATE MATERIALIZED VIEW tree AS \
    WITH RECURSIVE paths AS ( \
        SELECT id, parent_id, name, '/' || id AS path, 0 AS depth \
        FROM items WHERE parent_id IS NULL \
        UNION ALL \
        SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1 \
        FROM items c \
        JOIN paths p ON c.parent_id = p.id \
        WHERE p.depth < 50 \
    ) \
    SELECT * FROM paths";

fn bench_creation(n: usize, shape: &str) -> (f64, i64) {
    let temp_dir = tempfile::tempdir().unwrap();
    let db = setup_db(&temp_dir);
    exec(&db, "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)");

    let insert_sql = match shape {
        "tree" => generate_tree(n),
        "chain" => generate_flat_inserts(n),
        _ => panic!("Unknown shape"),
    };
    exec(&db, &insert_sql);

    let conn = db.connect().unwrap();

    let start = Instant::now();

    let prepare_start = Instant::now();
    let mut stmt = conn.query(RECURSIVE_MATVIEW_SQL).unwrap().unwrap();
    eprintln!("[BENCH] prepare took {:.1}ms", prepare_start.elapsed().as_secs_f64() * 1000.0);
    let run_start = Instant::now();
    stmt.run_ignore_rows().unwrap();
    eprintln!("[BENCH] run_ignore_rows took {:.1}ms", run_start.elapsed().as_secs_f64() * 1000.0);
    let elapsed = start.elapsed();

    let count = query_count(&conn, "SELECT COUNT(*) FROM tree");

    (elapsed.as_secs_f64() * 1000.0, count)
}

fn bench_cdc_insert(n: usize) -> f64 {
    let temp_dir = tempfile::tempdir().unwrap();
    let db = setup_db(&temp_dir);
    exec(&db, "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)");
    exec(&db, &generate_tree(n));
    exec(&db, RECURSIVE_MATVIEW_SQL);
    let conn = db.connect().unwrap();

    let mut total = 0.0f64;
    let iters = 5;
    for i in 0..iters {
        let id = format!("bench-leaf-{i}");
        let sql = format!("INSERT INTO items VALUES ('{id}', 'r0', 'bench')");
        let start = Instant::now();
        exec_conn(&conn, &sql);
        query_count(&conn, "SELECT COUNT(*) FROM tree");
        total += start.elapsed().as_secs_f64() * 1000.0;
        exec_conn(&conn, &format!("DELETE FROM items WHERE id = '{id}'"));
    }
    total / iters as f64
}

fn bench_query(n: usize) -> (f64, f64) {
    let temp_dir = tempfile::tempdir().unwrap();
    let db = setup_db(&temp_dir);
    exec(&db, "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)");
    exec(&db, &generate_tree(n));
    exec(&db, RECURSIVE_MATVIEW_SQL);
    let conn = db.connect().unwrap();

    // COUNT(*)
    let start = Instant::now();
    for _ in 0..10 {
        query_count(&conn, "SELECT COUNT(*) FROM tree");
    }
    let count_ms = start.elapsed().as_secs_f64() * 100.0; // per-iter in ms

    // Full scan
    let start = Instant::now();
    for _ in 0..10 {
        let mut stmt = conn.query("SELECT * FROM tree").unwrap().unwrap();
        stmt.run_collect_rows().unwrap();
    }
    let scan_ms = start.elapsed().as_secs_f64() * 100.0;

    (count_ms, scan_ms)
}

fn main() {
    println!("## Creation Time (balanced tree, depth ~4)");
    println!("{:<10} {:>12} {:>10} {:>12}", "N", "Create (ms)", "Rows", "ms/row");
    println!("{:-<10} {:-<12} {:-<10} {:-<12}", "", "", "", "");

    for n in [50, 100, 250, 500, 1000, 2500, 5000, 10000, 25000, 50000] {
        let (ms, rows) = bench_creation(n, "tree");
        let ms_per_row = ms / rows as f64;
        println!("{:<10} {:>12.1} {:>10} {:>12.3}", n, ms, rows, ms_per_row);
    }
}
