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

/// One mirror of a live view, with everything needed to keep it in step with
/// the foreign table it shadows.
///
/// Built once when the view is loaded, because the scan is only derivable from
/// the view's pre-redirect statement.
#[derive(Debug, Clone)]
pub struct MirrorSync {
    /// The foreign table being shadowed. Carried for diagnostics: the identity
    /// constraint fires on the mirror, but the mistake is the source's.
    pub source_table: String,
    /// The internal btree table holding the shadowed rows.
    pub mirror_table: String,
    /// Column names of the mirror, in declaration order.
    pub columns: Vec<String>,
    /// Indices into `columns` whose values identify a row across scans.
    pub identity: Vec<usize>,
    /// Scan of the foreign table, scoped by the view's predicate.
    pub scan_query: String,
}

impl MirrorSync {
    pub fn new(spec: &MirrorSpec, scan_query: String) -> Self {
        Self {
            source_table: spec.source_table.clone(),
            mirror_table: spec.mirror_table.clone(),
            columns: spec
                .columns
                .iter()
                .map(|c| {
                    c.name
                        .clone()
                        .expect("mirrored foreign columns are always named")
                })
                .collect(),
            identity: spec.identity.iter().map(|i| *i as usize).collect(),
            scan_query,
        }
    }

    fn table_ident(&self) -> String {
        ast::Name::exact(self.mirror_table.clone()).as_ident()
    }

    /// Restate a constraint violation on the mirror in terms of the source.
    ///
    /// The mirror carries exactly two constraints, both on the identity: the
    /// columns are `NOT NULL` and together they are the `PRIMARY KEY`. Each
    /// means a different broken promise by the driver and has a different fix,
    /// so they are reported apart. Anything else cannot arise, and is passed
    /// through rather than guessed at.
    pub fn identity_violation(&self, violation: LimboError) -> LimboError {
        let LimboError::Constraint(message) = &violation else {
            return violation;
        };
        // The wording is `op_halt`'s, which is where every constraint failure
        // on a btree write is turned into this error.
        if message.starts_with("NOT NULL constraint failed") {
            self.null_identity_error()
        } else {
            self.duplicate_identity_error()
        }
    }

    fn identity_column_list(&self) -> String {
        self.identity
            .iter()
            .map(|i| self.columns[*i].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The driver returned a row with no identity at all.
    ///
    /// Such a row cannot be recognised on a later scan, so neither its update
    /// nor its removal could ever be propagated to the view.
    pub fn null_identity_error(&self) -> LimboError {
        LimboError::Constraint(format!(
            "foreign table '{}' returned a row whose declared identity ({}) is NULL; \
             a NULL identity cannot be matched across scans and is not supported",
            self.source_table,
            self.identity_column_list()
        ))
    }

    /// The driver returned two rows the engine cannot tell apart.
    pub fn duplicate_identity_error(&self) -> LimboError {
        LimboError::Constraint(format!(
            "foreign table '{}' returned more than one row with the same identity ({}); \
             a declared identity must identify a row uniquely within a scan",
            self.source_table,
            self.identity_column_list()
        ))
    }

    fn ident(name: &str) -> String {
        ast::Name::exact(name.to_string()).as_ident()
    }

    /// `c1, c2` over the identity columns.
    fn identity_list(&self) -> String {
        self.identity
            .iter()
            .map(|i| Self::ident(&self.columns[*i]))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn non_identity_columns(&self) -> Vec<&str> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.identity.contains(i))
            .map(|(_, name)| name.as_str())
            .collect()
    }

    /// Statements that discard the mirror and refill it from the source.
    ///
    /// The clear precedes the fill because this runs again on a mirror that is
    /// already full, where re-inserting would collide on the identity index. It
    /// is DML rather than a btree wipe so that index is maintained along with
    /// the table.
    ///
    /// Every row is retracted and re-inserted, so this is only usable where the
    /// view is rebuilt from scratch anyway. `sweep_sql` is the incremental form.
    pub fn rebuild_sql(&self) -> Vec<String> {
        let table = self.table_ident();
        crate::alloc::vec![
            format!("DELETE FROM {table}"),
            // A mirror's columns are the foreign table's columns in order, so
            // the scan's `SELECT *` aligns positionally.
            format!("INSERT INTO {table} {}", self.scan_query),
        ]
    }

    /// Statements that bring the mirror in step with the source, touching only
    /// the rows that actually differ.
    ///
    /// An unchanged row produces no DML at all — the `DO UPDATE` is guarded by
    /// a value comparison — and therefore no delta, which is what keeps a
    /// no-change sync from churning the view's state and spamming CDC. Changed
    /// rows keep their rowid, so their retraction carries the identity their
    /// insertion had.
    ///
    /// Costs two scans of the foreign source: one to upsert, one to bound the
    /// anti-join. The alternative — materialising the first scan into a staging
    /// table — trades that for a second copy of the data and per-sync DDL.
    pub fn sweep_sql(&self) -> Vec<String> {
        let table = self.table_ident();
        let identity = self.identity_list();
        let scan = &self.scan_query;

        let changed = self.non_identity_columns();
        // `WHERE true` disambiguates `ON CONFLICT` from a SELECT's own tail.
        let upsert = if changed.is_empty() {
            // Nothing to update: the row *is* its identity.
            format!("INSERT INTO {table} SELECT * FROM ({scan}) WHERE true ON CONFLICT({identity}) DO NOTHING")
        } else {
            let assignments = changed
                .iter()
                .map(|c| {
                    let c = Self::ident(c);
                    format!("{c} = excluded.{c}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            // `IS NOT` rather than `<>` so a NULL on either side compares.
            let differs = changed
                .iter()
                .map(|c| {
                    let c = Self::ident(c);
                    format!("{table}.{c} IS NOT excluded.{c}")
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            format!(
                "INSERT INTO {table} SELECT * FROM ({scan}) WHERE true \
                 ON CONFLICT({identity}) DO UPDATE SET {assignments} WHERE {differs}"
            )
        };

        crate::alloc::vec![
            upsert,
            format!(
                "DELETE FROM {table} WHERE ({identity}) NOT IN (SELECT {identity} FROM ({scan}))"
            ),
        ]
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

    fn sync(identity: Vec<u32>) -> MirrorSync {
        MirrorSync::new(
            &spec(identity),
            "SELECT * FROM cc_message_fdw WHERE session_id = 's1'".to_string(),
        )
    }

    #[test]
    fn sweep_upsert_updates_only_the_columns_that_differ() {
        let sql = sync(crate::alloc::vec![0]).sweep_sql();
        assert!(
            sql[0].contains("ON CONFLICT(uuid) DO UPDATE SET session_id = excluded.session_id, body = excluded.body"),
            "{}", sql[0]
        );
        assert!(
            sql[0].contains(
                "WHERE __turso_internal_fdw_mirror_v1_mv__cc_message_fdw.session_id IS NOT excluded.session_id"
            ),
            "the update must be guarded, or an unchanged row emits a delta: {}",
            sql[0]
        );
        assert!(
            !sql[0].contains("uuid = excluded.uuid"),
            "identity columns are equal by construction: {}",
            sql[0]
        );
    }

    /// The scan is wrapped so `ON CONFLICT` cannot be read as the SELECT's own
    /// tail, and it is the view-scoped scan, not a whole-table one.
    #[test]
    fn sweep_upsert_reads_the_view_scoped_scan() {
        let sql = sync(crate::alloc::vec![0]).sweep_sql();
        assert!(
            sql[0].contains(
                "SELECT * FROM (SELECT * FROM cc_message_fdw WHERE session_id = 's1') WHERE true"
            ),
            "{}",
            sql[0]
        );
    }

    /// Rows that left the source are found by anti-join against the same scan.
    #[test]
    fn sweep_deletes_rows_the_scan_no_longer_returns() {
        let sql = sync(crate::alloc::vec![1, 0]).sweep_sql();
        assert!(
            sql[1].contains("WHERE (session_id, uuid) NOT IN (SELECT session_id, uuid FROM ("),
            "composite identity must compare as a row value: {}",
            sql[1]
        );
    }

    /// A mirror that is all identity has nothing to update.
    #[test]
    fn sweep_upsert_does_nothing_when_every_column_identifies() {
        let sql = sync(crate::alloc::vec![0, 1, 2]).sweep_sql();
        assert!(sql[0].ends_with("DO NOTHING"), "{}", sql[0]);
    }

    /// A rebuild clears before it fills, or the identity index rejects the refill.
    #[test]
    fn rebuild_clears_before_it_fills() {
        let sql = sync(crate::alloc::vec![0]).rebuild_sql();
        assert!(sql[0].starts_with("DELETE FROM"), "{}", sql[0]);
        assert!(sql[1].starts_with("INSERT INTO"), "{}", sql[1]);
    }

    #[test]
    fn a_not_null_violation_is_not_read_as_a_duplicate() {
        let sync = sync(crate::alloc::vec![0]);
        let null = sync.identity_violation(LimboError::Constraint(
            "NOT NULL constraint failed: t.uuid (19)".to_string(),
        ));
        assert!(null.to_string().contains("is NULL"), "{null}");
        let duplicate = sync.identity_violation(LimboError::Constraint(
            "UNIQUE constraint failed: t.uuid (19)".to_string(),
        ));
        assert!(
            duplicate.to_string().contains("more than one row"),
            "{duplicate}"
        );
    }

    #[test]
    fn index_name_matches_the_automatic_primary_key_index() {
        assert_eq!(
            spec(crate::alloc::vec![0]).index_name(),
            "sqlite_autoindex___turso_internal_fdw_mirror_v1_mv__cc_message_fdw_1"
        );
    }
}
