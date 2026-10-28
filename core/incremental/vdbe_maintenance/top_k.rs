use super::consolidate::consolidate_delta;
use super::members::{emit_add_to_member, emit_exact_value_record, MemberTable};
use super::output::EmittedNodeOutput;
use super::plan::OperatorStateDef;
use super::stream::{emit_ephemeral_delta_row, open_ephemeral_delta, EphemeralDelta};
use crate::incremental::dag;
use crate::schema::Schema;
use crate::turso_assert;
use crate::vdbe::builder::ProgramBuilder;
use crate::vdbe::insn::{CmpInsFlags, Insn};
use crate::vdbe::BranchOffset;
use crate::Result;

/// Maintain the window of a [`dag::OpNode::TopK`].
///
/// The state table holds every input row in window order. The output delta
/// retracts the window as it was before the input delta and inserts the
/// window after it; the rows both windows share cancel when the delta is
/// consolidated. A batch that does not change the input emits nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_top_k(
    program: &mut ProgramBuilder,
    view_name: &str,
    node_id: dag::NodeId,
    input: &EphemeralDelta,
    sort_keys: &[dag::SortKey],
    limit: Option<i64>,
    offset: i64,
    operator_state: &OperatorStateDef,
    schema: &Schema,
) -> Result<EmittedNodeOutput> {
    let contract = &operator_state.output;
    let output = open_ephemeral_delta(
        program,
        &format!("{view_name}_top_k_delta_{node_id}"),
        contract.schema.as_ref().clone(),
        contract.emitted_identity,
        contract.binding_rowids.clone(),
    )?;
    turso_assert!(
        output.identity == input.identity
            && output.width == input.width
            && output.value_start == output.identity_width(),
        "a window keeps its input's identity and values and no other rowids"
    );
    let input = consolidate_delta(
        program,
        &format!("{view_name}_top_k_input_{node_id}"),
        input,
    )?;
    let members = MemberTable::open(
        program,
        schema,
        view_name,
        operator_state.state_table_name()?,
    )?;
    let identity_width = input.identity_width();
    turso_assert!(
        members.key_width == sort_keys.len() + identity_width + 1 + input.width,
        "window state must be keyed by sort keys, identity, exact record and values"
    );

    let end_label = program.allocate_label();
    let corrupt_label = program.allocate_label();
    let window = WindowWalk {
        members: &members,
        output: &output,
        first_identity_column: sort_keys.len(),
        limit,
        offset,
        corrupt_label,
    };

    program.emit_insn(Insn::Rewind {
        cursor_id: input.cursor_id,
        pc_if_empty: end_label,
    });
    window.emit(program, -1);

    // [s.., i.., exact, c..]: the member key of one input row.
    let key_start = program.alloc_registers(members.key_width);
    let identity_start = key_start + sort_keys.len();
    let exact_reg = identity_start + identity_width;
    let values_start = exact_reg + 1;
    let weight_reg = program.alloc_register();
    let old_count_reg = program.alloc_register();
    let new_count_reg = program.alloc_register();
    let loop_label = program.allocate_label();
    let applied_label = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: input.cursor_id,
        pc_if_empty: applied_label,
    });
    program.preassign_label_to_next_insn(loop_label);
    for column in 0..identity_width {
        program.emit_insn(Insn::Column {
            cursor_id: input.cursor_id,
            column,
            dest: identity_start + column,
            default: None,
        });
    }
    for column in 0..input.width {
        program.emit_insn(Insn::Column {
            cursor_id: input.cursor_id,
            column: input.value_start + column,
            dest: values_start + column,
            default: None,
        });
    }
    for (index, sort_key) in sort_keys.iter().enumerate() {
        program.emit_insn(Insn::Copy {
            src_reg: values_start + sort_key.column,
            dst_reg: key_start + index,
            extra_amount: 0,
        });
    }
    emit_exact_value_record(program, values_start, input.width, exact_reg);
    program.emit_insn(Insn::Column {
        cursor_id: input.cursor_id,
        column: input.weight_column,
        dest: weight_reg,
        default: None,
    });
    emit_add_to_member(
        program,
        &members,
        key_start,
        weight_reg,
        old_count_reg,
        new_count_reg,
        corrupt_label,
    );
    program.emit_insn(Insn::Next {
        cursor_id: input.cursor_id,
        pc_if_next: loop_label,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(applied_label);

    window.emit(program, 1);
    program.emit_insn(Insn::Goto {
        target_pc: end_label,
    });
    program.preassign_label_to_next_insn(corrupt_label);
    program.emit_insn(Insn::Halt {
        constraint_info: None,
        err_code: crate::error::SQLITE_ERROR,
        description: format!("materialized view {view_name} window state is corrupted"),
        on_error: None,
        description_reg: None,
    });
    program.preassign_label_to_next_insn(end_label);
    EmittedNodeOutput::from_ephemeral(node_id, output, contract)
}

struct WindowWalk<'a> {
    members: &'a MemberTable,
    output: &'a EphemeralDelta,
    /// The member column of the first identity column.
    first_identity_column: usize,
    limit: Option<i64>,
    offset: i64,
    corrupt_label: BranchOffset,
}

impl WindowWalk<'_> {
    /// Append every row of the current window to the output with its
    /// multiplicity in the window times `sign`. A row on the window edge
    /// may count only partly.
    fn emit(&self, program: &mut ProgramBuilder, sign: i64) {
        if self.limit == Some(0) {
            return;
        }
        let output = self.output;
        let identity_width = output.identity_width();
        let first_value_column = self.first_identity_column + identity_width + 1;
        let index_cursor = self.members.index_cursor_id();
        let table_cursor = self.members.table_cursor_id();
        let skip_reg = program.alloc_register();
        let left_reg = program.alloc_register();
        let zero_reg = program.alloc_register();
        let sign_reg = program.alloc_register();
        let rowid_reg = program.alloc_register();
        let mult_reg = program.alloc_register();
        let take_reg = program.alloc_register();
        let weight_reg = program.alloc_register();
        let identity_start = program.alloc_registers(identity_width);
        let values_start = program.alloc_registers(output.width);
        let binding_rowid_regs = output
            .binding_rowid_columns
            .iter()
            .map(|column| column.map(|column| identity_start + column))
            .collect::<Vec<_>>();
        let done_label = program.allocate_label();
        let loop_label = program.allocate_label();
        let next_label = program.allocate_label();
        let skipped_label = program.allocate_label();
        let take_label = program.allocate_label();

        program.emit_int(self.offset, skip_reg);
        if let Some(limit) = self.limit {
            program.emit_int(limit, left_reg);
        }
        program.emit_int(0, zero_reg);
        program.emit_int(sign, sign_reg);
        program.emit_insn(Insn::Rewind {
            cursor_id: index_cursor,
            pc_if_empty: done_label,
        });
        program.preassign_label_to_next_insn(loop_label);
        program.emit_insn(Insn::IdxRowId {
            cursor_id: index_cursor,
            dest: rowid_reg,
        });
        program.emit_insn(Insn::SeekRowid {
            cursor_id: table_cursor,
            src_reg: rowid_reg,
            target_pc: self.corrupt_label,
        });
        program.emit_insn(Insn::Column {
            cursor_id: table_cursor,
            column: self.members.key_width,
            dest: mult_reg,
            default: None,
        });
        program.emit_insn(Insn::Subtract {
            lhs: mult_reg,
            rhs: skip_reg,
            dest: take_reg,
        });
        program.emit_insn(Insn::Gt {
            lhs: take_reg,
            rhs: zero_reg,
            target_pc: skipped_label,
            flags: CmpInsFlags::default(),
            collation: None,
        });
        // The whole row is before the window.
        program.emit_insn(Insn::Subtract {
            lhs: skip_reg,
            rhs: mult_reg,
            dest: skip_reg,
        });
        program.emit_insn(Insn::Goto {
            target_pc: next_label,
        });

        program.preassign_label_to_next_insn(skipped_label);
        program.emit_int(0, skip_reg);
        if self.limit.is_some() {
            program.emit_insn(Insn::Le {
                lhs: take_reg,
                rhs: left_reg,
                target_pc: take_label,
                flags: CmpInsFlags::default(),
                collation: None,
            });
            program.emit_insn(Insn::Copy {
                src_reg: left_reg,
                dst_reg: take_reg,
                extra_amount: 0,
            });
            program.preassign_label_to_next_insn(take_label);
            program.emit_insn(Insn::Subtract {
                lhs: left_reg,
                rhs: take_reg,
                dest: left_reg,
            });
        }
        for column in 0..identity_width {
            program.emit_insn(Insn::Column {
                cursor_id: table_cursor,
                column: self.first_identity_column + column,
                dest: identity_start + column,
                default: None,
            });
        }
        for column in 0..output.width {
            program.emit_insn(Insn::Column {
                cursor_id: table_cursor,
                column: first_value_column + column,
                dest: values_start + column,
                default: None,
            });
        }
        program.emit_insn(Insn::Multiply {
            lhs: take_reg,
            rhs: sign_reg,
            dest: weight_reg,
        });
        emit_ephemeral_delta_row(
            program,
            output,
            identity_start,
            &binding_rowid_regs,
            values_start,
            weight_reg,
        );
        if self.limit.is_some() {
            program.emit_insn(Insn::IfNot {
                reg: left_reg,
                target_pc: done_label,
                jump_if_null: false,
            });
        }

        program.preassign_label_to_next_insn(next_label);
        program.emit_insn(Insn::Next {
            cursor_id: index_cursor,
            pc_if_next: loop_label,
            fullscan: false,
            is_index: true,
        });
        program.preassign_label_to_next_insn(done_label);
    }
}
