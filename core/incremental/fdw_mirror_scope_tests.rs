//! Scoped `REFRESH`: the sweep when the scan speaks for only part of the source.
//!
//! Test-only. The code under test is [`crate::incremental::fdw_mirror`]; these
//! cases live apart from it so the fix and its pins are separable.
//!
//! Absence of a row from a scan means "deleted" only within the scan's scope.
//! An attribute-scoped scan (`modified > w`) cannot witness a deletion at all —
//! a deleted row has no attributes — so a sweep driven by one must retract
//! inside its scope and leave everything else alone.

use crate::incremental::fdw_mirror::{mirror_table_name, MirrorSpec, MirrorSync, ScanQuery};
use crate::schema::{ColDef, Column, Type};
use turso_parser::ast::{self, RefreshScope};
use turso_parser::parser::Parser;

fn col(name: &str, ty: &str) -> Column {
    Column::new(
        Some(name.to_string()),
        ty.to_string(),
        None,
        None,
        Type::Text,
        None,
        ColDef::default(),
    )
}

const MIRROR: &str = "__turso_internal_fdw_mirror_v1_mv__cc_message_fdw";
const SCAN: &str = "SELECT * FROM cc_message_fdw WHERE session_id = 's1'";

fn sync(identity: Vec<u32>) -> MirrorSync {
    let spec = MirrorSpec {
        source_table: "cc_message_fdw".to_string(),
        mirror_table: mirror_table_name("mv", "cc_message_fdw"),
        columns: crate::alloc::vec![
            col("uuid", "TEXT"),
            col("session_id", "TEXT"),
            col("body", "TEXT")
        ],
        identity,
    };
    MirrorSync::new(
        &spec,
        ScanQuery::new(
            "SELECT * FROM cc_message_fdw".to_string(),
            Some("session_id = 's1'".to_string()),
        ),
    )
}

/// The scope a `REFRESH … WHERE <predicate>` carries, taken from the parser so
/// the surface syntax and the typed value are pinned together.
fn scope(predicate: &str) -> RefreshScope {
    let sql = format!("REFRESH MATERIALIZED VIEW mv WHERE {predicate}");
    let mut parser = Parser::new(sql.as_bytes());
    match parser.next_cmd() {
        Ok(Some(ast::Cmd::Stmt(ast::Stmt::RefreshMaterializedView { scope, .. }))) => scope,
        other => panic!("REFRESH with a scope must parse as one: {other:?}"),
    }
}

/// `REFRESH` without a `WHERE` is `Full` by construction, not an empty scope.
#[test]
fn a_refresh_without_a_where_carries_no_scope_at_all() {
    let mut parser = Parser::new(b"REFRESH MATERIALIZED VIEW mv");
    let ast::Cmd::Stmt(ast::Stmt::RefreshMaterializedView { scope, .. }) =
        parser.next_cmd().unwrap().unwrap()
    else {
        panic!("REFRESH must parse as a refresh");
    };
    assert!(
        matches!(scope, RefreshScope::Full),
        "'no scope' must be a distinct value, never an empty predicate"
    );
}

/// The unscoped sweep, pinned to the byte. A scope must be a different bound on
/// the same statements, not a rewrite of them.
#[test]
fn a_full_scope_sweep_is_byte_identical_to_the_sweep_that_had_no_scope() {
    let sql = sync(crate::alloc::vec![0]).sweep_sql(&RefreshScope::Full);
    assert_eq!(
        sql,
        crate::alloc::vec![
            format!(
                "SELECT CASE WHEN any_null > 0 THEN 'null' ELSE 'duplicate' END \
                 FROM (SELECT max(identity_is_null) AS any_null, max(identity_rows) AS max_rows \
                 FROM (SELECT (uuid IS NULL) AS identity_is_null, count(*) AS identity_rows \
                 FROM ({SCAN}) GROUP BY uuid)) WHERE any_null > 0 OR max_rows > 1"
            ),
            format!(
                "INSERT INTO {MIRROR} SELECT * FROM ({SCAN}) WHERE true \
                 ON CONFLICT(uuid) DO UPDATE SET session_id = excluded.session_id, \
                 body = excluded.body WHERE {MIRROR}.session_id IS NOT excluded.session_id \
                 OR {MIRROR}.body IS NOT excluded.body"
            ),
            format!("DELETE FROM {MIRROR} WHERE (uuid) NOT IN (SELECT uuid FROM ({SCAN}))"),
        ]
    );
}

/// The whole of the scope's effect on the retraction: a mirror row the scan did
/// not cover is not a row the scan says anything about.
#[test]
fn a_scoped_sweep_retracts_only_mirror_rows_inside_the_scope() {
    let sql = sync(crate::alloc::vec![0]).sweep_sql(&scope("body > 'w'"));
    let delete = sql.last().unwrap();
    assert!(
        delete.starts_with(&format!(
            "DELETE FROM {MIRROR} WHERE (body > 'w') AND (uuid) NOT IN ("
        )),
        "the retraction must be bounded by the scope: {delete}"
    );
}

/// The scope narrows the source's own query block, beside the view's own
/// predicate. One block further out the driver would never see it and would be
/// asked for the whole scan (`test_the_scope_is_pushed_down_to_the_driver`).
#[test]
fn a_scoped_sweep_reads_the_scope_pushed_into_the_scan() {
    let sql = sync(crate::alloc::vec![0]).sweep_sql(&scope("body > 'w'"));
    for statement in &sql {
        assert!(
            statement.contains(
                "SELECT * FROM cc_message_fdw WHERE (session_id = 's1') AND (body > 'w')"
            ),
            "every statement reads the scoped scan: {statement}"
        );
        assert!(
            !statement.contains(&format!("({SCAN})")),
            "the unscoped scan must not survive as an inner query block: {statement}"
        );
    }
}

/// Invariant 3: push and sweep share the upsert, and a scope must not touch it.
#[test]
fn a_scope_leaves_the_upsert_tail_byte_identical() {
    let sync = sync(crate::alloc::vec![0]);
    let tail = |sql: &str| {
        sql.split_once("ON CONFLICT")
            .expect("the upsert carries a conflict tail")
            .1
            .to_string()
    };
    let full = sync.sweep_sql(&RefreshScope::Full);
    let scoped = sync.sweep_sql(&scope("body > 'w'"));
    assert_eq!(tail(&full[full.len() - 2]), tail(&scoped[scoped.len() - 2]));
    assert_eq!(
        tail(&sync.push_upsert_sql()),
        tail(&scoped[scoped.len() - 2]),
        "a scoped sweep must land on the row a push would land on"
    );
}

/// The guard validates the scan it is given, so a scoped sweep checks the
/// identity contract over exactly the rows it is about to write.
#[test]
fn a_scoped_sweep_guards_the_scan_it_actually_reads() {
    let sql = sync(crate::alloc::vec![0]).sweep_sql(&scope("body > 'w'"));
    assert_eq!(sql.len(), 3, "{sql:?}");
    assert!(
        sql[0].starts_with("SELECT CASE WHEN any_null"),
        "{}",
        sql[0]
    );
    assert_eq!(
        sql[0].matches("FROM cc_message_fdw").count(),
        1,
        "naming the scan twice would scan it twice: {}",
        sql[0]
    );
}

fn rejection(predicate: &str) -> String {
    sync(crate::alloc::vec![0])
        .validate_scope(&scope(predicate))
        .expect_err("scope must be refused")
        .to_string()
}

/// The retraction bound is evaluated against the mirror, so a scope naming
/// anything else is not a bound the engine can honour.
#[test]
fn a_scope_over_a_column_the_mirror_does_not_have_is_refused() {
    let message = rejection("modified > 5");
    assert!(
        message.contains("modified") && message.contains("cc_message_fdw"),
        "the refusal must name the column and the source: {message}"
    );
}

/// The scope is applied to the mirror in one statement and to the scan in
/// another; only a bare column name means the same thing in both.
#[test]
fn a_qualified_scope_column_is_refused() {
    let message = rejection("cc_message_fdw.body > 'w'");
    assert!(message.contains("unqualified"), "{message}");
}

/// The sweep prepares its own statements with nothing bound to them.
#[test]
fn a_parameterised_scope_is_refused() {
    let message = rejection("body > ?");
    assert!(message.contains("parameter"), "{message}");
}

/// A subquery could name any table at all, which is not a bound over the
/// mirror's own rows.
#[test]
fn a_scope_containing_a_subquery_is_refused() {
    let message = rejection("body IN (SELECT body FROM other)");
    assert!(message.contains("subquer"), "{message}");
}

/// The scope is evaluated twice — once by the scan against the source, once by
/// the retraction bound against the mirror — and nothing makes the two
/// evaluations agree except the predicate being a function of the row alone.
/// A scope that answers differently each time it runs makes the bound unsound:
/// the sweep would retract rows no scan spoke for, or spare rows one did.
#[test]
fn a_non_deterministic_scope_is_refused() {
    for predicate in [
        "random() > 0",
        "body > randomblob(4)",
        "session_id = datetime('now')",
        "body > date()",
        "changes() > 0",
        "upper(body) = upper(sqlite_version())",
        "body < CURRENT_TIMESTAMP",
    ] {
        let message = rejection(predicate);
        assert!(
            message.contains("evaluate the scope separately"),
            "{predicate} must be refused as non-deterministic: {message}"
        );
    }
}

/// An aggregate has no value for a single row, which is all the bound and the
/// scan ever look at one of.
#[test]
fn an_aggregate_scope_is_refused() {
    let message = rejection("count(*) > 0");
    assert!(message.contains("aggregate"), "{message}");
}

/// A name the engine cannot resolve cannot be shown deterministic either, and
/// the scope is the wrong place to find that out late.
#[test]
fn a_scope_calling_an_unknown_function_is_refused() {
    let message = rejection("no_such_fn(body) = 1");
    assert!(
        message.contains("no_such_fn") && message.contains("resolve"),
        "{message}"
    );
}

#[test]
fn a_scope_over_mirror_columns_is_accepted() {
    let sync = sync(crate::alloc::vec![0]);
    for predicate in [
        "body > 'w'",
        "body IS NULL OR session_id = 's1'",
        "uuid < 'z'",
        // Deterministic calls stay usable: the refusal is of the result that
        // moves, not of function calls as such.
        "upper(body) = 'W'",
        "length(body) > 2",
        "date(session_id) = '2020-01-01'",
    ] {
        sync.validate_scope(&scope(predicate))
            .unwrap_or_else(|err| panic!("{predicate} must be a usable scope: {err}"));
    }
}
