//! Turning a correlated `EXISTS` subquery into a keyed indicator.
//!
//! `EXISTS (SELECT 1 FROM r WHERE r.fk = l.id AND <local>)` is a semi-join on
//! `l.id = r.fk`. Splitting the subquery's WHERE into that one equality and the rest
//! leaves a plain scan of `r` the existing operators already compile, and the equality
//! becomes the key an indicator operator counts matches under.
//!
//! Every shape this split cannot express is refused by name. The alternative — guessing
//! which scope a column belongs to — is how a correlated reference silently binds to an
//! inner column that happens to share its bare name.

use crate::translate::logical::{
    Column, Filter, LogicalExpr, LogicalPlan, LogicalSchema, Projection,
};
use crate::{LimboError, Result};
use std::sync::Arc;

/// Columns named `__exists_*` are minted by this rewrite, so a source column of that
/// name would be indistinguishable from an indicator.
pub(crate) const INDICATOR_PREFIX: &str = "__exists_";

pub(crate) fn indicator_name(index: usize) -> String {
    format!("{INDICATOR_PREFIX}{index}")
}

/// One `EXISTS` reduced to a key pair plus an uncorrelated subquery plan.
pub(crate) struct CorrelatedExists {
    /// Column of the outer plan the subquery correlates to.
    pub outer_key: Column,
    /// Column of the subquery that the outer key is matched against.
    pub inner_key: Column,
    /// The subquery with the correlation removed: an ordinary scan/filter subgraph.
    pub source: LogicalPlan,
}

fn unsupported(what: &str) -> LimboError {
    LimboError::ParseError(format!(
        "EXISTS subquery in a materialized view is not supported: {what}"
    ))
}

/// Which scope a column resolves to. A column resolving to both is ambiguous and is
/// treated as unresolvable rather than guessed at.
enum Scope {
    Outer,
    Inner,
    Unknown,
}

fn scope_of(col: &Column, outer: &LogicalSchema, inner: &LogicalSchema) -> Scope {
    let in_outer = outer.find_column(&col.name, col.table.as_deref()).is_some();
    let in_inner = inner.find_column(&col.name, col.table.as_deref()).is_some();
    match (in_outer, in_inner) {
        (true, false) => Scope::Outer,
        (false, true) => Scope::Inner,
        _ => Scope::Unknown,
    }
}

fn split_conjuncts(expr: &LogicalExpr) -> Vec<&LogicalExpr> {
    match expr {
        LogicalExpr::BinaryExpr {
            left,
            op: crate::translate::logical::BinaryOperator::And,
            right,
        } => {
            let mut out = split_conjuncts(left);
            out.extend(split_conjuncts(right));
            out
        }
        other => vec![other],
    }
}

fn conjoin(parts: Vec<LogicalExpr>) -> Option<LogicalExpr> {
    parts
        .into_iter()
        .reduce(|acc, next| LogicalExpr::BinaryExpr {
            left: Box::new(acc),
            op: crate::translate::logical::BinaryOperator::And,
            right: Box::new(next),
        })
}

/// Every column the expression mentions that belongs to the outer scope.
fn outer_refs(expr: &LogicalExpr, outer: &LogicalSchema, inner: &LogicalSchema) -> Vec<String> {
    let mut found = Vec::new();
    collect_columns(expr, &mut |col| {
        if matches!(scope_of(col, outer, inner), Scope::Outer) {
            found.push(match &col.table {
                Some(t) => format!("{t}.{}", col.name),
                None => col.name.clone(),
            });
        }
    });
    found
}

/// Visit every column reference, including inside subqueries: a nested EXISTS may itself
/// reach the outer query.
fn collect_columns(expr: &LogicalExpr, visit: &mut impl FnMut(&Column)) {
    match expr {
        LogicalExpr::Column(col) => visit(col),
        LogicalExpr::BinaryExpr { left, right, .. } => {
            collect_columns(left, visit);
            collect_columns(right, visit);
        }
        LogicalExpr::UnaryExpr { expr, .. }
        | LogicalExpr::IsNull { expr, .. }
        | LogicalExpr::Cast { expr, .. }
        | LogicalExpr::Alias { expr, .. } => collect_columns(expr, visit),
        LogicalExpr::Like { expr, pattern, .. } => {
            collect_columns(expr, visit);
            collect_columns(pattern, visit);
        }
        LogicalExpr::Between {
            expr, low, high, ..
        } => {
            collect_columns(expr, visit);
            collect_columns(low, visit);
            collect_columns(high, visit);
        }
        LogicalExpr::InList { expr, list, .. } => {
            collect_columns(expr, visit);
            list.iter().for_each(|e| collect_columns(e, visit));
        }
        LogicalExpr::ScalarFunction { args, .. } | LogicalExpr::AggregateFunction { args, .. } => {
            args.iter().for_each(|e| collect_columns(e, visit));
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_expr,
        } => {
            if let Some(e) = expr {
                collect_columns(e, visit);
            }
            for (w, t) in when_then {
                collect_columns(w, visit);
                collect_columns(t, visit);
            }
            if let Some(e) = else_expr {
                collect_columns(e, visit);
            }
        }
        LogicalExpr::Literal(_)
        | LogicalExpr::Exists { .. }
        | LogicalExpr::InSubquery { .. }
        | LogicalExpr::ScalarSubquery(_) => {}
    }
}

/// Peel the `SELECT 1` projection the plan builder puts over every EXISTS subquery.
fn subquery_body(plan: &LogicalPlan) -> &LogicalPlan {
    match plan {
        LogicalPlan::Projection(Projection { input, .. }) => input,
        other => other,
    }
}

pub(crate) fn analyze(subquery: &LogicalPlan, outer: &LogicalSchema) -> Result<CorrelatedExists> {
    let body = subquery_body(subquery);
    let LogicalPlan::Filter(Filter { input, predicate }) = body else {
        return Err(unsupported(
            "it has no WHERE clause correlating it to the outer query",
        ));
    };
    let inner = input.schema();

    let mut correlations: Vec<(Column, Column)> = Vec::new();
    let mut local: Vec<LogicalExpr> = Vec::new();
    for conjunct in split_conjuncts(predicate) {
        let pair = match conjunct {
            LogicalExpr::BinaryExpr { left, op, right } => match (left.as_ref(), right.as_ref()) {
                (LogicalExpr::Column(a), LogicalExpr::Column(b)) => Some((a, b, *op)),
                _ => None,
            },
            _ => None,
        };
        let Some((a, b, op)) = pair else {
            local.push(conjunct.clone());
            continue;
        };
        let crosses_scopes = matches!(
            (scope_of(a, outer, &inner), scope_of(b, outer, &inner)),
            (Scope::Outer, Scope::Inner) | (Scope::Inner, Scope::Outer)
        );
        if !crosses_scopes {
            local.push(conjunct.clone());
            continue;
        }
        if op != crate::translate::logical::BinaryOperator::Equals {
            return Err(unsupported(
                "its correlation to the outer query is not an equality; \
                 only equality correlation can be maintained incrementally",
            ));
        }
        let (outer_col, inner_col) = match scope_of(a, outer, &inner) {
            Scope::Outer => (a.clone(), b.clone()),
            _ => (b.clone(), a.clone()),
        };
        correlations.push((outer_col, inner_col));
    }

    if correlations.is_empty() {
        return Err(unsupported(
            "it is not correlated to the outer query; an uncorrelated EXISTS has no key \
             to maintain matches under",
        ));
    }
    if correlations.len() > 1 {
        return Err(unsupported(
            "it correlates to the outer query on more than one equality",
        ));
    }

    // Anything left referring to the outer query cannot be evaluated by a scan of the
    // subquery's own table, and resolving it against the inner scope would bind it to a
    // same-named inner column.
    for conjunct in &local {
        let refs = outer_refs(conjunct, outer, &inner);
        if !refs.is_empty() {
            return Err(unsupported(&format!(
                "it refers to the outer query outside its correlation ({}); only a single \
                 equality may reach the outer query",
                refs.join(", ")
            )));
        }
    }

    let (outer_key, inner_key) = correlations.pop().expect("exactly one correlation");
    let source = match conjoin(local) {
        Some(predicate) => LogicalPlan::Filter(Filter {
            input: input.clone(),
            predicate,
        }),
        None => (**input).clone(),
    };

    Ok(CorrelatedExists {
        outer_key,
        inner_key,
        source,
    })
}

/// Replace the `index`-th EXISTS with a test of its indicator column.
pub(crate) fn substitute(expr: &LogicalExpr, index: usize, target: &LogicalExpr) -> LogicalExpr {
    if expr == target {
        return LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column(Column {
                name: indicator_name(index),
                table: None,
            })),
            op: crate::translate::logical::BinaryOperator::Equals,
            right: Box::new(LogicalExpr::Literal(crate::types::Value::from_i64(1))),
        };
    }
    match expr {
        LogicalExpr::BinaryExpr { left, op, right } => LogicalExpr::BinaryExpr {
            left: Box::new(substitute(left, index, target)),
            op: *op,
            right: Box::new(substitute(right, index, target)),
        },
        LogicalExpr::UnaryExpr { op, expr } => LogicalExpr::UnaryExpr {
            op: *op,
            expr: Box::new(substitute(expr, index, target)),
        },
        other => other.clone(),
    }
}

/// Rewrite `NOT (__exists_i = 1)` to `__exists_i = 0`.
///
/// The indicator is always 0 or 1 and never NULL, so negating it is a change of constant
/// rather than a logical negation. That keeps `NOT` out of the filter predicates, where a
/// boolean negation would contradict SQL's three-valued comparisons.
pub(crate) fn fold_indicator_negations(expr: &LogicalExpr) -> LogicalExpr {
    match expr {
        LogicalExpr::UnaryExpr {
            op: crate::translate::logical::UnaryOperator::Not,
            expr: inner,
        } => match inner.as_ref() {
            LogicalExpr::BinaryExpr {
                left,
                op: crate::translate::logical::BinaryOperator::Equals,
                right,
            } if is_indicator(left) && is_one(right) => LogicalExpr::BinaryExpr {
                left: left.clone(),
                op: crate::translate::logical::BinaryOperator::Equals,
                right: Box::new(LogicalExpr::Literal(crate::types::Value::from_i64(0))),
            },
            other => LogicalExpr::UnaryExpr {
                op: crate::translate::logical::UnaryOperator::Not,
                expr: Box::new(fold_indicator_negations(other)),
            },
        },
        LogicalExpr::BinaryExpr { left, op, right } => LogicalExpr::BinaryExpr {
            left: Box::new(fold_indicator_negations(left)),
            op: *op,
            right: Box::new(fold_indicator_negations(right)),
        },
        other => other.clone(),
    }
}

fn is_indicator(expr: &LogicalExpr) -> bool {
    matches!(expr, LogicalExpr::Column(c) if c.name.starts_with(INDICATOR_PREFIX))
}

fn is_one(expr: &LogicalExpr) -> bool {
    matches!(expr, LogicalExpr::Literal(v) if *v == crate::types::Value::from_i64(1))
}

/// The first EXISTS in a deterministic left-to-right walk, if any.
pub(crate) fn first_exists(expr: &LogicalExpr) -> Option<&LogicalExpr> {
    match expr {
        LogicalExpr::Exists { .. } => Some(expr),
        LogicalExpr::BinaryExpr { left, right, .. } => {
            first_exists(left).or_else(|| first_exists(right))
        }
        LogicalExpr::UnaryExpr { expr, .. } => first_exists(expr),
        _ => None,
    }
}

/// Reject a source column that would be indistinguishable from an indicator column.
pub(crate) fn reject_reserved_names(schema: &LogicalSchema) -> Result<()> {
    for col in &schema.columns {
        if col.name.starts_with(INDICATOR_PREFIX) {
            return Err(LimboError::ParseError(format!(
                "column '{}' uses the reserved prefix '{INDICATOR_PREFIX}', which a \
                 materialized view with an EXISTS subquery needs for its indicator columns",
                col.name
            )));
        }
    }
    Ok(())
}

/// The schema a plan has once its indicator column is appended.
pub(crate) fn schema_with_indicator(base: &LogicalSchema, index: usize) -> LogicalSchema {
    let mut columns = base.columns.clone();
    columns.push(crate::translate::logical::ColumnInfo {
        name: indicator_name(index),
        ty: crate::schema::Type::Integer,
        database: None,
        table: None,
        table_alias: None,
    });
    LogicalSchema::new(columns)
}
