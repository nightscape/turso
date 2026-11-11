//! Connections that share one WAL database: a write transaction that writes no
//! page leaves every other connection's open transaction able to write, and
//! one that writes a page makes every older read snapshot stale. Every outcome
//! and every read equals a model of SQLite's WAL locking.

use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use turso_core::{
    ConnectOpts, Connection, Database, DatabaseOpts, LimboError, OpenFlags, PlatformIO,
};
use turso_sqlite::SqliteDialect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fixture {
    Text,
    Integer,
    ArrayViewIndex,
}

/// Where the database file comes from.
#[derive(Clone, Copy, Debug)]
enum Origin {
    /// A new file without a table: no page is written yet, so its WAL has no header.
    Fresh {
        text_key: bool,
        without_rowid: bool,
        small_pages: bool,
    },
    /// A file this engine writes before the connections open.
    Built {
        text_key: bool,
        without_rowid: bool,
        small_pages: bool,
        index: bool,
        view: bool,
    },
    /// A file SQLite writes in WAL mode.
    Sqlite { text_key: bool, index: bool },
    /// A copy of a file an older engine wrote, its WAL truncated by a checkpoint.
    Fixture(Fixture),
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Passive,
    Full,
    Restart,
    Truncate,
}

impl Mode {
    fn sql(self) -> String {
        format!("PRAGMA wal_checkpoint({self:?})").to_uppercase()
    }
}

/// The table the connections write, as each origin names it.
struct Names {
    table: &'static str,
    key: &'static str,
    value: &'static str,
    index: &'static str,
    index_on: &'static str,
    /// A matview over the table and whether it keeps only rows whose value is not 'x'.
    view: Option<(&'static str, bool)>,
    text_key: bool,
    without_rowid: bool,
}

impl Origin {
    fn names(self) -> Names {
        let own = |text_key: bool, without_rowid: bool, view: bool| Names {
            table: "t",
            key: "k",
            value: "v",
            index: "tv",
            index_on: "t(v)",
            view: view.then_some(("tm", true)),
            text_key,
            without_rowid,
        };
        let node = |text_key: bool| Names {
            table: "node",
            key: "id",
            value: "label",
            index: "node_parent",
            index_on: "node(parent)",
            view: None,
            text_key,
            without_rowid: false,
        };
        match self {
            Origin::Fresh {
                text_key,
                without_rowid,
                ..
            } => own(text_key, without_rowid, false),
            Origin::Sqlite { text_key, .. } => own(text_key, false, false),
            Origin::Built {
                text_key,
                without_rowid,
                view,
                ..
            } => own(text_key, without_rowid, view),
            Origin::Fixture(Fixture::Text) => node(true),
            Origin::Fixture(Fixture::Integer) => node(false),
            Origin::Fixture(Fixture::ArrayViewIndex) => Names {
                table: "a",
                key: "id",
                value: "label",
                index: "am_ints",
                index_on: "am(ints)",
                view: Some(("am", false)),
                text_key: false,
                without_rowid: false,
            },
        }
    }
}

impl Names {
    fn key(&self, k: u8) -> String {
        if self.text_key {
            format!("'b{k}'")
        } else {
            k.to_string()
        }
    }

    fn key_text(&self, k: u8) -> String {
        if self.text_key {
            format!("b{k}")
        } else {
            k.to_string()
        }
    }

    fn create_table(&self) -> String {
        let ty = if self.text_key { "TEXT" } else { "INTEGER" };
        format!(
            "CREATE TABLE {}({} {ty} PRIMARY KEY, {} TEXT){}",
            self.table,
            self.key,
            self.value,
            if self.without_rowid {
                " WITHOUT ROWID"
            } else {
                ""
            }
        )
    }
}

#[derive(Clone, Debug)]
struct Config {
    origin: Origin,
    connections: usize,
    multiprocess_wal: bool,
    rows: Vec<(u8, Val)>,
    /// A checkpoint after the file is written, before the connections open.
    checkpoint: Option<Mode>,
    reopen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Val {
    A,
    B,
    X,
}

impl Val {
    fn text(self) -> &'static str {
        match self {
            Val::A => "a",
            Val::B => "b",
            Val::X => "x",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Begin {
    Deferred,
    Immediate,
    Exclusive,
}

#[derive(Clone, Copy, Debug)]
enum Read {
    Table,
    /// The keys of the rows whose value is 'a'.
    ByValue,
    /// The matview, or the table where the origin has none.
    View,
    Schema,
}

#[derive(Clone, Copy, Debug)]
enum Conflict {
    Abort,
    Ignore,
    Replace,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Begin(usize, Begin),
    Read(usize, Read),
    Insert(usize, u8, Val, Conflict),
    Update(usize, u8, Val),
    Delete(usize, u8),
    CreateTable(usize, bool),
    CreateIndex(usize, bool),
    DropIndex(usize, bool),
    Commit(usize),
    Rollback(usize),
    Checkpoint(usize, Mode),
    /// Close every connection and the database, then open them again.
    Reopen,
}

#[derive(Clone, Debug, PartialEq)]
struct State {
    table: Option<BTreeMap<u8, String>>,
    index: bool,
}

#[derive(Clone, Debug)]
enum Conn {
    Auto,
    Txn {
        /// The version its reads see, once it read or wrote.
        snapshot: Option<usize>,
        /// Its uncommitted state and what it wrote, while it holds the write lock.
        work: Option<(State, Wrote)>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Wrote {
    Nothing,
    /// Rows rewritten with the values they hold: whether a page is written
    /// is read from the WAL file when it commits.
    SameValues,
    Page,
}

#[derive(Debug, PartialEq)]
enum Want {
    Rows(Vec<Vec<String>>),
    Done,
    Busy,
    BusySnapshot,
    Locked,
    Refused,
    /// A DDL statement that does nothing while another connection holds the
    /// write lock or inside a transaction: not run.
    Skip,
}

/// What a write does to the state its connection sees.
enum Effect {
    /// Refused while the statement is prepared: no lock is taken.
    PrepareError,
    /// Refused while it runs, after it took the write lock.
    RunError,
    NoPage,
    SameValues,
    /// A DDL statement that does nothing.
    NoOpDdl,
    Page(State),
}

/// Versions are the states after each commit that wrote a page.
struct Model {
    versions: Vec<State>,
    /// The state a commit of same-value rewrites leaves, until the WAL file
    /// tells whether it wrote a page.
    pending: Option<State>,
    writer: Option<usize>,
    conns: Vec<Conn>,
}

impl Model {
    fn latest(&self) -> usize {
        self.versions.len() - 1
    }

    fn sees(&self, c: usize) -> &State {
        match &self.conns[c] {
            Conn::Txn {
                work: Some((state, _)),
                ..
            } => state,
            Conn::Txn {
                snapshot: Some(s), ..
            } => &self.versions[*s],
            _ => &self.versions[self.latest()],
        }
    }

    fn apply(&mut self, op: Op, names: &Names) -> Want {
        match op {
            Op::Begin(c, kind) => {
                if matches!(self.conns[c], Conn::Txn { .. }) {
                    // As in SQLite, BEGIN IMMEDIATE takes the write lock before it
                    // finds the open transaction, and the transaction keeps the lock.
                    return match kind {
                        Begin::Deferred => Want::Refused,
                        Begin::Immediate | Begin::Exclusive => self.write(c, |_| Effect::RunError),
                    };
                }
                match kind {
                    Begin::Deferred => {
                        self.conns[c] = Conn::Txn {
                            snapshot: None,
                            work: None,
                        }
                    }
                    Begin::Immediate | Begin::Exclusive => {
                        if self.writer.is_some() {
                            return Want::Busy;
                        }
                        self.writer = Some(c);
                        let latest = self.latest();
                        self.conns[c] = Conn::Txn {
                            snapshot: Some(latest),
                            work: Some((self.versions[latest].clone(), Wrote::Nothing)),
                        };
                    }
                }
                Want::Done
            }
            Op::Read(c, read) => {
                if !matches!(read, Read::Schema) && self.sees(c).table.is_none() {
                    // Refused while it is prepared: it reads nothing, so it takes no snapshot.
                    return Want::Refused;
                }
                let latest = self.latest();
                if let Conn::Txn { snapshot, .. } = &mut self.conns[c] {
                    snapshot.get_or_insert(latest);
                }
                let state = self.sees(c);
                if let Read::Schema = read {
                    let mut objects = Vec::new();
                    if state.table.is_some() {
                        objects.push(names.table.to_string());
                    }
                    if state.index {
                        objects.push(names.index.to_string());
                    }
                    objects.sort();
                    return Want::Rows(objects.into_iter().map(|o| vec![o]).collect());
                }
                let Some(table) = &state.table else {
                    return Want::Refused;
                };
                let row = |k: &u8, v: &String| vec![names.key_text(*k), v.clone()];
                Want::Rows(match read {
                    Read::Table => table.iter().map(|(k, v)| row(k, v)).collect(),
                    Read::ByValue => table
                        .iter()
                        .filter(|(_, v)| *v == "a")
                        .map(|(k, _)| vec![names.key_text(*k)])
                        .collect(),
                    Read::View => table
                        .iter()
                        .filter(|(_, v)| !(matches!(names.view, Some((_, true))) && *v == "x"))
                        .map(|(k, v)| row(k, v))
                        .collect(),
                    Read::Schema => unreachable!(),
                })
            }
            // The engine refuses these on a WITHOUT ROWID table while it prepares them.
            Op::Insert(c, _, _, Conflict::Ignore | Conflict::Replace)
            | Op::Update(c, ..)
            | Op::Delete(c, _)
            | Op::CreateIndex(c, _)
                if names.without_rowid =>
            {
                self.write(c, |_| Effect::PrepareError)
            }
            Op::Insert(c, k, v, conflict) => self.write(c, |s| {
                let Some(table) = &s.table else {
                    return Effect::PrepareError;
                };
                match (table.get(&k), conflict) {
                    (Some(_), Conflict::Abort) => Effect::RunError,
                    (Some(_), Conflict::Ignore) => Effect::NoPage,
                    (Some(old), Conflict::Replace) if old == v.text() => Effect::SameValues,
                    _ => Effect::Page(with_row(s, k, Some(v))),
                }
            }),
            Op::Update(c, k, v) => self.write(c, |s| match s.table.as_ref().map(|t| t.get(&k)) {
                None => Effect::PrepareError,
                Some(Some(old)) if old == v.text() => Effect::SameValues,
                Some(Some(_)) => Effect::Page(with_row(s, k, Some(v))),
                Some(None) => Effect::NoPage,
            }),
            Op::Delete(c, k) => self.write(c, |s| match &s.table {
                None => Effect::PrepareError,
                Some(t) if t.contains_key(&k) => Effect::Page(with_row(s, k, None)),
                Some(_) => Effect::NoPage,
            }),
            Op::CreateTable(c, if_not_exists) => {
                self.write(c, |s| match (&s.table, if_not_exists) {
                    (Some(_), true) => Effect::NoOpDdl,
                    (Some(_), false) => Effect::PrepareError,
                    (None, _) => Effect::Page(State {
                        table: Some(BTreeMap::new()),
                        index: s.index,
                    }),
                })
            }
            Op::CreateIndex(c, if_not_exists) => self.write(c, |s| match (&s.table, s.index) {
                (None, _) => Effect::PrepareError,
                (Some(_), true) if if_not_exists => Effect::NoOpDdl,
                (Some(_), true) => Effect::PrepareError,
                (Some(t), false) => Effect::Page(State {
                    table: Some(t.clone()),
                    index: true,
                }),
            }),
            Op::DropIndex(c, if_exists) => self.write(c, |s| match s.index {
                false if if_exists => Effect::NoOpDdl,
                false => Effect::PrepareError,
                true => Effect::Page(State {
                    table: s.table.clone(),
                    index: false,
                }),
            }),
            Op::Commit(c) | Op::Rollback(c) => {
                let Conn::Txn { work, .. } = std::mem::replace(&mut self.conns[c], Conn::Auto)
                else {
                    return Want::Refused;
                };
                if let Some((state, wrote)) = work {
                    self.writer = None;
                    match (op, wrote) {
                        (Op::Commit(_), Wrote::Page) => self.versions.push(state),
                        (Op::Commit(_), Wrote::SameValues) => self.pending = Some(state),
                        _ => {}
                    }
                }
                Want::Done
            }
            Op::Checkpoint(c, _) => match &self.conns[c] {
                Conn::Txn {
                    snapshot: None,
                    work: None,
                }
                | Conn::Auto => Want::Done,
                Conn::Txn { .. } => Want::Locked,
            },
            Op::Reopen => {
                self.writer = None;
                self.conns.iter_mut().for_each(|c| *c = Conn::Auto);
                Want::Done
            }
        }
    }

    /// A write statement: the write lock first, then the snapshot check, as
    /// SQLite takes them; in autocommit it commits at once.
    fn write(&mut self, c: usize, effect: impl FnOnce(&State) -> Effect) -> Want {
        let effect = effect(self.sees(c));
        match effect {
            Effect::PrepareError => return Want::Refused,
            Effect::NoOpDdl
                if matches!(self.conns[c], Conn::Txn { .. }) || self.writer.is_some() =>
            {
                return Want::Skip;
            }
            Effect::NoOpDdl => return Want::Done,
            _ => {}
        }
        let latest = self.latest();
        let auto = matches!(self.conns[c], Conn::Auto);
        if self.writer != Some(c) {
            if let Conn::Txn { snapshot, .. } = &mut self.conns[c] {
                // A write refused for the lock leaves a transaction that has not read without a snapshot.
                if self.writer.is_some() {
                    return Want::Busy;
                }
                if *snapshot.get_or_insert(latest) < latest {
                    return Want::BusySnapshot;
                }
            } else if self.writer.is_some() {
                return Want::Busy;
            }
            self.writer = Some(c);
            let work = Some((self.versions[latest].clone(), Wrote::Nothing));
            self.conns[c] = Conn::Txn {
                snapshot: Some(latest),
                work,
            };
        }
        let Conn::Txn {
            work: Some((state, wrote)),
            ..
        } = &mut self.conns[c]
        else {
            unreachable!("the writer holds its work");
        };
        let want = match effect {
            Effect::RunError => Want::Refused,
            Effect::NoPage => Want::Done,
            Effect::SameValues => {
                *wrote = (*wrote).max(Wrote::SameValues);
                Want::Done
            }
            Effect::Page(new) => {
                *state = new;
                *wrote = Wrote::Page;
                Want::Done
            }
            Effect::PrepareError | Effect::NoOpDdl => unreachable!(),
        };
        if auto {
            let commit = if want == Want::Refused {
                Op::Rollback(c)
            } else {
                Op::Commit(c)
            };
            let names = Origin::Fresh {
                text_key: false,
                without_rowid: false,
                small_pages: false,
            }
            .names();
            self.apply(commit, &names);
        }
        want
    }
}

fn with_row(s: &State, k: u8, v: Option<Val>) -> State {
    let mut table = s
        .table
        .clone()
        .expect("a write to a missing table is refused");
    match v {
        Some(v) => table.insert(k, v.text().to_string()),
        None => table.remove(&k),
    };
    State {
        table: Some(table),
        index: s.index,
    }
}

impl Op {
    fn sql(self, names: &Names) -> String {
        let (t, kc, vc) = (names.table, names.key, names.value);
        match self {
            Op::Begin(_, kind) => format!("BEGIN {kind:?}").to_uppercase(),
            Op::Read(_, Read::Table) => format!("SELECT {kc}, {vc} FROM {t} ORDER BY {kc}"),
            Op::Read(_, Read::ByValue) => {
                format!("SELECT {kc} FROM {t} WHERE {vc} = 'a' ORDER BY {kc}")
            }
            Op::Read(_, Read::View) => {
                let from = names.view.map_or(t, |(view, _)| view);
                format!("SELECT {kc}, {vc} FROM {from} ORDER BY {kc}")
            }
            Op::Read(_, Read::Schema) => format!(
                "SELECT name FROM sqlite_schema WHERE name IN ('{t}', '{}') ORDER BY name",
                names.index
            ),
            Op::Insert(_, k, v, conflict) => {
                let or = match conflict {
                    Conflict::Abort => "",
                    Conflict::Ignore => " OR IGNORE",
                    Conflict::Replace => " OR REPLACE",
                };
                format!(
                    "INSERT{or} INTO {t}({kc}, {vc}) VALUES ({}, '{}')",
                    names.key(k),
                    v.text()
                )
            }
            Op::Update(_, k, v) => {
                format!(
                    "UPDATE {t} SET {vc} = '{}' WHERE {kc} = {}",
                    v.text(),
                    names.key(k)
                )
            }
            Op::Delete(_, k) => format!("DELETE FROM {t} WHERE {kc} = {}", names.key(k)),
            Op::CreateTable(_, if_not_exists) => {
                let create = names.create_table();
                if if_not_exists {
                    create.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1)
                } else {
                    create
                }
            }
            Op::CreateIndex(_, if_not_exists) => format!(
                "CREATE INDEX {}{} ON {}",
                if if_not_exists { "IF NOT EXISTS " } else { "" },
                names.index,
                names.index_on
            ),
            Op::DropIndex(_, if_exists) => format!(
                "DROP INDEX {}{}",
                if if_exists { "IF EXISTS " } else { "" },
                names.index
            ),
            Op::Commit(_) => "COMMIT".to_string(),
            Op::Rollback(_) => "ROLLBACK".to_string(),
            Op::Checkpoint(_, mode) => mode.sql(),
            Op::Reopen => String::new(),
        }
    }

    fn conn(self) -> Option<usize> {
        match self {
            Op::Begin(c, _)
            | Op::Read(c, _)
            | Op::Insert(c, ..)
            | Op::Update(c, ..)
            | Op::Delete(c, _)
            | Op::CreateTable(c, _)
            | Op::CreateIndex(c, _)
            | Op::DropIndex(c, _)
            | Op::Commit(c)
            | Op::Rollback(c)
            | Op::Checkpoint(c, _) => Some(c),
            Op::Reopen => None,
        }
    }

    fn on(self, connections: usize) -> Op {
        let c = |c: usize| c % connections;
        match self {
            Op::Begin(x, b) => Op::Begin(c(x), b),
            Op::Read(x, r) => Op::Read(c(x), r),
            Op::Insert(x, k, v, o) => Op::Insert(c(x), k, v, o),
            Op::Update(x, k, v) => Op::Update(c(x), k, v),
            Op::Delete(x, k) => Op::Delete(c(x), k),
            Op::CreateTable(x, i) => Op::CreateTable(c(x), i),
            Op::CreateIndex(x, i) => Op::CreateIndex(c(x), i),
            Op::DropIndex(x, i) => Op::DropIndex(c(x), i),
            Op::Commit(x) => Op::Commit(c(x)),
            Op::Rollback(x) => Op::Rollback(c(x)),
            Op::Checkpoint(x, m) => Op::Checkpoint(c(x), m),
            Op::Reopen => Op::Reopen,
        }
    }
}

fn query(conn: &Arc<Connection>, sql: &str) -> Result<Vec<Vec<String>>, LimboError> {
    let rows = conn.prepare(sql)?.run_collect_rows()?;
    Ok(rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect())
}

fn open(path: &Path, config: &Config) -> Arc<Database> {
    let io: Arc<dyn turso_core::IO> = Arc::new(PlatformIO::new().unwrap());
    let opts = DatabaseOpts::new()
        .with_views(true)
        .with_custom_types(true)
        .with_without_rowid(config.origin.names().without_rowid)
        .with_multiprocess_wal(config.multiprocess_wal);
    Database::open_file_with_flags(
        io,
        path.to_str().unwrap(),
        OpenFlags::default(),
        opts,
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap()
}

fn fixture_path(fixture: Fixture) -> PathBuf {
    let name = match fixture {
        Fixture::Text => "text",
        Fixture::Integer => "integer",
        Fixture::ArrayViewIndex => "array_view_index",
    };
    PathBuf::from(format!(
        "{}/integration/query_processing/test_ivm_key_seek_fixture_{name}.db",
        env!("CARGO_MANIFEST_DIR")
    ))
}

/// The rows the `.sql` sibling of each fixture inserts.
fn fixture_rows(fixture: Fixture) -> BTreeMap<u8, String> {
    match fixture {
        Fixture::Text | Fixture::Integer => (0..6).map(|k| (k, "a".to_string())).collect(),
        Fixture::ArrayViewIndex => (1..=8).map(|k| (k, format!("x{k}"))).collect(),
    }
}

/// The WAL file past its 32-byte header: it changes iff a commit appends or
/// rewrites frames.
fn wal_frames(path: &Path) -> Vec<u8> {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    match std::fs::read(&wal) {
        Ok(bytes) => bytes.get(32..).unwrap_or_default().to_vec(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("read {wal:?}: {e}"),
    }
}

fn exec(conn: &Arc<Connection>, sql: &str) -> Result<(), String> {
    query(conn, sql)
        .map(|_| ())
        .map_err(|e| format!("setup `{sql}`: {e}"))
}

/// Writes the file of `config` and returns the state it holds.
fn write_file(config: &Config, path: &Path) -> Result<State, String> {
    let names = config.origin.names();
    let rows: BTreeMap<u8, Val> = config.rows.iter().copied().collect();
    let stored = || {
        rows.iter()
            .map(|(k, v)| (*k, v.text().to_string()))
            .collect()
    };
    let insert = |k: u8, v: Val| {
        format!(
            "INSERT INTO {}({}, {}) VALUES ({}, '{}')",
            names.table,
            names.key,
            names.value,
            names.key(k),
            v.text()
        )
    };
    match config.origin {
        Origin::Fresh { small_pages, .. } => {
            if small_pages {
                let db = open(path, config);
                exec(
                    &db.connect(ConnectOpts::new()).unwrap(),
                    "PRAGMA page_size = 512",
                )?;
            }
            Ok(State {
                table: None,
                index: false,
            })
        }
        Origin::Built {
            small_pages,
            index,
            view,
            ..
        } => {
            assert!(
                !(names.without_rowid && (index || view)),
                "a WITHOUT ROWID table takes no index and no matview"
            );
            let db = open(path, config);
            let conn = db.connect(ConnectOpts::new()).unwrap();
            if small_pages {
                exec(&conn, "PRAGMA page_size = 512")?;
            }
            exec(&conn, &names.create_table())?;
            for (k, v) in &rows {
                exec(&conn, &insert(*k, *v))?;
            }
            if index {
                exec(
                    &conn,
                    &format!("CREATE INDEX {} ON {}", names.index, names.index_on),
                )?;
            }
            if view {
                exec(
                    &conn,
                    "CREATE MATERIALIZED VIEW tm AS SELECT k, v FROM t WHERE v IS NOT 'x'",
                )?;
            }
            Ok(State {
                table: Some(stored()),
                index,
            })
        }
        Origin::Sqlite { index, .. } => {
            let conn = rusqlite::Connection::open(path).map_err(|e| e.to_string())?;
            conn.pragma_update(None, "journal_mode", "wal")
                .map_err(|e| e.to_string())?;
            let mut sql = vec![names.create_table()];
            sql.extend(rows.iter().map(|(k, v)| insert(*k, *v)));
            if index {
                sql.push(format!(
                    "CREATE INDEX {} ON {}",
                    names.index, names.index_on
                ));
            }
            for s in &sql {
                conn.execute(s, ())
                    .map_err(|e| format!("sqlite `{s}`: {e}"))?;
            }
            Ok(State {
                table: Some(stored()),
                index,
            })
        }
        Origin::Fixture(fixture) => {
            std::fs::copy(fixture_path(fixture), path).map_err(|e| e.to_string())?;
            Ok(State {
                table: Some(fixture_rows(fixture)),
                index: true,
            })
        }
    }
}

/// Every op's outcome and every read equal the model; at the end a new
/// connection reads the last committed state and the file passes integrity_check.
fn connections_follow_the_wal_model(config: &Config, ops: &[Op]) -> Result<(), String> {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("shared.db");
    let names = config.origin.names();
    let initial = write_file(config, &path)?;
    let mut db = open(&path, config);
    if let Some(mode) = config.checkpoint {
        exec(&db.connect(ConnectOpts::new()).unwrap(), &mode.sql())?;
    }
    if config.reopen {
        drop(db);
        db = open(&path, config);
    }
    let connect = |db: &Arc<Database>| -> Vec<Arc<Connection>> {
        (0..config.connections)
            .map(|_| db.connect(ConnectOpts::new()).unwrap())
            .collect()
    };
    let mut conns = connect(&db);
    let mut model = Model {
        versions: vec![initial],
        pending: None,
        writer: None,
        conns: vec![Conn::Auto; config.connections],
    };
    for (step, op) in ops.iter().enumerate() {
        let op = op.on(config.connections);
        let want = model.apply(op, &names);
        if want == Want::Skip {
            continue;
        }
        let Some(c) = op.conn() else {
            conns.clear();
            drop(db);
            db = open(&path, config);
            conns = connect(&db);
            continue;
        };
        let sql = op.sql(&names);
        let frames = wal_frames(&path);
        let got = query(&conns[c], &sql);
        if let Some(state) = model.pending.take() {
            if wal_frames(&path) != frames {
                model.versions.push(state);
            }
        }
        let ok = match (&want, &got) {
            (Want::Rows(rows), Ok(got)) => rows == got,
            (Want::Done, Ok(_)) => true,
            (Want::Busy, Err(LimboError::Busy)) => true,
            (Want::BusySnapshot, Err(LimboError::BusySnapshot)) => true,
            (Want::Locked, Err(LimboError::TableLocked)) => true,
            (Want::Refused, Err(e)) => !matches!(
                e,
                LimboError::Busy | LimboError::BusySnapshot | LimboError::TableLocked
            ),
            _ => false,
        };
        if !ok {
            return Err(format!(
                "step {step} {op:?} on conn {c}: `{sql}` gave {got:?}, expected {want:?}"
            ));
        }
    }
    let last = model.versions.last().unwrap();
    let fresh = db.connect(ConnectOpts::new()).unwrap();
    let schema =
        query(&fresh, &Op::Read(0, Read::Schema).sql(&names)).map_err(|e| e.to_string())?;
    let mut end = Model {
        versions: vec![last.clone()],
        pending: None,
        writer: None,
        conns: vec![Conn::Auto],
    };
    let want_schema = end.apply(Op::Read(0, Read::Schema), &names);
    if Want::Rows(schema.clone()) != want_schema {
        return Err(format!(
            "at the end: schema {schema:?}, expected {want_schema:?}"
        ));
    }
    if last.table.is_some() {
        let rows =
            query(&fresh, &Op::Read(0, Read::Table).sql(&names)).map_err(|e| e.to_string())?;
        let want = end.apply(Op::Read(0, Read::Table), &names);
        if Want::Rows(rows.clone()) != want {
            return Err(format!("at the end: rows {rows:?}, expected {want:?}"));
        }
    }
    let integrity = query(&fresh, "PRAGMA integrity_check").map_err(|e| e.to_string())?;
    if integrity != vec![vec!["ok".to_string()]] {
        return Err(format!("at the end: integrity_check {integrity:?}"));
    }
    Ok(())
}

fn mode() -> impl Strategy<Value = Mode> {
    prop_oneof![
        Just(Mode::Passive),
        Just(Mode::Full),
        Just(Mode::Restart),
        Just(Mode::Truncate),
    ]
}

fn val() -> impl Strategy<Value = Val> {
    prop_oneof![Just(Val::A), Just(Val::B), Just(Val::X)]
}

fn origin() -> impl Strategy<Value = Origin> {
    prop_oneof![
        2 => (any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
            |(text_key, without_rowid, small_pages)| Origin::Fresh {
                text_key,
                without_rowid,
                small_pages,
            }
        ),
        4 => (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
            |(text_key, small_pages, index, view)| Origin::Built {
                text_key,
                without_rowid: false,
                small_pages,
                index,
                view,
            }
        ),
        1 => (any::<bool>(), any::<bool>()).prop_map(|(text_key, small_pages)| Origin::Built {
            text_key,
            without_rowid: true,
            small_pages,
            index: false,
            view: false,
        }),
        1 => (any::<bool>(), any::<bool>())
            .prop_map(|(text_key, index)| Origin::Sqlite { text_key, index }),
        2 => prop_oneof![
            Just(Fixture::Text),
            Just(Fixture::Integer),
            Just(Fixture::ArrayViewIndex),
        ]
        .prop_map(Origin::Fixture),
    ]
}

fn config() -> impl Strategy<Value = Config> {
    (
        origin(),
        2usize..=3,
        any::<bool>(),
        prop::collection::vec((0u8..8, val()), 0..5),
        prop::option::of(mode()),
        any::<bool>(),
    )
        .prop_map(
            |(origin, connections, multiprocess_wal, rows, checkpoint, reopen)| Config {
                origin,
                connections,
                multiprocess_wal,
                rows,
                checkpoint,
                reopen,
            },
        )
}

fn op() -> impl Strategy<Value = Op> {
    let c = 0usize..3;
    let key = 0u8..10;
    let begin = prop_oneof![
        3 => Just(Begin::Deferred),
        1 => Just(Begin::Immediate),
        1 => Just(Begin::Exclusive),
    ];
    let read = prop_oneof![
        Just(Read::Table),
        Just(Read::ByValue),
        Just(Read::View),
        Just(Read::Schema),
    ];
    let conflict = prop_oneof![
        Just(Conflict::Abort),
        Just(Conflict::Ignore),
        Just(Conflict::Replace),
    ];
    prop_oneof![
        4 => (c.clone(), begin).prop_map(|(c, b)| Op::Begin(c, b)),
        4 => (c.clone(), read).prop_map(|(c, r)| Op::Read(c, r)),
        3 => (c.clone(), key.clone(), val(), conflict).prop_map(|(c, k, v, o)| Op::Insert(c, k, v, o)),
        3 => (c.clone(), key.clone(), val()).prop_map(|(c, k, v)| Op::Update(c, k, v)),
        2 => (c.clone(), key).prop_map(|(c, k)| Op::Delete(c, k)),
        1 => (c.clone(), any::<bool>()).prop_map(|(c, i)| Op::CreateTable(c, i)),
        1 => (c.clone(), any::<bool>()).prop_map(|(c, i)| Op::CreateIndex(c, i)),
        1 => (c.clone(), any::<bool>()).prop_map(|(c, i)| Op::DropIndex(c, i)),
        3 => c.clone().prop_map(Op::Commit),
        1 => c.clone().prop_map(Op::Rollback),
        2 => (c, mode()).prop_map(|(c, m)| Op::Checkpoint(c, m)),
        1 => Just(Op::Reopen),
    ]
}

fn built(rows: Vec<(u8, Val)>, checkpoint: Option<Mode>) -> Config {
    Config {
        origin: Origin::Built {
            text_key: false,
            without_rowid: false,
            small_pages: false,
            index: false,
            view: false,
        },
        connections: 2,
        multiprocess_wal: false,
        rows,
        checkpoint,
        reopen: false,
    }
}

fn on(origin: Origin, checkpoint: Option<Mode>) -> Config {
    Config {
        origin,
        ..built(Vec::new(), checkpoint)
    }
}

#[test]
fn a_write_that_writes_no_page_leaves_other_transactions_able_to_write() {
    use Op::*;
    let row = vec![(0, Val::A)];
    let fixed: Vec<(Config, Vec<Op>)> = vec![
        // A WAL truncated by a checkpoint has no header until a page is written.
        (
            built(row.clone(), Some(Mode::Truncate)),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Update(1, 5, Val::B),
                Insert(0, 6, Val::A, Conflict::Abort),
                Commit(0),
            ],
        ),
        (
            on(
                Origin::Fresh {
                    text_key: false,
                    without_rowid: false,
                    small_pages: false,
                },
                None,
            ),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Schema),
                Begin(1, self::Begin::Immediate),
                Commit(1),
                CreateTable(0, false),
                Commit(0),
            ],
        ),
        (
            on(Origin::Fixture(Fixture::Text), None),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Update(1, 9, Val::A),
                DropIndex(0, false),
            ],
        ),
        (
            on(Origin::Fixture(Fixture::ArrayViewIndex), None),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::View),
                Delete(1, 0),
                Insert(1, 1, Val::A, Conflict::Ignore),
                Update(0, 1, Val::A),
            ],
        ),
        (
            Config {
                origin: Origin::Sqlite {
                    text_key: true,
                    index: true,
                },
                ..built(row.clone(), None)
            },
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::ByValue),
                Insert(1, 0, Val::B, Conflict::Abort),
                Update(0, 0, Val::X),
            ],
        ),
        // A checkpoint between the read and the write that writes no page.
        (
            built(row.clone(), None),
            vec![
                Insert(1, 1, Val::B, Conflict::Abort),
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Checkpoint(1, Mode::Truncate),
                Begin(1, self::Begin::Exclusive),
                Delete(1, 7),
                Commit(1),
                Update(0, 0, Val::B),
                Commit(0),
            ],
        ),
        // A transaction that has not read yet, on a WAL fully copied back.
        (
            built(row.clone(), Some(Mode::Passive)),
            vec![
                Begin(0, self::Begin::Deferred),
                Update(1, 3, Val::B),
                Read(0, self::Read::Table),
                Insert(0, 3, Val::A, Conflict::Abort),
            ],
        ),
        // A commit that changes a row makes the older snapshot stale.
        (
            built(row.clone(), Some(Mode::Truncate)),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Update(1, 0, Val::B),
                Update(0, 0, Val::X),
                Rollback(0),
            ],
        ),
        (
            built(row.clone(), None),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Insert(1, 0, Val::B, Conflict::Replace),
                Delete(0, 0),
            ],
        ),
        (
            built(Vec::new(), None),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Insert(1, 2, Val::A, Conflict::Replace),
                CreateIndex(0, true),
            ],
        ),
        // Rewrites of a row with the value it holds: the WAL file tells whether a page was written.
        (
            built(row.clone(), None),
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Update(1, 0, Val::A),
                Delete(0, 0),
            ],
        ),
        (
            Config {
                origin: Origin::Built {
                    text_key: true,
                    without_rowid: false,
                    small_pages: false,
                    index: true,
                    view: true,
                },
                ..built(row.clone(), Some(Mode::Truncate))
            },
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::View),
                Insert(1, 0, Val::A, Conflict::Replace),
                Update(0, 0, Val::B),
            ],
        ),
        (
            Config {
                origin: Origin::Built {
                    text_key: true,
                    without_rowid: true,
                    small_pages: false,
                    index: false,
                    view: false,
                },
                ..built(row, Some(Mode::Truncate))
            },
            vec![
                Begin(0, self::Begin::Deferred),
                Read(0, self::Read::Table),
                Insert(1, 0, Val::B, Conflict::Ignore),
                Insert(1, 0, Val::B, Conflict::Abort),
                Update(1, 0, Val::B),
                Insert(0, 1, Val::A, Conflict::Abort),
            ],
        ),
    ];
    let failures: Vec<String> = fixed
        .iter()
        .enumerate()
        .filter_map(|(i, (config, ops))| {
            let e = connections_follow_the_wal_model(config, ops).err()?;
            Some(format!("fixed case {i}: {config:?} {ops:?}: {e}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    TestRunner::new(RunnerConfig {
        cases: std::env::var("PROPTEST_CASES").map_or(64, |n| n.parse().unwrap()),
        failure_persistence: None,
        ..RunnerConfig::default()
    })
    .run(
        &(config(), prop::collection::vec(op(), 0..16)),
        |(config, ops)| {
            connections_follow_the_wal_model(&config, &ops).map_err(TestCaseError::fail)
        },
    )
    .unwrap();
}
