//! Mirrors of foreign-table rows that back incremental materialized views.
//!
//! A materialized view cannot be maintained incrementally from a foreign table
//! directly: the IVM circuit is fed by btree DML deltas, and a foreign table has
//! no write path at all. It also cannot be maintained by diffing the view
//! against a fresh scan, because a retraction needs the *old row values* and the
//! view only stores query results.
//!
//! So each view keeps a mirror: an internal btree table shadowing the foreign
//! rows that view reads. Syncing the mirror emits ordinary DML deltas, and
//! everything downstream — the circuit, chained views, CDC — is the existing,
//! unmodified machinery.
//!
//! A mirror exists only when the driver declares
//! [`ForeignDataWrapper::identity_columns`], which is what makes a row
//! recognisable across scans. Without it the view keeps snapshot semantics.
//!
//! [`ForeignDataWrapper::identity_columns`]: crate::foreign::ForeignDataWrapper::identity_columns

use crate::schema::{Column, Schema, Table, FDW_MIRROR_TABLE_PREFIX};
use crate::{LimboError, Result};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use turso_parser::ast;

/// Everything needed to create and sync one view's mirror of one foreign table.
#[derive(Debug, Clone)]
pub struct MirrorSpec {
    /// The foreign table being shadowed.
    pub source_table: String,
    /// The internal btree table holding the shadowed rows.
    pub mirror_table: String,
    /// Columns of the foreign table, mirrored verbatim.
    pub columns: Vec<Column>,
    /// Indices into `columns` whose values identify a row across scans.
    pub identity: Vec<u32>,
}

/// Name the mirror of `source_table` for `view_name`.
///
/// Mirrors are per-view rather than per-foreign-table because a view's
/// predicate scopes which rows are fetched, and a driver may *require* a
/// qualifier (`KeyColumn::required`) and so be unable to enumerate the table at
/// all.
pub fn mirror_table_name(view_name: &str, source_table: &str) -> String {
    format!("{FDW_MIRROR_TABLE_PREFIX}{view_name}__{source_table}")
}

impl MirrorSpec {
    /// Name of the automatic index SQLite creates for the declared PRIMARY KEY.
    pub fn index_name(&self) -> String {
        format!(
            "{}{}_1",
            crate::util::PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX,
            self.mirror_table
        )
    }

    /// `CREATE TABLE` for the mirror.
    ///
    /// A rowid table, not `WITHOUT ROWID`: the rowid *is* the stable row
    /// identity the IVM circuit keys on, so it must exist and must survive an
    /// update of the non-identity columns.
    ///
    /// Identity columns are explicitly `NOT NULL` because `PRIMARY KEY` on a
    /// rowid table does not imply it (a long-standing SQLite compatibility
    /// quirk), and NULLs would let two rows share an identity.
    pub fn create_sql(&self) -> String {
        let mut defs: Vec<String> = Vec::with_capacity(self.columns.len() + 1);
        for (idx, column) in self.columns.iter().enumerate() {
            let name = column
                .name
                .as_deref()
                .expect("mirrored foreign columns are always named");
            let ident = ast::Name::exact(name.to_string()).as_ident();
            let ty = if column.ty_str.is_empty() {
                "TEXT"
            } else {
                column.ty_str.as_str()
            };
            let not_null = if self.identity.contains(&(idx as u32)) {
                " NOT NULL"
            } else {
                ""
            };
            defs.push(format!("{ident} {ty}{not_null}"));
        }

        let identity_idents: Vec<String> = self
            .identity
            .iter()
            .map(|idx| {
                let name = self.columns[*idx as usize]
                    .name
                    .as_deref()
                    .expect("mirrored foreign columns are always named");
                ast::Name::exact(name.to_string()).as_ident()
            })
            .collect();
        defs.push(format!("PRIMARY KEY ({})", identity_idents.join(", ")));

        let table_ident = ast::Name::exact(self.mirror_table.clone()).as_ident();
        format!("CREATE TABLE {table_ident} ({})", defs.join(", "))
    }
}

/// Build the mirror specs for a view over `referenced_tables`.
///
/// Returns an empty vec when the view reads no identity-declaring foreign
/// table, which is the signal to keep today's snapshot behaviour.
pub fn mirror_specs_for_view(
    view_name: &str,
    referenced_table_names: &[String],
    schema: &Schema,
) -> Result<Vec<MirrorSpec>> {
    let mut specs = Vec::new();
    for source_table in referenced_table_names {
        let Some(table) = schema.get_table(source_table) else {
            continue;
        };
        let Table::Virtual(vtab) = table.as_ref() else {
            continue;
        };
        let Some(fdw) = vtab.foreign_wrapper() else {
            continue;
        };
        let Some(identity) = fdw.identity_columns() else {
            continue;
        };

        let columns = vtab.columns.clone();
        for idx in identity {
            let column = columns.get(*idx as usize).ok_or_else(|| {
                LimboError::ParseError(format!(
                    "foreign table '{source_table}' declares identity column {idx}, \
                     but it only has {} columns",
                    columns.len()
                ))
            })?;
            if column.name.is_none() {
                return Err(LimboError::ParseError(format!(
                    "foreign table '{source_table}' declares identity column {idx}, \
                     which has no name"
                )));
            }
        }

        specs.push(MirrorSpec {
            source_table: source_table.clone(),
            mirror_table: mirror_table_name(view_name, source_table),
            columns,
            identity: identity.to_vec(),
        });
    }
    Ok(specs)
}

/// Point every source named in `mirrors` at its mirror table.
///
/// The source's name is kept as an alias, so column qualifiers written against
/// it (`msg_fdw.uuid`) still resolve once the table underneath has changed.
///
/// This visits exactly the table positions
/// `IncrementalView::extract_all_tables` reads, which is what keeps the
/// rewritten statement's referenced tables, its compiled circuit and its
/// populate scan naming the same thing.
pub fn rewrite_sources_to_mirrors(select: &mut ast::Select, mirrors: &HashMap<String, String>) {
    rewrite_select(select, mirrors, &HashSet::default());
}

fn rewrite_select(
    select: &mut ast::Select,
    mirrors: &HashMap<String, String>,
    parent_cte_names: &HashSet<String>,
) {
    let mut cte_names = parent_cte_names.clone();
    if let Some(with) = select.with.as_mut() {
        for cte in with.ctes.iter() {
            cte_names.insert(cte.tbl_name.as_str().to_string());
        }
        for cte in with.ctes.iter_mut() {
            rewrite_select(&mut cte.select, mirrors, &cte_names);
        }
    }

    rewrite_one_statement(&mut select.body.select, mirrors, &cte_names);
    for compound in select.body.compounds.iter_mut() {
        rewrite_one_statement(&mut compound.select, mirrors, &cte_names);
    }
}

fn rewrite_one_statement(
    select: &mut ast::OneSelect,
    mirrors: &HashMap<String, String>,
    cte_names: &HashSet<String>,
) {
    let ast::OneSelect::Select {
        from: Some(from), ..
    } = select
    else {
        return;
    };
    rewrite_select_table(from.select.as_mut(), mirrors, cte_names);
    for join in from.joins.iter_mut() {
        rewrite_select_table(join.table.as_mut(), mirrors, cte_names);
    }
}

fn rewrite_select_table(
    table: &mut ast::SelectTable,
    mirrors: &HashMap<String, String>,
    cte_names: &HashSet<String>,
) {
    let ast::SelectTable::Table(name, alias, _) = table else {
        return;
    };
    let source = name.name.as_str().to_string();
    if cte_names.contains(&source) {
        return;
    }
    let Some(mirror) = mirrors.get(&source) else {
        return;
    };
    name.name = ast::Name::exact(mirror.clone());
    if alias.is_none() {
        *alias = Some(ast::As::As(ast::Name::exact(source)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColDef, Type};

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

    fn spec(identity: Vec<u32>) -> MirrorSpec {
        MirrorSpec {
            source_table: "cc_message_fdw".to_string(),
            mirror_table: mirror_table_name("mv", "cc_message_fdw"),
            columns: crate::alloc::vec![
                col("uuid", "TEXT"),
                col("session_id", "TEXT"),
                col("body", "TEXT")
            ],
            identity,
        }
    }

    #[test]
    fn mirror_name_is_scoped_to_the_view() {
        assert_eq!(
            mirror_table_name("chat_view", "cc_message_fdw"),
            "__turso_internal_fdw_mirror_v1_chat_view__cc_message_fdw"
        );
    }

    #[test]
    fn mirror_name_uses_the_reserved_internal_prefix() {
        // RESERVED_TABLE_PREFIXES already blocks users from creating it.
        assert!(mirror_table_name("v", "t").starts_with("__turso_internal_"));
    }

    #[test]
    fn create_sql_declares_single_column_identity_as_primary_key() {
        let sql = spec(crate::alloc::vec![0]).create_sql();
        assert!(sql.contains("PRIMARY KEY (uuid)"), "{sql}");
    }

    /// A rowid table: the rowid is the IVM input key and must survive updates.
    #[test]
    fn create_sql_is_a_rowid_table() {
        let sql = spec(crate::alloc::vec![0]).create_sql();
        assert!(!sql.contains("WITHOUT ROWID"), "{sql}");
    }

    /// `PRIMARY KEY` on a rowid table does not imply NOT NULL, so identity
    /// columns must say so explicitly or NULLs can share an identity.
    #[test]
    fn create_sql_marks_identity_columns_not_null() {
        let sql = spec(crate::alloc::vec![0]).create_sql();
        assert!(sql.contains("uuid TEXT NOT NULL"), "{sql}");
        assert!(
            !sql.contains("body TEXT NOT NULL"),
            "non-identity columns must stay nullable: {sql}"
        );
    }

    #[test]
    fn create_sql_supports_composite_identity() {
        let sql = spec(crate::alloc::vec![1, 0]).create_sql();
        assert!(sql.contains("PRIMARY KEY (session_id, uuid)"), "{sql}");
        assert!(sql.contains("uuid TEXT NOT NULL"), "{sql}");
        assert!(sql.contains("session_id TEXT NOT NULL"), "{sql}");
    }

    #[test]
    fn index_name_matches_the_automatic_primary_key_index() {
        assert_eq!(
            spec(crate::alloc::vec![0]).index_name(),
            "sqlite_autoindex___turso_internal_fdw_mirror_v1_mv__cc_message_fdw_1"
        );
    }
}
