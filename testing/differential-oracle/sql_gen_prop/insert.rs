//! INSERT statement type and generation strategy.

use proptest::prelude::*;
use std::fmt;

use crate::expression::{Expression, ExpressionContext, ExpressionProfile};
use crate::function::builtin_functions;
use crate::profile::StatementProfile;
use crate::schema::{Schema, TableRef};

// =============================================================================
// INSERT STATEMENT PROFILE
// =============================================================================

/// Profile for controlling INSERT statement generation.
#[derive(Debug, Clone)]
pub struct InsertProfile {
    /// Maximum depth for expressions in VALUES.
    pub expression_max_depth: u32,
    /// Whether to allow aggregate functions (usually false for INSERT).
    pub allow_aggregates: bool,
    /// Expression profile for value expressions.
    pub expression_profile: ExpressionProfile,
}

impl Default for InsertProfile {
    fn default() -> Self {
        Self {
            expression_max_depth: 2,
            allow_aggregates: false,
            expression_profile: ExpressionProfile::default(),
        }
    }
}

impl InsertProfile {
    /// Builder method to set expression max depth.
    pub fn with_expression_max_depth(mut self, depth: u32) -> Self {
        self.expression_max_depth = depth;
        self
    }

    /// Builder method to set whether aggregates are allowed.
    pub fn with_aggregates(mut self, allow: bool) -> Self {
        self.allow_aggregates = allow;
        self
    }

    /// Builder method to set expression profile.
    pub fn with_expression_profile(mut self, profile: ExpressionProfile) -> Self {
        self.expression_profile = profile;
        self
    }
}

/// Conflict resolution for INSERT statements.
#[derive(Debug, Clone)]
pub enum ConflictAction {
    /// Plain INSERT (no conflict handling).
    None,
    /// INSERT OR REPLACE — replaces the conflicting row.
    OrReplace,
    /// INSERT ... ON CONFLICT(pk) DO UPDATE SET col=excluded.col — upsert.
    OnConflictUpdate {
        conflict_columns: Vec<String>,
        update_columns: Vec<String>,
    },
}

/// An INSERT statement.
#[derive(Debug, Clone)]
pub struct InsertStatement {
    pub table: String,
    pub columns: Vec<String>,
    /// The values to insert. These can be literals, function calls, or other expressions.
    pub values: Vec<Expression>,
    /// Conflict resolution action.
    pub conflict_action: ConflictAction,
}

impl fmt::Display for InsertStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.conflict_action {
            ConflictAction::OrReplace => write!(f, "INSERT OR REPLACE INTO {}", self.table)?,
            _ => write!(f, "INSERT INTO {}", self.table)?,
        }

        if !self.columns.is_empty() {
            let cols: Vec<String> = self.columns.iter().map(|c| c.to_string()).collect();
            write!(f, " ({})", cols.join(", "))?;
        }

        write!(f, " VALUES (")?;
        let vals: Vec<String> = self.values.iter().map(|v| v.to_string()).collect();
        write!(f, "{})", vals.join(", "))?;

        if let ConflictAction::OnConflictUpdate {
            conflict_columns,
            update_columns,
        } = &self.conflict_action
        {
            write!(
                f,
                " ON CONFLICT({}) DO UPDATE SET ",
                conflict_columns.join(", ")
            )?;
            let assignments: Vec<String> = update_columns
                .iter()
                .map(|c| format!("{c} = excluded.{c}"))
                .collect();
            write!(f, "{}", assignments.join(", "))?;
        }

        Ok(())
    }
}

fn value_strategies_for_table(
    table: &TableRef,
    schema: &Schema,
    profile: &StatementProfile,
) -> Vec<BoxedStrategy<Expression>> {
    let columns = &table.columns;
    let is_strict = table.strict;
    let functions = builtin_functions();

    let insert_profile = profile.insert_profile();
    let expression_max_depth = insert_profile.expression_max_depth;
    let allow_aggregates = insert_profile.allow_aggregates;

    let expr_profile = ExpressionProfile::default().with_subqueries_disabled();
    let ctx = ExpressionContext::new(functions, schema.clone())
        .with_max_depth(expression_max_depth)
        .with_aggregates(allow_aggregates)
        .with_profile(expr_profile);

    let profile_clone = profile.clone();
    columns
        .iter()
        .map(|c| {
            if is_strict {
                crate::value::value_for_type(&c.data_type, c.nullable, &profile_clone)
                    .prop_map(Expression::Value)
                    .boxed()
            } else {
                crate::expression::expression_for_type(Some(&c.data_type), &ctx)
            }
        })
        .collect()
}

/// Generate an INSERT statement for a table with profile.
pub fn insert_for_table(
    table: &TableRef,
    schema: &Schema,
    profile: &StatementProfile,
) -> BoxedStrategy<InsertStatement> {
    let table_name = table.qualified_name();
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let value_strats = value_strategies_for_table(table, schema, profile);

    value_strats
        .into_iter()
        .collect::<Vec<_>>()
        .prop_map(move |values| InsertStatement {
            table: table_name.clone(),
            columns: col_names.clone(),
            values,
            conflict_action: ConflictAction::None,
        })
        .boxed()
}

/// Generate an INSERT OR REPLACE statement for a table with a primary key.
pub fn insert_or_replace_for_table(
    table: &TableRef,
    schema: &Schema,
    profile: &StatementProfile,
) -> BoxedStrategy<InsertStatement> {
    let table_name = table.qualified_name();
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let value_strats = value_strategies_for_table(table, schema, profile);

    value_strats
        .into_iter()
        .collect::<Vec<_>>()
        .prop_map(move |values| InsertStatement {
            table: table_name.clone(),
            columns: col_names.clone(),
            values,
            conflict_action: ConflictAction::OrReplace,
        })
        .boxed()
}

/// Generate an INSERT ... ON CONFLICT DO UPDATE (upsert) for a table with a primary key.
pub fn upsert_for_table(
    table: &TableRef,
    schema: &Schema,
    profile: &StatementProfile,
) -> BoxedStrategy<InsertStatement> {
    let table_name = table.qualified_name();
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();

    let pk_cols: Vec<String> = table
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.clone())
        .collect();
    let update_cols: Vec<String> = table
        .columns
        .iter()
        .filter(|c| !c.primary_key)
        .map(|c| c.name.clone())
        .collect();

    let value_strats = value_strategies_for_table(table, schema, profile);

    value_strats
        .into_iter()
        .collect::<Vec<_>>()
        .prop_map(move |values| InsertStatement {
            table: table_name.clone(),
            columns: col_names.clone(),
            values,
            conflict_action: if update_cols.is_empty() {
                ConflictAction::OrReplace
            } else {
                ConflictAction::OnConflictUpdate {
                    conflict_columns: pk_cols.clone(),
                    update_columns: update_cols.clone(),
                }
            },
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::StatementProfile;
    use crate::schema::{ColumnDef, DataType, Table};
    use crate::value::SqlValue;

    #[test]
    fn test_insert_display() {
        let stmt = InsertStatement {
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![
                Expression::Value(SqlValue::Integer(1)),
                Expression::Value(SqlValue::Text("Alice".to_string())),
            ],
            conflict_action: ConflictAction::None,
        };

        let sql = stmt.to_string();
        assert_eq!(sql, "INSERT INTO users (id, name) VALUES (1, 'Alice')");
    }

    #[test]
    fn test_insert_or_replace_display() {
        let stmt = InsertStatement {
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![
                Expression::Value(SqlValue::Integer(1)),
                Expression::Value(SqlValue::Text("Alice".to_string())),
            ],
            conflict_action: ConflictAction::OrReplace,
        };

        let sql = stmt.to_string();
        assert_eq!(
            sql,
            "INSERT OR REPLACE INTO users (id, name) VALUES (1, 'Alice')"
        );
    }

    #[test]
    fn test_upsert_display() {
        let stmt = InsertStatement {
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![
                Expression::Value(SqlValue::Integer(1)),
                Expression::Value(SqlValue::Text("Alice".to_string())),
            ],
            conflict_action: ConflictAction::OnConflictUpdate {
                conflict_columns: vec!["id".to_string()],
                update_columns: vec!["name".to_string()],
            },
        };

        let sql = stmt.to_string();
        assert_eq!(
            sql,
            "INSERT INTO users (id, name) VALUES (1, 'Alice') ON CONFLICT(id) DO UPDATE SET name = excluded.name"
        );
    }

    #[test]
    fn test_insert_with_function() {
        let stmt = InsertStatement {
            table: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            values: vec![
                Expression::Value(SqlValue::Integer(1)),
                Expression::function_call(
                    "UPPER",
                    vec![Expression::Value(SqlValue::Text("alice".to_string()))],
                ),
            ],
            conflict_action: ConflictAction::None,
        };

        let sql = stmt.to_string();
        assert_eq!(
            sql,
            "INSERT INTO users (id, name) VALUES (1, UPPER('alice'))"
        );
    }

    proptest::proptest! {
        #[test]
        fn generated_insert_is_valid(
            stmt in {
                let table = Table::new(
                    "test",
                    vec![
                        ColumnDef::new("id", DataType::Integer).primary_key(),
                        ColumnDef::new("name", DataType::Text),
                    ],
                );
                // Use empty schema - INSERT values don't need to reference other tables
                let schema = Schema::default();
                let table_ref: TableRef = table.into();
                insert_for_table(&table_ref, &schema, &StatementProfile::default())
            }
        ) {
            let sql = stmt.to_string();
            proptest::prop_assert!(sql.starts_with("INSERT INTO test"));
            proptest::prop_assert!(sql.contains("VALUES"));
        }
    }

    proptest::proptest! {
        #[test]
        fn generated_upsert_is_valid(
            stmt in {
                let table = Table::new(
                    "test",
                    vec![
                        ColumnDef::new("id", DataType::Integer).primary_key(),
                        ColumnDef::new("name", DataType::Text),
                    ],
                );
                let schema = Schema::default();
                let table_ref: TableRef = table.into();
                upsert_for_table(&table_ref, &schema, &StatementProfile::default())
            }
        ) {
            let sql = stmt.to_string();
            proptest::prop_assert!(sql.starts_with("INSERT INTO test"));
            proptest::prop_assert!(sql.contains("ON CONFLICT(id) DO UPDATE SET name = excluded.name"));
        }
    }
}
