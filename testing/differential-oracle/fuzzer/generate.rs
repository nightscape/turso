//! SQL generation abstraction layer.
//!
//! Provides a trait-based interface to switch between different SQL generation
//! backends (sql_gen and sql_gen_prop) via a config flag.

use std::collections::HashMap;

use anyhow::Result;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use sql_gen::{Full, Policy, SqlGen, StmtKind};

/// Output of SQL generation with metadata needed by the oracle.
#[derive(Debug, Clone)]
pub struct GeneratedStatement {
    pub sql: String,
    pub is_ddl: bool,
    pub mutates_data: bool,
    pub has_unordered_limit: bool,
    pub unordered_limit_reason: Option<String>,
    /// True for CREATE/DROP MATERIALIZED VIEW — needs split execution
    /// (materialized on Turso, regular view on SQLite).
    pub is_matview_ddl: bool,
    /// For matview DDL: the equivalent regular view SQL to run on SQLite.
    pub sqlite_sql: Option<String>,
    /// For matview CREATE: the output columns of the view's SELECT.
    pub matview_output_columns: Option<Vec<sql_gen_prop::ColumnDef>>,
    /// True for ReopenDatabase — runner should close and reopen the Turso DB.
    pub is_reopen: bool,
}

impl std::fmt::Display for GeneratedStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.sql)
    }
}

/// Which generation backend to use.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum GeneratorKind {
    /// Type-state SQL generator (sql_gen crate)
    #[default]
    SqlGen,
    /// Proptest-based SQL generator (sql_gen_prop crate)
    SqlGenProp,
}

/// Trait abstracting SQL generation backends.
pub trait SqlGenerator {
    /// Generate the next SQL statement given the current schema.
    fn generate(&mut self, schema: &sql_gen::Schema) -> Result<GeneratedStatement> {
        self.generate_with_matviews(schema, &HashMap::new())
    }

    /// Generate the next SQL statement, with awareness of tracked matview info.
    fn generate_with_matviews(
        &mut self,
        schema: &sql_gen::Schema,
        matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
    ) -> Result<GeneratedStatement>;

    /// Take accumulated coverage data, if the backend supports it.
    fn take_coverage(&mut self) -> Option<sql_gen::Coverage> {
        None
    }
}

/// sql_gen (type-state) backend.
pub struct SqlGenBackend {
    ctx: sql_gen::Context,
    policy: Policy,
}

impl SqlGenBackend {
    pub fn new(seed: u64) -> Self {
        let ctx = sql_gen::Context::new_with_seed(seed);
        let mut policy = Policy::default()
            .with_stmt_weights(sql_gen::StmtWeights {
                update: 30,
                ..sql_gen::StmtWeights::default()
            })
            .with_function_config(
                sql_gen::FunctionConfig::deterministic().disable(&["LIKELY", "UNLIKELY"]),
            );
        policy.select_config.require_order_by_with_limit = true;
        // Disable expression values for inserts, enable conflict clauses for updates
        policy.insert_config.expression_value_probability = 0.0;
        policy.insert_config.or_replace_probability = 0.0;
        policy.insert_config.or_ignore_probability = 0.0;
        policy.update_config.expression_value_probability = 0.0;
        policy.update_config.or_replace_probability = 0.1;
        policy.update_config.or_ignore_probability = 0.1;
        // Boost UPDATE FROM coverage
        policy.update_config.from_probability = 0.4;
        policy.update_config.returning_probability = 0.2;
        policy.update_config.self_join_probability = 0.3;
        policy.update_config.join_in_from_probability = 0.3;
        policy.update_config.subquery_from_probability = 0.15;
        policy.update_config.target_alias_probability = 0.2;
        policy.update_config.from_set_reference_probability = 0.5;
        Self { ctx, policy }
    }
}

impl SqlGenerator for SqlGenBackend {
    fn generate_with_matviews(
        &mut self,
        schema: &sql_gen::Schema,
        _matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
    ) -> Result<GeneratedStatement> {
        // sql_gen backend doesn't support matview generation
        let generator: SqlGen<Full> = SqlGen::new(schema.clone(), self.policy.clone());
        let stmt = generator
            .statement(&mut self.ctx)
            .map_err(|e| anyhow::anyhow!("Failed to generate statement: {e}"))?;
        let sql = stmt.to_string();
        let stmt_kind = StmtKind::from(&stmt);
        let is_ddl = stmt_kind.is_ddl();
        let mutates_data = matches!(
            stmt_kind,
            StmtKind::Insert | StmtKind::Update | StmtKind::Delete
        );
        let has_unordered_limit =
            stmt.has_unordered_limit() || stmt.non_unique_order_by_reason(schema).is_some();
        let unordered_limit_reason = stmt
            .unordered_limit_reason()
            .or_else(|| stmt.non_unique_order_by_reason(schema))
            .map(str::to_string);
        Ok(GeneratedStatement {
            sql,
            is_ddl,
            mutates_data,
            has_unordered_limit,
            unordered_limit_reason,
            is_matview_ddl: false,
            sqlite_sql: None,
            matview_output_columns: None,
            is_reopen: false,
        })
    }

    fn take_coverage(&mut self) -> Option<sql_gen::Coverage> {
        Some(self.ctx.take_coverage())
    }
}

/// sql_gen_prop (proptest) backend.
pub struct PropTestBackend {
    test_runner: TestRunner,
    profile: sql_gen_prop::StatementProfile,
}

impl PropTestBackend {
    pub fn new(seed_bytes: [u8; 32], matview: bool) -> Self {
        let test_runner = TestRunner::new_with_rng(
            proptest::test_runner::Config::default(),
            proptest::test_runner::TestRng::from_seed(
                proptest::test_runner::RngAlgorithm::ChaCha,
                &seed_bytes,
            ),
        );
        let mut profile = sql_gen_prop::StatementProfile::default();
        profile
            .generation
            .expression
            .base
            .order_by_allow_integer_positions = false;

        if matview {
            profile.create_materialized_view_weight = 8;
            profile.drop_materialized_view_weight = 2;
            profile.reopen_database_weight = 2;
            // Matviews cannot reference attached databases, so any table
            // created in `temp.*` or `aux.*` is dead weight (and the
            // table-tracking divergence between Turso and SQLite for
            // those schemas eats most of the iteration budget). Restrict
            // CREATE TABLE to the main schema in matview mode.
            profile.create_table.extra.target_schemas =
                sql_gen_prop::SchemaTargetProfile::main_only();
        }

        Self {
            test_runner,
            profile,
        }
    }
}

impl SqlGenerator for PropTestBackend {
    fn generate_with_matviews(
        &mut self,
        schema: &sql_gen::Schema,
        matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
    ) -> Result<GeneratedStatement> {
        let prop_schema = to_prop_schema_with_matviews(schema, matview_info);
        let strategy = sql_gen_prop::strategies::statement_for_schema(&prop_schema, &self.profile);
        let value_tree = strategy
            .new_tree(&mut self.test_runner)
            .map_err(|e| anyhow::anyhow!("Failed to generate statement: {e}"))?;
        let stmt = value_tree.current();
        let kind = sql_gen_prop::StatementKind::from(&stmt);

        if matches!(kind, sql_gen_prop::StatementKind::ReopenDatabase) {
            return Ok(GeneratedStatement {
                sql: "-- REOPEN DATABASE".to_string(),
                is_ddl: false,
                mutates_data: false,
                has_unordered_limit: false,
                unordered_limit_reason: None,
                is_matview_ddl: false,
                sqlite_sql: None,
                matview_output_columns: None,
                is_reopen: true,
            });
        }

        let sql = stmt.to_string();
        let is_ddl = kind.is_ddl();
        let mutates_data = matches!(
            kind,
            sql_gen_prop::StatementKind::Insert
                | sql_gen_prop::StatementKind::Update
                | sql_gen_prop::StatementKind::Delete
        );
        let has_unordered_limit = stmt.has_unordered_limit();
        let is_matview_ddl = matches!(
            kind,
            sql_gen_prop::StatementKind::CreateMaterializedView
                | sql_gen_prop::StatementKind::DropMaterializedView
        );

        let sqlite_sql = match &stmt {
            sql_gen_prop::SqlStatement::CreateMaterializedView(create) => {
                Some(create.as_regular_view_sql())
            }
            sql_gen_prop::SqlStatement::DropMaterializedView(drop_stmt) => {
                Some(drop_stmt.to_string())
            }
            _ => None,
        };

        let matview_output_columns = match &stmt {
            sql_gen_prop::SqlStatement::CreateMaterializedView(create) => {
                Some(create.output_columns.clone())
            }
            _ => None,
        };

        Ok(GeneratedStatement {
            sql,
            is_ddl,
            mutates_data,
            has_unordered_limit,
            unordered_limit_reason: None,
            is_matview_ddl,
            sqlite_sql,
            matview_output_columns,
            is_reopen: false,
        })
    }
}

/// Convert a `sql_gen::Schema` to a `sql_gen_prop::Schema`,
/// including tracked materialized view info (names + output columns).
fn to_prop_schema_with_matviews(
    schema: &sql_gen::Schema,
    matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
) -> sql_gen_prop::Schema {
    let mut builder = sql_gen_prop::SchemaBuilder::new();
    for db in &schema.attached_databases {
        builder = builder.add_database(db.clone());
    }
    for table in &schema.tables {
        let columns: Vec<sql_gen_prop::ColumnDef> = table
            .columns
            .iter()
            .map(|c| {
                let dt = match c.data_type {
                    sql_gen::DataType::Integer => sql_gen_prop::DataType::Integer,
                    sql_gen::DataType::Real => sql_gen_prop::DataType::Real,
                    sql_gen::DataType::Text => sql_gen_prop::DataType::Text,
                    sql_gen::DataType::Blob => sql_gen_prop::DataType::Blob,
                    sql_gen::DataType::Null => sql_gen_prop::DataType::Null,
                    // Array types have no prop equivalent — map to Blob
                    sql_gen::DataType::IntegerArray
                    | sql_gen::DataType::RealArray
                    | sql_gen::DataType::TextArray => sql_gen_prop::DataType::Blob,
                };
                let mut col = sql_gen_prop::ColumnDef::new(c.name.clone(), dt);
                if !c.nullable {
                    col = col.not_null();
                }
                if c.primary_key {
                    col = col.primary_key();
                }
                if c.unique {
                    col = col.unique();
                }
                if let Some(ref default) = c.default {
                    col = col.default_value(default.clone());
                }
                col
            })
            .collect();
        let prop_table = if table.strict {
            sql_gen_prop::Table::new_strict(table.name.clone(), columns)
        } else {
            sql_gen_prop::Table::new(table.name.clone(), columns)
        };
        let prop_table = match &table.database {
            Some(db) => prop_table.in_database(db.clone()),
            None => prop_table,
        };
        builder = builder.add_table(prop_table);
    }
    for index in &schema.indexes {
        let mut idx = sql_gen_prop::Index::new(
            index.name.clone(),
            index.table_name.clone(),
            index.columns.clone(),
        );
        if index.unique {
            idx = idx.unique();
        }
        if let Some(db) = &index.database {
            idx = idx.in_database(db.clone());
        }
        builder = builder.add_index(idx);
    }
    for (name, cols) in matview_info {
        let view =
            sql_gen_prop::View::new(name.clone(), "/* tracked */").with_columns(cols.clone());
        builder = builder.add_materialized_view(view);
    }
    builder.build()
}
