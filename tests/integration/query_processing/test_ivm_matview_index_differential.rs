//! Differential generator: an indexed materialized view must answer every
//! query exactly as (a) a rowid table holding the same rows with the same
//! index and (b) the SAME matview with no index.
//!
//! Three arms in one database, kept in lockstep by every write:
//!   `v` — matview over `src`, indexed on `k`
//!   `t` — rowid table, indexed on `k`
//!   `u` — matview over `src2`, NOT indexed
//!
//! Arm (b) isolates "the index changed the answer" from "matviews differ from
//! tables"; arm (a) catches both agreeing on something wrong.
//!
//! Rowids are not comparable across arms, so a projected rowid is compared as
//! `rowid IS NULL` — which is what every rowid defect on this path is about.

use std::sync::Arc;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::common::{limbo_exec_rows, TempDatabase};

const SEEDS: [u64; 6] = [1, 2, 3, 0xD1FF, 0xC0FFEE, 0x5EED];

/// `@B@` is the relation under test, `@J@` a second reference to it for
/// self-joins, `@T@` the plain side table.
///
/// Every ORDER BY must be a TOTAL order over the projection: with ties the
/// row order (and, under LIMIT, the row SET) is unspecified, so the arms may
/// legitimately differ and the oracle would report a phantom defect.
const SHAPES: &[&str] = &[
    // point and range predicates on the indexed column
    "SELECT k, a, b FROM @B@ WHERE k = 'k03' ORDER BY k, a, b",
    "SELECT k, a FROM @B@ WHERE k >= 'k02' AND k < 'k07' ORDER BY k, a",
    "SELECT k FROM @B@ WHERE k > 'k05' ORDER BY k DESC",
    "SELECT k, a FROM @B@ WHERE k IN ('k01','k04','k09') ORDER BY k, a",
    "SELECT k, a FROM @B@ WHERE k = 'k03' OR a = 2 ORDER BY k, a",
    // projections in varied column order, including rowid
    "SELECT a, k, b FROM @B@ ORDER BY k, a, b",
    "SELECT rowid IS NULL, k FROM @B@ ORDER BY k",
    "SELECT k, rowid IS NULL, a FROM @B@ ORDER BY k, a",
    "SELECT rowid IS NULL FROM @B@ WHERE k = 'k03'",
    // aggregates and grouping on the indexed column and off it
    "SELECT count(*) FROM @B@",
    "SELECT k, count(*) FROM @B@ GROUP BY k ORDER BY k",
    "SELECT a, count(*), min(k), max(k) FROM @B@ GROUP BY a ORDER BY a",
    "SELECT min(k), max(k) FROM @B@",
    "SELECT sum(a), count(DISTINCT k) FROM @B@",
    "SELECT DISTINCT a FROM @B@ ORDER BY a",
    // ordering and limits
    "SELECT k, a FROM @B@ ORDER BY k, a LIMIT 3",
    "SELECT k, a FROM @B@ ORDER BY k DESC, a DESC LIMIT 3",
    "SELECT k FROM @B@ ORDER BY a, k",
    // inner and outer joins, both directions, with and without a match
    "SELECT x.k, y.a FROM @B@ x JOIN @J@ y ON y.k = x.k ORDER BY x.k, y.a",
    "SELECT x.k, y.k, y.a FROM @B@ x LEFT JOIN @J@ y ON y.k = x.b ORDER BY x.k, y.k, y.a",
    "SELECT x.k, y.rowid IS NULL, y.a FROM @B@ x LEFT JOIN @J@ y ON y.k = x.b ORDER BY x.k, y.a",
    "SELECT x.k, y.a, y.rowid IS NULL FROM @B@ x LEFT JOIN @J@ y ON y.k = x.b ORDER BY x.k, y.a",
    // rowid read BEFORE the table columns on the inner side — defect B's
    // opcode order (RowId resolves the DeferredSeek, then Column must still
    // find the row).
    //
    // These must reach the indexed relation from a THIRD relation's column
    // (`@T@`), not from a self-join. A self-join with rowid projected first
    // does NOT produce the sequence — measured in `lane-logs/b-shape-probe.log`,
    // where the self-join candidates stayed identical under a build with the
    // fix mutated out and only the side-table ones diverged. That is why the
    // first version of this file missed B.
    "SELECT x.k, y.rowid IS NULL, y.a FROM @B@ x LEFT JOIN @T@ s ON s.k = x.k \
     LEFT JOIN @J@ y ON y.k = s.tag ORDER BY x.k, y.a",
    "SELECT s.tag, y.rowid IS NULL, y.a, y.b FROM @T@ s JOIN @B@ y ON y.k = s.tag \
     ORDER BY s.tag, y.a, y.b",
    "SELECT x.k, y.a, y.b FROM @B@ x LEFT JOIN @J@ y ON y.k = x.b AND y.rowid IS NOT NULL \
     ORDER BY x.k, y.a, y.b",
    "SELECT x.k, max(y.rowid IS NULL), min(y.a) FROM @B@ x LEFT JOIN @T@ s ON s.k = x.k \
     LEFT JOIN @J@ y ON y.k = s.tag GROUP BY x.k ORDER BY x.k",
    "SELECT x.k, y.rowid IS NULL, y.a, y.b FROM @B@ x LEFT JOIN @J@ y ON y.k = x.b ORDER BY x.k, y.a, y.b",
    "SELECT x.k, count(y.k) FROM @B@ x LEFT JOIN @J@ y ON y.k = x.b GROUP BY x.k ORDER BY x.k",
    // join against a plain table so only one side is the relation under test
    "SELECT x.k, s.tag FROM @B@ x LEFT JOIN @T@ s ON s.k = x.k ORDER BY x.k, s.tag",
    "SELECT x.k, count(s.tag) FROM @B@ x LEFT JOIN @T@ s ON s.k = x.k \
     GROUP BY x.k HAVING count(s.tag) = 0 ORDER BY x.k",
];

/// One independent set of relations per seed, all inside the single database
/// the test macro provides (which is where `--experimental-views` is on).
struct Arms {
    sfx: String,
}

impl Arms {
    fn new(conn: &Arc<turso_core::Connection>, sfx: &str) -> anyhow::Result<Self> {
        for src in [format!("src{sfx}"), format!("src2{sfx}"), format!("t{sfx}")] {
            conn.execute(&format!(
                "CREATE TABLE {src} (id INTEGER PRIMARY KEY, k TEXT, a INTEGER, b TEXT)"
            ))?;
        }
        conn.execute(&format!("CREATE TABLE tags{sfx} (k TEXT, tag TEXT)"))?;
        conn.execute(&format!(
            "CREATE MATERIALIZED VIEW v{sfx} AS SELECT k, a, b FROM src{sfx}"
        ))?;
        conn.execute(&format!(
            "CREATE MATERIALIZED VIEW u{sfx} AS SELECT k, a, b FROM src2{sfx}"
        ))?;
        conn.execute(&format!("CREATE INDEX idx_v_k{sfx} ON v{sfx}(k)"))?;
        conn.execute(&format!("CREATE INDEX idx_t_k{sfx} ON t{sfx}(k)"))?;
        // A CHAINED level: `v2` reads the indexed `v` and carries its OWN
        // index, `u2` reads the unindexed `u`. Both are the identity over
        // their source, so the same rowid table `t` is the control for them.
        // A base write stages a delta for `v`/`u` only, so this level is what
        // catches a guard that is not transitive.
        conn.execute(&format!(
            "CREATE MATERIALIZED VIEW v2{sfx} AS SELECT k, a, b FROM v{sfx}"
        ))?;
        conn.execute(&format!(
            "CREATE MATERIALIZED VIEW u2{sfx} AS SELECT k, a, b FROM u{sfx}"
        ))?;
        conn.execute(&format!("CREATE INDEX idx_v2_k{sfx} ON v2{sfx}(k)"))?;
        Ok(Self {
            sfx: sfx.to_string(),
        })
    }

    /// The three write targets that must stay in lockstep: the two matview
    /// sources and the control table.
    fn write_targets(&self) -> [String; 3] {
        let s = &self.sfx;
        [format!("src{s}"), format!("src2{s}"), format!("t{s}")]
    }

    fn bind(&self, shape: &str, rel: &str) -> String {
        let r = format!("{rel}{}", self.sfx);
        shape
            .replace("@B@", &r)
            .replace("@J@", &r)
            .replace("@T@", &format!("tags{}", self.sfx))
    }
}

/// Apply the same logical write to all three arms.
fn write_all(
    conn: &Arc<turso_core::Connection>,
    arms: &Arms,
    stmt_for: impl Fn(&str) -> String,
) -> anyhow::Result<()> {
    for rel in arms.write_targets() {
        conn.execute(&stmt_for(&rel))?;
    }
    Ok(())
}

fn churn(
    conn: &Arc<turso_core::Connection>,
    arms: &Arms,
    rng: &mut ChaCha8Rng,
    steps: usize,
) -> anyhow::Result<()> {
    for step in 0..steps {
        let id = rng.random_range(0..12);
        let k = format!("k{:02}", rng.random_range(0..10));
        let a = rng.random_range(0..4);
        let b = format!("k{:02}", rng.random_range(0..12));
        match rng.random_range(0..5) {
            0 | 1 => write_all(conn, arms, |r| {
                format!("INSERT OR REPLACE INTO {r} (id,k,a,b) VALUES ({id},'{k}',{a},'{b}')")
            })?,
            2 => write_all(conn, arms, |r| {
                format!("UPDATE {r} SET k = '{k}' WHERE id = {id}")
            })?,
            3 => write_all(conn, arms, |r| {
                format!("UPDATE {r} SET a = {a}, b = '{b}' WHERE id = {id}")
            })?,
            _ => write_all(conn, arms, |r| format!("DELETE FROM {r} WHERE id = {id}"))?,
        }
        if rng.random_bool(0.3) {
            // `tag` is key-shaped so the side-table joins actually match;
            // `step` only decides which key.
            conn.execute(&format!(
                "INSERT INTO tags{} (k, tag) VALUES ('{k}', 'k{:02}')",
                arms.sfx,
                step % 10
            ))?;
        }
    }
    Ok(())
}

/// Every shape, on every arm, must agree. Returns the number of comparisons.
fn compare_all_shapes(
    conn: &Arc<turso_core::Connection>,
    arms: &Arms,
    seed: u64,
    phase: &str,
) -> usize {
    let mut checked = 0;
    for shape in SHAPES {
        // Level 1 is the matview over a table; level 2 is a matview over THAT
        // matview, each with its own index. Both must read like the table.
        for (indexed, plain, level) in [("v", "u", "direct"), ("v2", "u2", "chained")] {
            let indexed_view = limbo_exec_rows(conn, &arms.bind(shape, indexed));
            let table = limbo_exec_rows(conn, &arms.bind(shape, "t"));
            let plain_view = limbo_exec_rows(conn, &arms.bind(shape, plain));
            let phase = &format!("{phase}/{level}");

            assert_eq!(
                indexed_view,
                table,
                "seed {seed} [{phase}]: INDEXED MATVIEW vs ROWID TABLE disagree\n  \
             matview : {}\n  table   : {}",
                arms.bind(shape, indexed),
                arms.bind(shape, "t"),
            );
            assert_eq!(
                indexed_view,
                plain_view,
                "seed {seed} [{phase}]: the INDEX changed the answer — indexed matview \
             vs the SAME matview unindexed\n  indexed  : {}\n  unindexed: {}",
                arms.bind(shape, indexed),
                arms.bind(shape, plain),
            );
            checked += 1;
        }
    }
    checked
}

/// Committed state: the domain an indexed matview must be correct in today.
#[turso_macros::test(views)]
fn differential_committed_state(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let mut total = 0;
    for (i, seed) in SEEDS.iter().enumerate() {
        let arms = Arms::new(&conn, &format!("_s{i}"))?;
        let mut rng = ChaCha8Rng::seed_from_u64(*seed);
        churn(&conn, &arms, &mut rng, 8)?;
        total += compare_all_shapes(&conn, &arms, *seed, "after initial load");
        churn(&conn, &arms, &mut rng, 12)?;
        total += compare_all_shapes(&conn, &arms, *seed, "after more churn");
    }
    assert_eq!(
        total,
        SEEDS.len() * SHAPES.len() * 2 * 2,
        "the generator must actually have compared something"
    );
    Ok(())
}

/// Inside an OPEN write transaction the indexed arm must either AGREE with
/// the other two or REFUSE (ruling C2) — it must never disagree silently.
///
/// One transaction per shape: a refusal aborts the transaction, so a single
/// long transaction would end at the first refused shape and the rest would
/// not be exercised.
#[turso_macros::test(views)]
fn differential_inside_open_transaction(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let mut refused = 0;
    let mut agreed = 0;
    for (i, seed) in SEEDS.iter().enumerate() {
        let arms = Arms::new(&conn, &format!("_x{i}"))?;
        let mut rng = ChaCha8Rng::seed_from_u64(*seed);
        churn(&conn, &arms, &mut rng, 8)?;

        // One transaction per (shape, arm): a refusal aborts the
        // transaction, so testing two arms inside one would silently skip the
        // second — which is exactly how a non-transitive guard hid from this
        // generator once already.
        for shape in SHAPES {
            for indexed in ["v", "v2"] {
                conn.execute("BEGIN")?;
                churn(&conn, &arms, &mut rng, 2)?;

                let indexed_sql = arms.bind(shape, indexed);
                if let Err(e) = conn.execute(&indexed_sql) {
                    let msg = e.to_string();
                    assert!(
                        msg.contains(
                            "cannot be read while this transaction has uncommitted changes to it"
                        ),
                        "seed {seed}: the only tolerated in-transaction failure is the C2 \
                         refusal; got: {msg}\n  query: {indexed_sql}"
                    );
                    refused += 1;
                    // The refusal aborted the transaction.
                    continue;
                }

                let indexed_view = limbo_exec_rows(&conn, &indexed_sql);
                let table = limbo_exec_rows(&conn, &arms.bind(shape, "t"));
                let plain = if indexed == "v" { "u" } else { "u2" };
                let plain_view = limbo_exec_rows(&conn, &arms.bind(shape, plain));
                assert_eq!(
                    indexed_view,
                    table,
                    "seed {seed} [in tx, {indexed}]: the indexed matview neither refused nor \
                     agreed with the rowid table\n  matview: {indexed_sql}\n  table  : {}",
                    arms.bind(shape, "t"),
                );
                assert_eq!(
                    indexed_view,
                    plain_view,
                    "seed {seed} [in tx, {indexed}]: the indexed matview neither refused nor \
                     agreed with the SAME matview unindexed\n  indexed  : {indexed_sql}\n  \
                     unindexed: {}",
                    arms.bind(shape, plain),
                );
                agreed += 1;
                conn.execute("ROLLBACK")?;
            }
        }
    }
    assert!(
        refused > 0 && agreed > 0,
        "the in-transaction phase must exercise BOTH outcomes; refused={refused} agreed={agreed}"
    );
    Ok(())
}

/// A deterministic hand-written case, kept beside the generator so a failure
/// there has a minimal companion to shrink against.
#[turso_macros::test(views)]
fn differential_minimal_fixture(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let arms = Arms::new(&conn, "_m")?;
    for (id, k, a, b) in [
        (1, "k01", 1, "k02"),
        (2, "k02", 2, "zz"),
        (3, "k03", 1, "k01"),
        (4, "k04", 3, "zz"),
    ] {
        write_all(&conn, &arms, |r| {
            format!("INSERT INTO {r} (id,k,a,b) VALUES ({id},'{k}',{a},'{b}')")
        })?;
    }
    conn.execute("INSERT INTO tags_m (k, tag) VALUES ('k01','k02'), ('k03','k04')")?;
    let checked = compare_all_shapes(&conn, &arms, 0, "minimal fixture");
    assert_eq!(checked, SHAPES.len() * 2, "both levels must be compared");
    Ok(())
}
