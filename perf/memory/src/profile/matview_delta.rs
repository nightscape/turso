use super::{Phase, Profile, WorkItem};

const SEED_ROWS: usize = 1_000;
const KINDS: [&str; 5] = ["page", "heading", "paragraph", "list", "code"];

/// IVM/DBSP delta-churn workload: several materialized views (join,
/// aggregate, LEFT JOIN + aggregate) over one table, seeded with ~1k rows,
/// then driven with single-row UPDATEs plus occasional INSERT/DELETE in
/// committed batches. Every DML commit runs the DBSP circuit for all views,
/// exercising the operator-boundary delta clones that dominated dhat
/// profiles (Value clone <- HashableRow clone <- to_vec <- process_node).
///
/// Measurement procedure (compare a branch against main):
///   cargo run --release -p memory-benchmark -- --workload matview-delta
///   python3 perf/memory/analyze-dhat.py dhat-heap.json
pub struct MatviewDelta {
    iterations: usize,
    batch_size: usize,
    current_iteration: usize,
    phase: InternalPhase,
    seed_offset: usize,
    next_insert_id: usize,
    statement_counter: usize,
}

enum InternalPhase {
    CreateSchema,
    Seed,
    CreateViews,
    Run,
}

impl MatviewDelta {
    pub fn new(iterations: usize, batch_size: usize) -> Self {
        Self {
            iterations,
            batch_size,
            current_iteration: 0,
            phase: InternalPhase::CreateSchema,
            seed_offset: 0,
            next_insert_id: SEED_ROWS,
            statement_counter: 0,
        }
    }

    fn insert_item(&mut self) -> WorkItem {
        let id = self.next_insert_id;
        self.next_insert_id += 1;
        WorkItem {
            sql: "INSERT INTO block (id, parent_id, kind, content, score) VALUES (?, ?, ?, ?, ?)"
                .to_string(),
            params: vec![
                turso::Value::Integer(id as i64),
                turso::Value::Integer((id % SEED_ROWS.max(1)) as i64),
                turso::Value::Text(KINDS[id % KINDS.len()].to_string()),
                turso::Value::Text(format!("content_{id}")),
                turso::Value::Real(id as f64 * 0.25),
            ],
        }
    }
}

impl Profile for MatviewDelta {
    fn name(&self) -> &str {
        "matview-delta"
    }

    fn next_batch(&mut self, connections: usize) -> (Phase, Vec<Vec<WorkItem>>) {
        match self.phase {
            InternalPhase::CreateSchema => {
                self.phase = InternalPhase::Seed;
                (
                    Phase::Setup,
                    vec![vec![WorkItem {
                        sql: "CREATE TABLE block (id INTEGER PRIMARY KEY, parent_id INTEGER, \
                              kind TEXT NOT NULL, content TEXT NOT NULL, score REAL)"
                            .to_string(),
                        params: vec![],
                    }]],
                )
            }
            InternalPhase::Seed => {
                let remaining = SEED_ROWS - self.seed_offset;
                let batch = remaining.min(250);
                let mut items = Vec::with_capacity(batch);
                for i in 0..batch {
                    let id = self.seed_offset + i;
                    items.push(WorkItem {
                        sql: "INSERT INTO block (id, parent_id, kind, content, score) \
                              VALUES (?, ?, ?, ?, ?)"
                            .to_string(),
                        params: vec![
                            turso::Value::Integer(id as i64),
                            // Tree-ish shape: each row points at an earlier row.
                            turso::Value::Integer((id / 4) as i64),
                            turso::Value::Text(KINDS[id % KINDS.len()].to_string()),
                            turso::Value::Text(format!("content_{id}")),
                            turso::Value::Real(id as f64 * 0.25),
                        ],
                    });
                }
                self.seed_offset += batch;
                if self.seed_offset >= SEED_ROWS {
                    self.phase = InternalPhase::CreateViews;
                }
                (Phase::Setup, vec![items])
            }
            InternalPhase::CreateViews => {
                self.phase = InternalPhase::Run;
                let views = [
                    "CREATE MATERIALIZED VIEW mv_join AS \
                     SELECT b.id AS id, b.kind AS kind, b.content AS content, \
                            p.kind AS parent_kind \
                     FROM block b JOIN block p ON b.parent_id = p.id",
                    "CREATE MATERIALIZED VIEW mv_agg AS \
                     SELECT kind, count(*) AS cnt, sum(score) AS total \
                     FROM block GROUP BY kind",
                    "CREATE MATERIALIZED VIEW mv_lj_agg AS \
                     SELECT p.id AS id, p.kind AS kind, count(c.id) AS child_count \
                     FROM block p LEFT JOIN block c ON c.parent_id = p.id \
                     GROUP BY p.id, p.kind",
                ];
                (
                    Phase::Setup,
                    vec![
                        views
                            .iter()
                            .map(|sql| WorkItem {
                                sql: sql.to_string(),
                                params: vec![],
                            })
                            .collect(),
                    ],
                )
            }
            InternalPhase::Run => {
                if self.current_iteration >= self.iterations {
                    return (Phase::Done, vec![]);
                }

                // Single connection: matview maintenance is per-commit and we
                // want deterministic, comparable delta volume across runs.
                let _ = connections;
                let mut items = Vec::with_capacity(self.batch_size);
                for _ in 0..self.batch_size {
                    let n = self.statement_counter;
                    self.statement_counter += 1;
                    if n % 10 == 9 {
                        items.push(self.insert_item());
                    } else if n % 20 == 13 {
                        // Delete a previously inserted (non-seed) row if any exist.
                        let target = SEED_ROWS + (n / 20) % (self.next_insert_id - SEED_ROWS + 1);
                        items.push(WorkItem {
                            sql: "DELETE FROM block WHERE id = ?".to_string(),
                            params: vec![turso::Value::Integer(target as i64)],
                        });
                    } else {
                        let target = n % SEED_ROWS;
                        items.push(WorkItem {
                            sql: "UPDATE block SET content = ?, score = score + 1 WHERE id = ?"
                                .to_string(),
                            params: vec![
                                turso::Value::Text(format!("updated_{n}")),
                                turso::Value::Integer(target as i64),
                            ],
                        });
                    }
                }

                self.current_iteration += 1;
                (Phase::Run, vec![items])
            }
        }
    }
}
