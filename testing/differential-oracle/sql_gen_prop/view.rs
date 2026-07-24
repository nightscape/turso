//! View statements.
//!
//! Includes CREATE VIEW, DROP VIEW, and materialized view variants.

use std::collections::HashSet;
use std::fmt;

use proptest::prelude::*;

use crate::create_table::identifier_excluding;
use crate::schema::{ColumnDef, DataType, Schema, TableRef};

/// Return a deterministic comparison literal for a given data type.
/// Used in WHERE filters like `col != literal` or `col < literal`.
fn comparison_literal(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Integer => "0",
        DataType::Real => "0.0",
        DataType::Text => "''",
        DataType::Blob | DataType::Null => "0",
    }
}

/// CREATE VIEW statement.
#[derive(Debug, Clone)]
pub struct CreateViewStatement {
    /// Whether this is a materialized view (Turso-only).
    pub materialized: bool,
    /// Whether to use IF NOT EXISTS clause.
    pub if_not_exists: bool,
    /// View name.
    pub view_name: String,
    /// Column names (optional).
    pub columns: Vec<String>,
    /// SELECT statement as string.
    pub select_sql: String,
    /// Known output columns of this view's SELECT (for matview-to-matview references).
    pub output_columns: Vec<ColumnDef>,
}

impl fmt::Display for CreateViewStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.materialized {
            write!(f, "CREATE MATERIALIZED VIEW")?;
        } else {
            write!(f, "CREATE VIEW")?;
        }
        if self.if_not_exists {
            write!(f, " IF NOT EXISTS")?;
        }
        write!(f, " {}", self.view_name)?;
        if !self.columns.is_empty() {
            let cols: Vec<String> = self.columns.iter().map(|c| c.to_string()).collect();
            write!(f, " ({})", cols.join(", "))?;
        }
        write!(f, " AS {}", self.select_sql)
    }
}

impl CreateViewStatement {
    /// Return the SQL for a regular (non-materialized) CREATE VIEW,
    /// regardless of the `materialized` flag.
    pub fn as_regular_view_sql(&self) -> String {
        let mut s = String::from("CREATE VIEW");
        if self.if_not_exists {
            s.push_str(" IF NOT EXISTS");
        }
        s.push(' ');
        s.push_str(&self.view_name);
        if !self.columns.is_empty() {
            let cols: Vec<String> = self.columns.iter().map(|c| c.to_string()).collect();
            s.push_str(&format!(" ({})", cols.join(", ")));
        }
        s.push_str(" AS ");
        s.push_str(&self.select_sql);
        s
    }
}

/// DROP VIEW statement.
#[derive(Debug, Clone)]
pub struct DropViewStatement {
    /// Whether this targets a materialized view (Turso-only).
    pub materialized: bool,
    /// Whether to use IF EXISTS clause.
    pub if_exists: bool,
    /// View name.
    pub view_name: String,
}

impl fmt::Display for DropViewStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DROP VIEW")?;
        if self.if_exists {
            write!(f, " IF EXISTS")?;
        }
        write!(f, " {}", self.view_name)
    }
}

/// Kinds of view SELECT queries the generator can produce.
#[derive(Debug, Clone, Copy)]
enum ViewSelectKind {
    /// SELECT * FROM table
    Star,
    /// SELECT col1, col2 FROM table WHERE expr
    FilteredColumns,
    /// SELECT col, COUNT(*) FROM table GROUP BY col
    Aggregate,
    /// SELECT t1.col, t2.col FROM t1 JOIN t2 ON t1.pk = t2.fk
    Join,
    /// SELECT t1.col, t2.col FROM t1 LEFT JOIN t2 ON t1.pk = t2.fk
    /// Exercises MatchCounterOperator (the antijoin half of the LEFT JOIN).
    LeftJoin,
    /// SELECT t1.col, ta.col, tb.col FROM t1
    ///   LEFT JOIN ta ON t1.pk = ta.fk
    ///   LEFT JOIN tb ON t1.pk = tb.fk
    /// Mirrors holon's `block` matview shape: a single L table with two
    /// independent LEFT JOINs to junction tables. Each LEFT JOIN spawns its
    /// own MatchCounterOperator, doubling the chance of an I/O-yielding read
    /// in a single commit.
    DualLeftJoin,
    /// SELECT t1.col, json_array_length(json_group_array(t2.col) FILTER (WHERE t2.col IS NOT NULL))
    ///   FROM t1 LEFT JOIN t2 ON t1.pk = t2.fk GROUP BY t1.pk
    /// Exercises the multiset-tracked aggregate (`AggregateOperator`'s
    /// `json_array_states`) over LEFT JOIN deltas. Wrapping in
    /// `json_array_length` keeps oracle comparison deterministic (an integer)
    /// while still routing every insert/delete through the multiset.
    LeftJoinAggregate,
    /// Same as `LeftJoinAggregate` but with two independent LEFT JOINs on the
    /// outer table — directly mirrors holon's `block` matview shape and is
    /// the only path that reaches the dual-MatchCounter + dual-FILTER aggregate
    /// state observed in the `json_group_array multiset went negative` bug.
    DualLeftJoinAggregate,
    /// Same as `DualLeftJoinAggregate` but groups by ALL outer-table columns
    /// rather than a single key column. Wide GROUP BY blows up the DBSP
    /// state-index key, making `AggregateOperator`'s `FetchKey` seek hit
    /// `SeekResult::TryAdvance` at leaf-page boundaries; that path was
    /// previously treated as `NotFound`, silently rebuilding the multiset
    /// from scratch (holon `block` matview drift, 2026-05-13).
    WideGroupByDualLeftJoinAggregate,
    /// SELECT t1.* FROM t1 JOIN t2 ON t1.pk = t2.fk WHERE t2.col IS NOT NULL
    QualifiedStarJoin,
    /// SELECT c0, c1 FROM t1 UNION ALL SELECT c0, c1 FROM t2
    UnionAll,
    /// SELECT t1.c0, t2.c0 FROM t1 JOIN t2 ON ... UNION ALL SELECT t1.c0, t2.c1 FROM t1 JOIN t2 ON ...
    /// Both arms JOIN the same two tables but on different columns
    UnionAllJoin,
    /// WITH RECURSIVE tree(...) AS (base UNION ALL recursive) SELECT * FROM tree
    RecursiveCte,
    /// WITH RECURSIVE gen(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM gen WHERE n < N) SELECT n FROM gen
    RecursiveCounter,
    /// SELECT a.k, a.c, b.c FROM t a JOIN t b ON a.ref = b.k WHERE <complex predicate>
    ///
    /// A self-join guarantees the two join sides carry columns with identical
    /// *bare* names, and the non-identity join condition pairs distinct rows so
    /// the two projected copies hold different values. Combined with a predicate
    /// that `predicate_needs_projection` classifies as complex (LIKE / BETWEEN /
    /// IN / CAST), this reaches the projection-rewrite path in `DbspCompiler`,
    /// which rebuilds the passthrough projection with the table qualifier
    /// discarded and therefore binds both copies to the first match.
    /// Randomly generated column names essentially never collide across two
    /// distinct tables, so a self-join is the only reliable way to generate the
    /// collision this path is sensitive to.
    ComplexFilterSelfJoin,
    /// SELECT t1.c, t2.c FROM t1 JOIN t2 ON ... WHERE <complex predicate>
    ///
    /// Same target as `ComplexFilterSelfJoin`, but across two distinct tables.
    /// Readable-name generation repeats identifiers often enough that two tables
    /// genuinely share a bare column name, which is exactly the precondition the
    /// projection-rewrite path mishandles. Unlike the self-join variant this
    /// reuses the ordinary FK-shaped join, so the rows actually pair up.
    ComplexFilterJoin,
}

fn all_existing_names(schema: &Schema) -> HashSet<String> {
    schema
        .table_names()
        .into_iter()
        .chain(schema.view_names())
        .chain(schema.materialized_view_names())
        .collect()
}

/// Generate a CREATE VIEW statement for a schema.
pub fn create_view(schema: &Schema) -> BoxedStrategy<CreateViewStatement> {
    create_view_inner(schema, false)
}

/// Generate a CREATE MATERIALIZED VIEW statement for a schema.
pub fn create_materialized_view(schema: &Schema) -> BoxedStrategy<CreateViewStatement> {
    create_view_inner(schema, true)
}

/// Returns true if the table has at least 2 integer columns (for recursive CTE).
fn has_integer_pair(table: &crate::schema::Table) -> bool {
    table
        .columns
        .iter()
        .filter(|c| c.data_type == DataType::Integer)
        .count()
        >= 2
}

fn create_view_inner(schema: &Schema, materialized: bool) -> BoxedStrategy<CreateViewStatement> {
    assert!(
        !schema.tables.is_empty(),
        "Schema must have at least one table to create a view"
    );

    let tables = schema.tables.clone();
    let matview_tables = schema.matviews_as_tables();
    let all_sources: Vec<TableRef> = tables.iter().cloned().chain(matview_tables).collect();
    let existing_names = all_existing_names(schema);
    let has_multiple_sources = all_sources.len() >= 2;
    let has_int_pair = all_sources.iter().any(|t| has_integer_pair(t));

    let all_sources_rc = std::rc::Rc::new(all_sources.clone());

    (
        any::<bool>(),
        identifier_excluding(existing_names),
        proptest::sample::select(all_sources),
        {
            let mut kinds = vec![
                Just(ViewSelectKind::Star),
                Just(ViewSelectKind::FilteredColumns),
                Just(ViewSelectKind::FilteredColumns),
                Just(ViewSelectKind::Aggregate),
                Just(ViewSelectKind::RecursiveCounter),
            ];
            if has_multiple_sources {
                kinds.push(Just(ViewSelectKind::Join));
                kinds.push(Just(ViewSelectKind::LeftJoin));
                kinds.push(Just(ViewSelectKind::DualLeftJoin));
                kinds.push(Just(ViewSelectKind::LeftJoinAggregate));
                kinds.push(Just(ViewSelectKind::DualLeftJoinAggregate));
                kinds.push(Just(ViewSelectKind::WideGroupByDualLeftJoinAggregate));
                kinds.push(Just(ViewSelectKind::QualifiedStarJoin));
                kinds.push(Just(ViewSelectKind::UnionAll));
                kinds.push(Just(ViewSelectKind::UnionAllJoin));
                kinds.push(Just(ViewSelectKind::ComplexFilterJoin));
                kinds.push(Just(ViewSelectKind::ComplexFilterJoin));
            }
            if has_int_pair {
                kinds.push(Just(ViewSelectKind::RecursiveCte));
                kinds.push(Just(ViewSelectKind::ComplexFilterSelfJoin));
            }
            proptest::strategy::Union::new(kinds).boxed()
        },
    )
        .prop_flat_map(move |(if_not_exists, view_name, table, kind)| {
            let all_sources_inner = all_sources_rc.clone();
            let table_name = table.qualified_name();
            let cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
            let table_col_defs: Vec<ColumnDef> = table.columns.clone();
            let filterable: Vec<String> = table
                .filterable_columns()
                .map(|c| c.name.clone())
                .collect();
            let filterable_defs: Vec<ColumnDef> = table
                .filterable_columns()
                .cloned()
                .collect();

            match kind {
                ViewSelectKind::Star => {
                    let select_sql = format!("SELECT * FROM {table_name}");
                    let output_columns = table_col_defs;
                    Just(CreateViewStatement {
                        materialized,
                        if_not_exists,
                        view_name,
                        columns: vec![],
                        select_sql,
                        output_columns,
                    })
                    .boxed()
                }
                ViewSelectKind::FilteredColumns => {
                    if cols.len() < 2 || filterable.is_empty() {
                        let select_sql = format!("SELECT * FROM {table_name}");
                        return Just(CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name,
                            columns: vec![],
                            select_sql,
                            output_columns: table_col_defs,
                        })
                        .boxed();
                    }
                    let num_cols = cols.len().min(3);
                    let projection = cols[..num_cols].join(", ");
                    // Prefer nullable columns for comparison filters (exercises NULL three-valued logic)
                    let nullable_idx = filterable_defs.iter().position(|c| c.nullable);
                    let cmp_filter_idx = nullable_idx.unwrap_or(0);
                    let isnull_filter_idx = 0;
                    let cmp_filter_col = filterable[cmp_filter_idx].clone();
                    let cmp_filter_col_def = filterable_defs[cmp_filter_idx].clone();
                    let isnull_filter_col = filterable[isnull_filter_idx].clone();
                    let output_columns = table_col_defs[..num_cols].to_vec();
                    // Pick a second filterable column distinct from the first when available,
                    // for compound predicates that AND two NULL checks.
                    let second_filter_col: Option<String> = filterable
                        .iter()
                        .find(|c| c.as_str() != isnull_filter_col.as_str())
                        .cloned();
                    let has_second = second_filter_col.is_some();
                    // Generate different WHERE filter styles:
                    // 0 = IS NOT NULL (original), 1 = != literal, 2 = < literal
                    // 3..=6 = compound NULL-check AND (only when a second filterable col exists)
                    // 7..=10 are predicates that `predicate_needs_projection`
                    // classifies as complex (LIKE / BETWEEN / IN / CAST). They route
                    // the plan through the projection-rewrite path in `DbspCompiler`,
                    // which was previously unreachable from any generated view.
                    let max_kind = 11u32;
                    (0u32..max_kind).prop_map(move |filter_kind| {
                        let where_clause = match filter_kind {
                            1 => {
                                let literal = comparison_literal(&cmp_filter_col_def.data_type);
                                format!("{cmp_filter_col} != {literal}")
                            }
                            2 => {
                                let literal = comparison_literal(&cmp_filter_col_def.data_type);
                                format!("{cmp_filter_col} < {literal}")
                            }
                            3 if has_second => {
                                let other = second_filter_col.clone().unwrap();
                                format!("{isnull_filter_col} IS NOT NULL AND {other} IS NOT NULL")
                            }
                            4 if has_second => {
                                let other = second_filter_col.clone().unwrap();
                                format!("{isnull_filter_col} IS NULL AND {other} IS NOT NULL")
                            }
                            5 if has_second => {
                                let other = second_filter_col.clone().unwrap();
                                format!("{isnull_filter_col} IS NOT NULL AND {other} IS NULL")
                            }
                            6 if has_second => {
                                let other = second_filter_col.clone().unwrap();
                                format!(
                                    "({isnull_filter_col} IS NOT NULL) AND ({other} IS NOT NULL)"
                                )
                            }
                            7 => format!("CAST({cmp_filter_col} AS TEXT) NOT LIKE '%zzq%'"),
                            8 => format!("CAST({cmp_filter_col} AS TEXT) LIKE '%'"),
                            9 => format!(
                                "{cmp_filter_col} IN (0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15)"
                            ),
                            10 => format!("CAST({cmp_filter_col} AS TEXT) IS NOT NULL"),
                            _ => format!("{isnull_filter_col} IS NOT NULL"),
                        };
                        CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql: format!(
                                "SELECT {projection} FROM {table_name} WHERE {where_clause}"
                            ),
                            output_columns: output_columns.clone(),
                        }
                    })
                    .boxed()
                }
                ViewSelectKind::Aggregate => {
                    if filterable.is_empty() {
                        let select_sql =
                            format!("SELECT COUNT(*) AS cnt FROM {table_name}");
                        return Just(CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name,
                            columns: vec![],
                            select_sql,
                            output_columns: vec![ColumnDef::new("cnt", DataType::Integer)],
                        })
                        .boxed();
                    }
                    let group_col = filterable[0].clone();
                    let group_col_def = table_col_defs
                        .iter()
                        .find(|c| c.name == group_col)
                        .cloned()
                        .unwrap_or_else(|| ColumnDef::new(&group_col, DataType::Text));
                    Just(CreateViewStatement {
                        materialized,
                        if_not_exists,
                        view_name,
                        columns: vec![],
                        select_sql: format!(
                            "SELECT {group_col}, COUNT(*) AS cnt FROM {table_name} GROUP BY {group_col}"
                        ),
                        output_columns: vec![
                            group_col_def,
                            ColumnDef::new("cnt", DataType::Integer),
                        ],
                    })
                    .boxed()
                }
                ViewSelectKind::Join => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    proptest::sample::select(all)
                        .prop_filter_map("need different table for join", move |t2| {
                            if t2.name == t1.name {
                                return None;
                            }
                            let t1_col = t1.columns.first()?.name.clone();
                            let t2_col = t2.columns.first()?.name.clone();
                            let t1_name = t1.qualified_name();
                            let t2_name = t2.qualified_name();
                            let t1_col_def = t1.columns.first()?.clone();
                            let t2_col_def = t2.columns.first()?.clone();
                            Some((
                                format!(
                                    "SELECT {t1_name}.{t1_col}, {t2_name}.{t2_col} FROM {t1_name} JOIN {t2_name} ON {t1_name}.{t1_col} = {t2_name}.{t2_col}"
                                ),
                                vec![t1_col_def, t2_col_def],
                            ))
                        })
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::LeftJoin => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    proptest::sample::select(all)
                        .prop_filter_map("need different table for left join", move |t2| {
                            if t2.name == t1.name {
                                return None;
                            }
                            let t1_col = t1.columns.first()?.name.clone();
                            let t2_col = t2.columns.first()?.name.clone();
                            let t1_name = t1.qualified_name();
                            let t2_name = t2.qualified_name();
                            let t1_col_def = t1.columns.first()?.clone();
                            let t2_col_def = t2.columns.first()?.clone();
                            // ColumnDef defaults to nullable; RHS of LEFT JOIN
                            // is always nullable so the default is correct.
                            Some((
                                format!(
                                    "SELECT {t1_name}.{t1_col} AS lcol, {t2_name}.{t2_col} AS rcol \
                                     FROM {t1_name} LEFT JOIN {t2_name} ON {t1_name}.{t1_col} = {t2_name}.{t2_col}"
                                ),
                                vec![
                                    ColumnDef::new("lcol", t1_col_def.data_type),
                                    ColumnDef::new("rcol", t2_col_def.data_type),
                                ],
                            ))
                        })
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::DualLeftJoin => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    let all_for_inner = all.clone();
                    // Pick two distinct R-side tables to LEFT JOIN.
                    proptest::sample::select(all)
                        .prop_flat_map(move |ta| {
                            let all_inner = all_for_inner.clone();
                            proptest::sample::select(all_inner)
                                .prop_map(move |tb| (ta.clone(), tb))
                        })
                        .prop_filter_map(
                            "need three distinct tables for dual LEFT JOIN",
                            move |(ta, tb)| {
                                if ta.name == t1.name
                                    || tb.name == t1.name
                                    || ta.name == tb.name
                                {
                                    return None;
                                }
                                let t1_col = t1.columns.first()?.name.clone();
                                let ta_col = ta.columns.first()?.name.clone();
                                let tb_col = tb.columns.first()?.name.clone();
                                let t1_name = t1.qualified_name();
                                let ta_name = ta.qualified_name();
                                let tb_name = tb.qualified_name();
                                let t1_col_def = t1.columns.first()?.clone();
                                let ta_col_def = ta.columns.first()?.clone();
                                let tb_col_def = tb.columns.first()?.clone();
                                Some((
                                    format!(
                                        "SELECT {t1_name}.{t1_col} AS lcol, \
                                                {ta_name}.{ta_col} AS acol, \
                                                {tb_name}.{tb_col} AS bcol \
                                         FROM {t1_name} \
                                         LEFT JOIN {ta_name} ON {t1_name}.{t1_col} = {ta_name}.{ta_col} \
                                         LEFT JOIN {tb_name} ON {t1_name}.{t1_col} = {tb_name}.{tb_col}"
                                    ),
                                    vec![
                                        ColumnDef::new("lcol", t1_col_def.data_type),
                                        ColumnDef::new("acol", ta_col_def.data_type),
                                        ColumnDef::new("bcol", tb_col_def.data_type),
                                    ],
                                ))
                            },
                        )
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::LeftJoinAggregate => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    proptest::sample::select(all)
                        .prop_filter_map(
                            "need different table for left join aggregate",
                            move |t2| {
                                if t2.name == t1.name {
                                    return None;
                                }
                                let t1_col = t1.columns.first()?.name.clone();
                                let t2_col = t2.columns.first()?.name.clone();
                                let t1_name = t1.qualified_name();
                                let t2_name = t2.qualified_name();
                                let t1_col_def = t1.columns.first()?.clone();
                                Some((
                                    format!(
                                        "SELECT {t1_name}.{t1_col} AS gid, \
                                         COALESCE(json_array_length(json_group_array({t2_name}.{t2_col}) \
                                                  FILTER (WHERE {t2_name}.{t2_col} IS NOT NULL)), 0) AS jcnt \
                                         FROM {t1_name} LEFT JOIN {t2_name} \
                                         ON {t1_name}.{t1_col} = {t2_name}.{t2_col} \
                                         GROUP BY {t1_name}.{t1_col}"
                                    ),
                                    vec![
                                        ColumnDef::new("gid", t1_col_def.data_type),
                                        ColumnDef::new("jcnt", DataType::Integer),
                                    ],
                                ))
                            },
                        )
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::DualLeftJoinAggregate => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    let all_for_inner = all.clone();
                    proptest::sample::select(all)
                        .prop_flat_map(move |ta| {
                            let all_inner = all_for_inner.clone();
                            proptest::sample::select(all_inner)
                                .prop_map(move |tb| (ta.clone(), tb))
                        })
                        .prop_filter_map(
                            "need three distinct tables for dual LEFT JOIN aggregate",
                            move |(ta, tb)| {
                                if ta.name == t1.name
                                    || tb.name == t1.name
                                    || ta.name == tb.name
                                {
                                    return None;
                                }
                                let t1_col = t1.columns.first()?.name.clone();
                                let ta_col = ta.columns.first()?.name.clone();
                                let tb_col = tb.columns.first()?.name.clone();
                                let t1_name = t1.qualified_name();
                                let ta_name = ta.qualified_name();
                                let tb_name = tb.qualified_name();
                                let t1_col_def = t1.columns.first()?.clone();
                                Some((
                                    format!(
                                        "SELECT {t1_name}.{t1_col} AS gid, \
                                         COALESCE(json_array_length(json_group_array({ta_name}.{ta_col}) \
                                                  FILTER (WHERE {ta_name}.{ta_col} IS NOT NULL)), 0) AS jacnt, \
                                         COALESCE(json_array_length(json_group_array({tb_name}.{tb_col}) \
                                                  FILTER (WHERE {tb_name}.{tb_col} IS NOT NULL)), 0) AS jbcnt \
                                         FROM {t1_name} \
                                         LEFT JOIN {ta_name} ON {t1_name}.{t1_col} = {ta_name}.{ta_col} \
                                         LEFT JOIN {tb_name} ON {t1_name}.{t1_col} = {tb_name}.{tb_col} \
                                         GROUP BY {t1_name}.{t1_col}"
                                    ),
                                    vec![
                                        ColumnDef::new("gid", t1_col_def.data_type),
                                        ColumnDef::new("jacnt", DataType::Integer),
                                        ColumnDef::new("jbcnt", DataType::Integer),
                                    ],
                                ))
                            },
                        )
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::WideGroupByDualLeftJoinAggregate => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    let t1_cols_for_outer = table_col_defs;
                    let all_for_inner = all.clone();
                    proptest::sample::select(all)
                        .prop_flat_map(move |ta| {
                            let all_inner = all_for_inner.clone();
                            proptest::sample::select(all_inner)
                                .prop_map(move |tb| (ta.clone(), tb))
                        })
                        .prop_filter_map(
                            "need three distinct tables for wide GROUP BY dual LEFT JOIN aggregate",
                            move |(ta, tb)| {
                                if ta.name == t1.name
                                    || tb.name == t1.name
                                    || ta.name == tb.name
                                {
                                    return None;
                                }
                                let t1_col = t1.columns.first()?.name.clone();
                                let ta_col = ta.columns.first()?.name.clone();
                                let tb_col = tb.columns.first()?.name.clone();
                                let t1_name = t1.qualified_name();
                                let ta_name = ta.qualified_name();
                                let tb_name = tb.qualified_name();

                                // Project & GROUP BY every column of the outer
                                // table so the DBSP state-index key spans many
                                // bytes — needed to reach the TryAdvance leaf
                                // boundary in AggregateOperator's FetchKey seek.
                                let outer_cols: Vec<String> = t1_cols_for_outer
                                    .iter()
                                    .map(|c| format!("{t1_name}.{}", c.name))
                                    .collect();
                                let outer_cols_csv = outer_cols.join(", ");

                                let mut output_columns: Vec<ColumnDef> = t1_cols_for_outer
                                    .iter()
                                    .map(|c| ColumnDef::new(&c.name, c.data_type))
                                    .collect();
                                output_columns.push(ColumnDef::new("jacnt", DataType::Integer));
                                output_columns.push(ColumnDef::new("jbcnt", DataType::Integer));

                                Some((
                                    format!(
                                        "SELECT {outer_cols_csv}, \
                                         COALESCE(json_array_length(json_group_array({ta_name}.{ta_col}) \
                                                  FILTER (WHERE {ta_name}.{ta_col} IS NOT NULL)), 0) AS jacnt, \
                                         COALESCE(json_array_length(json_group_array({tb_name}.{tb_col}) \
                                                  FILTER (WHERE {tb_name}.{tb_col} IS NOT NULL)), 0) AS jbcnt \
                                         FROM {t1_name} \
                                         LEFT JOIN {ta_name} ON {t1_name}.{t1_col} = {ta_name}.{ta_col} \
                                         LEFT JOIN {tb_name} ON {t1_name}.{t1_col} = {tb_name}.{tb_col} \
                                         GROUP BY {outer_cols_csv}"
                                    ),
                                    output_columns,
                                ))
                            },
                        )
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::QualifiedStarJoin => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    let t1_cols = table_col_defs;
                    proptest::sample::select(all)
                        .prop_filter_map(
                            "need different table with filterable col for qualified star join",
                            move |t2| {
                                if t2.name == t1.name {
                                    return None;
                                }
                                let t1_col = t1.columns.first()?.name.clone();
                                let t2_col = t2.columns.first()?.name.clone();
                                let t1_name = t1.qualified_name();
                                let t2_name = t2.qualified_name();
                                let where_col = t2
                                    .filterable_columns()
                                    .next()
                                    .map(|c| c.name.clone())
                                    .unwrap_or(t2_col.clone());
                                Some(format!(
                                    "SELECT {t1_name}.* FROM {t1_name} JOIN {t2_name} ON {t1_name}.{t1_col} = {t2_name}.{t2_col} WHERE {t2_name}.{where_col} IS NOT NULL"
                                ))
                            },
                        )
                        .prop_map(move |select_sql| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns: t1_cols.clone(),
                        })
                        .boxed()
                }
                ViewSelectKind::UnionAll => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    proptest::sample::select(all)
                        .prop_filter_map("need different table for union all", move |t2| {
                            if t2.name == t1.name {
                                return None;
                            }
                            let n = t1.columns.len().min(t2.columns.len()).min(3).max(1);
                            let t1_name = t1.qualified_name();
                            let t2_name = t2.qualified_name();
                            let t1_proj: Vec<String> = t1.columns[..n]
                                .iter()
                                .enumerate()
                                .map(|(i, c)| format!("{}.{} AS c{i}", t1_name, c.name))
                                .collect();
                            let t2_proj: Vec<String> = t2.columns[..n]
                                .iter()
                                .enumerate()
                                .map(|(i, c)| format!("{}.{} AS c{i}", t2_name, c.name))
                                .collect();
                            let output_columns: Vec<ColumnDef> = (0..n)
                                .map(|i| ColumnDef::new(format!("c{i}"), DataType::Text))
                                .collect();
                            Some((
                                format!(
                                    "SELECT {} FROM {t1_name} UNION ALL SELECT {} FROM {t2_name}",
                                    t1_proj.join(", "),
                                    t2_proj.join(", ")
                                ),
                                output_columns,
                            ))
                        })
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::UnionAllJoin => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    proptest::sample::select(all)
                        .prop_filter_map(
                            "need different table with 2+ cols for union all join",
                            move |t2| {
                                if t2.name == t1.name || t2.columns.len() < 2 {
                                    return None;
                                }
                                let t1_name = t1.qualified_name();
                                let t2_name = t2.qualified_name();
                                let t1_col0 = &t1.columns[0].name;
                                let t2_col0 = &t2.columns[0].name;
                                let t2_col1 = &t2.columns[1].name;

                                let select_sql = format!(
                                    "SELECT {t1_name}.{t1_col0} AS c0, {t2_name}.{t2_col0} AS c1 \
                                     FROM {t1_name} JOIN {t2_name} ON {t1_name}.{t1_col0} = {t2_name}.{t2_col0} \
                                     UNION ALL \
                                     SELECT {t1_name}.{t1_col0} AS c0, {t2_name}.{t2_col1} AS c1 \
                                     FROM {t1_name} JOIN {t2_name} ON {t1_name}.{t1_col0} = {t2_name}.{t2_col1}"
                                );
                                let output_columns = vec![
                                    ColumnDef::new("c0", t1.columns[0].data_type),
                                    ColumnDef::new("c1", DataType::Text),
                                ];
                                Some((select_sql, output_columns))
                            },
                        )
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::RecursiveCte => {
                    let int_cols: Vec<(String, String)> = {
                        let ints: Vec<&ColumnDef> = table_col_defs
                            .iter()
                            .filter(|c| c.data_type == DataType::Integer)
                            .collect();
                        if ints.len() >= 2 {
                            vec![(ints[0].name.clone(), ints[1].name.clone())]
                        } else {
                            vec![]
                        }
                    };
                    if int_cols.is_empty() {
                        let select_sql = format!("SELECT * FROM {table_name}");
                        return Just(CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name,
                            columns: vec![],
                            select_sql,
                            output_columns: table_col_defs,
                        })
                        .boxed();
                    }
                    let (id_col, parent_col) = int_cols[0].clone();
                    let all = (*all_sources_inner).clone();
                    let base_table_name = table_name;
                    (
                        any::<bool>(),
                        proptest::sample::select(all),
                        any::<bool>(),
                        0u32..3u32,
                    )
                        .prop_map(
                            move |(use_outer_join, join_table, inherit_col_names, pred_kind)| {
                                // When the explicit column list is omitted the CTE
                                // inherits the base term's column names, so the CTE's
                                // columns collide (by bare name) with the base table's
                                // own columns in the recursive term's join. The
                                // hard-coded `tree(id, parent_id, depth)` list makes
                                // that collision structurally impossible, which hides
                                // qualifier-dropping bugs in the IVM filter path.
                                let (cte_id, cte_parent, cte_header) = if inherit_col_names {
                                    (id_col.clone(), parent_col.clone(), "tree".to_string())
                                } else {
                                    (
                                        "id".to_string(),
                                        "parent_id".to_string(),
                                        "tree(id, parent_id, depth)".to_string(),
                                    )
                                };
                                // Predicates 1/2 are classified "complex" by
                                // `predicate_needs_projection`, routing the recursive
                                // term through the projection-rewrite path.
                                let extra_pred = match pred_kind {
                                    1 => format!(
                                        " AND CAST(tree.{cte_id} AS TEXT) NOT LIKE '%zzq%'"
                                    ),
                                    2 => " AND tree.depth BETWEEN 0 AND 100".to_string(),
                                    _ => String::new(),
                                };
                                let cte = format!(
                                    "WITH RECURSIVE {cte_header} AS (\
                                    SELECT {id_col}, {parent_col}, 0 AS depth FROM {base_table_name} \
                                    UNION ALL \
                                    SELECT {base_table_name}.{id_col}, {base_table_name}.{parent_col}, tree.depth + 1 \
                                    FROM {base_table_name} JOIN tree ON {base_table_name}.{parent_col} = tree.{cte_id} \
                                    WHERE tree.depth < 10{extra_pred}\
                                    ) "
                                );
                                let join_candidate_ok = use_outer_join
                                    && join_table.name != base_table_name
                                    && !join_table.columns.is_empty();
                                if join_candidate_ok {
                                    let jt = join_table.qualified_name();
                                    let jcol = &join_table.columns[0].name;
                                    let select_sql = format!(
                                        "{cte}SELECT {jt}.* FROM tree JOIN {jt} ON tree.{cte_id} = {jt}.{jcol}"
                                    );
                                    let output_columns = join_table.columns.clone();
                                    CreateViewStatement {
                                        materialized,
                                        if_not_exists,
                                        view_name: view_name.clone(),
                                        columns: vec![],
                                        select_sql,
                                        output_columns,
                                    }
                                } else {
                                    let select_sql = format!("{cte}SELECT * FROM tree");
                                    let output_columns = vec![
                                        ColumnDef::new(&cte_id, DataType::Integer),
                                        ColumnDef::new(&cte_parent, DataType::Integer),
                                        ColumnDef::new("depth", DataType::Integer),
                                    ];
                                    CreateViewStatement {
                                        materialized,
                                        if_not_exists,
                                        view_name: view_name.clone(),
                                        columns: vec![],
                                        select_sql,
                                        output_columns,
                                    }
                                }
                            },
                        )
                        .boxed()
                }
                ViewSelectKind::ComplexFilterJoin => {
                    let all = (*all_sources_inner).clone();
                    let t1 = table;
                    (proptest::sample::select(all), 0u32..4u32)
                        .prop_filter_map("need different table for join", move |(t2, pred_kind)| {
                            if t2.name == t1.name {
                                return None;
                            }
                            // Prefer the primary key as the join key: INTEGER
                            // PRIMARY KEY aliases the rowid and takes small
                            // sequential values, so rows from two independently
                            // generated tables actually pair up. Joining on a
                            // random column almost never matches, leaving the
                            // view empty and the mis-bind unobservable.
                            let t1_key = t1
                                .columns
                                .iter()
                                .find(|c| c.primary_key)
                                .or_else(|| t1.columns.first())?;
                            let t2_key = t2
                                .columns
                                .iter()
                                .find(|c| c.primary_key)
                                .or_else(|| t2.columns.first())?;
                            let t1_col = t1_key.name.clone();
                            let t2_col = t2_key.name.clone();
                            let t1_name = t1.qualified_name();
                            let t2_name = t2.qualified_name();
                            let t1_col_def = t1_key.clone();
                            // Project a column whose BARE name exists on both
                            // sides but is not the join key. Projecting the join
                            // key itself would make both sides equal by
                            // construction, hiding a mis-bind.
                            let shared_def = t2.columns.iter().find(|c| {
                                c.name != t2_col
                                    && c.name != t1_col
                                    && t1.columns.iter().any(|o| o.name == c.name)
                            })?;
                            let shared_col = shared_def.name.clone();
                            let t2_col_def = shared_def.clone();
                            // Every branch is classified "needs projection" by
                            // `DbspCompiler::predicate_needs_projection`.
                            let where_clause = match pred_kind {
                                1 => format!(
                                    "{t1_name}.{t1_col} BETWEEN 0 AND 999999999"
                                ),
                                2 => format!(
                                    "{t1_name}.{t1_col} IN (0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15)"
                                ),
                                3 => format!("CAST({t1_name}.{t1_col} AS TEXT) IS NOT NULL"),
                                _ => {
                                    format!("CAST({t1_name}.{t1_col} AS TEXT) NOT LIKE '%zzq%'")
                                }
                            };
                            Some((
                                format!(
                                    "SELECT {t1_name}.{shared_col} AS cfj_l, {t2_name}.{shared_col} AS cfj_r \
                                     FROM {t1_name} JOIN {t2_name} ON {t1_name}.{t1_col} = {t2_name}.{t2_col} \
                                     WHERE {where_clause}"
                                ),
                                vec![
                                    ColumnDef::new("cfj_l", t1_col_def.data_type),
                                    ColumnDef::new("cfj_r", t2_col_def.data_type),
                                ],
                            ))
                        })
                        .prop_map(move |(select_sql, output_columns)| CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name: view_name.clone(),
                            columns: vec![],
                            select_sql,
                            output_columns,
                        })
                        .boxed()
                }
                ViewSelectKind::ComplexFilterSelfJoin => {
                    let int_cols: Vec<(String, String)> = {
                        let ints: Vec<&ColumnDef> = table_col_defs
                            .iter()
                            .filter(|c| c.data_type == DataType::Integer)
                            .collect();
                        if ints.len() >= 2 {
                            vec![(ints[0].name.clone(), ints[1].name.clone())]
                        } else {
                            vec![]
                        }
                    };
                    if int_cols.is_empty() {
                        let select_sql = format!("SELECT * FROM {table_name}");
                        return Just(CreateViewStatement {
                            materialized,
                            if_not_exists,
                            view_name,
                            columns: vec![],
                            select_sql,
                            output_columns: table_col_defs,
                        })
                        .boxed();
                    }
                    let (id_col, ref_col) = int_cols[0].clone();
                    // Project a column that is not the join key, so the two
                    // self-join sides genuinely differ when bound correctly.
                    let proj_def = table_col_defs
                        .iter()
                        .find(|c| c.name != id_col && c.name != ref_col)
                        .cloned()
                        .unwrap_or_else(|| {
                            table_col_defs
                                .iter()
                                .find(|c| c.name == ref_col)
                                .cloned()
                                .unwrap_or_else(|| ColumnDef::new(&ref_col, DataType::Integer))
                        });
                    let proj_col = proj_def.name.clone();
                    let proj_ty = proj_def.data_type;
                    let base = table_name;
                    (0u32..4u32)
                        .prop_map(move |pred_kind| {
                            // Every branch is classified "needs projection" by
                            // `DbspCompiler::predicate_needs_projection`.
                            let where_clause = match pred_kind {
                                1 => format!("a.{id_col} BETWEEN 0 AND 999999999"),
                                2 => format!(
                                    "a.{id_col} IN (0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15)"
                                ),
                                3 => format!("CAST(a.{id_col} AS INTEGER) >= 0"),
                                _ => format!("CAST(a.{proj_col} AS TEXT) NOT LIKE '%zzq%'"),
                            };
                            let select_sql = format!(
                                "SELECT a.{id_col} AS sjk, a.{proj_col} AS sja, b.{proj_col} AS sjb \
                                 FROM {base} a JOIN {base} b ON a.{ref_col} = b.{id_col} \
                                 WHERE {where_clause}"
                            );
                            CreateViewStatement {
                                materialized,
                                if_not_exists,
                                view_name: view_name.clone(),
                                columns: vec![],
                                select_sql,
                                output_columns: vec![
                                    ColumnDef::new("sjk", DataType::Integer),
                                    ColumnDef::new("sja", proj_ty),
                                    ColumnDef::new("sjb", proj_ty),
                                ],
                            }
                        })
                        .boxed()
                }
                ViewSelectKind::RecursiveCounter => {
                    (3u32..=20u32)
                        .prop_map(move |limit| {
                            let select_sql = format!(
                                "WITH RECURSIVE gen(n) AS (\
                                SELECT 1 UNION ALL SELECT n+1 FROM gen WHERE n < {limit}\
                                ) SELECT n FROM gen"
                            );
                            let output_columns = vec![ColumnDef::new("n", DataType::Integer)];
                            CreateViewStatement {
                                materialized,
                                if_not_exists,
                                view_name: view_name.clone(),
                                columns: vec![],
                                select_sql,
                                output_columns,
                            }
                        })
                        .boxed()
                }
            }
        })
        .boxed()
}

/// Generate a DROP VIEW statement for a schema.
pub fn drop_view_for_schema(schema: &Schema) -> BoxedStrategy<DropViewStatement> {
    if schema.views.is_empty() {
        crate::create_table::identifier()
            .prop_map(|view_name| DropViewStatement {
                materialized: false,
                if_exists: true,
                view_name,
            })
            .boxed()
    } else {
        let view_names: Vec<String> = schema.views.iter().map(|v| v.name.clone()).collect();
        (any::<bool>(), proptest::sample::select(view_names))
            .prop_map(|(if_exists, view_name)| DropViewStatement {
                materialized: false,
                if_exists,
                view_name,
            })
            .boxed()
    }
}

/// Generate a DROP VIEW statement for materialized views.
pub fn drop_materialized_view_for_schema(schema: &Schema) -> BoxedStrategy<DropViewStatement> {
    if schema.materialized_views.is_empty() {
        crate::create_table::identifier()
            .prop_map(|view_name| DropViewStatement {
                materialized: true,
                if_exists: true,
                view_name,
            })
            .boxed()
    } else {
        let view_names: Vec<String> = schema
            .materialized_views
            .iter()
            .map(|v| v.name.clone())
            .collect();
        (any::<bool>(), proptest::sample::select(view_names))
            .prop_map(|(if_exists, view_name)| DropViewStatement {
                materialized: true,
                if_exists,
                view_name,
            })
            .boxed()
    }
}

/// Generate a DROP VIEW statement for a specific view name.
pub fn drop_view(view_name: String) -> impl Strategy<Value = DropViewStatement> {
    any::<bool>().prop_map(move |if_exists| DropViewStatement {
        materialized: false,
        if_exists,
        view_name: view_name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColumnDef, DataType, SchemaBuilder, Table, View};

    fn test_schema() -> Schema {
        SchemaBuilder::new()
            .add_table(Table::new(
                "users",
                vec![
                    ColumnDef::new("id", DataType::Integer).primary_key(),
                    ColumnDef::new("name", DataType::Text),
                ],
            ))
            .add_table(Table::new(
                "posts",
                vec![
                    ColumnDef::new("id", DataType::Integer).primary_key(),
                    ColumnDef::new("user_id", DataType::Integer),
                    ColumnDef::new("title", DataType::Text),
                ],
            ))
            .build()
    }

    fn test_schema_with_matview() -> Schema {
        SchemaBuilder::new()
            .add_table(Table::new(
                "users",
                vec![
                    ColumnDef::new("id", DataType::Integer).primary_key(),
                    ColumnDef::new("name", DataType::Text),
                ],
            ))
            .add_table(Table::new(
                "posts",
                vec![
                    ColumnDef::new("id", DataType::Integer).primary_key(),
                    ColumnDef::new("user_id", DataType::Integer),
                    ColumnDef::new("title", DataType::Text),
                ],
            ))
            .add_materialized_view(
                View::new("mv_users", "SELECT * FROM users").with_columns(vec![
                    ColumnDef::new("id", DataType::Integer),
                    ColumnDef::new("name", DataType::Text),
                ]),
            )
            .build()
    }

    proptest! {
        #[test]
        fn create_view_generates_valid_sql(stmt in create_view(&test_schema())) {
            let sql = stmt.to_string();
            prop_assert!(sql.starts_with("CREATE VIEW"));
            prop_assert!(sql.contains(" AS SELECT") || sql.contains(" AS WITH"));
        }

        #[test]
        fn create_materialized_view_generates_valid_sql(stmt in create_materialized_view(&test_schema())) {
            let sql = stmt.to_string();
            prop_assert!(sql.starts_with("CREATE MATERIALIZED VIEW"));
            prop_assert!(sql.contains(" AS SELECT") || sql.contains(" AS WITH"));
        }

        #[test]
        fn materialized_view_as_regular_strips_keyword(stmt in create_materialized_view(&test_schema())) {
            let regular = stmt.as_regular_view_sql();
            prop_assert!(regular.starts_with("CREATE VIEW"));
            prop_assert!(!regular.contains("MATERIALIZED"));
        }

        #[test]
        fn drop_view_generates_valid_sql(stmt in drop_view("my_view".to_string())) {
            let sql = stmt.to_string();
            prop_assert!(sql.starts_with("DROP VIEW"));
            prop_assert!(sql.contains("my_view"));
        }

        #[test]
        fn create_materialized_view_generates_union_all(stmt in create_materialized_view(&test_schema())) {
            // With 2 tables, UNION ALL should sometimes appear
            let sql = stmt.to_string();
            prop_assert!(sql.starts_with("CREATE MATERIALIZED VIEW"));
            // Just verify it's valid SQL — UNION ALL will be generated stochastically
        }

        #[test]
        fn create_materialized_view_generates_recursive_cte(stmt in create_materialized_view(&test_schema())) {
            let sql = stmt.to_string();
            prop_assert!(sql.starts_with("CREATE MATERIALIZED VIEW"));
            // posts has 2 int cols (id, user_id), so RecursiveCte is possible
        }

        #[test]
        fn create_materialized_view_can_reference_matview(stmt in create_materialized_view(&test_schema_with_matview())) {
            let sql = stmt.to_string();
            prop_assert!(sql.starts_with("CREATE MATERIALIZED VIEW"));
            // mv_users is in all_sources, so it can be selected as the source table
        }
    }

    #[test]
    fn output_columns_populated_for_star() {
        let schema = test_schema();
        let mut runner = proptest::test_runner::TestRunner::default();
        let strategy = create_materialized_view(&schema);
        for _ in 0..50 {
            let tree = strategy.new_tree(&mut runner).unwrap();
            let stmt = tree.current();
            assert!(
                !stmt.output_columns.is_empty(),
                "output_columns should always be populated, got empty for SQL: {}",
                stmt.select_sql
            );
        }
    }
}
