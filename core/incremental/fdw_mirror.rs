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
            let ty = self.declared_type(idx, column);
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

    /// The type the mirror declares for column `idx`, which is the source's own
    /// except where that would make the column an alias of the rowid.
    ///
    /// A sole identity column typed exactly `INTEGER` would be one, and a rowid
    /// alias breaks the mirror twice over: it gets no automatic index, so the
    /// index the creation path writes is an orphan the schema layer refuses to
    /// reparse; and a NULL in it is handed a generated rowid instead of being
    /// refused. `INT` has the same affinity and is not an alias.
    fn declared_type<'a>(&self, idx: usize, column: &'a Column) -> &'a str {
        let ty = if column.ty_str.is_empty() {
            "TEXT"
        } else {
            column.ty_str.as_str()
        };
        let sole_identity = self.identity.as_slice() == [idx as u32];
        if sole_identity && crate::util::type_from_name(ty).1 {
            return "INT";
        }
        ty
    }
}

/// What a sweep does with a source that hands it two rows sharing an identity.
///
/// `CREATE` has no choice — the mirror's primary key refuses them — so the seam
/// exists to let a sweep be told to differ. Nothing constructs [`LastWins`];
/// it is the shape a driver-facing knob would take, kept honest by
/// [`MirrorSync::guard_sql`] being the single place the two diverge.
///
/// [`LastWins`]: DuplicatePolicy::LastWins
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicatePolicy {
    /// Refuse the sweep, naming the source. Symmetric with `CREATE`.
    Refuse,
    /// Let the upsert collapse them. Which row survives is scan order, which no
    /// driver promises.
    #[allow(dead_code)]
    LastWins,
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
    /// How the sweep answers a source repeating an identity. Private so the
    /// seam cannot be opened from outside this module by accident.
    policy: DuplicatePolicy,
}

/// What the guard reports when the identity columns are NULL, and when they
/// repeat. The guard has to say which, because the two are different broken
/// promises with different fixes.
const GUARD_NULL: &str = "null";
const GUARD_DUPLICATE: &str = "duplicate";

impl MirrorSync {
    pub fn new(spec: &MirrorSpec, scan_query: String) -> Self {
        Self {
            // The only construction site, and the only place the policy is
            // chosen: a sweep refuses what `CREATE` refuses.
            policy: DuplicatePolicy::Refuse,
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
    /// columns are `NOT NULL` and together they are the `PRIMARY KEY`. Both are
    /// always enforced, including for a single `INTEGER` identity — which
    /// [`MirrorSpec::declared_type`] keeps from becoming a rowid alias, where
    /// neither would be. Each means a different broken promise by the driver and
    /// has a different fix, so they are reported apart. Anything else cannot
    /// arise, and is passed through rather than guessed at.
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

    /// A pushed change carries a different number of values than the table it
    /// claims to describe.
    ///
    /// A push is positional — value *i* is column *i* of the declared schema —
    /// so a payload of the wrong width names no row the engine can act on.
    /// Binding the values that did arrive would leave the rest NULL, which for
    /// an insert fabricates cells the source never had and for a retraction
    /// reads an identity column that is not there.
    pub fn width_violation(&self, given: usize) -> LimboError {
        LimboError::InvalidArgument(format!(
            "foreign table '{}' pushed a change carrying {given} value(s), \
             but the table declares {} column(s) ({}); \
             a pushed change must carry one value per declared column",
            self.source_table,
            self.columns.len(),
            self.columns.join(", ")
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
    /// Costs three scans of the foreign source: one to check the identity
    /// contract still holds, one to upsert, one to bound the anti-join. The
    /// alternative — materialising the first scan into a staging table —
    /// trades that for a second copy of the data and per-sync DDL.
    ///
    /// The guard comes first so a source that broke its promise costs the sweep
    /// nothing but the scan: no mirror row is written, so no rowid moves and no
    /// delta is staged.
    pub fn sweep_sql(&self) -> Vec<String> {
        let table = self.table_ident();
        let identity = self.identity_list();
        let scan = &self.scan_query;

        let mut statements = Vec::with_capacity(3);
        statements.extend(self.guard_sql());
        // `WHERE true` disambiguates `ON CONFLICT` from a SELECT's own tail.
        statements.push(format!(
            "INSERT INTO {table} SELECT * FROM ({scan}) WHERE true {}",
            self.upsert_tail()
        ));
        statements.push(format!(
            "DELETE FROM {table} WHERE ({identity}) NOT IN (SELECT {identity} FROM ({scan}))"
        ));
        statements
    }

    /// The statement that refuses a scan the identity contract no longer holds
    /// for, or `None` when duplicates are allowed to collapse.
    ///
    /// It emits a row only to refuse, and that row says which promise broke.
    /// `count(DISTINCT …)` cannot: it ignores NULLs, so a single NULL identity
    /// makes it disagree with `count(*)` and report a duplicate that is not
    /// there — and SQLite has no `count(DISTINCT a, b)` for a composite
    /// identity anyway. Grouping answers both: a group's size counts repeats,
    /// and a per-row NULL flag survives the grouping to be checked first.
    ///
    /// The scan is named once, because naming it twice would scan it twice
    /// (`test_scan_named_once_and_read_twice`).
    fn guard_sql(&self) -> Option<String> {
        if self.policy == DuplicatePolicy::LastWins {
            return None;
        }
        let identity = self.identity_list();
        let any_null = self
            .identity
            .iter()
            .map(|i| format!("{} IS NULL", Self::ident(&self.columns[*i])))
            .collect::<Vec<_>>()
            .join(" OR ");
        let scan = &self.scan_query;
        Some(format!(
            "SELECT CASE WHEN any_null > 0 THEN '{GUARD_NULL}' \
             ELSE '{GUARD_DUPLICATE}' END \
             FROM (SELECT max(identity_is_null) AS any_null, \
             max(identity_rows) AS max_rows FROM \
             (SELECT ({any_null}) AS identity_is_null, count(*) AS identity_rows \
             FROM ({scan}) GROUP BY {identity})) \
             WHERE any_null > 0 OR max_rows > 1"
        ))
    }

    /// Read the guard's refusal row as the broken promise it stands for.
    pub fn guard_refusal(&self, marker: &str) -> LimboError {
        match marker {
            GUARD_NULL => self.null_identity_error(),
            GUARD_DUPLICATE => self.duplicate_identity_error(),
            other => unreachable!("the mirror guard emits no marker but {other}"),
        }
    }

    /// The `ON CONFLICT` clause that makes an insert of an already-mirrored
    /// identity land on the row it already has.
    ///
    /// Two things ride on it and both are why the sweep and a push must share
    /// it: the row keeps its rowid, so a later retraction carries the identity
    /// its insertion had; and the update is guarded by a value comparison, so a
    /// row that did not actually change produces no DML and therefore no delta.
    fn upsert_tail(&self) -> String {
        let identity = self.identity_list();
        let changed = self.non_identity_columns();
        if changed.is_empty() {
            // Nothing to update: the row *is* its identity.
            return format!("ON CONFLICT({identity}) DO NOTHING");
        }
        let table = self.table_ident();
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
        format!("ON CONFLICT({identity}) DO UPDATE SET {assignments} WHERE {differs}")
    }

    /// Applies one pushed row. Takes one parameter per mirror column, in
    /// declaration order.
    ///
    /// A push may carry a row the view's predicate excludes: the mirror is
    /// scoped by that predicate but the compiled circuit re-applies it anyway,
    /// so an over-approximating push costs storage, never correctness.
    pub fn push_upsert_sql(&self) -> String {
        let params = (1..=self.columns.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO {} VALUES ({params}) {}",
            self.table_ident(),
            self.upsert_tail()
        )
    }

    /// Retracts one pushed row. Takes one parameter per identity column, in
    /// identity order.
    pub fn push_delete_sql(&self) -> String {
        let predicate = self
            .identity
            .iter()
            .enumerate()
            .map(|(param, column)| {
                format!("{} = ?{}", Self::ident(&self.columns[*column]), param + 1)
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        format!("DELETE FROM {} WHERE {predicate}", self.table_ident())
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

    /// The sweep's upsert, whatever precedes it.
    fn upsert(sync: &MirrorSync) -> String {
        let sql = sync.sweep_sql();
        sql[sql.len() - 2].clone()
    }

    /// The sweep's anti-join delete.
    fn anti_join(sync: &MirrorSync) -> String {
        sync.sweep_sql().last().unwrap().clone()
    }

    #[test]
    fn sweep_upsert_updates_only_the_columns_that_differ() {
        let sql = upsert(&sync(crate::alloc::vec![0]));
        assert!(
            sql.contains("ON CONFLICT(uuid) DO UPDATE SET session_id = excluded.session_id, body = excluded.body"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "WHERE __turso_internal_fdw_mirror_v1_mv__cc_message_fdw.session_id IS NOT excluded.session_id"
            ),
            "the update must be guarded, or an unchanged row emits a delta: {sql}"
        );
        assert!(
            !sql.contains("uuid = excluded.uuid"),
            "identity columns are equal by construction: {sql}"
        );
    }

    /// The scan is wrapped so `ON CONFLICT` cannot be read as the SELECT's own
    /// tail, and it is the view-scoped scan, not a whole-table one.
    #[test]
    fn sweep_upsert_reads_the_view_scoped_scan() {
        let sql = upsert(&sync(crate::alloc::vec![0]));
        assert!(
            sql.contains(
                "SELECT * FROM (SELECT * FROM cc_message_fdw WHERE session_id = 's1') WHERE true"
            ),
            "{sql}"
        );
    }

    /// Rows that left the source are found by anti-join against the same scan.
    #[test]
    fn sweep_deletes_rows_the_scan_no_longer_returns() {
        let sql = anti_join(&sync(crate::alloc::vec![1, 0]));
        assert!(
            sql.contains("WHERE (session_id, uuid) NOT IN (SELECT session_id, uuid FROM ("),
            "composite identity must compare as a row value: {sql}"
        );
    }

    /// A mirror that is all identity has nothing to update.
    #[test]
    fn sweep_upsert_does_nothing_when_every_column_identifies() {
        let sql = upsert(&sync(crate::alloc::vec![0, 1, 2]));
        assert!(sql.ends_with("DO NOTHING"), "{sql}");
    }

    /// The guard runs before the sweep writes anything, or a refusal would
    /// leave the mirror half-swept.
    #[test]
    fn sweep_checks_the_identity_contract_before_it_writes() {
        let sql = sync(crate::alloc::vec![0]).sweep_sql();
        assert_eq!(sql.len(), 3, "{sql:?}");
        assert!(
            sql[0].starts_with("SELECT CASE WHEN any_null"),
            "{}",
            sql[0]
        );
    }

    /// A single NULL identity must not read as a duplicate, so the guard counts
    /// NULLs apart from repeats and reports them first.
    #[test]
    fn guard_separates_null_identities_from_repeated_ones() {
        let sql = sync(crate::alloc::vec![0]).sweep_sql()[0].clone();
        assert!(sql.contains("(uuid IS NULL) AS identity_is_null"), "{sql}");
        assert!(
            !sql.contains("count(DISTINCT"),
            "count(DISTINCT) ignores NULLs and would misreport them: {sql}"
        );
        assert!(sql.contains("GROUP BY uuid"), "{sql}");
        assert!(sql.ends_with("WHERE any_null > 0 OR max_rows > 1"), "{sql}");
    }

    /// The scan is named once. Naming it twice would cost a fourth read of the
    /// foreign source.
    #[test]
    fn guard_reads_the_scan_once() {
        let sql = sync(crate::alloc::vec![0]).sweep_sql()[0].clone();
        assert_eq!(sql.matches("FROM cc_message_fdw").count(), 1, "{sql}");
    }

    /// SQLite has no `count(DISTINCT a, b)`, so a composite identity has to be
    /// checked by grouping on every column and flagging a NULL in any of them.
    #[test]
    fn guard_handles_a_composite_identity() {
        let sql = sync(crate::alloc::vec![1, 0]).sweep_sql()[0].clone();
        assert!(
            sql.contains("(session_id IS NULL OR uuid IS NULL) AS identity_is_null"),
            "{sql}"
        );
        assert!(sql.contains("GROUP BY session_id, uuid"), "{sql}");
    }

    /// The seam: `LastWins` is exactly the absence of the guard, so opening the
    /// knob later adds no second code path to keep in step.
    #[test]
    fn last_wins_omits_the_guard_and_changes_nothing_else() {
        let refusing = sync(crate::alloc::vec![0]);
        let mut collapsing = refusing.clone();
        collapsing.policy = DuplicatePolicy::LastWins;

        assert_eq!(collapsing.guard_sql(), None);
        assert_eq!(collapsing.sweep_sql(), refusing.sweep_sql()[1..].to_vec());
    }

    /// The guard's marker decides which broken promise the user is told about.
    #[test]
    fn a_guard_refusal_names_the_promise_that_broke() {
        let sync = sync(crate::alloc::vec![0]);
        assert!(sync
            .guard_refusal(GUARD_NULL)
            .to_string()
            .contains("is NULL"));
        assert!(sync
            .guard_refusal(GUARD_DUPLICATE)
            .to_string()
            .contains("more than one row"));
    }

    /// A rebuild clears before it fills, or the identity index rejects the refill.
    #[test]
    fn rebuild_clears_before_it_fills() {
        let sql = sync(crate::alloc::vec![0]).rebuild_sql();
        assert!(sql[0].starts_with("DELETE FROM"), "{}", sql[0]);
        assert!(sql[1].starts_with("INSERT INTO"), "{}", sql[1]);
    }

    /// A push must land on the row the sweep would have landed on, or its
    /// retraction would carry a different identity than its insertion.
    #[test]
    fn push_upsert_shares_the_sweeps_conflict_handling() {
        let sync = sync(crate::alloc::vec![0]);
        let push = sync.push_upsert_sql();
        let sweep = &upsert(&sync);
        let tail = "ON CONFLICT(uuid) DO UPDATE SET";
        let (_, push_tail) = push.split_once(tail).expect("{push}");
        let (_, sweep_tail) = sweep.split_once(tail).expect("{sweep}");
        assert_eq!(push_tail, sweep_tail);
        assert!(push.contains("VALUES (?1, ?2, ?3)"), "{push}");
    }

    #[test]
    fn push_delete_keys_on_every_identity_column() {
        let sql = sync(crate::alloc::vec![1, 0]).push_delete_sql();
        assert!(
            sql.ends_with("WHERE session_id = ?1 AND uuid = ?2"),
            "{sql}"
        );
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

    fn int_spec(identity: Vec<u32>) -> MirrorSpec {
        MirrorSpec {
            source_table: "msg_fdw".to_string(),
            mirror_table: mirror_table_name("mv", "msg_fdw"),
            columns: crate::alloc::vec![
                col("id", "INTEGER"),
                col("seq", "INTEGER"),
                col("body", "TEXT")
            ],
            identity,
        }
    }

    /// A lone `INTEGER PRIMARY KEY` would alias the rowid: no automatic index
    /// for the creation path's index to match, and NULLs handed a rowid instead
    /// of being refused.
    #[test]
    fn create_sql_keeps_a_single_integer_identity_off_the_rowid() {
        let sql = int_spec(crate::alloc::vec![0]).create_sql();
        assert!(sql.contains("id INT NOT NULL"), "{sql}");
        assert!(sql.contains("PRIMARY KEY (id)"), "{sql}");
        assert!(
            sql.contains("seq INTEGER,"),
            "only the identity moves: {sql}"
        );
    }

    /// A composite primary key is never a rowid alias, so nothing needs doing.
    #[test]
    fn create_sql_leaves_composite_integer_identities_alone() {
        let sql = int_spec(crate::alloc::vec![0, 1]).create_sql();
        assert!(sql.contains("id INTEGER NOT NULL"), "{sql}");
        assert!(sql.contains("seq INTEGER NOT NULL"), "{sql}");
    }

    #[test]
    fn index_name_matches_the_automatic_primary_key_index() {
        assert_eq!(
            spec(crate::alloc::vec![0]).index_name(),
            "sqlite_autoindex___turso_internal_fdw_mirror_v1_mv__cc_message_fdw_1"
        );
    }
}
