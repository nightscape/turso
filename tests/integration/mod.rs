mod abandoned_create_index;
mod abandoned_statement_pager;
mod assert_details;
mod attach;
mod checkpoint_crash_atomicity;
mod common;
mod conflict_resolution;
mod custom_types;
mod database;
mod expr_depth_stack_overflow;
mod external_apis;
mod functions;
mod fuzz_transaction;
mod index_method;
mod integrity_check;
mod matview_exists_decorrelation;
mod matview_exists_predicate_refused;
mod matview_left_join_antijoin;
mod mvcc;
mod pragma;
mod query_processing;
mod query_timeout;
mod queued_io;
mod recursive_cte_dependent_matview;
mod recursive_cte_iteration_cap;
mod recursive_cte_order_by;
mod recursive_cte_plan_shape;
mod recursive_cte_reopen_restore;
mod recursive_cte_runaway;
mod reindex;
mod statement_metadata;
mod statement_reset;
mod stmt_journal;
mod stmt_readonly;
mod storage;
mod suspended_statement_checkpoint;
mod trigger;
mod unreliable_io;
mod wal;

#[cfg(test)]
mod tests {
    use tracing_subscriber::EnvFilter;

    #[ctor::ctor]
    fn init() {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_ansi(false)
            .init();
    }
}
