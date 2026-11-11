//! Materialized views that join on a key column: a recursive CTE view, a
//! materialized view over a materialized view, a materialized view over a
//! recursive view, and a plain join. The id and parent columns each take any
//! declared type a caller can write, the join column has an index of any form
//! or none, and the views exist before or after the data.
//!
//! Two oracles. Correctness: after every statement each view holds the rows of
//! a fresh run of its SELECT, and a write succeeds exactly when it succeeds on
//! the same table without views. Cost: the VM steps of one INSERT, maintenance
//! included, counted by a progress handler that fires on every step, must not
//! grow with the table when an index fits every join of the view. A view that
//! joins on a key of a type with its own order (numeric(10,2), arrays) fails
//! loud at CREATE and names the type and the collation.

use crate::common::{limbo_exec_rows, try_limbo_exec_rows, TempDatabase};
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use turso_core::Connection;

/// A declared column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decl {
    Integer,
    Text,
    TextNocase,
    TextRtrim,
    /// TEXT with a locale collation, which the engine knows by name.
    TextLocale,
    Real,
    Blob,
    Untyped,
    NumericAffinity,
    /// The built-in custom type numeric(10,2); it needs a STRICT table.
    Decimal,
    /// Array types; they need a STRICT table.
    IntArray,
    TextArray,
}

impl Decl {
    pub const ALL: [Decl; 12] = [
        Decl::Integer,
        Decl::Text,
        Decl::TextNocase,
        Decl::TextRtrim,
        Decl::TextLocale,
        Decl::Real,
        Decl::Blob,
        Decl::Untyped,
        Decl::NumericAffinity,
        Decl::Decimal,
        Decl::IntArray,
        Decl::TextArray,
    ];

    fn sql(self, strict: bool) -> &'static str {
        match self {
            Decl::Integer => "INTEGER",
            Decl::Text => "TEXT",
            Decl::TextNocase => "TEXT COLLATE NOCASE",
            Decl::TextRtrim => "TEXT COLLATE RTRIM",
            Decl::TextLocale => "TEXT COLLATE \"en-US\"",
            Decl::Real => "REAL",
            Decl::Blob => "BLOB",
            // A STRICT table has no untyped or NUMERIC column; ANY stores what it gets.
            Decl::Untyped | Decl::NumericAffinity if strict => "ANY",
            Decl::Untyped => "",
            Decl::NumericAffinity => "NUMERIC",
            Decl::Decimal => "numeric(10,2)",
            Decl::IntArray => "INTEGER[]",
            Decl::TextArray => "TEXT[]",
        }
    }

    /// The names a loud refusal may give a type whose values carry their own
    /// order: no view may compare them yet. Empty for every other type.
    fn refused_names(self) -> &'static [&'static str] {
        match self {
            Decl::Decimal => &["numeric"],
            Decl::IntArray | Decl::TextArray => &["array", "[]"],
            _ => &[],
        }
    }

    fn refused(self) -> bool {
        !self.refused_names().is_empty()
    }

    /// Whether a key of this type is stored as TEXT, as a CAST to TEXT stores it.
    fn stores_text(self) -> bool {
        matches!(
            self,
            Decl::Text | Decl::TextRtrim | Decl::TextNocase | Decl::TextLocale | Decl::Untyped
        )
    }

    /// The literal of key `i` as a caller of a column of this type writes it.
    pub fn key(self, i: i64) -> String {
        match self {
            Decl::Integer => i.to_string(),
            Decl::Text | Decl::TextRtrim | Decl::Untyped => format!("'b{i}'"),
            Decl::TextNocase | Decl::TextLocale => format!("'B{i}'"),
            Decl::Real => format!("{i}.5"),
            Decl::Blob => format!("X'{}'", hex(&format!("b{i}"))),
            Decl::NumericAffinity => format!("'{i}'"),
            Decl::Decimal => format!("{i}.25"),
            Decl::IntArray => format!("'{{{i}}}'"),
            Decl::TextArray => format!("'{{b{i}}}'"),
        }
    }
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02X}")).collect()
}

/// Compares ASCII case-insensitively, like NOCASE, but as an application
/// collation that the engine does not know.
unsafe extern "C" fn fold(
    _: usize,
    left: *const u8,
    left_len: usize,
    right: *const u8,
    right_len: usize,
) -> i32 {
    let bytes = |ptr: *const u8, len: usize| -> Vec<u8> {
        if len == 0 {
            return Vec::new();
        }
        unsafe { std::slice::from_raw_parts(ptr, len) }.to_ascii_lowercase()
    };
    bytes(left, left_len).cmp(&bytes(right, right_len)) as i32
}

/// A connection that knows the collation `fold`.
pub fn connect(db: &TempDatabase) -> Arc<Connection> {
    let conn = db.connect_limbo();
    conn.register_external_collation("fold".to_string(), 0, fold, None);
    conn
}

/// An explicit collation on the join comparison `parent = id` of every view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinCollate {
    /// None: the comparison takes the columns' collation.
    Columns,
    Binary,
    Nocase,
    /// The application collation `fold`, see [`connect`].
    Fold,
}

impl JoinCollate {
    fn sql(self) -> &'static str {
        match self {
            JoinCollate::Columns => "",
            JoinCollate::Binary => " COLLATE BINARY",
            JoinCollate::Nocase => " COLLATE NOCASE",
            JoinCollate::Fold => " COLLATE fold",
        }
    }
}

/// The types of `node.id` and `node.parent`, and the collation of the join on
/// them. A child stores its parent's id literal in `parent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keys {
    pub id: Decl,
    pub parent: Decl,
    pub join: JoinCollate,
    /// The depth bound of every recursive view.
    pub depth: u32,
}

impl Keys {
    pub fn same(decl: Decl) -> Keys {
        Keys {
            id: decl,
            parent: decl,
            join: JoinCollate::Columns,
            depth: COST_DEPTH,
        }
    }

    /// A refused type needs a STRICT table.
    pub fn strict(self) -> bool {
        self.id.refused() || self.parent.refused()
    }

    pub fn key(self, i: i64) -> String {
        self.id.key(i)
    }

    pub fn create_table(self, id: IdConstraint) -> String {
        let constraint = match id {
            IdConstraint::PrimaryKey => " PRIMARY KEY",
            IdConstraint::Unique => " UNIQUE",
            IdConstraint::Indexed | IdConstraint::Without => "",
        };
        format!(
            "CREATE TABLE node(id {}{constraint}, parent {}, label TEXT){}",
            self.id.sql(self.strict()),
            self.parent.sql(self.strict()),
            if self.strict() { " STRICT" } else { "" }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdConstraint {
    PrimaryKey,
    Unique,
    Indexed,
    Without,
}

impl IdConstraint {
    pub const ALL: [IdConstraint; 4] = [
        IdConstraint::PrimaryKey,
        IdConstraint::Unique,
        IdConstraint::Indexed,
        IdConstraint::Without,
    ];

    fn index(self) -> Option<&'static str> {
        (self == IdConstraint::Indexed).then_some("CREATE INDEX node_id ON node(id)")
    }

    fn fits(self) -> bool {
        self != IdConstraint::Without
    }
}

/// The form of an index on a join column: `node.parent`, or `id` and
/// `parent` of a view that another view reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexForm {
    Plain,
    Descending,
    TwoColumn,
    Partial,
    OtherCollation,
    Expression,
}

impl IndexForm {
    pub const ALL: [IndexForm; 6] = [
        IndexForm::Plain,
        IndexForm::Descending,
        IndexForm::TwoColumn,
        IndexForm::Partial,
        IndexForm::OtherCollation,
        IndexForm::Expression,
    ];

    /// The index `<table>_<column>`; every table it goes on has a `label`.
    fn create(self, table: &str, column: &str) -> String {
        let on = match self {
            IndexForm::Plain => format!("({column})"),
            IndexForm::Descending => format!("({column} DESC)"),
            IndexForm::TwoColumn => format!("({column}, label)"),
            IndexForm::Partial => format!("({column}) WHERE label IS NOT NULL"),
            IndexForm::OtherCollation => format!("({column} COLLATE NOCASE)"),
            IndexForm::Expression => format!("(lower({column}))"),
        };
        format!("CREATE INDEX {table}_{column} ON {table}{on}")
    }

    /// A materialized view refuses a partial or an expression index.
    fn refused_on_view(self) -> bool {
        matches!(self, IndexForm::Partial | IndexForm::Expression)
    }

    /// An equality on the column alone, under the collation of a column of
    /// type `decl`, can seek this index for every row.
    fn fits(self, decl: Decl) -> bool {
        match self {
            IndexForm::Plain | IndexForm::Descending | IndexForm::TwoColumn => true,
            IndexForm::OtherCollation => decl == Decl::TextNocase,
            IndexForm::Partial | IndexForm::Expression => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shape {
    Recursive,
    MatviewOverMatview,
    MatviewOverRecursive,
    PlainJoin,
    RecursiveOverMatview,
}

/// The recursive part stops at `keys.depth`: keys that compare equal across
/// distinct rows (affinity, collation, duplicate ids) can close a cycle, and
/// a recursion without a bound has no finite result then.
pub fn recursive_select(keys: Keys, source: &str) -> String {
    format!(
        "WITH RECURSIVE paths AS ( \
         SELECT id, parent, label, '/' || id AS path, 0 AS depth FROM {source} WHERE parent IS NULL \
         UNION ALL \
         SELECT b.id, b.parent, b.label, p.path || '/' || b.id, p.depth + 1 \
         FROM {source} b JOIN paths p ON b.parent = p.id{} WHERE p.depth < {}) \
         SELECT * FROM paths",
        keys.join.sql(),
        keys.depth
    )
}

/// The view `mm_in` or `rm_in`: a copy of `node` without a join.
const COPY_SELECT: &str = "SELECT id, parent, label FROM node WHERE label IS NOT 'x'";

impl Shape {
    pub const ALL: [Shape; 5] = [
        Shape::Recursive,
        Shape::MatviewOverMatview,
        Shape::MatviewOverRecursive,
        Shape::PlainJoin,
        Shape::RecursiveOverMatview,
    ];

    /// The view that reads another view, when the shape has one.
    fn input(self) -> Option<&'static str> {
        match self {
            Shape::MatviewOverMatview => Some("mm_in"),
            Shape::MatviewOverRecursive => Some("mr_in"),
            Shape::RecursiveOverMatview => Some("rm_in"),
            Shape::Recursive | Shape::PlainJoin => None,
        }
    }

    fn top(self) -> &'static str {
        match self {
            Shape::Recursive => "rv",
            Shape::MatviewOverMatview => "mm",
            Shape::MatviewOverRecursive => "mr",
            Shape::PlainJoin => "pj",
            Shape::RecursiveOverMatview => "rm",
        }
    }

    /// The views of the shape in creation order.
    fn names(self) -> Vec<&'static str> {
        match self {
            Shape::Recursive => vec!["rv"],
            Shape::MatviewOverMatview => vec!["mm_in", "mm"],
            Shape::MatviewOverRecursive => vec!["mr_in", "mr"],
            Shape::PlainJoin => vec!["pj"],
            Shape::RecursiveOverMatview => vec!["rm_in", "rm"],
        }
    }

    /// The views of the shape in creation order, each with its SELECT.
    pub fn views(self, keys: Keys) -> Vec<(&'static str, String)> {
        let on = keys.join.sql();
        let selects = match self {
            Shape::Recursive => vec![recursive_select(keys, "node")],
            Shape::MatviewOverMatview => vec![
                COPY_SELECT.to_string(),
                format!(
                    "SELECT c.id AS child, p.id AS up, p.label FROM mm_in c JOIN mm_in p ON c.parent = p.id{on}"
                ),
            ],
            Shape::MatviewOverRecursive => vec![
                recursive_select(keys, "node"),
                format!(
                    "SELECT c.id AS child, p.path AS up_path FROM mr_in c JOIN mr_in p ON c.parent = p.id{on}"
                ),
            ],
            Shape::PlainJoin => vec![format!(
                "SELECT a.id AS child, b.id AS up, b.label FROM node a JOIN node b ON a.parent = b.id{on}"
            )],
            Shape::RecursiveOverMatview => {
                vec![COPY_SELECT.to_string(), recursive_select(keys, "rm_in")]
            }
        };
        self.names().into_iter().zip(selects).collect()
    }

    fn create(self, keys: Keys) -> Vec<String> {
        self.views(keys)
            .into_iter()
            .map(|(name, select)| format!("CREATE MATERIALIZED VIEW {name} AS {select}"))
            .collect()
    }

    fn drop(self) -> Vec<String> {
        self.names()
            .into_iter()
            .rev()
            .map(|name| format!("DROP VIEW {name}"))
            .collect()
    }

    /// Every view but the copies `mm_in` and `rm_in` joins on the key.
    fn joins(view: &str) -> bool {
        view != "mm_in" && view != "rm_in"
    }

    fn create_input_indexes(self, form: IndexForm) -> Vec<String> {
        self.input()
            .map(|name| vec![form.create(name, "parent"), form.create(name, "id")])
            .unwrap_or_default()
    }

    fn drop_input_indexes(self) -> Vec<String> {
        self.input()
            .map(|name| {
                vec![
                    format!("DROP INDEX {name}_parent"),
                    format!("DROP INDEX {name}_id"),
                ]
            })
            .unwrap_or_default()
    }
}

pub fn open(keys: Keys, views: bool) -> TempDatabase {
    let mut builder = TempDatabase::builder().with_views(views);
    if keys.strict() {
        builder = builder.with_opts(turso_core::DatabaseOpts::new().with_custom_types(true));
    }
    builder.build()
}

/// VM steps of `sql`, maintenance of every view included.
pub fn steps_of(conn: &Arc<Connection>, sql: &str) -> Result<u64, turso_core::LimboError> {
    let steps = Arc::new(AtomicU64::new(0));
    let counter = steps.clone();
    conn.set_progress_handler(
        1,
        Some(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            false
        })),
    );
    let result = conn.execute(sql);
    conn.set_progress_handler(0, None);
    result.map(|()| steps.load(Ordering::Relaxed))
}

pub fn sorted(rows: Vec<Vec<rusqlite::types::Value>>) -> Vec<String> {
    let mut rows: Vec<String> = rows.into_iter().map(|row| format!("{row:?}")).collect();
    rows.sort();
    rows
}

fn rows(db: &TempDatabase, conn: &Arc<Connection>, sql: &str) -> Result<Vec<String>, String> {
    try_limbo_exec_rows(db, conn, sql)
        .map(sorted)
        .map_err(|e| format!("`{sql}`: {e}"))
}

/// Every view of `shapes` holds the rows of a fresh run of its SELECT. An
/// input view is checked before the view that reads it, so a fresh SELECT
/// over the input reads correct rows.
fn check_views(
    db: &TempDatabase,
    conn: &Arc<Connection>,
    shapes: &BTreeSet<Shape>,
    keys: Keys,
    at: &str,
) -> Result<(), String> {
    for shape in shapes {
        for (name, select) in shape.views(keys) {
            let held = rows(db, conn, &format!("SELECT * FROM {name}"))
                .map_err(|e| format!("{at}: reading view {name}: {e}"))?;
            let fresh = rows(db, conn, &select).map_err(|e| format!("{at}: {e}"))?;
            if held != fresh {
                return Err(format!(
                    "{at}: view {name} differs from its SELECT\nview:   {held:?}\nselect: {fresh:?}"
                ));
            }
        }
    }
    Ok(())
}

/// The loud refusal that an unsupported key type gets: it names a refused
/// key type of `decls` and the collation. Two different refused types meet
/// the older refusal of every SELECT comparing them, which names both.
fn names_type_and_collation(error: &str, decls: &[Decl]) -> bool {
    let error = error.to_lowercase();
    let names = |decl: &Decl| decl.refused_names().iter().any(|name| error.contains(name));
    let refused: Vec<&Decl> = decls.iter().filter(|decl| decl.refused()).collect();
    let two_types = matches!(refused[..], [a, b] if a != b);
    error.contains("collat") && refused.iter().any(|decl| names(decl))
        || two_types
            && error.contains("cannot compare type")
            && refused.iter().all(|decl| names(decl))
}

/// The view a `CREATE MATERIALIZED VIEW` statement creates.
fn created_view(sql: &str) -> Option<&str> {
    sql.strip_prefix("CREATE MATERIALIZED VIEW ")?
        .split(' ')
        .next()
}

/// Whether `sql` creates a view that joins on a key of a refused type: it
/// must fail loud.
fn must_fail_loud(keys: Keys, sql: &str) -> bool {
    keys.strict() && created_view(sql).is_some_and(Shape::joins)
}

// ---------------------------------------------------------------------------
// Cost of one insert
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct CostCase {
    pub keys: Keys,
    pub shape: Shape,
    pub id: IdConstraint,
    pub parent_index: Option<IndexForm>,
    /// The form of the indexes on `id` and `parent` of the input view.
    pub input_index: Option<IndexForm>,
    pub fanout: i64,
    pub views_before_data: bool,
}

impl CostCase {
    /// An index fits every join the maintenance of the view runs for a leaf
    /// insert, and both key columns have one type and the join compares under
    /// their declared collation, so one collation and one affinity.
    fn claims_flat(self) -> bool {
        let decl = self.keys.id;
        let keys =
            decl == self.keys.parent && !decl.refused() && self.keys.join == JoinCollate::Columns;
        let parent = self.parent_index.is_some_and(|form| form.fits(decl));
        let input = self.input_index.is_some_and(|form| form.fits(decl));
        keys && match self.shape {
            Shape::Recursive => parent,
            Shape::MatviewOverMatview => input,
            // A recursive input may store a value outside its column's declared
            // class, so a join reads it without its index until each arm's stored type is known.
            Shape::MatviewOverRecursive => false,
            Shape::PlainJoin => parent && self.id.fits(),
            Shape::RecursiveOverMatview => input,
        }
    }
}

/// Depth 8 keeps a fanout-2 scan visible; cost-case keys are unique, so paths stay linear.
const COST_DEPTH: u32 = 8;
/// Keys that collide make paths grow as rows^depth.
const DIFFERENTIAL_DEPTH: u32 = 3;

/// Mostly [`DIFFERENTIAL_DEPTH`]; deeper bounds less often, as their cases
/// run longest.
fn differential_depth() -> impl Strategy<Value = u32> {
    prop_oneof![6 => Just(DIFFERENTIAL_DEPTH), 2 => 4u32..=6]
}
const SMALL: i64 = 120;
const GROWTH: i64 = 8;

enum Built {
    Steps(u64),
    /// The write is refused on the same table without views too.
    Refused,
    /// The view creation failed loud for a key type it cannot seek.
    Loud,
}

fn build_forest(conn: &Arc<Connection>, case: CostCase, n: i64, views: bool) -> Result<(), String> {
    let keys = case.keys;
    let run = |sql: &str| conn.execute(sql).map_err(|e| format!("`{sql}`: {e}"));
    let create_views = || -> Result<(), String> {
        for sql in case.shape.create(keys) {
            run(&sql)?;
        }
        if let Some(form) = case.input_index {
            for sql in case.shape.create_input_indexes(form) {
                match (run(&sql), form.refused_on_view()) {
                    (Ok(()), false) | (Err(_), true) => {}
                    (Ok(()), true) => return Err(format!("`{sql}` succeeds; a view refuses it")),
                    (Err(e), false) => return Err(e),
                }
            }
        }
        Ok(())
    };
    run(&keys.create_table(case.id))?;
    if let Some(sql) = case.id.index() {
        run(sql)?;
    }
    if let Some(form) = case.parent_index {
        run(&form.create("node", "parent"))?;
    }
    if views && case.views_before_data {
        create_views()?;
    }
    let roots = 10;
    run("BEGIN")?;
    for chunk in (0..n).collect::<Vec<_>>().chunks(200) {
        let values: Vec<String> = chunk
            .iter()
            .map(|&i| {
                let parent = if i < roots {
                    "NULL".to_string()
                } else {
                    keys.key((i - roots) / case.fanout)
                };
                format!("({}, {parent}, 'c')", keys.key(i))
            })
            .collect();
        run(&format!("INSERT INTO node VALUES {}", values.join(",")))?;
    }
    run("COMMIT")?;
    if views && !case.views_before_data {
        create_views()?;
    }
    Ok(())
}

fn leaf_inserts(case: CostCase, n: i64) -> Vec<String> {
    (0..3)
        .map(|j| {
            format!(
                "INSERT INTO node VALUES ({}, {}, 'c')",
                case.keys.key(n + j),
                case.keys.key((j * 37 + n / 2) % n)
            )
        })
        .collect()
}

/// The median VM-step count of three leaf inserts into a forest of `n` rows.
fn insert_steps(case: CostCase, n: i64) -> Result<Built, String> {
    let db = open(case.keys, true);
    let conn = connect(&db);
    if let Err(e) = build_forest(&conn, case, n, true) {
        let twin = open(case.keys, false);
        if build_forest(&connect(&twin), case, n, false).is_err() {
            return Ok(Built::Refused);
        }
        let statement = e.strip_prefix('`').unwrap_or(&e);
        if must_fail_loud(case.keys, statement)
            && names_type_and_collation(&e, &[case.keys.id, case.keys.parent])
        {
            return Ok(Built::Loud);
        }
        return Err(format!(
            "the table without views accepts it, the views refuse it: {e}"
        ));
    }
    if let Some(sql) = case
        .shape
        .create(case.keys)
        .into_iter()
        .find(|sql| must_fail_loud(case.keys, sql))
    {
        return Err(format!(
            "`{sql}` succeeds; a join key of a refused type must fail loud naming the type and the collation"
        ));
    }
    let mut samples = Vec::new();
    for sql in leaf_inserts(case, n) {
        match steps_of(&conn, &sql) {
            Ok(steps) => samples.push(steps),
            Err(e) => {
                let twin = open(case.keys, false);
                let twin_conn = connect(&twin);
                build_forest(&twin_conn, case, n, false)?;
                if twin_conn.execute(&sql).is_ok() {
                    return Err(format!("`{sql}` fails with views only: {e}"));
                }
            }
        }
    }
    check_views(
        &db,
        &conn,
        &BTreeSet::from([case.shape]),
        case.keys,
        &format!("after the leaf inserts into {n} rows"),
    )?;
    if samples.is_empty() {
        return Ok(Built::Refused);
    }
    samples.sort();
    Ok(Built::Steps(samples[samples.len() / 2]))
}

fn insert_cost_is_flat(case: CostCase) -> Result<(), TestCaseError> {
    let small = insert_steps(case, SMALL).map_err(TestCaseError::fail)?;
    let large = insert_steps(case, SMALL * GROWTH).map_err(TestCaseError::fail)?;
    match (small, large) {
        (Built::Steps(small), Built::Steps(large)) => {
            if case.claims_flat() && large >= 2 * small {
                return Err(TestCaseError::fail(format!(
                    "one insert takes {small} VM steps at {SMALL} rows and {large} at {} rows: the maintenance scans",
                    SMALL * GROWTH
                )));
            }
            Ok(())
        }
        (Built::Loud, Built::Loud) | (Built::Refused, Built::Refused) => Ok(()),
        _ => Err(TestCaseError::fail(
            "the two table sizes got different outcomes".to_string(),
        )),
    }
}

fn decl() -> impl Strategy<Value = Decl> {
    prop::sample::select(Decl::ALL.to_vec())
}

fn index_form() -> impl Strategy<Value = IndexForm> {
    prop::sample::select(IndexForm::ALL.to_vec())
}

fn keys(depth: impl Strategy<Value = u32>) -> impl Strategy<Value = Keys> {
    let join = prop_oneof![
        6 => Just(JoinCollate::Columns),
        1 => Just(JoinCollate::Binary),
        1 => Just(JoinCollate::Nocase),
        1 => Just(JoinCollate::Fold),
    ];
    (decl(), decl(), join, depth).prop_map(|(id, parent, join, depth)| Keys {
        id,
        parent,
        join,
        depth,
    })
}

fn cost_case() -> impl Strategy<Value = CostCase> {
    (
        keys(Just(COST_DEPTH)),
        prop::sample::select(Shape::ALL.to_vec()),
        prop::sample::select(IdConstraint::ALL.to_vec()),
        prop::option::weighted(0.8, index_form()),
        prop::option::weighted(0.7, index_form()),
        2i64..=8,
        any::<bool>(),
    )
        .prop_map(
            |(keys, shape, id_constraint, parent_index, input_index, fanout, before)| CostCase {
                keys,
                shape,
                id: id_constraint,
                parent_index,
                input_index,
                fanout,
                views_before_data: before,
            },
        )
}

fn fitting(keys: Keys, shape: Shape) -> CostCase {
    CostCase {
        keys,
        shape,
        id: IdConstraint::PrimaryKey,
        parent_index: Some(IndexForm::Plain),
        input_index: Some(IndexForm::Plain),
        fanout: 4,
        views_before_data: false,
    }
}

fn runner(cases: u32) -> TestRunner {
    TestRunner::new(RunnerConfig {
        cases,
        max_shrink_iters: 48,
        failure_persistence: None,
        ..RunnerConfig::default()
    })
}

#[test]
fn ivm_insert_cost_stays_flat_when_an_index_fits_every_join() {
    let mut fixed = vec![
        // A TEXT key with an index on the join column.
        fitting(Keys::same(Decl::Text), Shape::Recursive),
        // A TEXT key without an index: correct, the cost is not claimed.
        CostCase {
            parent_index: None,
            ..fitting(Keys::same(Decl::Text), Shape::Recursive)
        },
        // A NOCASE key with an index declared NOCASE on the join column.
        CostCase {
            parent_index: Some(IndexForm::OtherCollation),
            ..fitting(Keys::same(Decl::TextNocase), Shape::Recursive)
        },
        // An INTEGER key that is not the rowid alias.
        CostCase {
            id: IdConstraint::Unique,
            ..fitting(Keys::same(Decl::Integer), Shape::PlainJoin)
        },
        // A partial index on the input view: correct, the cost is not claimed.
        CostCase {
            input_index: Some(IndexForm::Partial),
            ..fitting(Keys::same(Decl::Text), Shape::MatviewOverMatview)
        },
    ];
    for decl in Decl::ALL {
        for shape in Shape::ALL {
            fixed.push(fitting(Keys::same(decl), shape));
        }
    }
    let fixed = fixed
        .into_iter()
        .map(|case| (format!("{case:?}"), case))
        .collect();
    check_all(fixed, 24, cost_case(), insert_cost_is_flat);
}

/// Runs every fixed example, then the random cases, and reports every failure.
fn check_all<S>(
    fixed: Vec<(String, S::Value)>,
    cases: u32,
    strategy: S,
    property: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S: Strategy,
    S::Value: Clone + std::fmt::Debug,
{
    let mut failures: Vec<String> = fixed
        .into_iter()
        .filter_map(|(name, value)| {
            property(value)
                .err()
                .map(|e| format!("fixed example {name}: {e}"))
        })
        .collect();
    if let Err(e) = runner(cases).run(&strategy, &property) {
        failures.push(format!("random cases: {e}"));
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Correctness under every statement that touches the joined table
// ---------------------------------------------------------------------------

/// How a key literal is written: in the id column's own form, or in a form
/// that contradicts it (another case, a trailing space, another storage class).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Own,
    Lower,
    Upper,
    TrailingSpace,
    Int,
    IntText,
    Real,
    LeadingZero,
    Blob,
    Decimal,
    Null,
}

impl Form {
    const CONTRADICTING: [Form; 10] = [
        Form::Lower,
        Form::Upper,
        Form::TrailingSpace,
        Form::Int,
        Form::IntText,
        Form::Real,
        Form::LeadingZero,
        Form::Blob,
        Form::Decimal,
        Form::Null,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lit {
    key: i64,
    form: Form,
}

impl Lit {
    fn sql(self, keys: Keys) -> String {
        let i = self.key;
        match self.form {
            Form::Own => keys.key(i),
            Form::Lower => format!("'b{i}'"),
            Form::Upper => format!("'B{i}'"),
            Form::TrailingSpace => format!("'b{i} '"),
            Form::Int => i.to_string(),
            Form::IntText => format!("'{i}'"),
            Form::Real => format!("{i}.0"),
            Form::LeadingZero => format!("'0{i}'"),
            Form::Blob => format!("X'{}'", hex(&format!("b{i}"))),
            Form::Decimal => format!("{i}.25"),
            Form::Null => "NULL".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    Kept,
    Filtered,
    Null,
}

impl Label {
    fn sql(self) -> &'static str {
        match self {
            Label::Kept => "'a'",
            Label::Filtered => "'x'",
            Label::Null => "NULL",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Txn {
    Begin,
    Commit,
    Rollback,
    Savepoint,
    RollbackTo,
    Release,
}

impl Txn {
    fn sql(self) -> &'static str {
        match self {
            Txn::Begin => "BEGIN",
            Txn::Commit => "COMMIT",
            Txn::Rollback => "ROLLBACK",
            Txn::Savepoint => "SAVEPOINT s",
            Txn::RollbackTo => "ROLLBACK TO s",
            Txn::Release => "RELEASE s",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    Insert {
        rows: Vec<(Lit, Lit, Label)>,
        replace: bool,
        conn: usize,
    },
    Delete {
        id: Lit,
        conn: usize,
    },
    Reparent {
        id: Lit,
        parent: Lit,
        conn: usize,
    },
    Rekey {
        id: Lit,
        new: Lit,
        conn: usize,
    },
    Relabel {
        id: Lit,
        label: Label,
        conn: usize,
    },
    Txn(Txn, usize),
    CreateParentIndex(IndexForm),
    DropParentIndex,
    Reindex,
    CreateView(Shape),
    DropView(Shape),
    CreateInputIndexes(Shape, IndexForm),
    DropInputIndexes(Shape),
    Refresh(Shape),
    /// An index on node(label) named like a view: one namespace holds both.
    CreateLabelIndexNamedLike(Shape),
    DropLabelIndexNamedLike(Shape),
}

impl Op {
    /// Statements run on both databases; their outcome must agree.
    fn table_sql(&self, keys: Keys) -> Option<(usize, String)> {
        let row = |(id, parent, label): &(Lit, Lit, Label)| {
            format!("({}, {}, {})", id.sql(keys), parent.sql(keys), label.sql())
        };
        Some(match self {
            Op::Insert {
                rows,
                replace,
                conn,
            } => (
                *conn,
                format!(
                    "INSERT {}INTO node VALUES {}",
                    if *replace { "OR REPLACE " } else { "" },
                    rows.iter().map(row).collect::<Vec<_>>().join(", ")
                ),
            ),
            Op::Delete { id, conn } => (
                *conn,
                format!("DELETE FROM node WHERE id = {}", id.sql(keys)),
            ),
            Op::Reparent { id, parent, conn } => (
                *conn,
                format!(
                    "UPDATE node SET parent = {} WHERE id = {}",
                    parent.sql(keys),
                    id.sql(keys)
                ),
            ),
            Op::Rekey { id, new, conn } => (
                *conn,
                format!(
                    "UPDATE node SET id = {} WHERE id = {}",
                    new.sql(keys),
                    id.sql(keys)
                ),
            ),
            Op::Relabel { id, label, conn } => (
                *conn,
                format!(
                    "UPDATE node SET label = {} WHERE id = {}",
                    label.sql(),
                    id.sql(keys)
                ),
            ),
            Op::Txn(txn, conn) => (*conn, txn.sql().to_string()),
            Op::CreateParentIndex(form) => (0, form.create("node", "parent")),
            Op::DropParentIndex => (0, "DROP INDEX node_parent".to_string()),
            Op::Reindex => (0, "REINDEX".to_string()),
            _ => return None,
        })
    }
}

/// The schema objects the views database holds beside the table, as far as
/// the outcome of view DDL depends on them.
#[derive(Clone, Debug, Default)]
struct Objects {
    views: BTreeSet<Shape>,
    input_indexes: BTreeSet<Shape>,
    label_indexes: BTreeSet<Shape>,
}

impl Objects {
    fn name_taken(&self, name: &str) -> bool {
        self.views.iter().any(|shape| shape.names().contains(&name))
            || self.label_indexes.iter().any(|shape| shape.top() == name)
    }

    /// The statements of a view-DDL op, each with whether it must succeed, and
    /// the objects after it.
    fn plan(&self, op: &Op, keys: Keys) -> (Vec<(String, bool)>, Objects) {
        let mut next = self.clone();
        let all = |sqls: Vec<String>, ok: bool| sqls.into_iter().map(|s| (s, ok)).collect();
        let sqls = match *op {
            Op::CreateView(shape) => {
                let creates = shape.create(keys);
                let names = shape.names();
                match names.iter().position(|name| self.name_taken(name)) {
                    None => {
                        next.views.insert(shape);
                        all(creates, true)
                    }
                    // The views before the taken name are created; the op drops them again.
                    Some(taken) => creates[..taken]
                        .iter()
                        .map(|sql| (sql.clone(), true))
                        .chain([(creates[taken].clone(), false)])
                        .chain(
                            names[..taken]
                                .iter()
                                .rev()
                                .map(|name| (format!("DROP VIEW {name}"), true)),
                        )
                        .collect(),
                }
            }
            Op::DropView(shape) => {
                let ok = self.views.contains(&shape);
                if ok {
                    next.views.remove(&shape);
                    next.input_indexes.remove(&shape);
                }
                let mut sqls: Vec<(String, bool)> = all(shape.drop(), ok);
                sqls.truncate(if ok { sqls.len() } else { 1 });
                sqls
            }
            Op::CreateInputIndexes(shape, form) => {
                let ok = self.views.contains(&shape)
                    && !self.input_indexes.contains(&shape)
                    && !(form.refused_on_view() && shape.input().is_some());
                if ok {
                    next.input_indexes.insert(shape);
                }
                let mut sqls: Vec<(String, bool)> = all(shape.create_input_indexes(form), ok);
                sqls.truncate(if ok { sqls.len() } else { sqls.len().min(1) });
                sqls
            }
            Op::DropInputIndexes(shape) => {
                let ok = self.input_indexes.contains(&shape);
                if ok {
                    next.input_indexes.remove(&shape);
                }
                let mut sqls: Vec<(String, bool)> = all(shape.drop_input_indexes(), ok);
                sqls.truncate(if ok { sqls.len() } else { sqls.len().min(1) });
                sqls
            }
            Op::Refresh(shape) => {
                vec![(
                    format!("REFRESH MATERIALIZED VIEW {}", shape.top()),
                    self.views.contains(&shape),
                )]
            }
            Op::CreateLabelIndexNamedLike(shape) => {
                let ok = !self.name_taken(shape.top());
                if ok {
                    next.label_indexes.insert(shape);
                }
                vec![(format!("CREATE INDEX {} ON node(label)", shape.top()), ok)]
            }
            Op::DropLabelIndexNamedLike(shape) => {
                let ok = self.label_indexes.contains(&shape);
                if ok {
                    next.label_indexes.remove(&shape);
                }
                vec![(format!("DROP INDEX {}", shape.top()), ok)]
            }
            _ => unreachable!("not a view-DDL op: {op:?}"),
        };
        (sqls, next)
    }
}

#[derive(Clone, Debug)]
pub struct Scenario {
    keys: Keys,
    id: IdConstraint,
    parent_index: Option<IndexForm>,
    shapes: BTreeSet<Shape>,
    input_index: Option<IndexForm>,
    ops: Vec<Op>,
    /// After each statement only the top view of each shape is read, against
    /// its SELECT on the database without materialized views.
    top_only: bool,
}

struct Side {
    db: TempDatabase,
    conns: [Arc<Connection>; 2],
}

impl Side {
    fn new(keys: Keys, views: bool) -> Side {
        Side::of(open(keys, views))
    }

    fn of(db: TempDatabase) -> Side {
        let conns = [connect(&db), connect(&db)];
        for conn in &conns {
            conn.set_busy_timeout(Duration::ZERO);
        }
        Side { db, conns }
    }

    fn table(&self) -> Result<Vec<String>, String> {
        rows(
            &self.db,
            &self.conns[0],
            "SELECT id, parent, label FROM node",
        )
    }
}

/// Objects stacked at BEGIN and each SAVEPOINT, restored by a rollback.
#[derive(Default)]
struct Frames(Vec<(Txn, Objects)>);

impl Frames {
    fn apply(&mut self, txn: Txn, objects: &mut Objects) {
        match txn {
            Txn::Begin | Txn::Savepoint => self.0.push((txn, objects.clone())),
            Txn::Commit => self.0.clear(),
            Txn::Rollback => {
                if let Some((_, first)) = self.0.first() {
                    *objects = first.clone();
                }
                self.0.clear();
            }
            Txn::RollbackTo => {
                let at = self.0.iter().rposition(|(t, _)| *t == Txn::Savepoint);
                if let Some(at) = at {
                    *objects = self.0[at].1.clone();
                    self.0.truncate(at + 1);
                }
            }
            Txn::Release => {
                let at = self.0.iter().rposition(|(t, _)| *t == Txn::Savepoint);
                if let Some(at) = at {
                    self.0.truncate(at);
                }
            }
        }
    }
}

const INITIAL_ROWS: i64 = 6;

fn views_match_their_select(s: Scenario) -> Result<(), TestCaseError> {
    run_scenario(&s).map_err(TestCaseError::fail)
}

/// The statements that create the table `node` with its first rows, run on
/// both databases.
pub fn setup_sql(keys: Keys, id: IdConstraint, parent_index: Option<IndexForm>) -> Vec<String> {
    // An index on both sides gives REINDEX a write on both, whatever views exist.
    let mut setup = vec![
        "CREATE TABLE probe (x)".to_string(),
        "CREATE INDEX probe_x ON probe (x)".to_string(),
        keys.create_table(id),
    ];
    setup.extend(id.index().map(str::to_string));
    setup.extend(parent_index.map(|form| form.create("node", "parent")));
    for i in 0..INITIAL_ROWS {
        let parent = if i < 2 {
            "NULL".to_string()
        } else {
            keys.key((i - 2) / 2)
        };
        setup.push(format!(
            "INSERT INTO node VALUES ({}, {parent}, 'a')",
            keys.key(i)
        ));
    }
    setup
}

/// Each input view as a plain view, on the database without materialized
/// views: the top view's SELECT reads it there.
fn plain_input_views(twin: &Side, keys: Keys) -> Result<(), String> {
    for shape in Shape::ALL {
        if let Some(input) = shape.input() {
            let (_, select) = &shape.views(keys)[0];
            let sql = format!("CREATE VIEW {input} AS {select}");
            twin.conns[0]
                .execute(&sql)
                .map_err(|e| format!("`{sql}` without materialized views: {e}"))?;
        }
    }
    Ok(())
}

/// The top view of each shape holds the rows its SELECT returns on the
/// database without materialized views, where every input is a plain view.
fn check_tops(
    views: &Side,
    twin: &Side,
    shapes: &BTreeSet<Shape>,
    keys: Keys,
    at: &str,
) -> Result<(), String> {
    for shape in shapes {
        let (name, select) = shape.views(keys).pop().expect("a shape has a view");
        let held = rows(&views.db, &views.conns[0], &format!("SELECT * FROM {name}"))
            .map_err(|e| format!("{at}: reading view {name}: {e}"))?;
        let fresh = rows(&twin.db, &twin.conns[0], &select).map_err(|e| format!("{at}: {e}"))?;
        if held != fresh {
            return Err(format!(
                "{at}: view {name} differs from its SELECT without materialized views\nview:   {held:?}\nselect: {fresh:?}"
            ));
        }
    }
    Ok(())
}

fn run_scenario(s: &Scenario) -> Result<(), String> {
    let views = Side::new(s.keys, true);
    let twin = Side::new(s.keys, false);
    for sql in &setup_sql(s.keys, s.id, s.parent_index) {
        let accepted = twin.conns[0].execute(sql).is_ok();
        match views.conns[0].execute(sql) {
            Ok(()) if accepted => {}
            Err(_) if !accepted => {}
            Ok(()) => return Err(format!("setup `{sql}` succeeds with views only")),
            Err(e) => return Err(format!("setup `{sql}` fails with views only: {e}")),
        }
    }
    plain_input_views(&twin, s.keys)?;
    let mut objects = Objects::default();
    for &shape in &s.shapes {
        let mut ops = vec![Op::CreateView(shape)];
        if let Some(form) = s.input_index {
            ops.push(Op::CreateInputIndexes(shape, form));
        }
        for op in ops {
            if view_ddl(&views, &twin, &mut objects, &op, s.keys, false, "setup")? {
                return Ok(());
            }
        }
    }
    run_ops(&views, &twin, objects, s.keys, &s.ops, s.top_only)
}

/// Runs `ops` on both databases from the views in `objects`, then commits,
/// checks every view on both connections and the integrity, and drops them.
fn run_ops(
    views: &Side,
    twin: &Side,
    mut objects: Objects,
    keys: Keys,
    ops: &[Op],
    top_only: bool,
) -> Result<(), String> {
    let mut frames = Frames::default();
    check_views(
        &views.db,
        &views.conns[0],
        &objects.views,
        keys,
        "after setup",
    )?;
    for (step, op) in ops.iter().enumerate() {
        let at = format!("after step {step} {op:?}");
        if let Some((conn, sql)) = op.table_sql(keys) {
            let in_txn = !views.conns[conn].get_auto_commit();
            let expected = twin.conns[conn].execute(&sql);
            let got = views.conns[conn].execute(&sql);
            // Inside an open transaction, a failed row rolls back the earlier rows of
            // its statement only when the statement may abort (statement journal, as
            // in SQLite). View maintenance counts as such a source; a runtime type
            // error on a plain table does not, so that table keeps the earlier rows.
            if in_txn
                && matches!(op, Op::Insert { rows, .. } if rows.len() > 1)
                && expected.is_err()
                && got.is_err()
            {
                return Ok(());
            }
            match (&expected, &got) {
                (Ok(()), Err(e)) => {
                    return Err(format!("{at}: `{sql}` fails with views only: {e}"));
                }
                (Err(e), Ok(())) => {
                    return Err(format!(
                        "{at}: `{sql}` succeeds with views only; without: {e}"
                    ));
                }
                (_, Err(e @ turso_core::LimboError::Corrupt(_)))
                | (Err(e @ turso_core::LimboError::Corrupt(_)), _) => {
                    return Err(format!("{at}: `{sql}` reports corruption: {e}"));
                }
                _ => {}
            }
            if let (Op::Txn(txn, 0), Ok(())) = (op, &got) {
                frames.apply(*txn, &mut objects);
            }
            let (table, twin_table) = (views.table()?, twin.table()?);
            if table != twin_table {
                return Err(format!(
                    "{at}: node differs from the table without views\nviews:   {table:?}\nwithout: {twin_table:?}"
                ));
            }
        } else if view_ddl(
            views,
            twin,
            &mut objects,
            op,
            keys,
            !frames.0.is_empty(),
            &at,
        )? {
            return Ok(());
        }
        if top_only {
            check_tops(views, twin, &objects.views, keys, &at)?;
        } else {
            check_views(&views.db, &views.conns[0], &objects.views, keys, &at)?;
        }
    }
    for conn in 0..2 {
        if !views.conns[conn].get_auto_commit() {
            views.conns[conn]
                .execute("COMMIT")
                .map_err(|e| format!("final COMMIT on connection {conn}: {e}"))?;
        }
    }
    check_views(
        &views.db,
        &views.conns[1],
        &objects.views,
        keys,
        "on the second connection",
    )?;
    let check = limbo_exec_rows(&views.conns[0], "PRAGMA integrity_check");
    if check != vec![vec![rusqlite::types::Value::Text("ok".to_string())]] {
        return Err(format!("integrity_check: {check:?}"));
    }
    let drops = objects
        .views
        .iter()
        .flat_map(|shape| shape.drop())
        .collect();
    drop_and_check_nothing_left(views, drops, "at the end")
}

/// Runs `drops`; then no view, no index on a view and no view storage is left.
fn drop_and_check_nothing_left(side: &Side, drops: Vec<String>, at: &str) -> Result<(), String> {
    for sql in drops {
        side.conns[0]
            .execute(&sql)
            .map_err(|e| format!("{at}: `{sql}`: {e}"))?;
    }
    let views: Vec<&str> = Shape::ALL.iter().flat_map(|shape| shape.names()).collect();
    let left = objects_left(&side.conns[0], &views);
    if !left.is_empty() {
        return Err(format!("{at}: DROP VIEW left objects behind: {left:?}"));
    }
    Ok(())
}

/// Schema entries of the views `names`, their indexes and every view storage.
/// The storage of the fixture's view `dv` is no view of the shapes; it stays.
fn objects_left(conn: &Arc<Connection>, names: &[&str]) -> Vec<Vec<rusqlite::types::Value>> {
    let names: Vec<String> = names.iter().map(|name| format!("'{name}'")).collect();
    limbo_exec_rows(
        conn,
        &format!(
            "SELECT type, name FROM sqlite_schema WHERE (name LIKE '__turso_internal_dbsp%' \
             OR tbl_name LIKE '__turso_internal_dbsp%' OR tbl_name IN ({})) \
             AND name NOT LIKE '%\\_v2\\_dv%' ESCAPE '\\'",
            names.join(", ")
        ),
    )
}

/// Runs a view-DDL op on the views database. A view that joins on a
/// key of a refused type must fail loud at CREATE; that ends the scenario, `true`.
fn view_ddl(
    side: &Side,
    twin: &Side,
    objects: &mut Objects,
    op: &Op,
    keys: Keys,
    in_transaction: bool,
    at: &str,
) -> Result<bool, String> {
    let (statements, next) = objects.plan(op, keys);
    let mut created = Vec::new();
    for (sql, must_succeed) in statements {
        // A write fails while another connection holds the write lock or made
        // this transaction's snapshot stale; a write without views shows whether.
        if must_succeed {
            if let Err(stale) = twin.conns[0].execute("INSERT INTO probe VALUES (1)") {
                return match (side.conns[0].execute(&sql), created.is_empty()) {
                    (Err(_), true) => Ok(false),
                    (Err(_), false) => Err(format!(
                        "{at}: `{sql}` fails after views of the same statement group were created"
                    )),
                    (Ok(()), _) => Err(format!(
                        "{at}: `{sql}` succeeds; a write on the table without views fails: {stale}"
                    )),
                };
            }
        }
        let loud = must_succeed && must_fail_loud(keys, &sql);
        match (side.conns[0].execute(&sql), must_succeed) {
            (Ok(()), true) if loud => {
                return Err(format!(
                    "{at}: `{sql}` succeeds; a join key of a refused type must fail loud naming the type and the collation"
                ));
            }
            (Ok(()), true) => created.extend(created_view(&sql).map(str::to_string)),
            (Err(e), false) => {
                // A name held by an object of another kind is found after the
                // write lock is taken, and an open transaction keeps that lock.
                if in_transaction && e.to_string().starts_with("there is already a table named") {
                    twin.conns[0]
                        .execute("INSERT INTO probe VALUES (1)")
                        .map_err(|t| {
                            format!(
                                "{at}: `{sql}` takes the write lock with views only; a write without views fails: {t}"
                            )
                        })?;
                }
            }
            (Ok(()), false) => return Err(format!("{at}: `{sql}` succeeds; it must fail")),
            (Err(e), true) => {
                let e = e.to_string();
                if !(loud && names_type_and_collation(&e, &[keys.id, keys.parent])) {
                    return Err(format!("{at}: `{sql}` fails: {e}"));
                }
                let drops = created
                    .iter()
                    .rev()
                    .map(|name| format!("DROP VIEW {name}"))
                    .chain(objects.views.iter().flat_map(|shape| shape.drop()))
                    .collect();
                drop_and_check_nothing_left(
                    side,
                    drops,
                    &format!("{at}, after `{sql}` failed loud"),
                )?;
                return Ok(true);
            }
        }
    }
    *objects = next;
    Ok(false)
}

fn lit() -> impl Strategy<Value = Lit> {
    (
        0i64..8,
        prop_oneof![
            6 => Just(Form::Own),
            4 => prop::sample::select(Form::CONTRADICTING.to_vec()),
        ],
    )
        .prop_map(|(key, form)| Lit { key, form })
}

fn label() -> impl Strategy<Value = Label> {
    prop::sample::select(vec![Label::Kept, Label::Kept, Label::Filtered, Label::Null])
}

fn shape() -> impl Strategy<Value = Shape> {
    prop::sample::select(Shape::ALL.to_vec())
}

fn conn() -> impl Strategy<Value = usize> {
    prop_oneof![4 => Just(0usize), 1 => Just(1usize)]
}

fn op() -> impl Strategy<Value = Op> {
    op_on(conn)
}

fn op_on<C: Strategy<Value = usize> + 'static>(conn: fn() -> C) -> impl Strategy<Value = Op> {
    let txn = prop::sample::select(vec![
        Txn::Begin,
        Txn::Commit,
        Txn::Rollback,
        Txn::Savepoint,
        Txn::RollbackTo,
        Txn::Release,
    ]);
    prop_oneof![
        8 => (prop::collection::vec((lit(), lit(), label()), 1..=3), prop::bool::weighted(0.2), conn())
            .prop_map(|(rows, replace, conn)| Op::Insert { rows, replace, conn }),
        3 => (lit(), conn()).prop_map(|(id, conn)| Op::Delete { id, conn }),
        6 => (lit(), lit(), conn()).prop_map(|(id, parent, conn)| Op::Reparent { id, parent, conn }),
        2 => (lit(), lit(), conn()).prop_map(|(id, new, conn)| Op::Rekey { id, new, conn }),
        2 => (lit(), label(), conn()).prop_map(|(id, label, conn)| Op::Relabel { id, label, conn }),
        5 => (txn, conn()).prop_map(|(txn, conn)| Op::Txn(txn, conn)),
        1 => index_form().prop_map(Op::CreateParentIndex),
        1 => Just(Op::DropParentIndex),
        1 => Just(Op::Reindex),
        1 => shape().prop_map(Op::CreateView),
        1 => shape().prop_map(Op::DropView),
        1 => (shape(), index_form()).prop_map(|(shape, form)| Op::CreateInputIndexes(shape, form)),
        1 => shape().prop_map(Op::DropInputIndexes),
        1 => shape().prop_map(Op::Refresh),
        1 => shape().prop_map(Op::CreateLabelIndexNamedLike),
        1 => shape().prop_map(Op::DropLabelIndexNamedLike),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        keys(differential_depth()),
        prop::sample::select(IdConstraint::ALL.to_vec()),
        prop::option::weighted(0.7, index_form()),
        prop::sample::subsequence(Shape::ALL.to_vec(), 1..=Shape::ALL.len()),
        prop::option::weighted(0.5, index_form()),
        prop::collection::vec(op(), 1..20),
        prop::bool::weighted(0.3),
    )
        .prop_map(
            |(keys, id_constraint, parent_index, shapes, input_index, ops, top_only)| Scenario {
                keys,
                id: id_constraint,
                parent_index,
                shapes: shapes.into_iter().collect(),
                input_index,
                ops,
                top_only,
            },
        )
}

fn own(key: i64) -> Lit {
    Lit {
        key,
        form: Form::Own,
    }
}

fn fixed_scenarios() -> Vec<(&'static str, Scenario)> {
    let all: BTreeSet<Shape> = Shape::ALL.into_iter().collect();
    let text = Scenario {
        keys: Keys {
            depth: DIFFERENTIAL_DEPTH,
            ..Keys::same(Decl::Text)
        },
        id: IdConstraint::PrimaryKey,
        parent_index: Some(IndexForm::Plain),
        shapes: all.clone(),
        input_index: Some(IndexForm::Plain),
        ops: vec![
            Op::Insert {
                rows: vec![(own(6), own(1), Label::Kept)],
                replace: false,
                conn: 0,
            },
            Op::Reparent {
                id: own(2),
                parent: own(1),
                conn: 0,
            },
            Op::Txn(Txn::Begin, 0),
            Op::Delete {
                id: own(0),
                conn: 0,
            },
            Op::Txn(Txn::Rollback, 0),
            Op::DropView(Shape::MatviewOverMatview),
            Op::CreateView(Shape::MatviewOverMatview),
            Op::CreateInputIndexes(Shape::MatviewOverMatview, IndexForm::Plain),
            Op::Insert {
                rows: vec![(own(7), own(6), Label::Kept)],
                replace: false,
                conn: 0,
            },
        ],
        top_only: false,
    };
    let nocase_collision = Scenario {
        keys: Keys {
            depth: DIFFERENTIAL_DEPTH,
            ..Keys::same(Decl::TextNocase)
        },
        id: IdConstraint::Without,
        ops: vec![Op::Insert {
            rows: vec![(
                Lit {
                    key: 1,
                    form: Form::Lower,
                },
                own(0),
                Label::Kept,
            )],
            replace: false,
            conn: 0,
        }],
        ..text.clone()
    };
    let text_without_index = Scenario {
        parent_index: None,
        input_index: None,
        ..text.clone()
    };
    let decimal = Scenario {
        keys: Keys {
            depth: DIFFERENTIAL_DEPTH,
            ..Keys::same(Decl::Decimal)
        },
        ..text.clone()
    };
    let decimal_over_input = Scenario {
        keys: Keys {
            id: Decl::Integer,
            parent: Decl::Decimal,
            join: JoinCollate::Columns,
            depth: DIFFERENTIAL_DEPTH,
        },
        shapes: BTreeSet::from([Shape::MatviewOverMatview]),
        ..text.clone()
    };
    let fold = Scenario {
        keys: Keys {
            join: JoinCollate::Fold,
            depth: DIFFERENTIAL_DEPTH,
            ..Keys::same(Decl::Text)
        },
        ..nocase_collision.clone()
    };
    let partial_input = Scenario {
        input_index: Some(IndexForm::Partial),
        ..text.clone()
    };
    let real = Scenario {
        keys: Keys {
            depth: DIFFERENTIAL_DEPTH,
            ..Keys::same(Decl::Real)
        },
        ..text.clone()
    };
    vec![
        ("a TEXT key with an index on the join column", text.clone()),
        ("a TEXT key without any index", text_without_index),
        (
            "two ids that are equal under NOCASE both reach their children",
            nocase_collision,
        ),
        (
            "a numeric(10,2) join key fails loud at CREATE naming the type and the collation",
            decimal,
        ),
        (
            "a numeric(10,2) column that is no join key gets its view; the join over it fails loud",
            decimal_over_input,
        ),
        (
            "two ids that are equal under an application collation both reach their children",
            fold,
        ),
        ("a partial index on an input view", partial_input),
        ("a REAL key, an ordered type with a seek", real),
        (
            "an INTEGER[] join key fails loud at CREATE naming the type and the collation",
            Scenario {
                keys: Keys {
                    depth: DIFFERENTIAL_DEPTH,
                    ..Keys::same(Decl::IntArray)
                },
                ..text.clone()
            },
        ),
        (
            "only the top views are read inside a transaction",
            Scenario {
                ops: vec![
                    Op::Txn(Txn::Begin, 0),
                    Op::Insert {
                        rows: vec![(own(6), own(1), Label::Kept)],
                        replace: false,
                        conn: 0,
                    },
                    Op::Reparent {
                        id: own(2),
                        parent: own(1),
                        conn: 0,
                    },
                    Op::Delete {
                        id: own(0),
                        conn: 0,
                    },
                    Op::Txn(Txn::Commit, 0),
                ],
                top_only: true,
                ..text.clone()
            },
        ),
        (
            "a DDL that fails on a name of another kind keeps the write lock of its transaction",
            Scenario {
                keys: Keys {
                    depth: DIFFERENTIAL_DEPTH,
                    ..Keys::same(Decl::Integer)
                },
                parent_index: None,
                shapes: BTreeSet::from([Shape::MatviewOverMatview, Shape::PlainJoin]),
                input_index: None,
                ops: vec![
                    Op::Txn(Txn::Begin, 0),
                    Op::CreateLabelIndexNamedLike(Shape::MatviewOverMatview),
                    Op::Reparent {
                        id: own(0),
                        parent: own(0),
                        conn: 1,
                    },
                    Op::Txn(Txn::Rollback, 0),
                    Op::DropView(Shape::PlainJoin),
                    Op::CreateLabelIndexNamedLike(Shape::PlainJoin),
                    Op::Txn(Txn::Begin, 0),
                    Op::CreateView(Shape::PlainJoin),
                    Op::Reparent {
                        id: own(0),
                        parent: own(0),
                        conn: 1,
                    },
                    Op::Txn(Txn::Commit, 0),
                    Op::Reparent {
                        id: own(0),
                        parent: own(0),
                        conn: 1,
                    },
                ],
                ..text.clone()
            },
        ),
        (
            "outside a transaction such a DDL keeps no lock and leaves a savepoint of another connection fresh",
            Scenario {
                keys: Keys {
                    depth: 3,
                    ..Keys::same(Decl::Integer)
                },
                parent_index: None,
                shapes: BTreeSet::from([Shape::Recursive]),
                input_index: None,
                ops: vec![
                    Op::Txn(Txn::Savepoint, 1),
                    Op::CreateLabelIndexNamedLike(Shape::Recursive),
                    Op::Insert {
                        rows: vec![(
                            own(5),
                            Lit {
                                key: 7,
                                form: Form::Blob,
                            },
                            Label::Kept,
                        )],
                        replace: false,
                        conn: 1,
                    },
                    Op::Relabel {
                        id: Lit {
                            key: 2,
                            form: Form::TrailingSpace,
                        },
                        label: Label::Kept,
                        conn: 0,
                    },
                ],
                ..text.clone()
            },
        ),
        (
            "an index named like the top view lets the input view of the same group be created",
            Scenario {
                keys: Keys {
                    depth: 3,
                    ..Keys::same(Decl::Integer)
                },
                parent_index: None,
                shapes: BTreeSet::from([Shape::PlainJoin]),
                input_index: None,
                ops: vec![
                    Op::CreateLabelIndexNamedLike(Shape::RecursiveOverMatview),
                    Op::CreateView(Shape::RecursiveOverMatview),
                    Op::Txn(Txn::Begin, 0),
                    Op::CreateView(Shape::RecursiveOverMatview),
                    Op::Reparent {
                        id: own(0),
                        parent: own(0),
                        conn: 1,
                    },
                    Op::Txn(Txn::Commit, 0),
                    Op::DropLabelIndexNamedLike(Shape::RecursiveOverMatview),
                    Op::CreateView(Shape::RecursiveOverMatview),
                ],
                ..text.clone()
            },
        ),
    ]
}

#[test]
fn ivm_views_on_any_key_type_match_their_select_after_every_statement() {
    let fixed = fixed_scenarios()
        .into_iter()
        .map(|(name, scenario)| (format!("\"{name}\""), scenario))
        .collect();
    check_all(fixed, 256, scenario(), views_match_their_select);
}

// ---------------------------------------------------------------------------
// Every way a view compares a key: join, grouping and set operations
// ---------------------------------------------------------------------------

/// A view that compares key values other than by `JOIN ... ON a = b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyForm {
    OnIs,
    WhereEq,
    WhereIs,
    RecursiveWhere,
    GroupBy,
    Distinct,
    Union,
}

impl KeyForm {
    pub const ALL: [KeyForm; 7] = [
        KeyForm::OnIs,
        KeyForm::WhereEq,
        KeyForm::WhereIs,
        KeyForm::RecursiveWhere,
        KeyForm::GroupBy,
        KeyForm::Distinct,
        KeyForm::Union,
    ];

    fn select(self, keys: Keys) -> String {
        let pair = "SELECT a.label AS child, b.label AS up FROM node a";
        match self {
            KeyForm::OnIs => format!("{pair} JOIN node b ON a.parent IS b.id"),
            KeyForm::WhereEq => format!("{pair}, node b WHERE a.parent = b.id"),
            KeyForm::WhereIs => format!("{pair}, node b WHERE a.parent IS b.id"),
            KeyForm::RecursiveWhere => format!(
                "WITH RECURSIVE paths AS ( \
                 SELECT id, label, 0 AS depth FROM node WHERE parent IS NULL \
                 UNION ALL \
                 SELECT b.id, b.label, p.depth + 1 FROM node b, paths p \
                 WHERE b.parent = p.id AND p.depth < {}) \
                 SELECT label, depth FROM paths",
                keys.depth
            ),
            KeyForm::GroupBy => {
                "SELECT parent, count(*) AS n FROM node GROUP BY parent".to_string()
            }
            KeyForm::Distinct => "SELECT DISTINCT parent FROM node".to_string(),
            KeyForm::Union => "SELECT parent AS k FROM node UNION SELECT id FROM node".to_string(),
        }
    }

    /// Whether the view compares a key whose declared type is refused: then
    /// CREATE fails loud. A key that `input` computes has no declared type
    /// and compares by plain value.
    fn compares_refused(self, keys: Keys, input: Option<InputForm>) -> bool {
        let (id, parent) = input.map_or((true, true), InputForm::declares);
        let parent = parent && keys.parent.refused();
        match self {
            KeyForm::GroupBy | KeyForm::Distinct => parent,
            _ => parent || id && keys.id.refused(),
        }
    }
}

/// The materialized view `kin` that the view reads instead of `node`. No
/// scalar subquery column: the engine before the change refuses it at
/// CREATE too, so no stored view holds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputForm {
    Copy,
    /// Some rows keep their key values, others hold them as TEXT: the
    /// column mixes storage classes.
    Mixed,
    /// Two arms, the second casts the keys to TEXT.
    UnionAll,
    /// The block shape: a LEFT JOIN on a column that is no key.
    LeftJoin,
    /// Grouped on both keys, with an aggregate column.
    GroupBy,
    CastInteger,
    CastText,
    CastNumeric,
    CastReal,
    /// A NULL parent becomes TEXT: the column mixes storage classes.
    Coalesce,
    FromSubquery,
    With,
    /// Over two more materialized views that copy `node`.
    Chain,
    OverPlainView,
}

impl InputForm {
    pub const ALL: [InputForm; 14] = [
        InputForm::Copy,
        InputForm::Mixed,
        InputForm::UnionAll,
        InputForm::LeftJoin,
        InputForm::GroupBy,
        InputForm::CastInteger,
        InputForm::CastText,
        InputForm::CastNumeric,
        InputForm::CastReal,
        InputForm::Coalesce,
        InputForm::FromSubquery,
        InputForm::With,
        InputForm::Chain,
        InputForm::OverPlainView,
    ];

    /// Whether the columns `id` and `parent` of the view keep the declared
    /// type of the columns of `node`: a column reference keeps it, an
    /// expression has none, and a compound SELECT declares its first arm's.
    fn declares(self) -> (bool, bool) {
        match self {
            InputForm::Mixed
            | InputForm::CastInteger
            | InputForm::CastText
            | InputForm::CastNumeric
            | InputForm::CastReal => (false, false),
            InputForm::Coalesce => (true, false),
            InputForm::Copy
            | InputForm::UnionAll
            | InputForm::LeftJoin
            | InputForm::GroupBy
            | InputForm::FromSubquery
            | InputForm::With
            | InputForm::Chain
            | InputForm::OverPlainView => (true, true),
        }
    }

    /// Whether creating the view itself compares the keys: then a key of a
    /// refused type fails it loud.
    fn compares_refused(self, keys: Keys) -> bool {
        self == InputForm::GroupBy && keys.strict()
    }

    /// The views created before `kin`, as (name, statement); a plain view
    /// has no name here, it stores nothing to compare.
    fn sources(self) -> Vec<(Option<&'static str>, &'static str)> {
        match self {
            InputForm::Chain => vec![
                (
                    Some("kin0"),
                    "CREATE MATERIALIZED VIEW kin0 AS SELECT id, parent, label FROM node",
                ),
                (
                    Some("kin1"),
                    "CREATE MATERIALIZED VIEW kin1 AS SELECT id, parent, label FROM kin0",
                ),
            ],
            InputForm::OverPlainView => vec![(
                None,
                "CREATE VIEW kplain AS SELECT id, parent, label FROM node",
            )],
            _ => Vec::new(),
        }
    }

    fn select(self) -> &'static str {
        match self {
            InputForm::Copy => "SELECT id, parent, label FROM node",
            InputForm::LeftJoin => {
                "SELECT n.id AS id, n.parent AS parent, n.label AS label FROM node n \
                 LEFT JOIN node q ON q.label = n.label AND n.label < 'n3'"
            }
            InputForm::GroupBy => {
                "SELECT id, parent, max(label) AS label FROM node GROUP BY id, parent"
            }
            InputForm::CastInteger => {
                "SELECT CAST(id AS INTEGER) AS id, CAST(parent AS INTEGER) AS parent, label FROM node"
            }
            InputForm::CastText => {
                "SELECT CAST(id AS TEXT) AS id, CAST(parent AS TEXT) AS parent, label FROM node"
            }
            InputForm::CastNumeric => {
                "SELECT CAST(id AS NUMERIC) AS id, CAST(parent AS NUMERIC) AS parent, label FROM node"
            }
            InputForm::CastReal => {
                "SELECT CAST(id AS REAL) AS id, CAST(parent AS REAL) AS parent, label FROM node"
            }
            InputForm::Coalesce => "SELECT id, COALESCE(parent, 'x') AS parent, label FROM node",
            InputForm::FromSubquery => {
                "SELECT id, parent, label FROM (SELECT id, parent, label FROM node)"
            }
            InputForm::With => {
                "WITH t AS (SELECT id, parent, label FROM node) SELECT id, parent, label FROM t"
            }
            InputForm::Chain => "SELECT id, parent, label FROM kin1",
            InputForm::OverPlainView => "SELECT id, parent, label FROM kplain",
            InputForm::Mixed => {
                "SELECT CASE WHEN label < 'n3' THEN id ELSE CAST(id AS TEXT) END AS id, \
                 CASE WHEN label < 'n3' THEN CAST(parent AS TEXT) ELSE parent END AS parent, \
                 label FROM node"
            }
            InputForm::UnionAll => {
                "SELECT id, parent, label FROM node WHERE label < 'n3' UNION ALL \
                 SELECT CAST(id AS TEXT), CAST(parent AS TEXT), label FROM node WHERE label >= 'n3'"
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct KeyFormCase {
    pub keys: Keys,
    pub form: KeyForm,
    /// The view reads the materialized view `kin` instead of `node`.
    pub input: Option<InputForm>,
    /// The form of the indexes on `kin(id)` and `kin(parent)`.
    pub input_index: Option<IndexForm>,
    pub views_before_data: bool,
}

fn key_form_case_holds(case: KeyFormCase) -> Result<(), TestCaseError> {
    run_key_form_case(case).map_err(TestCaseError::fail)
}

fn run_key_form_case(case: KeyFormCase) -> Result<(), String> {
    let KeyFormCase {
        keys,
        form,
        input,
        input_index,
        views_before_data,
    } = case;
    let db = open(keys, true);
    let conn = db.connect_limbo();
    let mut views = Vec::new();
    let mut creates = Vec::new();
    let create_kin =
        input.map(|input| format!("CREATE MATERIALIZED VIEW kin AS {}", input.select()));
    if let Some(input) = input {
        for (name, sql) in input.sources() {
            if let Some(name) = name {
                let (_, select) = sql.split_once(" AS ").expect("a view statement");
                views.push((name, select.to_string()));
            }
            creates.push(sql.to_string());
        }
        views.push(("kin", input.select().to_string()));
        creates.push(create_kin.clone().expect("an input view"));
        if let Some(index) = input_index {
            creates.push(index.create("kin", "parent"));
            creates.push(index.create("kin", "id"));
        }
    }
    let select = form.select(keys);
    let select = if input.is_some() {
        select.replace("node", "kin")
    } else {
        select
    };
    let create_kv = format!("CREATE MATERIALIZED VIEW kv AS {select}");
    views.push(("kv", select));
    creates.push(create_kv.clone());
    let mut steps = vec![keys.create_table(IdConstraint::Without)];
    let inserts: Vec<String> = (0..INITIAL_ROWS)
        .map(|i| {
            let parent = if i < 2 {
                "NULL".to_string()
            } else {
                keys.parent.key((i - 2) / 2)
            };
            format!(
                "INSERT INTO node VALUES ({}, {parent}, 'n{i}')",
                keys.key(i)
            )
        })
        .collect();
    if !views_before_data {
        steps.extend(inserts.iter().cloned());
    }
    steps.extend(creates);
    if views_before_data {
        steps.extend(inserts);
    }
    steps.extend([
        format!(
            "INSERT INTO node VALUES ({}, {}, 'n{INITIAL_ROWS}')",
            keys.key(INITIAL_ROWS),
            keys.parent.key(1)
        ),
        format!(
            "UPDATE node SET parent = {} WHERE label = 'n5'",
            keys.parent.key(0)
        ),
        "DELETE FROM node WHERE label = 'n0'".to_string(),
    ]);
    let mut created = Vec::new();
    for sql in &steps {
        let result = conn.execute(sql);
        let loud = sql == &create_kv && form.compares_refused(keys, input)
            || Some(sql) == create_kin.as_ref() && input.is_some_and(|i| i.compares_refused(keys));
        if loud {
            return match result {
                Ok(()) => Err(format!(
                    "`{sql}` succeeds; a key of a refused type must fail loud naming the type and the collation"
                )),
                Err(e) if names_type_and_collation(&e.to_string(), &[keys.id, keys.parent]) => {
                    for name in created.iter().rev() {
                        conn.execute(format!("DROP VIEW {name}"))
                            .map_err(|e| format!("DROP VIEW {name}: {e}"))?;
                    }
                    let left = objects_left(&conn, &["kv", "kin", "kin0", "kin1"]);
                    if left.is_empty() {
                        Ok(())
                    } else {
                        Err(format!(
                            "`{sql}` failed loud but left objects behind: {left:?}"
                        ))
                    }
                }
                Err(e) => Err(format!(
                    "`{sql}` fails, but not naming the refused type and the collation: {e}"
                )),
            };
        }
        result.map_err(|e| format!("`{sql}`: {e}"))?;
        created.extend(created_view(sql));
        for (name, select) in &views {
            // A view that stores values of a refused type holds their encoded form, not
            // the decoded text its SELECT returns: decoding is a separate change.
            // A UNION ALL view stores the second arm's values with the first
            // arm's column affinity, which its SELECT does not apply.
            let skipped = keys.strict() || input == Some(InputForm::UnionAll);
            // A recursive view over such a view with an index on its keys differs
            // from its SELECT at CREATE already before this change; a separate change fixes it.
            let mixed_recursion = form == KeyForm::RecursiveWhere
                && input == Some(InputForm::UnionAll)
                && input_index.is_some()
                && !(keys.id.stores_text() && keys.parent.stores_text());
            if !created.contains(name)
                || skipped && name.starts_with("kin")
                || mixed_recursion && *name == "kv"
            {
                continue;
            }
            let held = rows(&db, &conn, &format!("SELECT * FROM {name}"))?;
            let fresh = rows(&db, &conn, select)?;
            if held != fresh {
                return Err(format!(
                    "after `{sql}`: view {name} differs from its SELECT\nview:   {held:?}\nselect: {fresh:?}"
                ));
            }
        }
    }
    Ok(())
}

/// The index forms a materialized view accepts with every key type.
fn view_index_form() -> impl Strategy<Value = IndexForm> {
    prop::sample::select(vec![
        IndexForm::Plain,
        IndexForm::Descending,
        IndexForm::TwoColumn,
    ])
}

fn key_form_case() -> impl Strategy<Value = KeyFormCase> {
    (
        decl(),
        decl(),
        prop::sample::select(KeyForm::ALL.to_vec()),
        prop::option::weighted(0.6, prop::sample::select(InputForm::ALL.to_vec())),
        prop::option::weighted(0.5, view_index_form()),
        any::<bool>(),
        differential_depth(),
    )
        .prop_map(
            |(id, parent, form, input, input_index, views_before_data, depth)| KeyFormCase {
                input,
                keys: Keys {
                    id,
                    // A UNION view declares its column with the first arm's type, and the
                    // second arm's values take that affinity; only a refused CREATE mixes them.
                    parent: if form == KeyForm::Union && !id.refused() && !parent.refused() {
                        id
                    } else {
                        parent
                    },
                    join: JoinCollate::Columns,
                    depth,
                },
                form,
                input_index,
                views_before_data,
            },
        )
}

#[test]
fn ivm_views_comparing_a_numeric_key_fail_loud_and_others_match_their_select() {
    let mut fixed = Vec::new();
    for form in KeyForm::ALL {
        for decl in [Decl::Decimal, Decl::Text, Decl::Integer, Decl::IntArray] {
            for (input, input_index, views_before_data) in [
                (None, None, false),
                (None, None, true),
                (Some(InputForm::Copy), None, false),
                (Some(InputForm::Copy), Some(IndexForm::Plain), true),
            ] {
                let case = KeyFormCase {
                    keys: Keys {
                        depth: DIFFERENTIAL_DEPTH,
                        ..Keys::same(decl)
                    },
                    form,
                    input,
                    input_index,
                    views_before_data,
                };
                fixed.push((format!("{case:?}"), case));
            }
        }
        for input in [InputForm::Mixed, InputForm::UnionAll] {
            for decl in [
                Decl::Integer,
                Decl::Real,
                Decl::Untyped,
                Decl::Decimal,
                Decl::IntArray,
            ] {
                let case = KeyFormCase {
                    keys: Keys {
                        depth: DIFFERENTIAL_DEPTH,
                        ..Keys::same(decl)
                    },
                    form,
                    input: Some(input),
                    input_index: Some(IndexForm::Plain),
                    views_before_data: false,
                };
                fixed.push((format!("{case:?}"), case));
            }
        }
        for input in &InputForm::ALL[3..] {
            for (id, parent) in [
                (Decl::Integer, Decl::Integer),
                (Decl::Text, Decl::Text),
                (Decl::Decimal, Decl::Decimal),
                (Decl::Decimal, Decl::Integer),
                (Decl::Integer, Decl::Decimal),
            ] {
                let case = KeyFormCase {
                    keys: Keys {
                        id,
                        parent,
                        join: JoinCollate::Columns,
                        depth: DIFFERENTIAL_DEPTH,
                    },
                    form,
                    input: Some(*input),
                    input_index: Some(IndexForm::Plain),
                    views_before_data: false,
                };
                fixed.push((format!("{case:?}"), case));
            }
        }
        for (id, parent) in [
            (Decl::Decimal, Decl::IntArray),
            (Decl::IntArray, Decl::TextArray),
            (Decl::Decimal, Decl::Text),
        ] {
            let case = KeyFormCase {
                keys: Keys {
                    id,
                    parent,
                    join: JoinCollate::Columns,
                    depth: DIFFERENTIAL_DEPTH,
                },
                form,
                input: None,
                input_index: None,
                views_before_data: false,
            };
            fixed.push((format!("{case:?}"), case));
        }
    }
    check_all(fixed, 128, key_form_case(), key_form_case_holds);
}

// ---------------------------------------------------------------------------
// A database file whose views an older engine created
// ---------------------------------------------------------------------------

/// The key types of the fixture files: Holon's TEXT ids, and an INTEGER key
/// that the older engine stored in a TEXT probe column.
pub const FIXTURE_DECLS: [Decl; 2] = [Decl::Text, Decl::Integer];

pub fn fixture_keys(decl: Decl) -> Keys {
    Keys {
        depth: DIFFERENTIAL_DEPTH,
        ..Keys::same(decl)
    }
}

/// The fixture file of `decl`, next to this file.
fn fixture_name(decl: Decl) -> String {
    format!(
        "test_ivm_key_seek_fixture_{}",
        format!("{decl:?}").to_lowercase()
    )
}

/// The statements that wrote the fixture file of `decl`; its `.sql` sibling
/// lists them with the engine that ran them.
pub fn fixture_statements(decl: Decl) -> Vec<String> {
    let keys = fixture_keys(decl);
    let mut sql = setup_sql(keys, IdConstraint::PrimaryKey, Some(IndexForm::Plain));
    for shape in Shape::ALL {
        sql.extend(shape.create(keys));
        sql.extend(shape.create_input_indexes(IndexForm::Plain));
    }
    sql.push(
        "CREATE TABLE dnode(id numeric(10,2), parent numeric(10,2), label TEXT) STRICT".to_string(),
    );
    for i in 0..INITIAL_ROWS {
        let parent = if i < 2 {
            "NULL".to_string()
        } else {
            Decl::Decimal.key((i - 2) / 2)
        };
        sql.push(format!(
            "INSERT INTO dnode VALUES ({}, {parent}, 'n{i}')",
            Decl::Decimal.key(i)
        ));
    }
    sql.push(
        "CREATE MATERIALIZED VIEW dv AS \
         SELECT a.label AS child, b.label AS up FROM dnode a JOIN dnode b ON a.parent = b.id"
            .to_string(),
    );
    sql
}

/// Reading the view `dv`, which joins on a numeric(10,2) key, fails loud.
fn reading_dv_fails_loud(side: &Side, at: &str) -> Result<(), String> {
    match try_limbo_exec_rows(&side.db, &side.conns[0], "SELECT * FROM dv") {
        Ok(rows) => Err(format!(
            "{at}: reading dv returns {rows:?}; a numeric(10,2) join key must fail loud"
        )),
        Err(e) if names_type_and_collation(&e.to_string(), &[Decl::Decimal]) => Ok(()),
        Err(e) => Err(format!(
            "{at}: reading dv fails, but not naming numeric and the collation: {e}"
        )),
    }
}

/// Opens a copy of the fixture file of `decl`.
fn open_fixture(decl: Decl) -> Result<(tempfile::TempDir, Side), String> {
    let fixture = format!(
        "{}/integration/query_processing/{}.db",
        env!("CARGO_MANIFEST_DIR"),
        fixture_name(decl)
    );
    let dir = tempfile::TempDir::new().map_err(|e| format!("temp dir: {e}"))?;
    let path = dir.path().join("views-of-an-older-engine.db");
    std::fs::copy(&fixture, &path).map_err(|e| format!("copying {fixture}: {e}"))?;
    let views = Side::of(
        TempDatabase::builder()
            .with_db_path(&path)
            .with_views(true)
            .with_opts(turso_core::DatabaseOpts::new().with_custom_types(true))
            .build(),
    );
    Ok((dir, views))
}

/// The views on `node` in the fixture file of `decl` stay equal to their
/// SELECT through `ops`.
fn run_fixture_case(decl: Decl, ops: &[Op], top_only: bool) -> Result<(), String> {
    let keys = fixture_keys(decl);
    let (_dir, views) = open_fixture(decl)?;
    let twin = Side::new(keys, false);
    for sql in setup_sql(keys, IdConstraint::PrimaryKey, Some(IndexForm::Plain)) {
        twin.conns[0]
            .execute(&sql)
            .map_err(|e| format!("`{sql}` without views: {e}"))?;
    }
    plain_input_views(&twin, keys)?;
    let objects = Objects {
        views: Shape::ALL.into_iter().collect(),
        input_indexes: Shape::ALL
            .into_iter()
            .filter(|shape| shape.input().is_some())
            .collect(),
        label_indexes: BTreeSet::new(),
    };
    run_ops(&views, &twin, objects, keys, ops, top_only)
}

/// A write to `dnode`: kind 0 inserts row `a` under `b`, kind 1 moves the
/// row labelled `a` under `b`, kind 2 deletes the row labelled `a`.
fn dnode_write((kind, a, b): (u8, i64, i64)) -> String {
    let (key_a, key_b) = (Decl::Decimal.key(a), Decl::Decimal.key(b));
    match kind {
        0 => format!("INSERT INTO dnode VALUES ({key_a}, {key_b}, 'w{a}')"),
        1 => format!("UPDATE dnode SET parent = {key_b} WHERE label = 'n{a}'"),
        _ => format!("DELETE FROM dnode WHERE label = 'n{a}'"),
    }
}

/// What follows the writes to `dnode` in a refused-view fixture case.
#[derive(Clone, Copy, Debug)]
enum RefusedEnd {
    Nothing,
    /// A materialized view over `dv` is refused loud and leaves nothing.
    CreateOver,
    /// `DROP VIEW dv` succeeds and leaves nothing of `dv`.
    Drop,
}

/// In the fixture file of `decl`, reading `dv` fails loud after open and
/// after each of `writes` to its table, and each write succeeds; then `end`.
fn run_refused_fixture_case(
    decl: Decl,
    writes: &[(u8, i64, i64)],
    end: RefusedEnd,
) -> Result<(), String> {
    let (_dir, views) = open_fixture(decl)?;
    reading_dv_fails_loud(&views, "after open")?;
    for &write in writes {
        let sql = dnode_write(write);
        views.conns[0]
            .execute(&sql)
            .map_err(|e| format!("`{sql}` on the table of a refused view: {e}"))?;
        reading_dv_fails_loud(&views, &format!("after `{sql}`"))?;
    }
    let left_of = |name: &str| {
        limbo_exec_rows(
            &views.conns[0],
            &format!(
                "SELECT type, name FROM sqlite_schema WHERE name = '{name}' \
                 OR name LIKE '%\\_v2\\_{name}%' ESCAPE '\\'"
            ),
        )
    };
    match end {
        RefusedEnd::Nothing => Ok(()),
        RefusedEnd::CreateOver => {
            let sql = "CREATE MATERIALIZED VIEW dvv AS SELECT child FROM dv";
            match views.conns[0].execute(sql) {
                Ok(()) => Err(format!(
                    "`{sql}` succeeds; a view over a refused view must fail loud"
                )),
                Err(e) if !names_type_and_collation(&e.to_string(), &[Decl::Decimal]) => Err(
                    format!("`{sql}` fails, but not naming numeric and the collation: {e}"),
                ),
                Err(_) => match left_of("dvv") {
                    left if left.is_empty() => {
                        reading_dv_fails_loud(&views, &format!("after `{sql}`"))
                    }
                    left => Err(format!(
                        "`{sql}` failed loud but left objects behind: {left:?}"
                    )),
                },
            }
        }
        RefusedEnd::Drop => {
            views.conns[0]
                .execute("DROP VIEW dv")
                .map_err(|e| format!("DROP VIEW of a refused view: {e}"))?;
            match left_of("dv") {
                left if left.is_empty() => Ok(()),
                left => Err(format!("DROP VIEW dv left objects behind: {left:?}")),
            }
        }
    }
}

/// Writes the fixture files and their `.sql` siblings into the directory
/// `FIXTURE_OUT`; run it with the engine whose files the fixtures stand for.
#[test]
#[ignore = "writes the fixture files"]
fn write_the_fixture_files() {
    let out = std::path::PathBuf::from(
        std::env::var("FIXTURE_OUT").expect("FIXTURE_OUT names the directory to write into"),
    );
    for decl in FIXTURE_DECLS {
        let name = fixture_name(decl);
        let db_path = out.join(format!("{name}.db"));
        assert!(!db_path.exists(), "{db_path:?} exists");
        let db = TempDatabase::builder()
            .with_db_path(&db_path)
            .with_views(true)
            .with_opts(turso_core::DatabaseOpts::new().with_custom_types(true))
            .build();
        let conn = db.connect_limbo();
        conn.execute("PRAGMA page_size = 512").unwrap();
        let statements = fixture_statements(decl);
        for sql in &statements {
            conn.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
        }
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        let mut script = format!(
            "-- The statements that wrote {name}.db, run by the engine at commit\n\
             -- ac81f0251738 (nightscape/turso) through TempDatabase with views and custom\n\
             -- types enabled, after PRAGMA page_size = 512 (keeps the file small), then\n\
             -- PRAGMA wal_checkpoint(TRUNCATE).\n"
        );
        for sql in &statements {
            script.push_str(sql);
            script.push_str(";\n");
        }
        std::fs::write(out.join(format!("{name}.sql")), script).unwrap();
    }
}

#[test]
fn ivm_views_an_older_engine_wrote_match_their_select() {
    let mut fixed = Vec::new();
    for decl in FIXTURE_DECLS {
        fixed.push((format!("{decl:?}, no statement"), (decl, Vec::new(), false)));
        fixed.push((
            format!("{decl:?}, the statements of the TEXT scenario, top views only"),
            (decl, fixed_scenarios()[0].1.ops.clone(), true),
        ));
    }
    fixed.push((
        "Text file, an open transaction on one connection, an update matching no row on another, then a drop of view indexes".to_string(),
        (
            Decl::Text,
            vec![
                Op::Txn(Txn::Begin, 0),
                Op::Reparent {
                    id: Lit {
                        key: 0,
                        form: Form::Upper,
                    },
                    parent: own(0),
                    conn: 1,
                },
                Op::DropInputIndexes(Shape::MatviewOverMatview),
            ],
            false,
        ),
    ));
    check_all(
        fixed,
        32,
        (
            prop::sample::select(FIXTURE_DECLS.to_vec()),
            prop::collection::vec(op(), 0..20),
            any::<bool>(),
        ),
        |(decl, ops, top_only)| run_fixture_case(decl, &ops, top_only).map_err(TestCaseError::fail),
    );
}

#[test]
fn a_numeric_key_view_an_older_engine_wrote_fails_loud_on_every_read_and_its_table_takes_writes() {
    let fixed = FIXTURE_DECLS
        .into_iter()
        .flat_map(|decl| {
            [
                RefusedEnd::Nothing,
                RefusedEnd::CreateOver,
                RefusedEnd::Drop,
            ]
            .map(|end| {
                (
                    format!("{decl:?} file, insert, move, delete, then {end:?}"),
                    (decl, vec![(0, 6, 1), (1, 5, 0), (2, 0, 0)], end),
                )
            })
        })
        .collect();
    check_all(
        fixed,
        16,
        (
            prop::sample::select(FIXTURE_DECLS.to_vec()),
            prop::collection::vec((0u8..3, 0..INITIAL_ROWS + 2, 0..INITIAL_ROWS + 2), 0..6),
            prop::sample::select(vec![
                RefusedEnd::Nothing,
                RefusedEnd::CreateOver,
                RefusedEnd::Drop,
            ]),
        ),
        |(decl, writes, end)| {
            run_refused_fixture_case(decl, &writes, end).map_err(TestCaseError::fail)
        },
    );
}
