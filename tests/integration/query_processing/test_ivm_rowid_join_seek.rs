//! Materialized views whose join compares a key with the rowid of a table:
//! `ON key = t.id` where `id` is declared INTEGER PRIMARY KEY (in each spelling
//! a caller can write), or the term names the rowid as `rowid`, `_rowid_` or
//! `oid`, also where a column of the table takes such a name.
//!
//! Three oracles. Cost: the VM steps of one write, maintenance included, do not
//! grow with the table when every join phase the write runs can seek the rowid
//! or a fitting index. Correctness: after every statement the view holds the
//! rows of a fresh run of its SELECT on the same data without materialized
//! views, and a statement succeeds exactly when it succeeds there. SQL: at the
//! end, the view holds the rows SQLite returns for its SELECT over the same
//! table contents, so a key that converts to an integer matches its row and
//! any other key matches none.

use super::test_ivm_key_seek::{sorted, steps_of};
use crate::common::{limbo_exec_rows, try_limbo_exec_rows, TempDatabase};
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};
use rusqlite::types::Value;
use std::sync::Arc;
use std::time::Duration;
use turso_core::Connection;

/// How the join term names the rowid of the inner table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowidName {
    /// The column `id`.
    Alias,
    Rowid,
    UnderRowid,
    Oid,
}

impl RowidName {
    pub const ALL: [RowidName; 4] = [
        RowidName::Alias,
        RowidName::Rowid,
        RowidName::UnderRowid,
        RowidName::Oid,
    ];

    fn sql(self) -> &'static str {
        match self {
            RowidName::Alias => "id",
            RowidName::Rowid => "rowid",
            RowidName::UnderRowid => "_rowid_",
            RowidName::Oid => "oid",
        }
    }
}

/// How the inner table `t` declares `id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inner {
    IntegerPrimaryKey,
    Autoincrement,
    Ascending,
    /// INTEGER PRIMARY KEY DESC: SQLite makes it no rowid alias.
    Descending,
    /// `PRIMARY KEY(id)` as a table constraint.
    TableConstraint,
    LowerCase,
    /// INT PRIMARY KEY: no rowid alias; an automatic index on `id`.
    IntPrimaryKey,
    /// `id INTEGER` without a key: only `rowid`, `_rowid_` and `oid` read the rowid.
    NoKey,
    /// A column of `t` is named like this rowid name and holds other values.
    Shadowing(RowidName),
    WithoutRowid,
}

impl Inner {
    pub const ALL: [Inner; 12] = [
        Inner::IntegerPrimaryKey,
        Inner::Autoincrement,
        Inner::Ascending,
        Inner::Descending,
        Inner::TableConstraint,
        Inner::LowerCase,
        Inner::IntPrimaryKey,
        Inner::NoKey,
        Inner::Shadowing(RowidName::Rowid),
        Inner::Shadowing(RowidName::UnderRowid),
        Inner::Shadowing(RowidName::Oid),
        Inner::WithoutRowid,
    ];

    fn create(self, ref_decl: RefDecl) -> String {
        let r = ref_decl.sql();
        match self {
            Inner::IntegerPrimaryKey => {
                format!("CREATE TABLE t(id INTEGER PRIMARY KEY, ref {r}, label TEXT)")
            }
            Inner::Autoincrement => {
                format!("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, ref {r}, label TEXT)")
            }
            Inner::Ascending => {
                format!("CREATE TABLE t(id INTEGER PRIMARY KEY ASC, ref {r}, label TEXT)")
            }
            Inner::Descending => {
                format!("CREATE TABLE t(id INTEGER PRIMARY KEY DESC, ref {r}, label TEXT)")
            }
            Inner::TableConstraint => {
                format!("CREATE TABLE t(id INTEGER, ref {r}, label TEXT, PRIMARY KEY(id))")
            }
            Inner::LowerCase => {
                format!("create table t(id integer primary key, ref {r}, label text)")
            }
            Inner::IntPrimaryKey => {
                format!("CREATE TABLE t(id INT PRIMARY KEY, ref {r}, label TEXT)")
            }
            Inner::NoKey => format!("CREATE TABLE t(id INTEGER, ref {r}, label TEXT)"),
            Inner::Shadowing(name) => format!(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, ref {r}, label TEXT, {} INTEGER)",
                name.sql()
            ),
            Inner::WithoutRowid => {
                format!("CREATE TABLE t(id INTEGER PRIMARY KEY, ref {r}, label TEXT) WITHOUT ROWID")
            }
        }
    }

    fn shadow(self) -> Option<RowidName> {
        match self {
            Inner::Shadowing(name) => Some(name),
            _ => None,
        }
    }

    /// A name that reads the rowid itself, or the key of a WITHOUT ROWID row.
    fn real_rowid(self) -> &'static str {
        match self {
            Inner::Shadowing(RowidName::Rowid) => "_rowid_",
            Inner::WithoutRowid => "id",
            _ => "rowid",
        }
    }

    /// The columns that store one row, the rowid included.
    fn stored(self) -> String {
        match self {
            Inner::WithoutRowid => self.columns(),
            _ => format!("{}, {}", self.real_rowid(), self.columns()),
        }
    }

    /// The columns an INSERT into `t` names.
    fn columns(self) -> String {
        match self.shadow() {
            Some(name) => format!("id, ref, label, {}", name.sql()),
            None => "id, ref, label".to_string(),
        }
    }

    /// Whether `name` reads the rowid of `t`.
    fn names_rowid(self, name: RowidName) -> bool {
        match name {
            RowidName::Alias => matches!(
                self,
                Inner::IntegerPrimaryKey
                    | Inner::Autoincrement
                    | Inner::Ascending
                    | Inner::TableConstraint
                    | Inner::LowerCase
                    | Inner::Shadowing(_)
            ),
            other => self != Inner::WithoutRowid && self.shadow() != Some(other),
        }
    }

    /// The term on `name` can seek: the rowid, or the automatic index of a
    /// primary key that is no rowid alias.
    fn seekable(self, name: RowidName) -> bool {
        self.names_rowid(name)
            || (name == RowidName::Alias
                && matches!(self, Inner::Descending | Inner::IntPrimaryKey))
    }
}

/// The declared type of `t.ref` and `cur.id`, the columns that hold a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefDecl {
    Integer,
    Text,
    Real,
    Blob,
    Untyped,
    Numeric,
    TextNocase,
}

impl RefDecl {
    pub const ALL: [RefDecl; 7] = [
        RefDecl::Integer,
        RefDecl::Text,
        RefDecl::Real,
        RefDecl::Blob,
        RefDecl::Untyped,
        RefDecl::Numeric,
        RefDecl::TextNocase,
    ];

    fn sql(self) -> &'static str {
        match self {
            RefDecl::Integer => "INTEGER",
            RefDecl::Text => "TEXT",
            RefDecl::Real => "REAL",
            RefDecl::Blob => "BLOB",
            RefDecl::Untyped => "",
            RefDecl::Numeric => "NUMERIC",
            RefDecl::TextNocase => "TEXT COLLATE NOCASE",
        }
    }

    /// The literal of id `k` as a caller of a column of this type writes it.
    fn own(self, k: i64) -> Key {
        match self {
            RefDecl::Integer | RefDecl::Blob | RefDecl::Untyped => Key::Int(k),
            RefDecl::Text | RefDecl::TextNocase | RefDecl::Numeric => Key::Text(k),
            RefDecl::Real => Key::Real(k),
        }
    }
}

/// A key literal. Integer-valued forms that SQL converts to the integer `k`
/// match the row with rowid `k`; the other forms match no row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Int(i64),
    /// `k.0`
    Real(i64),
    /// `k.5`
    Frac(i64),
    /// `'k'`
    Text(i64),
    /// `' k '`
    Spaced(i64),
    /// `'k.0'`
    TextReal(i64),
    /// `'+k'`
    Plus(i64),
    /// `'0xk'`
    Hex(i64),
    /// `'bk'`
    Word(i64),
    /// `'Bk'`
    Upper(i64),
    /// The one byte `k`.
    Blob(i64),
    /// `CAST('k' AS BLOB)`
    TextBlob(i64),
    Null,
    Max,
    Min,
    /// `1e19`: beyond every integer.
    Huge,
    NegZero,
}

impl Key {
    fn sql(self) -> String {
        match self {
            Key::Int(k) => k.to_string(),
            Key::Real(k) => format!("{k}.0"),
            Key::Frac(k) => format!("{k}.5"),
            Key::Text(k) => format!("'{k}'"),
            Key::Spaced(k) => format!("' {k} '"),
            Key::TextReal(k) => format!("'{k}.0'"),
            Key::Plus(k) => format!("'+{k}'"),
            Key::Hex(k) => format!("'0x{k}'"),
            Key::Word(k) => format!("'b{k}'"),
            Key::Upper(k) => format!("'B{k}'"),
            Key::Blob(k) => format!("X'{:02X}'", k.rem_euclid(256)),
            Key::TextBlob(k) => format!("CAST('{k}' AS BLOB)"),
            Key::Null => "NULL".to_string(),
            Key::Max => i64::MAX.to_string(),
            Key::Min => i64::MIN.to_string(),
            Key::Huge => "1e19".to_string(),
            Key::NegZero => "-0.0".to_string(),
        }
    }

    /// The same form for id `k`.
    fn at(self, k: i64) -> Key {
        match self {
            Key::Int(_) => Key::Int(k),
            Key::Real(_) => Key::Real(k),
            Key::Frac(_) => Key::Frac(k),
            Key::Text(_) => Key::Text(k),
            Key::Spaced(_) => Key::Spaced(k),
            Key::TextReal(_) => Key::TextReal(k),
            Key::Plus(_) => Key::Plus(k),
            Key::Hex(_) => Key::Hex(k),
            Key::Word(_) => Key::Word(k),
            Key::Upper(_) => Key::Upper(k),
            Key::Blob(_) => Key::Blob(k),
            Key::TextBlob(_) => Key::TextBlob(k),
            other => other,
        }
    }
}

/// The form of an index on `t.ref` or `cur.id`.
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

    fn create(self, table: &str, column: &str, other: &str) -> String {
        let on = match self {
            IndexForm::Plain => format!("({column})"),
            IndexForm::Descending => format!("({column} DESC)"),
            IndexForm::TwoColumn => format!("({column}, {other})"),
            IndexForm::Partial => format!("({column}) WHERE {other} IS NOT NULL"),
            IndexForm::OtherCollation => format!("({column} COLLATE NOCASE)"),
            IndexForm::Expression => format!("(lower({column}))"),
        };
        format!("CREATE INDEX {table}_{column} ON {table}{on}")
    }

    /// An equality of `ref` with an integer rowid can seek this index for every
    /// row: the index holds the values the comparison compares, under BINARY.
    fn fits_rowid_key(self, decl: RefDecl) -> bool {
        matches!(
            self,
            IndexForm::Plain | IndexForm::Descending | IndexForm::TwoColumn
        ) && matches!(decl, RefDecl::Integer | RefDecl::Real | RefDecl::Numeric)
    }
}

fn ref_index(form: IndexForm) -> String {
    form.create("t", "ref", "label")
}

fn hid_index(form: IndexForm) -> String {
    form.create("cur", "id", "region")
}

/// The SELECT of the view `v`. `cur(region, id)` holds keys into `t`, as a
/// cursor table holds ids of a history table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `t` joins itself on `ref = rowid`.
    SelfJoin,
    TwoTable,
    /// The term written `rowid = key`.
    Reversed,
    /// `FROM cur c, t h WHERE ...`
    CommaWhere,
    /// A second ON term on the inner table alone.
    ExtraTerm,
    /// The key is `+c.id`, which has no affinity.
    KeyExpression,
    /// The term carries `COLLATE NOCASE`.
    Collated,
    /// The key side carries `COLLATE NOCASE`.
    KeyCollated,
    /// The rowid side is in parentheses.
    Parenthesized,
    /// `(rowid) = (key)`
    ParenthesizedReversed,
    LeftJoin,
    /// `cur` joins `t` on the rowid, and `t` joins `t` on `ref = rowid`.
    ThreeWay,
    /// GROUP BY over the join.
    Aggregate,
    /// A semi join: `WHERE EXISTS (... WHERE rowid = key)`.
    Exists,
    /// A semi join: `WHERE key IN (SELECT rowid ...)`, correlated by a term
    /// that holds for every row.
    InSubquery,
    /// `WHERE key IN (SELECT rowid FROM t)`: no correlation.
    Uncorrelated,
    /// An anti join: `WHERE NOT EXISTS (... WHERE rowid = key)`.
    NotExists,
    /// An anti join: `WHERE key NOT IN (SELECT rowid ...)`.
    NotIn,
    /// [`Shape::SelfJoin`] with the term under `COLLATE NOCASE`.
    SelfCollated,
    /// The inner input is the view `m` over `t`; the term names `id` or the
    /// rowid of `m`.
    OverView,
    /// A recursive CTE whose step joins `t` on its rowid.
    Recursive,
    /// `cur c JOIN t h USING (<name>)`: `cur.id` is named like the rowid alias.
    Using,
    /// `cur c NATURAL JOIN t h`: `id` is the one column both tables have.
    Natural,
    LeftUsing,
}

impl Shape {
    pub const ALL: [Shape; 24] = [
        Shape::SelfJoin,
        Shape::TwoTable,
        Shape::Reversed,
        Shape::CommaWhere,
        Shape::ExtraTerm,
        Shape::KeyExpression,
        Shape::Collated,
        Shape::KeyCollated,
        Shape::Parenthesized,
        Shape::ParenthesizedReversed,
        Shape::LeftJoin,
        Shape::ThreeWay,
        Shape::Aggregate,
        Shape::Exists,
        Shape::InSubquery,
        Shape::Uncorrelated,
        Shape::NotExists,
        Shape::NotIn,
        Shape::SelfCollated,
        Shape::OverView,
        Shape::Recursive,
        Shape::Using,
        Shape::Natural,
        Shape::LeftUsing,
    ];

    /// The SELECT of the view `m` that [`Shape::OverView`] reads.
    fn inner_view(self) -> Option<&'static str> {
        match self {
            Shape::OverView => Some("SELECT id, ref, label FROM t"),
            _ => None,
        }
    }

    pub fn select(self, name: RowidName) -> String {
        self.select_with(name, true)
    }

    /// `seek: false` wraps the rowid side of each term in unary plus, so no
    /// engine seeks the rowid and the term compares values.
    fn select_with(self, name: RowidName, seek: bool) -> String {
        let r = name.sql();
        let p = if seek { "" } else { "+" };
        let cols = "SELECT c.region, c.id AS hid, h.id, h.label";
        let using = if seek {
            "SELECT c.region, id AS hid, h.id AS tid, h.label"
        } else {
            "SELECT c.region, c.id AS hid, h.id AS tid, h.label"
        };
        match self {
            Shape::SelfJoin => format!(
                "SELECT a.id AS child, a.ref, b.id AS up, b.label FROM t a JOIN t b ON a.ref = {p}b.{r}"
            ),
            Shape::TwoTable => format!("{cols} FROM cur c JOIN t h ON c.id = {p}h.{r}"),
            Shape::Reversed => format!("{cols} FROM cur c JOIN t h ON {p}h.{r} = c.id"),
            Shape::CommaWhere => format!("{cols} FROM cur c, t h WHERE c.id = {p}h.{r}"),
            Shape::ExtraTerm => {
                format!("{cols} FROM cur c JOIN t h ON c.id = {p}h.{r} AND h.label IS NOT 'x'")
            }
            Shape::KeyExpression => format!("{cols} FROM cur c JOIN t h ON +c.id = {p}h.{r}"),
            Shape::Collated => {
                format!("{cols} FROM cur c JOIN t h ON c.id = {p}h.{r} COLLATE NOCASE")
            }
            Shape::KeyCollated => {
                format!("{cols} FROM cur c JOIN t h ON c.id COLLATE NOCASE = {p}h.{r}")
            }
            Shape::Parenthesized => format!("{cols} FROM cur c JOIN t h ON c.id = ({p}h.{r})"),
            Shape::ParenthesizedReversed => {
                format!("{cols} FROM cur c JOIN t h ON ({p}h.{r}) = (c.id)")
            }
            Shape::LeftJoin => format!("{cols} FROM cur c LEFT JOIN t h ON c.id = {p}h.{r}"),
            Shape::ThreeWay => format!(
                "SELECT c.region, h.id, g.id AS up FROM cur c JOIN t h ON c.id = {p}h.{r} JOIN t g ON h.ref = {p}g.{r}"
            ),
            Shape::Aggregate => format!(
                "SELECT h.label, count(*) AS n FROM cur c JOIN t h ON c.id = {p}h.{r} GROUP BY h.label"
            ),
            Shape::Exists => format!(
                "SELECT c.region, c.id FROM cur c WHERE EXISTS (SELECT 1 FROM t h WHERE {p}h.{r} = c.id)"
            ),
            Shape::InSubquery => format!(
                "SELECT c.region, c.id FROM cur c WHERE c.id IN (SELECT {p}h.{r} FROM t h WHERE h.label IS NOT c.region)"
            ),
            Shape::Uncorrelated => format!(
                "SELECT c.region, c.id FROM cur c WHERE c.id IN (SELECT {p}h.{r} FROM t h)"
            ),
            Shape::NotExists => format!(
                "SELECT c.region, c.id FROM cur c WHERE NOT EXISTS (SELECT 1 FROM t h WHERE {p}h.{r} = c.id)"
            ),
            Shape::NotIn => format!(
                "SELECT c.region, c.id FROM cur c WHERE c.id NOT IN (SELECT {p}h.{r} FROM t h WHERE h.label IS NOT c.region)"
            ),
            Shape::SelfCollated => format!(
                "SELECT a.id AS child, a.ref, b.id AS up, b.label FROM t a JOIN t b ON a.ref = {p}b.{r} COLLATE NOCASE"
            ),
            Shape::OverView => format!("{cols} FROM cur c JOIN m h ON c.id = {p}h.{r}"),
            // The seek-free form is the ON term USING and NATURAL stand for.
            Shape::Using | Shape::Natural if !seek => {
                format!("{using} FROM cur c JOIN t h ON c.id = +h.id")
            }
            Shape::LeftUsing if !seek => {
                format!("{using} FROM cur c LEFT JOIN t h ON c.id = +h.id")
            }
            Shape::Using => format!("{using} FROM cur c JOIN t h USING ({r})"),
            Shape::Natural => format!("{using} FROM cur c NATURAL JOIN t h"),
            Shape::LeftUsing => format!("{using} FROM cur c LEFT JOIN t h USING ({r})"),
            Shape::Recursive => format!(
                "WITH RECURSIVE chain(region, at, depth) AS (\
                 SELECT region, id, 0 FROM cur \
                 UNION ALL SELECT chain.region, h.ref, chain.depth + 1 FROM t h \
                 JOIN chain ON {p}h.{r} = chain.at WHERE chain.depth < 4) \
                 SELECT region, at, depth FROM chain"
            ),
        }
    }

    /// The name the join term reads: NATURAL always joins on `id`.
    fn term_name(self, name: RowidName) -> RowidName {
        match self {
            Shape::Natural => RowidName::Alias,
            _ => name,
        }
    }

    /// `t` is also the outer side of a term on `t.ref`.
    fn joins_on_ref(self) -> bool {
        matches!(
            self,
            Shape::SelfJoin | Shape::ThreeWay | Shape::SelfCollated
        )
    }
}

/// A connection with no busy wait: a lock conflict fails at once on both
/// databases alike.
fn connect(db: &TempDatabase) -> Arc<Connection> {
    let conn = db.connect_limbo();
    conn.set_busy_timeout(Duration::ZERO);
    conn
}

fn rows(db: &TempDatabase, conn: &Arc<Connection>, sql: &str) -> Result<Vec<String>, String> {
    try_limbo_exec_rows(db, conn, sql)
        .map(sorted)
        .map_err(|e| format!("`{sql}`: {e}"))
}

fn open(views: bool) -> TempDatabase {
    TempDatabase::builder().with_views(views).build()
}

fn check_all<S>(
    fixed: Vec<S::Value>,
    cases: u32,
    strategy: S,
    property: impl Fn(S::Value) -> Result<(), TestCaseError>,
) where
    S: Strategy,
    S::Value: Clone + std::fmt::Debug,
{
    let mut failures: Vec<String> = fixed
        .into_iter()
        .filter_map(|value| {
            let name = format!("{value:?}");
            property(value)
                .err()
                .map(|e| format!("fixed example {name}: {e}"))
        })
        .collect();
    let mut runner = TestRunner::new(RunnerConfig {
        cases,
        max_shrink_iters: 48,
        failure_persistence: None,
        ..RunnerConfig::default()
    });
    if let Err(e) = runner.run(&strategy, &property) {
        failures.push(format!("random cases: {e}"));
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn key_with(k: impl Strategy<Value = i64> + Clone + 'static) -> impl Strategy<Value = Key> {
    prop_oneof![
        6 => k.clone().prop_map(Key::Int),
        2 => k.clone().prop_map(Key::Real),
        2 => k.clone().prop_map(Key::Frac),
        3 => k.clone().prop_map(Key::Text),
        1 => k.clone().prop_map(Key::Spaced),
        1 => k.clone().prop_map(Key::TextReal),
        1 => k.clone().prop_map(Key::Plus),
        1 => k.clone().prop_map(Key::Hex),
        1 => k.clone().prop_map(Key::Word),
        1 => k.clone().prop_map(Key::Upper),
        1 => k.clone().prop_map(Key::Blob),
        1 => k.prop_map(Key::TextBlob),
        2 => Just(Key::Null),
        1 => Just(Key::Max),
        1 => Just(Key::Min),
        1 => Just(Key::Huge),
        1 => Just(Key::NegZero),
    ]
}

fn inner() -> impl Strategy<Value = Inner> {
    prop::sample::select(Inner::ALL.to_vec())
}

fn rowid_name() -> impl Strategy<Value = RowidName> {
    prop::sample::select(RowidName::ALL.to_vec())
}

fn ref_decl() -> impl Strategy<Value = RefDecl> {
    prop::sample::select(RefDecl::ALL.to_vec())
}

fn index_form() -> impl Strategy<Value = IndexForm> {
    prop::sample::select(IndexForm::ALL.to_vec())
}

// ---------------------------------------------------------------------------
// Cost of one write
// ---------------------------------------------------------------------------

/// The write whose VM steps are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    InsertT,
    UpdateTRef,
    DeleteT,
    InsertCur,
    UpdateCur,
    ReplaceCur,
    DeleteCur,
}

impl Write {
    pub const ALL: [Write; 7] = [
        Write::InsertT,
        Write::UpdateTRef,
        Write::DeleteT,
        Write::InsertCur,
        Write::UpdateCur,
        Write::ReplaceCur,
        Write::DeleteCur,
    ];

    fn writes_t(self) -> bool {
        matches!(self, Write::InsertT | Write::UpdateTRef | Write::DeleteT)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CostCase {
    pub inner: Inner,
    pub name: RowidName,
    pub shape: Shape,
    pub ref_decl: RefDecl,
    pub ref_index: Option<IndexForm>,
    pub hid_index: Option<IndexForm>,
    pub write: Write,
    /// The form of the key the write stores; its id is an existing row's.
    pub key: Key,
    pub fanout: i64,
    pub views_before_data: bool,
}

const SMALL: i64 = 120;
const GROWTH: i64 = 8;
const ROOTS: i64 = 10;
const CURSORS: i64 = 4;

impl CostCase {
    /// Every join phase the write runs has a seek target for its term: the
    /// rowid or a fitting index on the inner side, or a fixed-size table.
    fn claims_flat(self) -> bool {
        // The contract's Post covers a plain join over one base table only.
        let plain = !matches!(
            self.shape,
            Shape::Aggregate | Shape::OverView | Shape::Recursive | Shape::ThreeWay
        );
        let ref_side = !(self.shape.joins_on_ref() && self.write.writes_t())
            || self.ref_index.is_some_and(|form| {
                if self.shape == Shape::SelfCollated {
                    form == IndexForm::OtherCollation
                        && matches!(
                            self.ref_decl,
                            RefDecl::Integer | RefDecl::Real | RefDecl::Numeric
                        )
                } else {
                    form.fits_rowid_key(self.ref_decl)
                }
            });
        // A collation on the term changes no rowid comparison, but a stored
        // index on `id` orders TEXT values under another collation.
        let target = if matches!(
            self.shape,
            Shape::Collated | Shape::KeyCollated | Shape::SelfCollated
        ) {
            self.inner.names_rowid(self.name)
        } else {
            self.inner.seekable(self.shape.term_name(self.name))
        };
        plain && target && ref_side
    }

    fn shadow_value(self, id: i64, n: i64) -> String {
        match self.inner.shadow() {
            Some(_) => format!(", {}", n + 1 - id),
            None => String::new(),
        }
    }

    /// Rows `1..=n` of `t`, in rowid order, so the rowid equals `id`.
    fn build(self, conn: &Arc<Connection>, n: i64, views: bool) -> Result<(), String> {
        let run = |sql: &str| conn.execute(sql).map_err(|e| format!("`{sql}`: {e}"));
        let make_views = || -> Result<(), String> {
            if let Some(m) = self.shape.inner_view() {
                run(&format!("CREATE MATERIALIZED VIEW m AS {m}"))?;
            }
            run(&format!(
                "CREATE MATERIALIZED VIEW v AS {}",
                self.shape.select(self.name)
            ))
        };
        run(&self.inner.create(self.ref_decl))?;
        run(&format!(
            "CREATE TABLE cur(region TEXT PRIMARY KEY, id {})",
            self.ref_decl.sql()
        ))?;
        if let Some(form) = self.ref_index {
            run(&ref_index(form))?;
        }
        if let Some(form) = self.hid_index {
            run(&hid_index(form))?;
        }
        if views && self.views_before_data {
            make_views()?;
        }
        run("BEGIN")?;
        for chunk in (1..=n).collect::<Vec<_>>().chunks(200) {
            let values: Vec<String> = chunk
                .iter()
                .map(|&i| {
                    let parent = if i <= ROOTS {
                        "NULL".to_string()
                    } else {
                        self.ref_decl.own((i - ROOTS - 1) / self.fanout + 1).sql()
                    };
                    format!("({i}, {parent}, 'c'{})", self.shadow_value(i, n))
                })
                .collect();
            run(&format!(
                "INSERT INTO t({}) VALUES {}",
                self.inner.columns(),
                values.join(",")
            ))?;
        }
        for j in 0..CURSORS {
            run(&format!(
                "INSERT INTO cur VALUES ('r{j}', {})",
                self.ref_decl.own(j * 7 + 1).sql()
            ))?;
        }
        run("COMMIT")?;
        if views && !self.views_before_data {
            make_views()?;
        }
        Ok(())
    }

    fn writes(self, n: i64) -> Vec<String> {
        (0..3)
            .map(|j| {
                let key = self.key.at((j * 37 + n / 2) % n + 1).sql();
                let at = self.inner.real_rowid();
                match self.write {
                    Write::InsertT => format!(
                        "INSERT INTO t({}) VALUES ({}, {key}, 'c'{})",
                        self.inner.columns(),
                        n + 1 + j,
                        self.shadow_value(n + 1 + j, n)
                    ),
                    Write::UpdateTRef => format!("UPDATE t SET ref = {key} WHERE {at} = {}", n - j),
                    Write::DeleteT => format!("DELETE FROM t WHERE {at} = {}", n - j),
                    Write::InsertCur => format!("INSERT INTO cur VALUES ('n{j}', {key})"),
                    Write::UpdateCur => format!("UPDATE cur SET id = {key} WHERE region = 'r{j}'"),
                    Write::ReplaceCur => {
                        format!("INSERT OR REPLACE INTO cur VALUES ('r{j}', {key})")
                    }
                    Write::DeleteCur => format!("DELETE FROM cur WHERE region = 'r{j}'"),
                }
            })
            .collect()
    }
}

/// The error of a materialized view over a WITHOUT ROWID table: that table
/// has no column `rowid`, `_rowid_` or `oid`, and the view is refused loud.
fn without_rowid_refusal(name: RowidName) -> String {
    match name {
        RowidName::Alias => WITHOUT_ROWID.to_string(),
        other => format!("no such column: {}", other.sql()),
    }
}

/// `cur` has no column named rowid, _rowid_ or oid.
fn using_refusal(name: RowidName) -> String {
    format!(
        "cannot join using column {} - column not present in both tables",
        name.sql()
    )
}

const WITHOUT_ROWID: &str = "WITHOUT ROWID base tables are not yet supported";

/// The error of the SELECT of `v` on every engine: the term names a rowid
/// that a WITHOUT ROWID table or a view does not have.
fn select_refusal(inner: Inner, shape: Shape, name: RowidName) -> Option<String> {
    match (inner, shape, shape.term_name(name)) {
        (_, _, RowidName::Alias) => None,
        (_, Shape::Using | Shape::LeftUsing, name) => Some(using_refusal(name)),
        (Inner::WithoutRowid, _, name) => Some(without_rowid_refusal(name)),
        _ => None,
    }
}

/// The error that creating the materialized views fails with.
fn view_refusal(inner: Inner, shape: Shape, name: RowidName) -> Option<String> {
    // Name binding fails first, then the subquery check, then the base-table check.
    let name = shape.term_name(name);
    match (inner, shape, name) {
        (_, Shape::Using | Shape::LeftUsing, name) if name != RowidName::Alias => {
            Some(using_refusal(name))
        }
        (Inner::WithoutRowid, Shape::OverView, _) => Some(WITHOUT_ROWID.to_string()),
        (Inner::WithoutRowid, _, name) if name != RowidName::Alias => {
            Some(without_rowid_refusal(name))
        }
        (_, Shape::Uncorrelated | Shape::NotIn, _) => {
            Some("materialized views with subqueries are not yet supported".to_string())
        }
        (Inner::WithoutRowid, _, _) => Some(WITHOUT_ROWID.to_string()),
        _ => select_refusal(inner, shape, name),
    }
}

enum Built {
    Steps(u64),
    /// The table without views refuses it too.
    Refused,
}

/// The median VM-step count of three writes into tables of `n` rows; the
/// view then holds the rows of its SELECT.
fn write_steps(case: CostCase, n: i64) -> Result<Built, String> {
    let db = open(true);
    let conn = connect(&db);
    let refusal = view_refusal(case.inner, case.shape, case.name);
    let built = case.build(&conn, n, true);
    if let (Ok(()), Some(refusal)) = (&built, &refusal) {
        return Err(format!(
            "the views are created; they must fail with `{refusal}`"
        ));
    }
    if let Err(e) = built {
        if refusal.is_some_and(|r| e.contains(&r)) {
            return Ok(Built::Refused);
        }
        let twin = open(false);
        return match case.build(&connect(&twin), n, false) {
            Err(_) => Ok(Built::Refused),
            Ok(()) => Err(format!(
                "the tables without views accept it, the views refuse it: {e}"
            )),
        };
    }
    let mut samples = Vec::new();
    for sql in case.writes(n) {
        match steps_of(&conn, &sql) {
            Ok(steps) => samples.push(steps),
            Err(e) => {
                let twin = open(false);
                let twin_conn = connect(&twin);
                case.build(&twin_conn, n, false)?;
                if twin_conn.execute(&sql).is_ok() {
                    return Err(format!("`{sql}` fails with views only: {e}"));
                }
            }
        }
    }
    let select = case.shape.select(case.name);
    let held = rows(&db, &conn, "SELECT * FROM v")?;
    let fresh = rows(&db, &conn, &select)?;
    if held != fresh {
        return Err(format!(
            "after the writes into {n} rows the view differs from its SELECT\nview:   {held:?}\nselect: {fresh:?}"
        ));
    }
    if samples.is_empty() {
        return Ok(Built::Refused);
    }
    samples.sort();
    Ok(Built::Steps(samples[samples.len() / 2]))
}

fn write_cost_is_flat(case: CostCase) -> Result<(), TestCaseError> {
    let small = write_steps(case, SMALL).map_err(TestCaseError::fail)?;
    let large = write_steps(case, SMALL * GROWTH).map_err(TestCaseError::fail)?;
    match (small, large) {
        (Built::Steps(small), Built::Steps(large)) => {
            if case.claims_flat() && large >= 2 * small {
                return Err(TestCaseError::fail(format!(
                    "one write takes {small} VM steps at {SMALL} rows and {large} at {} rows: the maintenance scans",
                    SMALL * GROWTH
                )));
            }
            Ok(())
        }
        (Built::Refused, Built::Refused) => Ok(()),
        _ => Err(TestCaseError::fail(
            "the two table sizes got different outcomes".to_string(),
        )),
    }
}

fn cost_case() -> impl Strategy<Value = CostCase> {
    (
        (
            inner(),
            rowid_name(),
            prop::sample::select(Shape::ALL.to_vec()),
            ref_decl(),
        ),
        (
            prop::option::weighted(0.6, index_form()),
            prop::option::weighted(0.3, index_form()),
            prop::sample::select(Write::ALL.to_vec()),
            key_with(Just(0)),
            2i64..=8,
            any::<bool>(),
        ),
    )
        .prop_map(
            |(
                (inner, name, shape, ref_decl),
                (ref_index, hid_index, write, key, fanout, before),
            )| {
                CostCase {
                    inner,
                    name,
                    shape,
                    ref_decl,
                    ref_index,
                    hid_index,
                    write,
                    key,
                    fanout,
                    views_before_data: before,
                }
            },
        )
}

/// One cursor write into the history join that a navigation runs.
fn cursor_write(inner: Inner, write: Write) -> CostCase {
    CostCase {
        inner,
        name: RowidName::Alias,
        shape: Shape::TwoTable,
        ref_decl: RefDecl::Integer,
        ref_index: None,
        hid_index: None,
        write,
        key: Key::Int(0),
        fanout: 4,
        views_before_data: false,
    }
}

#[test]
fn ivm_write_cost_stays_flat_when_a_join_term_names_the_rowid() {
    let self_join = CostCase {
        shape: Shape::SelfJoin,
        ref_index: Some(IndexForm::Plain),
        write: Write::InsertT,
        ..cursor_write(Inner::IntegerPrimaryKey, Write::InsertT)
    };
    let mut fixed = vec![
        // A leaf insert into a tree joined to itself on INTEGER PRIMARY KEY.
        self_join,
        // A cursor update and replace over a growing AUTOINCREMENT history.
        cursor_write(Inner::Autoincrement, Write::UpdateCur),
        cursor_write(Inner::Autoincrement, Write::ReplaceCur),
        cursor_write(Inner::IntegerPrimaryKey, Write::InsertCur),
        cursor_write(Inner::IntegerPrimaryKey, Write::DeleteCur),
        // Keys that convert to no integer miss, and still seek.
        CostCase {
            key: Key::Upper(0),
            ..cursor_write(Inner::IntegerPrimaryKey, Write::UpdateCur)
        },
        CostCase {
            key: Key::Frac(0),
            ..cursor_write(Inner::IntegerPrimaryKey, Write::InsertCur)
        },
        CostCase {
            key: Key::Text(0),
            ref_decl: RefDecl::Text,
            ..cursor_write(Inner::IntegerPrimaryKey, Write::UpdateCur)
        },
        CostCase {
            key: Key::Null,
            ..cursor_write(Inner::IntegerPrimaryKey, Write::UpdateCur)
        },
        // The rowid named without an alias.
        CostCase {
            name: RowidName::Rowid,
            ..cursor_write(Inner::NoKey, Write::UpdateCur)
        },
        CostCase {
            name: RowidName::Oid,
            ..cursor_write(Inner::Shadowing(RowidName::Rowid), Write::UpdateCur)
        },
        // A column named rowid is no rowid: correct, the cost is not claimed.
        CostCase {
            name: RowidName::Rowid,
            ..cursor_write(Inner::Shadowing(RowidName::Rowid), Write::UpdateCur)
        },
        // INT PRIMARY KEY seeks its automatic index.
        cursor_write(Inner::IntPrimaryKey, Write::UpdateCur),
        // The term under NOCASE seeks a NOCASE index on `ref`.
        CostCase {
            shape: Shape::SelfCollated,
            ref_index: Some(IndexForm::OtherCollation),
            ..self_join
        },
    ];
    for shape in [
        Shape::Reversed,
        Shape::CommaWhere,
        Shape::ExtraTerm,
        Shape::KeyExpression,
        Shape::Collated,
        Shape::KeyCollated,
        Shape::Parenthesized,
        Shape::ParenthesizedReversed,
        Shape::LeftJoin,
        Shape::ThreeWay,
        Shape::Exists,
        Shape::InSubquery,
        Shape::NotExists,
        Shape::NotIn,
        Shape::OverView,
        Shape::Recursive,
        Shape::Using,
        Shape::Natural,
        Shape::LeftUsing,
    ] {
        // The automatic index of INT PRIMARY KEY seeks the same term.
        for inner in [Inner::IntegerPrimaryKey, Inner::IntPrimaryKey] {
            fixed.push(CostCase {
                shape,
                ..cursor_write(inner, Write::UpdateCur)
            });
        }
    }
    check_all(fixed, 24, cost_case(), write_cost_is_flat);
}

// ---------------------------------------------------------------------------
// Correctness under every statement that touches the joined tables
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    Kept,
    /// Filtered by [`Shape::ExtraTerm`].
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Abort,
    Replace,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    id: Key,
    key: Key,
    label: Label,
    shadow: Key,
}

/// The column an UPDATE of `t` sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Set {
    Ref(Key),
    Label(Label),
    /// Moves the row to another rowid when `id` is the alias.
    Id(Key),
    /// `SET <name> = ...`: the rowid, or the column that shadows the name.
    Rowid(RowidName, Key),
}

#[derive(Clone, Debug)]
pub enum Op {
    InsertT {
        rows: Vec<Row>,
        conflict: Conflict,
        conn: usize,
    },
    UpsertT {
        row: Row,
        conn: usize,
    },
    UpdateT {
        set: Set,
        at: Key,
        conn: usize,
    },
    DeleteT {
        at: Key,
        conn: usize,
    },
    InsertCur {
        region: u8,
        hid: Key,
        replace: bool,
        conn: usize,
    },
    UpdateCur {
        region: u8,
        hid: Key,
        conn: usize,
    },
    DeleteCur {
        region: u8,
        conn: usize,
    },
    Txn(Txn, usize),
    CreateRefIndex(IndexForm),
    DropRefIndex,
    CreateHidIndex(IndexForm),
    DropHidIndex,
    Reindex,
    AddColumn,
    CreateView(usize),
    DropView(usize),
    Refresh(usize),
}

impl Op {
    /// The statement on both databases, or None for view DDL.
    fn table_sql(&self, inner: Inner, step: usize) -> Option<(usize, String)> {
        let row = |r: &Row| match inner.shadow() {
            Some(_) => format!(
                "({}, {}, {}, {})",
                r.id.sql(),
                r.key.sql(),
                r.label.sql(),
                r.shadow.sql()
            ),
            None => format!("({}, {}, {})", r.id.sql(), r.key.sql(), r.label.sql()),
        };
        let cols = inner.columns();
        Some(match self {
            Op::InsertT {
                rows,
                conflict,
                conn,
            } => (
                *conn,
                format!(
                    "INSERT {}INTO t({cols}) VALUES {}",
                    match conflict {
                        Conflict::Abort => "",
                        Conflict::Replace => "OR REPLACE ",
                        Conflict::Ignore => "OR IGNORE ",
                    },
                    rows.iter().map(row).collect::<Vec<_>>().join(", ")
                ),
            ),
            Op::UpsertT { row: r, conn } => (
                *conn,
                format!(
                    "INSERT INTO t({cols}) VALUES {} ON CONFLICT(id) DO UPDATE SET ref = excluded.ref, label = excluded.label",
                    row(r)
                ),
            ),
            Op::UpdateT { set, at, conn } => {
                let set = match set {
                    Set::Ref(key) => format!("ref = {}", key.sql()),
                    Set::Label(label) => format!("label = {}", label.sql()),
                    Set::Id(key) => format!("id = {}", key.sql()),
                    Set::Rowid(name, key) => format!("{} = {}", name.sql(), key.sql()),
                };
                (
                    *conn,
                    format!("UPDATE t SET {set} WHERE id = {}", at.sql()),
                )
            }
            Op::DeleteT { at, conn } => (*conn, format!("DELETE FROM t WHERE id = {}", at.sql())),
            Op::InsertCur {
                region,
                hid,
                replace,
                conn,
            } => (
                *conn,
                format!(
                    "INSERT {}INTO cur VALUES ('r{region}', {})",
                    if *replace { "OR REPLACE " } else { "" },
                    hid.sql()
                ),
            ),
            Op::UpdateCur { region, hid, conn } => (
                *conn,
                format!("UPDATE cur SET id = {} WHERE region = 'r{region}'", hid.sql()),
            ),
            Op::DeleteCur { region, conn } => {
                (*conn, format!("DELETE FROM cur WHERE region = 'r{region}'"))
            }
            Op::Txn(txn, conn) => (*conn, txn.sql().to_string()),
            Op::CreateRefIndex(form) => (0, ref_index(*form)),
            Op::DropRefIndex => (0, "DROP INDEX t_ref".to_string()),
            Op::CreateHidIndex(form) => (0, hid_index(*form)),
            Op::DropHidIndex => (0, "DROP INDEX cur_id".to_string()),
            Op::Reindex => (0, "REINDEX".to_string()),
            Op::AddColumn => (0, format!("ALTER TABLE t ADD COLUMN extra{step}")),
            Op::CreateView(_) | Op::DropView(_) | Op::Refresh(_) => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Scenario {
    inner: Inner,
    name: RowidName,
    shape: Shape,
    ref_decl: RefDecl,
    ref_index: Option<IndexForm>,
    hid_index: Option<IndexForm>,
    views_before_data: bool,
    ops: Vec<Op>,
}

struct Side {
    db: TempDatabase,
    conns: [Arc<Connection>; 2],
}

impl Side {
    fn new(views: bool) -> Side {
        let db = open(views);
        let conns = [connect(&db), connect(&db)];
        Side { db, conns }
    }

    fn read(&self, sql: &str) -> Result<Vec<String>, String> {
        self.read_on(0, sql)
    }

    fn read_on(&self, conn: usize, sql: &str) -> Result<Vec<String>, String> {
        rows(&self.db, &self.conns[conn], sql)
    }

    fn has_view(&self) -> Result<bool, String> {
        self.read("SELECT name FROM sqlite_schema WHERE name = 'v'")
            .map(|found| !found.is_empty())
    }
}

const INITIAL_ROWS: i64 = 6;
const REGIONS: u8 = 4;

fn initial_rows(s: &Scenario) -> Vec<String> {
    let mut sql = Vec::new();
    for i in 1..=INITIAL_ROWS {
        let parent = if i <= 2 {
            Key::Null
        } else {
            s.ref_decl.own((i - 1) / 2)
        };
        let row = Row {
            id: Key::Int(i),
            key: parent,
            label: Label::Kept,
            shadow: Key::Int(INITIAL_ROWS + 1 - i),
        };
        sql.push(
            Op::InsertT {
                rows: vec![row],
                conflict: Conflict::Abort,
                conn: 0,
            }
            .table_sql(s.inner, 0)
            .expect("an insert is a table statement")
            .1,
        );
    }
    for region in 0..REGIONS - 1 {
        sql.push(format!(
            "INSERT INTO cur VALUES ('r{region}', {})",
            s.ref_decl.own(i64::from(region) * 2 + 1).sql()
        ));
    }
    sql
}

/// Runs `sql` on both databases; their outcomes must agree. Returns whether it
/// succeeded.
fn on_both(views: &Side, twin: &Side, conn: usize, sql: &str, at: &str) -> Result<bool, String> {
    let expected = twin.conns[conn].execute(sql);
    let got = views.conns[conn].execute(sql);
    match (&expected, &got) {
        (Ok(()), Err(e)) => Err(format!("{at}: `{sql}` fails with views only: {e}")),
        (Err(e), Ok(())) => Err(format!(
            "{at}: `{sql}` succeeds with views only; without: {e}"
        )),
        (_, Err(e @ turso_core::LimboError::Corrupt(_)))
        | (Err(e @ turso_core::LimboError::Corrupt(_)), _) => {
            Err(format!("{at}: `{sql}` reports corruption: {e}"))
        }
        _ => Ok(got.is_ok()),
    }
}

/// View DDL on the views database; the twin runs the same DDL on a plain view
/// `v`, whose outcome is the expected one.
fn view_ddl(views: &Side, twin: &Side, op: &Op, s: &Scenario, at: &str) -> Result<(), String> {
    let select = s.shape.select(s.name);
    if let (Op::CreateView(conn), Some(expected)) = (op, view_refusal(s.inner, s.shape, s.name)) {
        let sql = format!("CREATE MATERIALIZED VIEW v AS {select}");
        return match views.conns[*conn].execute(&sql) {
            Err(e) if e.to_string().contains(&expected) => Ok(()),
            Err(e) => Err(format!(
                "{at}: `{sql}` fails without naming `{expected}`: {e}"
            )),
            Ok(()) => Err(format!(
                "{at}: `{sql}` succeeds; it must fail with `{expected}`"
            )),
        };
    }
    let (conn, expected, sql) = match op {
        Op::CreateView(conn) => (
            *conn,
            twin.conns[*conn].execute(format!("CREATE VIEW v AS {select}")),
            format!("CREATE MATERIALIZED VIEW v AS {select}"),
        ),
        Op::DropView(conn) => (
            *conn,
            twin.conns[*conn].execute("DROP VIEW v"),
            "DROP VIEW v".to_string(),
        ),
        Op::Refresh(conn) => {
            // A refresh writes; the twin's stand-in write meets the same locks. The
            // refreshing connection's own snapshot decides whether v exists.
            let exists = !twin
                .read_on(*conn, "SELECT name FROM sqlite_schema WHERE name = 'v'")?
                .is_empty();
            let expected = if exists {
                twin.conns[*conn].execute("UPDATE probe SET x = x + 1")
            } else {
                Err(turso_core::LimboError::InternalError(
                    "no view v".to_string(),
                ))
            };
            (*conn, expected, "REFRESH MATERIALIZED VIEW v".to_string())
        }
        _ => unreachable!("{op:?} is no view DDL"),
    };
    match (expected, views.conns[conn].execute(&sql)) {
        (Ok(()), Ok(())) | (Err(_), Err(_)) => Ok(()),
        (Ok(()), Err(e)) => Err(format!("{at}: `{sql}` fails: {e}")),
        (Err(e), Ok(())) => Err(format!(
            "{at}: `{sql}` succeeds; the plain view's DDL fails: {e}"
        )),
    }
}

/// View v, read on connection `on`, holds the rows of its SELECT on the
/// database without materialized views.
/// `own_rowid`: the SELECT names the rowid of a materialized view, which a
/// plain view does not have, so it runs on the views database itself.
fn check_view(
    views: &Side,
    on: usize,
    twin: &Side,
    own_rowid: bool,
    select: &str,
    at: &str,
) -> Result<(), String> {
    let (has, twin_has) = (views.has_view()?, twin.has_view()?);
    if has != twin_has {
        return Err(format!(
            "{at}: the materialized view exists: {has}; the plain view exists: {twin_has}"
        ));
    }
    if !has {
        return Ok(());
    }
    let held = views
        .read_on(on, "SELECT * FROM v")
        .map_err(|e| format!("{at}: reading v: {e}"))?;
    let fresh = if own_rowid {
        views.read_on(on, select)
    } else {
        twin.read(select)
    }
    .map_err(|e| format!("{at}: {e}"))?;
    if held != fresh {
        return Err(format!(
            "{at}: view v differs from its SELECT without materialized views\nview:   {held:?}\nselect: {fresh:?}"
        ));
    }
    Ok(())
}

/// A table that a materialized view reads refuses ALTER TABLE, naming the
/// view; a plain view does not hold it, so the twin is not altered.
fn altered_under_view(views: &Side, conn: usize, sql: &str, at: &str) -> Result<(), String> {
    match views.conns[conn].execute(sql) {
        Ok(()) => Err(format!(
            "{at}: `{sql}` succeeds; a table read by a materialized view refuses it"
        )),
        Err(e) if e.to_string().contains("dependent materialized view") => Ok(()),
        Err(e) => Err(format!(
            "{at}: `{sql}` fails without naming the dependent view: {e}"
        )),
    }
}

fn tables_match(views: &Side, twin: &Side, inner: Inner, at: &str) -> Result<(), String> {
    for table in [
        format!("SELECT {}, * FROM t", inner.real_rowid()),
        "SELECT * FROM cur".to_string(),
    ] {
        let (got, expected) = (views.read(&table)?, twin.read(&table)?);
        if got != expected {
            return Err(format!(
                "{at}: `{table}` differs from the tables without views\nviews:   {got:?}\nwithout: {expected:?}"
            ));
        }
    }
    Ok(())
}

/// The rows SQLite returns for `select` over the twin's tables.
fn sqlite_select(s: &Scenario, twin: &Side, select: &str) -> Result<Vec<String>, String> {
    let sqlite = rusqlite::Connection::open_in_memory().map_err(|e| e.to_string())?;
    let ddl = |sql: &str| {
        sqlite
            .execute_batch(sql)
            .map_err(|e| format!("SQLite `{sql}`: {e}"))
    };
    ddl(&s.inner.create(s.ref_decl))?;
    ddl(&format!(
        "CREATE TABLE cur(region TEXT PRIMARY KEY, id {})",
        s.ref_decl.sql()
    ))?;
    if let Some(m) = s.shape.inner_view() {
        ddl(&format!("CREATE VIEW m AS {m}"))?;
    }
    let cols = s.inner.stored();
    let copy = |read: String, write: String| -> Result<(), String> {
        let found = try_limbo_exec_rows(&twin.db, &twin.conns[0], &read)
            .map_err(|e| format!("`{read}`: {e}"))?;
        for row in found {
            sqlite
                .execute(&write, rusqlite::params_from_iter(row.iter()))
                .map_err(|e| format!("SQLite `{write}` {row:?}: {e}"))?;
        }
        Ok(())
    };
    let marks = |n: usize| vec!["?"; n].join(", ");
    copy(
        format!("SELECT {cols} FROM t"),
        format!(
            "INSERT INTO t({cols}) VALUES ({})",
            marks(cols.split(", ").count())
        ),
    )?;
    copy(
        "SELECT region, id FROM cur".to_string(),
        format!("INSERT INTO cur(region, id) VALUES ({})", marks(2)),
    )?;
    let mut stmt = sqlite
        .prepare(select)
        .map_err(|e| format!("SQLite `{select}`: {e}"))?;
    let width = stmt.column_count();
    let found = stmt
        .query_map([], |row| {
            (0..width)
                .map(|i| row.get::<_, Value>(i))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| format!("SQLite `{select}`: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("SQLite `{select}`: {e}"))?;
    Ok(sorted(found))
}

/// The SELECT that v is checked against. A REAL key of -2^63 equals rowid
/// i64::MIN, but a rowid seek of the SELECT misses it; the seek-free form
/// compares values, as SQL comparison does. An IN list takes the affinity of
/// its left side when the right side has none, so IN keeps the seek form.
fn reference_select(s: &Scenario, twin: &Side) -> Result<String, String> {
    let edge = twin.read(
        "SELECT 1 FROM (SELECT ref AS k FROM t UNION ALL SELECT id FROM cur) \
         WHERE typeof(k) = 'real' AND k = -9223372036854775808",
    )?;
    let in_list = matches!(
        s.shape,
        Shape::InSubquery | Shape::Uncorrelated | Shape::NotIn
    );
    Ok(s.shape.select_with(s.name, edge.is_empty() || in_list))
}

fn run_scenario(s: &Scenario) -> Result<(), String> {
    let own_rowid = s.shape == Shape::OverView && s.name != RowidName::Alias;
    let views = Side::new(true);
    let twin = Side::new(false);
    let select = s.shape.select(s.name);
    let mut setup = vec![
        "CREATE TABLE probe(x)".to_string(),
        "INSERT INTO probe VALUES (1)".to_string(),
        s.inner.create(s.ref_decl),
        format!(
            "CREATE TABLE cur(region TEXT PRIMARY KEY, id {})",
            s.ref_decl.sql()
        ),
    ];
    setup.extend(s.ref_index.map(ref_index));
    setup.extend(s.hid_index.map(hid_index));
    let data = initial_rows(s);
    if !s.views_before_data {
        setup.extend(data.iter().cloned());
    }
    for sql in &setup {
        if !on_both(&views, &twin, 0, sql, "setup")? {
            // Both refuse the table: there is nothing to join.
            return Ok(());
        }
    }
    if let Some(m) = s.shape.inner_view() {
        let sql = format!("CREATE MATERIALIZED VIEW m AS {m}");
        match (views.conns[0].execute(&sql), s.inner) {
            (Err(e), Inner::WithoutRowid) if e.to_string().contains(WITHOUT_ROWID) => return Ok(()),
            (Ok(()), Inner::WithoutRowid) => {
                return Err(format!(
                    "setup: `{sql}` succeeds over a WITHOUT ROWID table"
                ))
            }
            (Err(e), _) => return Err(format!("setup: `{sql}` fails: {e}")),
            (Ok(()), _) => {}
        }
        twin.conns[0]
            .execute(format!("CREATE VIEW m AS {m}"))
            .map_err(|e| format!("setup: the plain view m: {e}"))?;
    }
    view_ddl(&views, &twin, &Op::CreateView(0), s, "setup")?;
    if s.views_before_data {
        for sql in &data {
            on_both(&views, &twin, 0, sql, "setup")?;
        }
    }
    check_view(
        &views,
        0,
        &twin,
        own_rowid,
        &reference_select(s, &twin)?,
        "after setup",
    )?;
    let runs_on = |op: &Op| match op {
        Op::Txn(_, c) | Op::CreateView(c) | Op::DropView(c) | Op::Refresh(c) => Some(*c),
        _ => op.table_sql(s.inner, 0).map(|(c, _)| c),
    };
    // A connection in a SAVEPOINT-opened transaction that has not read yet.
    let mut unread = [false; 2];
    for (step, op) in s.ops.iter().enumerate() {
        let at = format!("after step {step} {op:?}");
        let opens = matches!(op, Op::Txn(Txn::Savepoint, c) if views.conns[*c].get_auto_commit());
        match op.table_sql(s.inner, step) {
            Some((conn, sql))
                if matches!(op, Op::AddColumn)
                    && (views.has_view()? || s.shape.inner_view().is_some()) =>
            {
                altered_under_view(&views, conn, &sql, &at)?
            }
            Some((conn, sql)) => {
                let in_txn = !views.conns[conn].get_auto_commit();
                let done = on_both(&views, &twin, conn, &sql, &at)?;
                // Inside a transaction, a failed multi-row insert keeps its earlier rows
                // without views and backs them out with a materialized view.
                if !done && in_txn && matches!(op, Op::InsertT { rows, .. } if rows.len() > 1) {
                    return Ok(());
                }
                tables_match(&views, &twin, s.inner, &at)?;
            }
            // shortcut: no refresh while the other connection is in an unread SAVEPOINT
            // transaction; its next write fails "Database snapshot is stale".
            None if matches!(op, Op::Refresh(c) if unread[1 - c]) => {}
            None => view_ddl(&views, &twin, op, s, &at)?,
        }
        if !matches!(op, Op::Txn(..)) {
            if let Some(c) = runs_on(op) {
                unread[c] = false;
            }
        }
        if opens {
            unread[runs_on(op).expect("a savepoint runs on a connection")] = true;
        }
        check_view(
            &views,
            0,
            &twin,
            own_rowid,
            &reference_select(s, &twin)?,
            &at,
        )?;
        unread[0] = false;
        for (c, flag) in unread.iter_mut().enumerate() {
            *flag &= !views.conns[c].get_auto_commit();
        }
    }
    for conn in 0..2 {
        if !views.conns[conn].get_auto_commit() {
            on_both(&views, &twin, conn, "COMMIT", "final COMMIT")?;
        }
    }
    let reference = reference_select(s, &twin)?;
    check_view(
        &views,
        1,
        &twin,
        own_rowid,
        &reference,
        "on the second connection",
    )?;
    let check = limbo_exec_rows(&views.conns[0], "PRAGMA integrity_check");
    if check != vec![vec![Value::Text("ok".to_string())]] {
        return Err(format!("integrity_check: {check:?}"));
    }
    if own_rowid {
        // shortcut: SQLite has no materialized views and its views have no rowid, so
        // no engine gives the expected rows; the check above is the only reference.
        return Ok(());
    }
    if let Some(refusal) = select_refusal(s.inner, s.shape, s.name) {
        return match (twin.read(&select), sqlite_select(s, &twin, &select)) {
            (Err(turso), Err(_)) if turso.contains(&refusal) => Ok(()),
            (turso, sqlite) => Err(format!(
                "at the end `{select}`: expected both engines to refuse with `{refusal}`\nturso:  {turso:?}\nSQLite: {sqlite:?}"
            )),
        };
    }
    let sql = sqlite_select(s, &twin, &reference)?;
    let fresh = twin.read(&reference)?;
    if views.has_view()? {
        let held = views.read("SELECT * FROM v")?;
        if held != sql {
            return Err(format!(
                "at the end view v differs from SQLite's rows for its SELECT\nview:   {held:?}\nSQLite: {sql:?}"
            ));
        }
    }
    if fresh != sql {
        return Err(format!(
            "at the end the SELECT without materialized views differs from SQLite's rows\nturso:  {fresh:?}\nSQLite: {sql:?}"
        ));
    }
    Ok(())
}

/// A row id: after a rowid of i64::MAX, a row inserted without an id gets a
/// random rowid, so two databases cannot hold the same rowids.
fn row_id() -> impl Strategy<Value = Key> {
    key_with(0i64..9).prop_filter("i64::MAX rowid", |k| *k != Key::Max)
}

fn row() -> impl Strategy<Value = Row> {
    (row_id(), key_with(0i64..9), label(), key_with(0i64..9)).prop_map(
        |(id, key, label, shadow)| Row {
            id,
            key,
            label,
            shadow,
        },
    )
}

fn label() -> impl Strategy<Value = Label> {
    prop::sample::select(vec![Label::Kept, Label::Filtered, Label::Null])
}

fn conn() -> impl Strategy<Value = usize> {
    prop_oneof![4 => Just(0usize), 1 => Just(1usize)]
}

fn region() -> impl Strategy<Value = u8> {
    0..REGIONS
}

fn op() -> impl Strategy<Value = Op> {
    let key = || key_with(0i64..9);
    let set = prop_oneof![
        3 => key().prop_map(Set::Ref),
        1 => label().prop_map(Set::Label),
        2 => row_id().prop_map(Set::Id),
        2 => (rowid_name(), row_id()).prop_map(|(name, key)| Set::Rowid(name, key)),
    ];
    let conflict = prop::sample::select(vec![Conflict::Abort, Conflict::Replace, Conflict::Ignore]);
    let txn = prop::sample::select(vec![
        Txn::Begin,
        Txn::Commit,
        Txn::Rollback,
        Txn::Savepoint,
        Txn::RollbackTo,
        Txn::Release,
    ]);
    prop_oneof![
        5 => (prop::collection::vec(row(), 1..3), conflict, conn())
            .prop_map(|(rows, conflict, conn)| Op::InsertT { rows, conflict, conn }),
        1 => (row(), conn()).prop_map(|(row, conn)| Op::UpsertT { row, conn }),
        4 => (set, key(), conn()).prop_map(|(set, at, conn)| Op::UpdateT { set, at, conn }),
        2 => (key(), conn()).prop_map(|(at, conn)| Op::DeleteT { at, conn }),
        3 => (region(), key(), any::<bool>(), conn())
            .prop_map(|(region, hid, replace, conn)| Op::InsertCur { region, hid, replace, conn }),
        4 => (region(), key(), conn()).prop_map(|(region, hid, conn)| Op::UpdateCur { region, hid, conn }),
        2 => (region(), conn()).prop_map(|(region, conn)| Op::DeleteCur { region, conn }),
        4 => (txn, conn()).prop_map(|(txn, conn)| Op::Txn(txn, conn)),
        1 => index_form().prop_map(Op::CreateRefIndex),
        1 => Just(Op::DropRefIndex),
        1 => index_form().prop_map(Op::CreateHidIndex),
        1 => Just(Op::DropHidIndex),
        1 => Just(Op::Reindex),
        1 => Just(Op::AddColumn),
        1 => conn().prop_map(Op::CreateView),
        1 => conn().prop_map(Op::DropView),
        1 => conn().prop_map(Op::Refresh),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        (
            inner(),
            rowid_name(),
            prop::sample::select(Shape::ALL.to_vec()),
            ref_decl(),
        ),
        (
            prop::option::weighted(0.5, index_form()),
            prop::option::weighted(0.3, index_form()),
            any::<bool>(),
            prop::collection::vec(op(), 1..24),
        ),
    )
        .prop_map(
            |((inner, name, shape, ref_decl), (ref_index, hid_index, views_before_data, ops))| {
                Scenario {
                    inner,
                    name,
                    shape,
                    ref_decl,
                    ref_index,
                    hid_index,
                    views_before_data,
                    ops,
                }
            },
        )
}

fn views_match_their_select(s: Scenario) -> Result<(), TestCaseError> {
    run_scenario(&s).map_err(TestCaseError::fail)
}

fn cursor_scenario(inner: Inner, ops: Vec<Op>) -> Scenario {
    Scenario {
        inner,
        name: RowidName::Alias,
        shape: Shape::TwoTable,
        ref_decl: RefDecl::Integer,
        ref_index: None,
        hid_index: None,
        views_before_data: false,
        ops,
    }
}

fn point(region: u8, hid: Key) -> Op {
    Op::UpdateCur {
        region,
        hid,
        conn: 0,
    }
}

#[test]
fn ivm_rowid_join_views_match_their_select_after_every_statement() {
    let misses = vec![
        point(0, Key::Upper(0)),
        point(1, Key::Frac(1)),
        point(2, Key::Text(3)),
        point(0, Key::Spaced(2)),
        point(1, Key::Real(4)),
        point(2, Key::Hex(1)),
        point(0, Key::Blob(2)),
        point(1, Key::Max),
        point(2, Key::Huge),
        point(0, Key::NegZero),
    ];
    let mut fixed = vec![
        // Keys of every class written into the cursor table.
        cursor_scenario(Inner::IntegerPrimaryKey, misses.clone()),
        cursor_scenario(Inner::Autoincrement, misses.clone()),
        Scenario {
            ref_decl: RefDecl::Text,
            ..cursor_scenario(Inner::IntegerPrimaryKey, misses.clone())
        },
        Scenario {
            ref_decl: RefDecl::Untyped,
            ..cursor_scenario(Inner::IntegerPrimaryKey, misses.clone())
        },
        // A history row moves to another rowid, and is deleted, under a cursor.
        cursor_scenario(
            Inner::IntegerPrimaryKey,
            vec![
                point(0, Key::Int(2)),
                Op::UpdateT {
                    set: Set::Id(Key::Int(7)),
                    at: Key::Int(2),
                    conn: 0,
                },
                point(1, Key::Int(7)),
                Op::DeleteT {
                    at: Key::Int(7),
                    conn: 0,
                },
                Op::InsertT {
                    rows: vec![Row {
                        id: Key::Null,
                        key: Key::Int(1),
                        label: Label::Kept,
                        shadow: Key::Null,
                    }],
                    conflict: Conflict::Abort,
                    conn: 0,
                },
            ],
        ),
        // A column named rowid is no rowid.
        Scenario {
            name: RowidName::Rowid,
            ..cursor_scenario(Inner::Shadowing(RowidName::Rowid), misses.clone())
        },
        // Writes inside a transaction that rolls back.
        cursor_scenario(
            Inner::IntegerPrimaryKey,
            vec![
                Op::Txn(Txn::Begin, 0),
                point(0, Key::Int(5)),
                Op::Txn(Txn::Savepoint, 0),
                point(1, Key::Text(6)),
                Op::Txn(Txn::RollbackTo, 0),
                Op::Txn(Txn::Rollback, 0),
            ],
        ),
    ];
    for shape in Shape::ALL {
        for name in RowidName::ALL {
            fixed.push(Scenario {
                shape,
                name,
                ..cursor_scenario(Inner::IntegerPrimaryKey, misses.clone())
            });
        }
    }
    for inner in Inner::ALL {
        fixed.push(cursor_scenario(inner, misses.clone()));
        for shape in [Shape::Using, Shape::Natural] {
            fixed.push(Scenario {
                shape,
                ..cursor_scenario(inner, misses.clone())
            });
        }
    }
    // Keys equal to i64::MIN as INTEGER and TEXT, at rowid i64::MIN; the REAL
    // form follows.
    for (shape, ref_decl) in [
        (Shape::TwoTable, RefDecl::Untyped),
        (Shape::LeftJoin, RefDecl::Untyped),
        (Shape::Exists, RefDecl::Untyped),
        (Shape::NotIn, RefDecl::Untyped),
        (Shape::Using, RefDecl::Untyped),
        (Shape::LeftUsing, RefDecl::Untyped),
        (Shape::TwoTable, RefDecl::Text),
        (Shape::TwoTable, RefDecl::Integer),
    ] {
        fixed.push(Scenario {
            shape,
            ref_decl,
            ..cursor_scenario(
                Inner::IntegerPrimaryKey,
                vec![
                    Op::UpsertT {
                        row: Row {
                            id: Key::Min,
                            key: Key::Int(1),
                            label: Label::Kept,
                            shadow: Key::Null,
                        },
                        conn: 0,
                    },
                    point(0, Key::Min),
                    point(2, Key::Text(i64::MIN)),
                ],
            )
        });
    }
    // A REAL key of -2^63 matches rowid i64::MIN, as SQL value comparison says.
    for shape in [
        Shape::TwoTable,
        Shape::LeftJoin,
        Shape::Exists,
        Shape::NotIn,
        Shape::Using,
        Shape::Natural,
        Shape::LeftUsing,
    ] {
        let edge = |id: Key| Op::UpsertT {
            row: Row {
                id,
                key: Key::Int(1),
                label: Label::Kept,
                shadow: Key::Null,
            },
            conn: 0,
        };
        fixed.push(Scenario {
            shape,
            ref_decl: RefDecl::Real,
            ..cursor_scenario(
                Inner::IntegerPrimaryKey,
                vec![
                    edge(Key::Min),
                    point(1, Key::Min),
                    edge(Key::Max),
                    point(2, Key::Max),
                ],
            )
        });
    }
    check_all(fixed, 600, scenario(), views_match_their_select);
}
