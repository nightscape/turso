use std::fmt::Display;

use serde::{Deserialize, Serialize};

use crate::model::table::SimValue;

use super::select::Select;

/// Conflict resolution action for INSERT statements.
/// Maps to SQLite's INSERT OR {action} syntax.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConflictAction {
    /// INSERT OR REPLACE - replaces the existing row on conflict
    Replace,
    /// INSERT OR IGNORE - silently ignores the insert on conflict
    Ignore,
    /// INSERT OR ABORT - aborts the current statement (default behavior)
    Abort,
    /// INSERT OR ROLLBACK - rolls back the entire transaction on conflict
    Rollback,
    /// INSERT OR FAIL - fails but keeps prior changes in the transaction
    Fail,
}

impl Display for ConflictAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConflictAction::Replace => write!(f, "REPLACE"),
            ConflictAction::Ignore => write!(f, "IGNORE"),
            ConflictAction::Abort => write!(f, "ABORT"),
            ConflictAction::Rollback => write!(f, "ROLLBACK"),
            ConflictAction::Fail => write!(f, "FAIL"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OnConflict {
    pub target_column: String,
    pub assignments: Vec<UpdateSetItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpdateSetItem {
    pub column: String,
    pub excluded_column: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Insert {
    Values {
        table: String,
        values: Vec<Vec<SimValue>>,
        #[serde(default)]
        on_conflict: Option<OnConflict>,
        /// Optional conflict resolution (INSERT OR REPLACE, etc.)
        conflict: Option<ConflictAction>,
    },
    /// Insert with explicit column list
    ValuesWithColumns {
        table: String,
        columns: Vec<String>,
        values: Vec<Vec<SimValue>>,
    },
    Select {
        table: String,
        #[serde(default)]
        columns: InsertColumns,
        select: Box<Select>,
        /// Optional conflict resolution (INSERT OR REPLACE, etc.)
        conflict: Option<ConflictAction>,
    },
}

/// Whether an INSERT specifies an explicit column list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum InsertColumns {
    /// No column list (`INSERT INTO t ...`).
    #[default]
    Implicit,
    /// Explicit column list (`INSERT INTO t (a, b, c...) ...`)
    Explicit(Vec<String>),
}

impl Insert {
    pub fn table(&self) -> &str {
        match self {
            Insert::Values { table, .. }
            | Insert::ValuesWithColumns { table, .. }
            | Insert::Select { table, .. } => table,
        }
    }

    pub fn rows(&self) -> &[Vec<SimValue>] {
        match self {
            Insert::Values { values, .. } | Insert::ValuesWithColumns { values, .. } => values,
            Insert::Select { .. } => unreachable!(),
        }
    }
}

impl Display for Insert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Insert::Values {
                table,
                values,
                on_conflict,
                conflict,
            } => {
                write!(f, "INSERT ")?;
                if let Some(action) = conflict {
                    write!(f, "OR {action} ")?;
                }
                write!(f, "INTO {table} VALUES ")?;
                for (i, row) in values.iter().enumerate() {
                    if i != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "(")?;
                    for (j, value) in row.iter().enumerate() {
                        if j != 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{value}")?;
                    }
                    write!(f, ")")?;
                }
                if let Some(on_conflict) = &on_conflict {
                    write!(f, " {on_conflict}")?;
                }
                Ok(())
            }
            Insert::ValuesWithColumns {
                table,
                columns,
                values,
            } => {
                write!(f, "INSERT INTO {table} (")?;
                for (i, col) in columns.iter().enumerate() {
                    if i != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{col}")?;
                }
                write!(f, ") VALUES ")?;
                for (i, row) in values.iter().enumerate() {
                    if i != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "(")?;
                    for (j, value) in row.iter().enumerate() {
                        if j != 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{value}")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Insert::Select {
                table,
                columns,
                select,
                conflict,
            } => {
                write!(f, "INSERT ")?;
                if let Some(action) = conflict {
                    write!(f, "OR {action} ")?;
                }
                write!(f, "INTO {table} ")?;
                if let InsertColumns::Explicit(columns) = columns {
                    write!(f, "(")?;
                    for (i, col) in columns.iter().enumerate() {
                        if i != 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{col}")?;
                    }
                    write!(f, ") ")?;
                }
                write!(f, "{select}")
            }
        }
    }
}

impl Display for OnConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ON CONFLICT({}) DO UPDATE SET ", self.target_column)?;
        for (i, a) in self.assignments.iter().enumerate() {
            if i != 0 {
                write!(f, ", ")?;
            }
            write!(f, "{a}")?;
        }
        Ok(())
    }
}

impl Display for UpdateSetItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = excluded.{}", self.column, self.excluded_column)
    }
}
