//! Maintenance of a `WITH RECURSIVE ... UNION ALL` CTE.
//!
//! The CTE result `R` lives in a hidden table: one row per derivation, keyed
//! by the derivation's identity, holding the CTE values and the round in
//! which the row was derived (seed rows are round 0, the children of a
//! round-`j` row are round `j + 1`). A change is computed one round at a time:
//!
//! ```text
//! dR(j + 1) = S(dR(j), E_new) + [S(R_old(j), E_new) - S(R_old(j), E_old)]
//! ```
//!
//! where `S` is the recursive arm and `E` its base tables. The bracket only
//! reads old rows, so it is computed once, before `R` changes ("copy A"). The
//! first term is the recursive arm run on the round's change with no base
//! table change ("copy B"). Keeping the rounds apart makes a move of a row
//! under its own descendant end: the retractions and the new cyclic rows meet
//! in the same round and cancel. The loop ends exactly when a fresh run of
//! the CTE ends; a CTE that does not end fails the statement.

use super::consolidate::{consolidate_delta, ConsolidatedDelta};
use super::output::{DeltaSource, EmittedNodeOutput, NodeOutput};
use super::plan::MaintenancePlan;
use super::source::{materialize_node_output, scan_node_output};
use super::stream::{
    btree_arrangement, emit_ephemeral_delta_row, emit_operator_rowid_delta, open_ephemeral_delta,
    scratch_table, ArrangementHandle, ArrangementIdentityColumn, DeltaIdentity, EphemeralDelta,
};
use super::{emit_node, MaintenanceInput};
use crate::incremental::dag;
use crate::namespace::NamespaceId;
use crate::schema::{BTreeTable, Index, Schema};
use crate::sync::Arc;
use crate::turso_assert;
use crate::vdbe::builder::{CursorType, ProgramBuilder};
use crate::vdbe::insn::{
    to_u32, CmpInsFlags, IdxInsertFlags, InsertFlags, Insn, RegisterOrLiteral,
};
use crate::{Connection, LimboError, Result};

/// A statement fails when its change of a recursive CTE needs more rounds.
const MAX_ROUNDS: i64 = 10_000;

/// The tag in the first key column of a seed row; recursive rows use 1.
const SEED_TAG: i64 = 0;
const STEP_TAG: i64 = 1;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_fixpoint(
    program: &mut ProgramBuilder,
    view_name: &str,
    plan: &MaintenancePlan,
    node_id: dag::NodeId,
    outputs: &[Option<NodeOutput>],
    input: MaintenanceInput,
    schema: &Schema,
    connection: &Arc<Connection>,
) -> Result<EmittedNodeOutput> {
    let dag = plan.dag();
    let dag::OpNode::Fixpoint {
        inputs: [initial, step],
        body_start,
    } = dag.nodes[node_id]
    else {
        return Err(LimboError::InternalError(format!(
            "maintenance DAG node {node_id} is not a fixpoint"
        )));
    };
    let body = Body {
        nodes: body_start..=step,
        outputs,
    };
    let contract = &plan.operator_states().for_node(node_id)?.output;
    let rows = RowsTable::open(plan, node_id, schema)?;
    let step_schema = dag.output_schema(step).clone();
    let open_keyed = |program: &mut ProgramBuilder, name: &str| {
        open_ephemeral_delta(
            program,
            &format!("{view_name}_{name}_{node_id}"),
            step_schema.as_ref().clone(),
            DeltaIdentity::OperatorKey(rows.key_width),
            vec![false; step_schema.bindings.len()].into(),
        )
    };
    let recursive_schema = body.recursive_input_schema(dag)?;
    let open_recursive_rows = |program: &mut ProgramBuilder, name: &str| {
        open_ephemeral_delta(
            program,
            &format!("{view_name}_{name}_{node_id}"),
            recursive_schema.as_ref().clone(),
            DeltaIdentity::BindingRowids(1),
            vec![true].into(),
        )
    };

    // Copy A: the change of the recursive arm for the unchanged old rows. It
    // reads `R` before any round changes it.
    let old_row_changes = match input {
        MaintenanceInput::TransactionDelta => {
            let no_rows = open_recursive_rows(program, "no_recursive_rows")?;
            let changes = body.emit(
                program, view_name, plan, &no_rows, &rows, true, schema, connection,
            )?;
            let keyed = open_keyed(program, "old_row_changes")?;
            emit_keyed_rows(program, &changes, &keyed, STEP_TAG, None);
            Some(keyed)
        }
        MaintenanceInput::BaseTable => None,
    };

    let round_reg = program.alloc_register();
    let max_rounds_reg = program.alloc_register();
    program.emit_int(0, round_reg);
    program.emit_int(MAX_ROUNDS, max_rounds_reg);
    let round_rows = open_keyed(program, "round_rows")?;
    let seeds = materialize_node_output(
        program,
        view_name,
        initial,
        body.declared_output(initial)?,
        input,
    )?;
    emit_keyed_rows(program, &seeds, &round_rows, SEED_TAG, Some(round_reg));
    let changes = open_ephemeral_delta(
        program,
        &format!("{view_name}_fixpoint_delta_{node_id}"),
        contract.schema.as_ref().clone(),
        contract.emitted_identity,
        contract.binding_rowids.clone(),
    )?;

    let round_label = program.allocate_label();
    let empty_round_label = program.allocate_label();
    let done_label = program.allocate_label();
    let diverged_label = program.allocate_label();
    program.preassign_label_to_next_insn(round_label);
    if let Some(old_row_changes) = &old_row_changes {
        emit_rows_of_round(program, old_row_changes, &round_rows, round_reg);
    }
    let round_change = consolidate_delta(
        program,
        &format!("{view_name}_round_{node_id}"),
        &round_rows,
    )?;
    program.emit_insn(Insn::Rewind {
        cursor_id: round_change.cursor_id,
        pc_if_empty: empty_round_label,
    });
    program.emit_insn(Insn::Ge {
        lhs: round_reg,
        rhs: max_rounds_reg,
        target_pc: diverged_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    let frontier = open_recursive_rows(program, "frontier")?;
    emit_round_merge(
        program,
        view_name,
        &round_change,
        round_reg,
        &rows,
        &frontier,
        &changes,
    )?;
    let derived = body.emit(
        program, view_name, plan, &frontier, &rows, false, schema, connection,
    )?;
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id: round_rows.cursor_id,
        is_table: true,
    });
    emit_keyed_rows(program, &derived, &round_rows, STEP_TAG, None);
    program.emit_insn(Insn::AddImm {
        register: round_reg,
        value: 1,
    });
    program.emit_insn(Insn::Goto {
        target_pc: round_label,
    });

    // An empty round ends the loop, unless an old row of a later round
    // changed its children.
    program.preassign_label_to_next_insn(empty_round_label);
    if let Some(old_row_changes) = &old_row_changes {
        let next_round_reg = program.alloc_register();
        emit_next_round(program, old_row_changes, round_reg, next_round_reg);
        program.emit_insn(Insn::IsNull {
            reg: next_round_reg,
            target_pc: done_label,
        });
        program.emit_insn(Insn::Copy {
            src_reg: next_round_reg,
            dst_reg: round_reg,
            extra_amount: 0,
        });
        program.emit_insn(Insn::OpenEphemeral {
            cursor_id: round_rows.cursor_id,
            is_table: true,
        });
        program.emit_insn(Insn::Goto {
            target_pc: round_label,
        });
    }
    program.emit_insn(Insn::Goto {
        target_pc: done_label,
    });
    program.preassign_label_to_next_insn(diverged_label);
    program.emit_insn(Insn::Halt {
        constraint_info: None,
        err_code: crate::error::SQLITE_ERROR,
        description: format!(
            "materialized view {view_name}: recursive CTE did not converge after {MAX_ROUNDS} rounds"
        ),
        on_error: None,
        description_reg: None,
    });
    program.preassign_label_to_next_insn(done_label);

    let arrangement = btree_arrangement(
        rows.table.clone(),
        DeltaIdentity::OperatorRowid,
        vec![ArrangementIdentityColumn::RowId],
        vec![None; contract.binding_rowids.len()],
        (rows.key_width..rows.key_width + rows.width).collect(),
        None,
    );
    EmittedNodeOutput::new(
        node_id,
        DeltaSource::Ephemeral(changes),
        Some(arrangement),
        contract,
    )
}

/// The hidden table of a fixpoint: `(key.., values.., round)`, with an index
/// on the key.
struct RowsTable {
    table: Arc<BTreeTable>,
    index: Arc<Index>,
    key_width: usize,
    /// The number of CTE columns.
    width: usize,
}

impl RowsTable {
    fn open(plan: &MaintenancePlan, node_id: dag::NodeId, schema: &Schema) -> Result<Self> {
        let table_name = plan
            .operator_states()
            .for_node(node_id)?
            .state_table_name()?;
        let table = schema
            .get_btree_table(NamespaceId::ROOT, table_name)
            .ok_or_else(|| {
                LimboError::InternalError(format!("recursive CTE table {table_name} not found"))
            })?;
        let index = schema
            .get_indices(NamespaceId::ROOT, table_name)
            .next()
            .cloned()
            .ok_or_else(|| {
                LimboError::InternalError(format!("recursive CTE table {table_name} has no index"))
            })?;
        let width = plan.dag().output_schema(node_id).len();
        let key_width = table.columns().len() - width - 1;
        Ok(Self {
            table,
            index,
            key_width,
            width,
        })
    }

    /// The table as the arrangement of a recursive CTE input: the row's
    /// rowid, its values and its round.
    fn recursive_input_arrangement(&self) -> ArrangementHandle {
        btree_arrangement(
            self.table.clone(),
            DeltaIdentity::BindingRowids(1),
            vec![ArrangementIdentityColumn::RowId],
            vec![Some(ArrangementIdentityColumn::RowId)],
            (self.key_width..=self.key_width + self.width).collect(),
            None,
        )
    }
}

/// The nodes of the recursive arm: from the first node after the seed arm to
/// the arm's root.
struct Body<'a> {
    nodes: std::ops::RangeInclusive<dag::NodeId>,
    outputs: &'a [Option<NodeOutput>],
}

impl Body<'_> {
    fn declared_output(&self, node_id: dag::NodeId) -> Result<&NodeOutput> {
        self.outputs[node_id].as_ref().ok_or_else(|| {
            LimboError::InternalError(format!("fixpoint input {node_id} was not compiled"))
        })
    }

    fn recursive_input_schema(&self, dag: &dag::MaintenanceDag) -> Result<Arc<dag::StreamSchema>> {
        self.nodes
            .clone()
            .find(|node| matches!(dag.nodes[*node], dag::OpNode::RecursiveInput { .. }))
            .map(|node| dag.output_schema(node).clone())
            .ok_or_else(|| {
                LimboError::InternalError("a recursive CTE arm does not read the CTE".to_string())
            })
    }

    /// Emit the recursive arm once and return its root's delta.
    ///
    /// Every recursive CTE input reads `recursive_rows` as its delta and `R`
    /// as its arrangement. With `base_changes`, base tables give their
    /// captured change; without, they give none, so a join reads only their
    /// btrees.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        program: &mut ProgramBuilder,
        view_name: &str,
        plan: &MaintenancePlan,
        recursive_rows: &EphemeralDelta,
        rows: &RowsTable,
        base_changes: bool,
        schema: &Schema,
        connection: &Arc<Connection>,
    ) -> Result<EphemeralDelta> {
        let dag = plan.dag();
        let mut outputs = self.outputs.to_vec();
        for node_id in self.nodes.clone() {
            let contract = &plan.operator_states().for_node(node_id)?.output;
            let output = match &dag.nodes[node_id] {
                dag::OpNode::RecursiveInput { .. } => NodeOutput::new(
                    node_id,
                    DeltaSource::Ephemeral(EphemeralDelta {
                        schema: contract.schema.clone(),
                        ..recursive_rows.clone()
                    }),
                    Some(rows.recursive_input_arrangement()),
                    contract,
                )?,
                dag::OpNode::Scan { .. } if !base_changes => {
                    let (_, arrangement) =
                        scan_node_output(dag, node_id, contract, schema)?.into_parts();
                    let no_change = open_ephemeral_delta(
                        program,
                        &format!("{view_name}_no_change_{node_id}"),
                        contract.schema.as_ref().clone(),
                        contract.emitted_identity,
                        contract.binding_rowids.clone(),
                    )?;
                    NodeOutput::new(
                        node_id,
                        DeltaSource::Ephemeral(no_change),
                        arrangement,
                        contract,
                    )?
                }
                _ => emit_node(
                    program,
                    view_name,
                    plan,
                    node_id,
                    &outputs,
                    MaintenanceInput::TransactionDelta,
                    schema,
                    connection,
                )?,
            };
            outputs[node_id] = Some(output);
        }
        let root = *self.nodes.end();
        let root_output = outputs[root].as_ref().ok_or_else(|| {
            LimboError::InternalError(format!("recursive CTE arm root {root} was not compiled"))
        })?;
        materialize_node_output(
            program,
            view_name,
            root,
            root_output,
            MaintenanceInput::TransactionDelta,
        )
    }
}

/// Append every row of `rows` to `keyed` as
/// `[tag, identity of the row.., 0.., values.., round, weight]`.
/// `round_reg` holds the round of rows whose values do not end with one.
fn emit_keyed_rows(
    program: &mut ProgramBuilder,
    rows: &EphemeralDelta,
    keyed: &EphemeralDelta,
    tag: i64,
    round_reg: Option<usize>,
) {
    let key_width = keyed.identity_width();
    turso_assert!(
        rows.identity_width() < key_width
            && rows.width + usize::from(round_reg.is_some()) == keyed.width
            && keyed.value_start == key_width,
        "keyed recursive CTE rows must hold the tag, the identity, the values and the round"
    );
    let record_start = program.alloc_registers(keyed.record_width());
    let record_reg = program.alloc_register();
    let rowid_reg = program.alloc_register();
    let end_label = program.allocate_label();
    let loop_label = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: rows.cursor_id,
        pc_if_empty: end_label,
    });
    program.preassign_label_to_next_insn(loop_label);
    program.emit_int(tag, record_start);
    for column in 0..rows.identity_width() {
        program.emit_insn(Insn::Column {
            cursor_id: rows.cursor_id,
            column,
            dest: record_start + 1 + column,
            default: None,
        });
    }
    for column in 1 + rows.identity_width()..key_width {
        program.emit_int(0, record_start + column);
    }
    for column in 0..rows.width {
        program.emit_insn(Insn::Column {
            cursor_id: rows.cursor_id,
            column: rows.value_start + column,
            dest: record_start + key_width + column,
            default: None,
        });
    }
    if let Some(round_reg) = round_reg {
        program.emit_insn(Insn::Copy {
            src_reg: round_reg,
            dst_reg: record_start + key_width + rows.width,
            extra_amount: 0,
        });
    }
    program.emit_insn(Insn::Column {
        cursor_id: rows.cursor_id,
        column: rows.weight_column,
        dest: record_start + keyed.weight_column,
        default: None,
    });
    emit_append(program, keyed, record_start, record_reg, rowid_reg);
    program.emit_insn(Insn::Next {
        cursor_id: rows.cursor_id,
        pc_if_next: loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(end_label);
}

/// Append the rows of `keyed` whose round is in `round_reg` to `round_rows`.
fn emit_rows_of_round(
    program: &mut ProgramBuilder,
    keyed: &EphemeralDelta,
    round_rows: &EphemeralDelta,
    round_reg: usize,
) {
    let record_start = program.alloc_registers(keyed.record_width());
    let record_reg = program.alloc_register();
    let rowid_reg = program.alloc_register();
    let round_column = keyed.weight_column - 1;
    let end_label = program.allocate_label();
    let loop_label = program.allocate_label();
    let next_label = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: keyed.cursor_id,
        pc_if_empty: end_label,
    });
    program.preassign_label_to_next_insn(loop_label);
    for column in 0..keyed.record_width() {
        program.emit_insn(Insn::Column {
            cursor_id: keyed.cursor_id,
            column,
            dest: record_start + column,
            default: None,
        });
    }
    program.emit_insn(Insn::Ne {
        lhs: record_start + round_column,
        rhs: round_reg,
        target_pc: next_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    emit_append(program, round_rows, record_start, record_reg, rowid_reg);
    program.preassign_label_to_next_insn(next_label);
    program.emit_insn(Insn::Next {
        cursor_id: keyed.cursor_id,
        pc_if_next: loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(end_label);
}

/// Set `next_round_reg` to the smallest round of `keyed` above `round_reg`,
/// or NULL when there is none.
fn emit_next_round(
    program: &mut ProgramBuilder,
    keyed: &EphemeralDelta,
    round_reg: usize,
    next_round_reg: usize,
) {
    let row_round_reg = program.alloc_register();
    let end_label = program.allocate_label();
    let loop_label = program.allocate_label();
    let next_label = program.allocate_label();
    let take_label = program.allocate_label();
    program.emit_insn(Insn::Null {
        dest: next_round_reg,
        dest_end: None,
    });
    program.emit_insn(Insn::Rewind {
        cursor_id: keyed.cursor_id,
        pc_if_empty: end_label,
    });
    program.preassign_label_to_next_insn(loop_label);
    program.emit_insn(Insn::Column {
        cursor_id: keyed.cursor_id,
        column: keyed.weight_column - 1,
        dest: row_round_reg,
        default: None,
    });
    program.emit_insn(Insn::Le {
        lhs: row_round_reg,
        rhs: round_reg,
        target_pc: next_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    program.emit_insn(Insn::IsNull {
        reg: next_round_reg,
        target_pc: take_label,
    });
    program.emit_insn(Insn::Ge {
        lhs: row_round_reg,
        rhs: next_round_reg,
        target_pc: next_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    program.preassign_label_to_next_insn(take_label);
    program.emit_insn(Insn::Copy {
        src_reg: row_round_reg,
        dst_reg: next_round_reg,
        extra_amount: 0,
    });
    program.preassign_label_to_next_insn(next_label);
    program.emit_insn(Insn::Next {
        cursor_id: keyed.cursor_id,
        pc_if_next: loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(end_label);
}

fn emit_append(
    program: &mut ProgramBuilder,
    output: &EphemeralDelta,
    record_start: usize,
    record_reg: usize,
    rowid_reg: usize,
) {
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(record_start),
        count: to_u32(output.record_width()),
        dest_reg: to_u32(record_reg),
        index_name: None,
        affinity_str: None,
    });
    program.emit_insn(Insn::NewRowid {
        cursor: output.cursor_id,
        rowid_reg,
        prev_largest_reg: 0,
    });
    program.emit_insn(Insn::Insert {
        cursor: output.cursor_id,
        key_reg: rowid_reg,
        record_reg,
        flag: InsertFlags::new().is_ephemeral_table_insert(),
        table_name: String::new(),
    });
}

/// Merge one consolidated round into `R` and append the change to
/// `frontier` (as `R` rowids, values and round) and to `changes` (as `R`
/// rowids and values).
///
/// A key and round hold at most one row, with weight 1. A key whose values
/// change keeps its rowid, so the rows derived from it keep
/// their keys too: the old and new children cancel in the next round. The
/// merge applies the retractions first and the insertions second, and
/// deletes the retracted rows that were not written again last, so a rowid
/// is never given to a new key while its old row is live.
#[allow(clippy::too_many_arguments)]
fn emit_round_merge(
    program: &mut ProgramBuilder,
    view_name: &str,
    round: &ConsolidatedDelta,
    round_reg: usize,
    rows: &RowsTable,
    frontier: &EphemeralDelta,
    changes: &EphemeralDelta,
) -> Result<()> {
    let key_width = rows.key_width;
    let row_width = key_width + rows.width + 1;
    turso_assert!(
        round.identity_width() == key_width
            && round.width == rows.width + 1
            && rows.table.columns().len() == row_width,
        "a recursive CTE round must have the layout of its table"
    );
    let table_name = rows.table.name.clone();
    let table_cursor = program.alloc_cursor_id(CursorType::BTreeTable(rows.table.clone()));
    program.emit_insn(Insn::OpenWrite {
        cursor_id: table_cursor,
        root_page: RegisterOrLiteral::Literal(rows.table.root_page),
        db: 0,
    });
    let index_cursor = program.alloc_cursor_id(CursorType::BTreeIndex(rows.index.clone()));
    program.emit_insn(Insn::OpenWrite {
        cursor_id: index_cursor,
        root_page: RegisterOrLiteral::Literal(rows.index.root_page),
        db: 0,
    });
    let retracted_cursor = program.alloc_cursor_id(CursorType::BTreeTable(Arc::new(
        scratch_table(&format!("{view_name}_retracted"), 1)?,
    )));
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id: retracted_cursor,
        is_table: true,
    });

    // [key.., values.., round]: the record of the row in `R`.
    let row_start = program.alloc_registers(row_width);
    let values_start = row_start + key_width;
    let row_round_reg = values_start + rows.width;
    // [key.., round, rowid]: the index entry.
    let entry_start = program.alloc_registers(key_width + 2);
    let entry_round_reg = entry_start + key_width;
    let rowid_reg = entry_round_reg + 1;
    let weight_reg = program.alloc_register();
    let zero_reg = program.alloc_register();
    let one_reg = program.alloc_register();
    let minus_one_reg = program.alloc_register();
    let record_reg = program.alloc_register();
    let stored_record_reg = program.alloc_register();
    program.emit_int(0, zero_reg);
    program.emit_int(1, one_reg);
    program.emit_int(-1, minus_one_reg);

    let corrupt_label = program.allocate_label();
    let not_unique_label = program.allocate_label();
    let end_label = program.allocate_label();

    let read_row = |program: &mut ProgramBuilder| {
        for column in 0..row_width {
            program.emit_insn(Insn::Column {
                cursor_id: round.cursor_id,
                column,
                dest: row_start + column,
                default: None,
            });
        }
        program.emit_insn(Insn::Ne {
            lhs: row_round_reg,
            rhs: round_reg,
            target_pc: corrupt_label,
            flags: CmpInsFlags::default(),
            collation: None,
        });
        program.emit_insn(Insn::MakeRecord {
            start_reg: to_u32(row_start),
            count: to_u32(row_width),
            dest_reg: to_u32(record_reg),
            index_name: None,
            affinity_str: None,
        });
        program.emit_insn(Insn::Copy {
            src_reg: row_start,
            dst_reg: entry_start,
            extra_amount: key_width - 1,
        });
        program.emit_insn(Insn::Copy {
            src_reg: row_round_reg,
            dst_reg: entry_round_reg,
            extra_amount: 0,
        });
    };
    let emit_change = |program: &mut ProgramBuilder| {
        emit_ephemeral_delta_row(
            program,
            frontier,
            rowid_reg,
            &[None],
            values_start,
            weight_reg,
        );
        emit_operator_rowid_delta(program, changes, rowid_reg, values_start, weight_reg);
    };

    // Retractions: each must remove the live row of its key.
    let retractions_done_label = program.allocate_label();
    let retraction_loop_label = program.allocate_label();
    let retraction_next_label = program.allocate_label();
    let retraction_found_label = program.allocate_label();
    let first_retraction_label = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: round.cursor_id,
        pc_if_empty: retractions_done_label,
    });
    program.preassign_label_to_next_insn(retraction_loop_label);
    program.emit_insn(Insn::Column {
        cursor_id: round.cursor_id,
        column: round.weight_column,
        dest: weight_reg,
        default: None,
    });
    program.emit_insn(Insn::Gt {
        lhs: weight_reg,
        rhs: zero_reg,
        target_pc: retraction_next_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    program.emit_insn(Insn::Ne {
        lhs: weight_reg,
        rhs: minus_one_reg,
        target_pc: not_unique_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    read_row(program);
    program.emit_insn(Insn::Found {
        cursor_id: index_cursor,
        target_pc: retraction_found_label,
        record_reg: entry_start,
        num_regs: key_width + 1,
    });
    program.emit_insn(Insn::Goto {
        target_pc: corrupt_label,
    });
    program.preassign_label_to_next_insn(retraction_found_label);
    program.emit_insn(Insn::IdxRowId {
        cursor_id: index_cursor,
        dest: rowid_reg,
    });
    program.emit_insn(Insn::SeekRowid {
        cursor_id: table_cursor,
        src_reg: rowid_reg,
        target_pc: corrupt_label,
    });
    program.emit_insn(Insn::RowData {
        cursor_id: table_cursor,
        dest: stored_record_reg,
    });
    program.emit_insn(Insn::Ne {
        lhs: stored_record_reg,
        rhs: record_reg,
        target_pc: corrupt_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    program.emit_insn(Insn::SeekRowid {
        cursor_id: retracted_cursor,
        src_reg: rowid_reg,
        target_pc: first_retraction_label,
    });
    program.emit_insn(Insn::Goto {
        target_pc: not_unique_label,
    });
    program.preassign_label_to_next_insn(first_retraction_label);
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(rowid_reg),
        count: 1,
        dest_reg: to_u32(stored_record_reg),
        index_name: None,
        affinity_str: None,
    });
    program.emit_insn(Insn::Insert {
        cursor: retracted_cursor,
        key_reg: rowid_reg,
        record_reg: stored_record_reg,
        flag: InsertFlags::new().is_ephemeral_table_insert(),
        table_name: String::new(),
    });
    emit_change(program);
    program.preassign_label_to_next_insn(retraction_next_label);
    program.emit_insn(Insn::Next {
        cursor_id: round.cursor_id,
        pc_if_next: retraction_loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(retractions_done_label);

    // Insertions: a key retracted in this round is written again in place.
    let insertions_done_label = program.allocate_label();
    let insertion_loop_label = program.allocate_label();
    let insertion_next_label = program.allocate_label();
    let rewrite_label = program.allocate_label();
    let insertion_emit_label = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: round.cursor_id,
        pc_if_empty: insertions_done_label,
    });
    program.preassign_label_to_next_insn(insertion_loop_label);
    program.emit_insn(Insn::Column {
        cursor_id: round.cursor_id,
        column: round.weight_column,
        dest: weight_reg,
        default: None,
    });
    program.emit_insn(Insn::Lt {
        lhs: weight_reg,
        rhs: zero_reg,
        target_pc: insertion_next_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    program.emit_insn(Insn::Ne {
        lhs: weight_reg,
        rhs: one_reg,
        target_pc: not_unique_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    read_row(program);
    program.emit_insn(Insn::Found {
        cursor_id: index_cursor,
        target_pc: rewrite_label,
        record_reg: entry_start,
        num_regs: key_width + 1,
    });
    program.emit_insn(Insn::NewRowid {
        cursor: table_cursor,
        rowid_reg,
        prev_largest_reg: 0,
    });
    emit_row_write(program, table_cursor, &table_name, rowid_reg, record_reg);
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(entry_start),
        count: to_u32(key_width + 2),
        dest_reg: to_u32(stored_record_reg),
        index_name: Some(rows.index.name.clone()),
        affinity_str: None,
    });
    program.emit_insn(Insn::IdxInsert {
        cursor_id: index_cursor,
        record_reg: stored_record_reg,
        unpacked_start: Some(entry_start),
        unpacked_count: Some(to_u32(key_width + 2)),
        flags: IdxInsertFlags::new(),
    });
    program.emit_insn(Insn::Goto {
        target_pc: insertion_emit_label,
    });
    program.preassign_label_to_next_insn(rewrite_label);
    program.emit_insn(Insn::IdxRowId {
        cursor_id: index_cursor,
        dest: rowid_reg,
    });
    program.emit_insn(Insn::SeekRowid {
        cursor_id: retracted_cursor,
        src_reg: rowid_reg,
        target_pc: not_unique_label,
    });
    program.emit_insn(Insn::Delete {
        cursor_id: retracted_cursor,
        table_name: String::new(),
        is_part_of_update: false,
    });
    program.emit_insn(Insn::SeekRowid {
        cursor_id: table_cursor,
        src_reg: rowid_reg,
        target_pc: corrupt_label,
    });
    emit_row_write(program, table_cursor, &table_name, rowid_reg, record_reg);
    program.preassign_label_to_next_insn(insertion_emit_label);
    emit_change(program);
    program.preassign_label_to_next_insn(insertion_next_label);
    program.emit_insn(Insn::Next {
        cursor_id: round.cursor_id,
        pc_if_next: insertion_loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(insertions_done_label);

    // Rows retracted and not written again leave `R`.
    let deletes_done_label = program.allocate_label();
    let delete_loop_label = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: retracted_cursor,
        pc_if_empty: deletes_done_label,
    });
    program.preassign_label_to_next_insn(delete_loop_label);
    program.emit_insn(Insn::RowId {
        cursor_id: retracted_cursor,
        dest: rowid_reg,
    });
    program.emit_insn(Insn::SeekRowid {
        cursor_id: table_cursor,
        src_reg: rowid_reg,
        target_pc: corrupt_label,
    });
    for column in 0..key_width {
        program.emit_insn(Insn::Column {
            cursor_id: table_cursor,
            column,
            dest: entry_start + column,
            default: None,
        });
    }
    program.emit_insn(Insn::Column {
        cursor_id: table_cursor,
        column: key_width + rows.width,
        dest: entry_round_reg,
        default: None,
    });
    program.emit_insn(Insn::IdxDelete {
        start_reg: entry_start,
        num_regs: key_width + 2,
        cursor_id: index_cursor,
        raise_error_if_no_matching_entry: true,
    });
    program.emit_insn(Insn::Delete {
        cursor_id: table_cursor,
        table_name: table_name.clone(),
        is_part_of_update: true,
    });
    program.emit_insn(Insn::Next {
        cursor_id: retracted_cursor,
        pc_if_next: delete_loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(deletes_done_label);
    program.emit_insn(Insn::Goto {
        target_pc: end_label,
    });

    program.preassign_label_to_next_insn(corrupt_label);
    program.emit_insn(Insn::Halt {
        constraint_info: None,
        err_code: crate::error::SQLITE_ERROR,
        description: format!(
            "materialized view {view_name}: recursive CTE table {table_name} is corrupted"
        ),
        on_error: None,
        description_reg: None,
    });
    program.preassign_label_to_next_insn(not_unique_label);
    program.emit_insn(Insn::Halt {
        constraint_info: None,
        err_code: crate::error::SQLITE_ERROR,
        description: format!(
            "materialized view {view_name}: a recursive CTE round changed one key more than once"
        ),
        on_error: None,
        description_reg: None,
    });
    program.preassign_label_to_next_insn(end_label);
    Ok(())
}

fn emit_row_write(
    program: &mut ProgramBuilder,
    table_cursor: usize,
    table_name: &str,
    rowid_reg: usize,
    record_reg: usize,
) {
    program.emit_insn(Insn::Insert {
        cursor: table_cursor,
        key_reg: rowid_reg,
        record_reg,
        flag: InsertFlags(
            InsertFlags::REQUIRE_SEEK
                | InsertFlags::SKIP_LAST_ROWID
                | InsertFlags::SKIP_STATEMENT_CHANGE_COUNT,
        ),
        table_name: table_name.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use crate::mvcc::yield_hooks::YieldPointMarker;
    use crate::mvcc::yield_points::{YieldInjector, YieldPoint};
    use crate::storage::btree::{BTreeWriteYieldPoint, BTREE_WRITE_YIELD_FAMILY};
    use crate::sync::Arc;
    use crate::{
        ConnectOpts, Connection, Database, DatabaseOpts, OpenFlags, SqliteDialect, StepResult,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Yields at the `fire_at`-th balance of the btree `selection_key` names,
    /// and counts every balance.
    #[derive(Debug)]
    struct NthBalanceYield {
        selection_key: u64,
        fire_at: u64,
        hits: AtomicU64,
    }

    impl YieldInjector for NthBalanceYield {
        fn should_yield(&self, _instance_id: u64, selection_key: u64, point: YieldPoint) -> bool {
            if point != BTreeWriteYieldPoint::AfterInsertOverflowCellBeforeBalance.point()
                || selection_key != self.selection_key
            {
                return false;
            }
            self.hits.fetch_add(1, Ordering::AcqRel) + 1 == self.fire_at
        }
    }

    const VIEW: &str = "CREATE MATERIALIZED VIEW tree AS \
         WITH RECURSIVE t(id, payload, depth) AS ( \
           SELECT id, payload, 0 FROM node WHERE parent IS NULL \
           UNION ALL \
           SELECT n.id, n.payload, t.depth + 1 FROM node n JOIN t ON n.parent = t.id) \
         SELECT id, depth, length(payload) FROM t";

    const FRESH: &str = "WITH RECURSIVE t(id, payload, depth) AS ( \
           SELECT id, payload, 0 FROM node WHERE parent IS NULL \
           UNION ALL \
           SELECT n.id, n.payload, t.depth + 1 FROM node n JOIN t ON n.parent = t.id) \
         SELECT id, depth, length(payload) FROM t ORDER BY id";

    /// Four levels of 12 rows below root 1; every row carries a 3000-byte
    /// payload, so each round's writes to the CTE table balance its btree.
    const BULK_INSERT: &str =
        "WITH RECURSIVE c(x) AS (SELECT 2 UNION ALL SELECT x + 1 FROM c WHERE x < 49) \
         INSERT INTO node SELECT x, CASE WHEN x < 14 THEN 1 ELSE x - 12 END, zeroblob(3000) FROM c";

    fn open(name: &str) -> Arc<Connection> {
        #[allow(clippy::arc_with_non_send_sync)]
        let io: Arc<dyn crate::IO> = Arc::new(crate::MemoryIO::new());
        let db = Database::open_file_with_flags(
            io,
            name,
            OpenFlags::default(),
            DatabaseOpts::new().with_views(true),
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();
        db.connect(ConnectOpts::new()).unwrap()
    }

    fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<String>> {
        conn.query(sql)
            .unwrap()
            .unwrap()
            .run_collect_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.iter().map(|value| value.to_string()).collect())
            .collect()
    }

    #[test]
    fn a_yield_while_writing_the_cte_table_resumes_in_the_same_round() {
        let conn = open("fixpoint-yield.db");
        conn.execute("CREATE TABLE node(id INTEGER PRIMARY KEY, parent INTEGER, payload BLOB)")
            .unwrap();
        conn.execute("INSERT INTO node VALUES (1, NULL, zeroblob(3000))")
            .unwrap();
        conn.execute(VIEW).unwrap();
        let cte_table_root: i64 = rows(
            &conn,
            "SELECT rootpage FROM sqlite_schema WHERE type = 'table' \
             AND name LIKE '__turso_internal_dbsp_state_v%_tree__n%'",
        )[0][0]
            .parse()
            .unwrap();
        let selection_key = BTREE_WRITE_YIELD_FAMILY ^ cte_table_root as u64;

        let counter = Arc::new(NthBalanceYield {
            selection_key,
            fire_at: 0,
            hits: AtomicU64::new(0),
        });
        conn.execute("BEGIN").unwrap();
        conn.set_yield_injector(Some(counter.clone()));
        conn.execute(BULK_INSERT).unwrap();
        conn.set_yield_injector(None);
        conn.execute("ROLLBACK").unwrap();
        let total_hits = counter.hits.load(Ordering::Acquire);
        assert!(
            total_hits > 4,
            "the bulk insert balanced the CTE table {total_hits} times"
        );

        let injector = Arc::new(NthBalanceYield {
            selection_key,
            fire_at: total_hits / 2 + 1,
            hits: AtomicU64::new(0),
        });
        conn.set_yield_injector(Some(injector.clone()));
        let mut stmt = conn.prepare(BULK_INSERT).unwrap();
        let mut yields = 0;
        loop {
            match stmt.step().unwrap() {
                StepResult::Yield => yields += 1,
                StepResult::IO => stmt._io().step().unwrap(),
                StepResult::Done => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        drop(stmt);
        conn.set_yield_injector(None);

        assert_eq!(yields, 1);
        assert_eq!(injector.hits.load(Ordering::Acquire), total_hits);
        assert_eq!(
            rows(&conn, "SELECT * FROM tree ORDER BY id"),
            rows(&conn, FRESH)
        );
        assert_eq!(
            rows(&conn, "SELECT count(*) FROM tree"),
            vec![vec!["49".to_string()]]
        );
        assert_eq!(
            rows(&conn, "PRAGMA integrity_check"),
            vec![vec!["ok".to_string()]]
        );
    }

    /// Ten roots, and below them every block `x > 10` has parent `x / 10`.
    fn open_ten_thousand_blocks(name: &str) -> Arc<Connection> {
        let conn = open(name);
        conn.execute("CREATE TABLE block(id INTEGER PRIMARY KEY, parent_id INTEGER, content TEXT)")
            .unwrap();
        conn.execute(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10000) \
             INSERT INTO block SELECT x, CASE WHEN x <= 10 THEN NULL ELSE x / 10 END, 'b' || x FROM c",
        )
        .unwrap();
        conn
    }

    fn create_path_view(conn: &Arc<Connection>) -> std::time::Duration {
        let started = std::time::Instant::now();
        conn.execute(
            "CREATE MATERIALIZED VIEW block_with_path AS \
             WITH RECURSIVE paths AS ( \
               SELECT id, parent_id, content, '/' || id AS path FROM block WHERE parent_id IS NULL \
               UNION ALL \
               SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id \
               FROM block b JOIN paths p ON b.parent_id = p.id) \
             SELECT * FROM paths",
        )
        .unwrap();
        started.elapsed()
    }

    #[test]
    #[ignore = "timing of a 10 000-block view creation"]
    fn create_a_path_view_over_ten_thousand_blocks() {
        let conn = open_ten_thousand_blocks("fixpoint-ten-thousand-blocks.db");
        let elapsed = create_path_view(&conn);
        eprintln!("CREATE of block_with_path over 10 000 blocks took {elapsed:?}");
        assert_eq!(
            rows(&conn, "SELECT count(*) FROM block_with_path"),
            vec![vec!["10000".to_string()]]
        );
        assert!(elapsed < std::time::Duration::from_secs(300));
    }

    #[test]
    #[ignore = "timing of one write to a 10 000-block view"]
    fn move_a_thousand_block_subtree_in_a_path_view_over_ten_thousand_blocks() {
        let conn = open_ten_thousand_blocks("fixpoint-move-ten-thousand-blocks.db");
        create_path_view(&conn);
        let started = std::time::Instant::now();
        conn.execute("UPDATE block SET parent_id = 2 WHERE id = 1")
            .unwrap();
        let elapsed = started.elapsed();
        eprintln!("moving a 1000-block subtree in block_with_path took {elapsed:?}");
        assert_eq!(
            rows(
                &conn,
                "SELECT count(*), sum(path LIKE '/2/1%'), sum(length(path)) FROM block_with_path"
            ),
            vec![vec![
                "10000".to_string(),
                "1000".to_string(),
                "136364".to_string()
            ]]
        );
        assert_eq!(
            rows(&conn, "SELECT path FROM block_with_path WHERE id = 1999"),
            vec![vec!["/2/1/19/199/1999".to_string()]]
        );
        assert!(elapsed < std::time::Duration::from_secs(60));
    }
}
