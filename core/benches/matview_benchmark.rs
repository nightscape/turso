//! Materialized View Benchmarks for Recursive CTEs
//!
//! Measures creation time, query time, and CDC propagation time for
//! recursive CTE materialized views at various scales.
//!
//! Run with: cargo bench --bench matview_benchmark

#[cfg(not(feature = "codspeed"))]
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
#[cfg(not(feature = "codspeed"))]
use pprof::criterion::{Output, PProfProfiler};

#[cfg(feature = "codspeed")]
use codspeed_criterion_compat::{criterion_group, criterion_main, BenchmarkId, Criterion};

use std::sync::Arc;
use tempfile::TempDir;
use turso_core::{Database, DatabaseOpts, Numeric, OpenFlags, PlatformIO, Value};

#[cfg(not(target_family = "wasm"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        std::sync::Arc::new(turso_core::dialect::SqliteDialect),
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

/// Generate balanced tree INSERT statements.
/// Returns (insert_sql, root_ids, leaf_ids, mid_id) for benchmark use.
fn generate_tree(n: usize, table: &str) -> (String, Vec<String>, Vec<String>, String) {
    let bf = (n as f64).powf(0.25).ceil() as usize;
    let mut inserts = Vec::new();
    let mut roots = Vec::new();
    let mut leaves = Vec::new();
    let mut mid_id = String::new();
    let mut count = 0;

    // Level 0: roots
    for i in 0..bf {
        if count >= n {
            break;
        }
        let id = format!("r{i}");
        inserts.push(format!("('{id}', NULL, 'root-{i}')"));
        roots.push(id);
        count += 1;
    }

    // Level 1
    let mut level1 = Vec::new();
    for root in &roots {
        for j in 0..bf {
            if count >= n {
                break;
            }
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
            if count >= n {
                break;
            }
            let id = format!("{parent}-c{j}");
            inserts.push(format!("('{id}', '{parent}', 'grandchild-{j}')"));
            level2.push(id);
            count += 1;
        }
    }
    if !level2.is_empty() {
        mid_id = level2[level2.len() / 2].clone();
    }

    // Level 3: leaves
    for parent in &level2 {
        for j in 0..bf {
            if count >= n {
                break;
            }
            let id = format!("{parent}-c{j}");
            inserts.push(format!("('{id}', '{parent}', 'leaf-{j}')"));
            leaves.push(id);
            count += 1;
        }
    }
    if leaves.is_empty() && !level2.is_empty() {
        leaves = level2.clone();
    } else if leaves.is_empty() && !level1.is_empty() {
        leaves = level1.clone();
    }

    // Batch inserts in groups of 500 to avoid overly long SQL
    let mut sql = String::new();
    for chunk in inserts.chunks(500) {
        sql.push_str(&format!(
            "INSERT INTO {table} VALUES {};\n",
            chunk.join(", ")
        ));
    }

    (sql, roots, leaves, mid_id)
}

const RECURSIVE_MATVIEW_SQL: &str = "CREATE MATERIALIZED VIEW tree AS \
    WITH RECURSIVE paths AS ( \
        SELECT id, parent_id, name, '/' || id AS path, 0 AS depth \
        FROM items WHERE parent_id IS NULL \
        UNION ALL \
        SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1 \
        FROM items c \
        JOIN paths p ON c.parent_id = p.id \
        WHERE p.depth < 20 \
    ) \
    SELECT * FROM paths";

/// 1. MatView Creation Time
fn bench_matview_creation(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("MatView Creation");
    group.sample_size(10);

    for n in [100, 1_000, 10_000, 100_000] {
        let (insert_sql, _, _, _) = generate_tree(n, "items");

        group.bench_function(BenchmarkId::new("recursive_cte", n), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let temp_dir = tempfile::tempdir().unwrap();
                    let db = setup_db(&temp_dir);
                    exec(
                        &db,
                        "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
                    );
                    exec(&db, &insert_sql);

                    let conn = db.connect().unwrap();
                    let start = std::time::Instant::now();
                    let mut stmt = conn.query(RECURSIVE_MATVIEW_SQL).unwrap().unwrap();
                    stmt.run_ignore_rows().unwrap();
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// 2. MatView Query Time
fn bench_matview_query(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("MatView Query");
    group.sample_size(20);

    for n in [100, 1_000, 10_000, 100_000] {
        let (insert_sql, roots, _, _) = generate_tree(n, "items");

        let temp_dir = tempfile::tempdir().unwrap();
        let db = setup_db(&temp_dir);
        exec(
            &db,
            "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
        );
        exec(&db, &insert_sql);
        exec(&db, RECURSIVE_MATVIEW_SQL);

        let root_id = &roots[0];

        // COUNT(*)
        {
            let conn = db.connect().unwrap();
            group.bench_function(BenchmarkId::new("count", n), |b| {
                b.iter(|| {
                    let mut stmt = conn.query("SELECT COUNT(*) FROM tree").unwrap().unwrap();
                    stmt.run_collect_rows().unwrap();
                });
            });
        }

        // Point lookup
        {
            let conn = db.connect().unwrap();
            let sql = format!("SELECT * FROM tree WHERE id = '{root_id}'");
            group.bench_function(BenchmarkId::new("point_lookup", n), |b| {
                b.iter(|| {
                    let mut stmt = conn.query(&sql).unwrap().unwrap();
                    stmt.run_collect_rows().unwrap();
                });
            });
        }

        // Subtree via path prefix
        {
            let conn = db.connect().unwrap();
            let sql = format!("SELECT * FROM tree WHERE path LIKE '/{root_id}/%'");
            group.bench_function(BenchmarkId::new("subtree_path", n), |b| {
                b.iter(|| {
                    let mut stmt = conn.query(&sql).unwrap().unwrap();
                    stmt.run_collect_rows().unwrap();
                });
            });
        }

        // Filter on depth
        {
            let conn = db.connect().unwrap();
            group.bench_function(BenchmarkId::new("depth_filter", n), |b| {
                b.iter(|| {
                    let mut stmt = conn
                        .query("SELECT * FROM tree WHERE depth = 0")
                        .unwrap()
                        .unwrap();
                    stmt.run_collect_rows().unwrap();
                });
            });
        }
    }

    group.finish();
}

/// 3. CDC Propagation Time
fn bench_matview_cdc(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("MatView CDC");
    group.sample_size(10);

    for n in [100, 1_000, 10_000] {
        let (insert_sql, _, leaves, mid_id) = generate_tree(n, "items");

        // INSERT leaf
        {
            let temp_dir = tempfile::tempdir().unwrap();
            let db = setup_db(&temp_dir);
            exec(
                &db,
                "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
            );
            exec(&db, &insert_sql);
            exec(&db, RECURSIVE_MATVIEW_SQL);
            let conn = db.connect().unwrap();

            let leaf_parent = if !leaves.is_empty() {
                leaves[0].clone()
            } else {
                "r0".to_string()
            };
            let mut counter = 0u64;

            group.bench_function(BenchmarkId::new("insert_leaf", n), |b| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let new_id = format!("bench-leaf-{counter}");
                        counter += 1;
                        let sql = format!(
                            "INSERT INTO items VALUES ('{new_id}', '{leaf_parent}', 'bench')"
                        );
                        let start = std::time::Instant::now();
                        exec_conn(&conn, &sql);
                        query_count(&conn, "SELECT COUNT(*) FROM tree");
                        total += start.elapsed();

                        // Clean up
                        exec_conn(&conn, &format!("DELETE FROM items WHERE id = '{new_id}'"));
                    }
                    total
                });
            });
        }

        // INSERT mid-tree (creates new subtree)
        if !mid_id.is_empty() {
            let temp_dir = tempfile::tempdir().unwrap();
            let db = setup_db(&temp_dir);
            exec(
                &db,
                "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
            );
            exec(&db, &insert_sql);
            exec(&db, RECURSIVE_MATVIEW_SQL);
            let conn = db.connect().unwrap();
            let mut counter = 0u64;

            // Find parent of mid_id by extracting it
            let mid_parent = mid_id
                .rfind("-c")
                .map(|pos| &mid_id[..pos])
                .unwrap_or("r0")
                .to_string();

            group.bench_function(BenchmarkId::new("insert_mid", n), |b| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let new_id = format!("bench-mid-{counter}");
                        counter += 1;
                        let sql = format!(
                            "INSERT INTO items VALUES ('{new_id}', '{mid_parent}', 'bench-mid')"
                        );
                        let start = std::time::Instant::now();
                        exec_conn(&conn, &sql);
                        query_count(&conn, "SELECT COUNT(*) FROM tree");
                        total += start.elapsed();

                        exec_conn(&conn, &format!("DELETE FROM items WHERE id = '{new_id}'"));
                    }
                    total
                });
            });
        }

        // DELETE leaf
        if !leaves.is_empty() {
            let temp_dir = tempfile::tempdir().unwrap();
            let db = setup_db(&temp_dir);
            exec(
                &db,
                "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
            );
            exec(&db, &insert_sql);
            exec(&db, RECURSIVE_MATVIEW_SQL);
            let conn = db.connect().unwrap();

            let leaf_id = &leaves[leaves.len() - 1];
            let leaf_parent = leaf_id
                .rfind("-c")
                .map(|pos| &leaf_id[..pos])
                .unwrap_or("r0");

            group.bench_function(BenchmarkId::new("delete_leaf", n), |b| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        // Re-insert it first if needed
                        let _ = conn.prepare_execute_batch(&format!(
                            "INSERT OR IGNORE INTO items VALUES ('{leaf_id}', '{leaf_parent}', 'leaf-restore')"
                        ));

                        let start = std::time::Instant::now();
                        exec_conn(
                            &conn,
                            &format!("DELETE FROM items WHERE id = '{leaf_id}'"),
                        );
                        query_count(&conn, "SELECT COUNT(*) FROM tree");
                        total += start.elapsed();
                    }
                    total
                });
            });
        }

        // UPDATE parent_id (reparent a subtree)
        if !leaves.is_empty() && !mid_id.is_empty() {
            let temp_dir = tempfile::tempdir().unwrap();
            let db = setup_db(&temp_dir);
            exec(
                &db,
                "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
            );
            exec(&db, &insert_sql);
            exec(&db, RECURSIVE_MATVIEW_SQL);
            let conn = db.connect().unwrap();

            let leaf_id = &leaves[0];
            let orig_parent = leaf_id
                .rfind("-c")
                .map(|pos| &leaf_id[..pos])
                .unwrap_or("r0")
                .to_string();

            group.bench_function(BenchmarkId::new("reparent", n), |b| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        // Reparent leaf under a different subtree
                        let sql = format!(
                            "UPDATE items SET parent_id = 'r0' WHERE id = '{leaf_id}'"
                        );
                        let start = std::time::Instant::now();
                        exec_conn(&conn, &sql);
                        query_count(&conn, "SELECT COUNT(*) FROM tree");
                        total += start.elapsed();

                        // Restore original parent
                        exec_conn(
                            &conn,
                            &format!(
                                "UPDATE items SET parent_id = '{orig_parent}' WHERE id = '{leaf_id}'"
                            ),
                        );
                    }
                    total
                });
            });
        }
    }

    group.finish();
}

/// 4. Chained MatView (UNION ALL + recursive CTE)
fn bench_chained_matview_creation(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("Chained MatView Creation");
    group.sample_size(10);

    for n in [100, 1_000, 10_000, 100_000] {
        let half = n / 2;
        let (insert_blocks, _, _, _) = generate_tree(half, "blocks");
        let (insert_tasks, _, _, _) = generate_tree(n - half, "ext_tasks");

        group.bench_function(BenchmarkId::new("union_recursive", n), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let temp_dir = tempfile::tempdir().unwrap();
                    let db = setup_db(&temp_dir);
                    exec(
                        &db,
                        "CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT); \
                         CREATE TABLE ext_tasks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
                    );
                    exec(&db, &insert_blocks);
                    exec(&db, &insert_tasks);

                    let conn = db.connect().unwrap();
                    let start = std::time::Instant::now();

                    let mut stmt = conn
                        .query(
                            "CREATE MATERIALIZED VIEW unified AS \
                             SELECT id, parent_id, name FROM blocks \
                             UNION ALL \
                             SELECT id, parent_id, name FROM ext_tasks",
                        )
                        .unwrap()
                        .unwrap();
                    stmt.run_ignore_rows().unwrap();

                    let mut stmt = conn
                        .query(
                            "CREATE MATERIALIZED VIEW unified_tree AS \
                             WITH RECURSIVE paths AS ( \
                                 SELECT id, parent_id, name, '/' || id AS path, 0 AS depth \
                                 FROM unified WHERE parent_id IS NULL \
                                 UNION ALL \
                                 SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1 \
                                 FROM unified c \
                                 JOIN paths p ON c.parent_id = p.id \
                                 WHERE p.depth < 20 \
                             ) \
                             SELECT * FROM paths",
                        )
                        .unwrap()
                        .unwrap();
                    stmt.run_ignore_rows().unwrap();

                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// 4b. Chained MatView Query
fn bench_chained_matview_query(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("Chained MatView Query");
    group.sample_size(20);

    for n in [100, 1_000, 10_000, 100_000] {
        let half = n / 2;
        let (insert_blocks, roots, _, _) = generate_tree(half, "blocks");
        let (insert_tasks, _, _, _) = generate_tree(n - half, "ext_tasks");

        let temp_dir = tempfile::tempdir().unwrap();
        let db = setup_db(&temp_dir);
        exec(
            &db,
            "CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT); \
             CREATE TABLE ext_tasks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
        );
        exec(&db, &insert_blocks);
        exec(&db, &insert_tasks);
        exec(
            &db,
            "CREATE MATERIALIZED VIEW unified AS \
             SELECT id, parent_id, name FROM blocks \
             UNION ALL \
             SELECT id, parent_id, name FROM ext_tasks",
        );
        exec(
            &db,
            "CREATE MATERIALIZED VIEW unified_tree AS \
             WITH RECURSIVE paths AS ( \
                 SELECT id, parent_id, name, '/' || id AS path, 0 AS depth \
                 FROM unified WHERE parent_id IS NULL \
                 UNION ALL \
                 SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1 \
                 FROM unified c \
                 JOIN paths p ON c.parent_id = p.id \
                 WHERE p.depth < 20 \
             ) \
             SELECT * FROM paths",
        );

        let root_id = &roots[0];

        // COUNT(*)
        {
            let conn = db.connect().unwrap();
            group.bench_function(BenchmarkId::new("count", n), |b| {
                b.iter(|| {
                    let mut stmt = conn
                        .query("SELECT COUNT(*) FROM unified_tree")
                        .unwrap()
                        .unwrap();
                    stmt.run_collect_rows().unwrap();
                });
            });
        }

        // Subtree
        {
            let conn = db.connect().unwrap();
            let sql = format!("SELECT * FROM unified_tree WHERE path LIKE '/{root_id}/%'");
            group.bench_function(BenchmarkId::new("subtree_path", n), |b| {
                b.iter(|| {
                    let mut stmt = conn.query(&sql).unwrap().unwrap();
                    stmt.run_collect_rows().unwrap();
                });
            });
        }
    }

    group.finish();
}

/// 4c. Chained MatView CDC
fn bench_chained_matview_cdc(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("Chained MatView CDC");
    group.sample_size(10);

    for n in [100, 1_000, 10_000] {
        let half = n / 2;
        let (insert_blocks, _, leaves, _) = generate_tree(half, "blocks");
        let (insert_tasks, _, _, _) = generate_tree(n - half, "ext_tasks");

        let temp_dir = tempfile::tempdir().unwrap();
        let db = setup_db(&temp_dir);
        exec(
            &db,
            "CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT); \
             CREATE TABLE ext_tasks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT)",
        );
        exec(&db, &insert_blocks);
        exec(&db, &insert_tasks);
        exec(
            &db,
            "CREATE MATERIALIZED VIEW unified AS \
             SELECT id, parent_id, name FROM blocks \
             UNION ALL \
             SELECT id, parent_id, name FROM ext_tasks",
        );
        exec(
            &db,
            "CREATE MATERIALIZED VIEW unified_tree AS \
             WITH RECURSIVE paths AS ( \
                 SELECT id, parent_id, name, '/' || id AS path, 0 AS depth \
                 FROM unified WHERE parent_id IS NULL \
                 UNION ALL \
                 SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1 \
                 FROM unified c \
                 JOIN paths p ON c.parent_id = p.id \
                 WHERE p.depth < 20 \
             ) \
             SELECT * FROM paths",
        );

        let conn = db.connect().unwrap();

        // INSERT leaf into blocks
        {
            let leaf_parent = if !leaves.is_empty() {
                leaves[0].clone()
            } else {
                "r0".to_string()
            };
            let mut counter = 0u64;

            group.bench_function(BenchmarkId::new("insert_leaf", n), |b| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let new_id = format!("chained-leaf-{counter}");
                        counter += 1;
                        let sql = format!(
                            "INSERT INTO blocks VALUES ('{new_id}', '{leaf_parent}', 'bench')"
                        );
                        let start = std::time::Instant::now();
                        exec_conn(&conn, &sql);
                        query_count(&conn, "SELECT COUNT(*) FROM unified_tree");
                        total += start.elapsed();

                        exec_conn(&conn, &format!("DELETE FROM blocks WHERE id = '{new_id}'"));
                    }
                    total
                });
            });
        }
    }

    group.finish();
}

#[cfg(not(feature = "codspeed"))]
criterion_group! {
    name = matview_benches;
    config = Criterion::default()
        .with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)))
        .sample_size(10);
    targets =
        bench_matview_creation,
        bench_matview_query,
        bench_matview_cdc,
        bench_chained_matview_creation,
        bench_chained_matview_query,
        bench_chained_matview_cdc
}

#[cfg(feature = "codspeed")]
criterion_group! {
    name = matview_benches;
    config = Criterion::default().sample_size(10);
    targets =
        bench_matview_creation,
        bench_matview_query,
        bench_matview_cdc,
        bench_chained_matview_creation,
        bench_chained_matview_query,
        bench_chained_matview_cdc
}

criterion_main!(matview_benches);
