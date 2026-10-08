//! Iterator-lifecycle transition emission.
//!
//! # Contents
//! - `IteratorNext` over a fast Array record, stepped in place.
//! - `GetIterator` opening, and `IteratorClose` finishing, a fast Array
//!   record in place while the realm's baked Array iteration proof holds.
//! - Reentrant calls to the VM-owned iterator completion helper.
//! - Uniform success, throw, and exact pre-effect bailout routing.
//!
//! # Invariants
//! - The VM helper commits every supported iterator opcode before returning
//!   success, so generated code only falls through once.
//! - A missing published activation is the sole bailout case and occurs before
//!   the opcode's observable work.
//! - The in-place step writes nothing until its last guard passes, so every
//!   miss reaches the helper with the record untouched.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_iterator_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::ic_probe::{element_access_for, emit_element_address, emit_element_read};
use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_u64,
    emit_prototype_validity_guard, emit_store_reg,
};
use crate::Unsupported;
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{
    NUMBER_TAG_HI16, THREAD_OFFSET, VALUE_FALSE, VALUE_TRUE, VM_THREAD_ACTIVE_REALM_CELL_OFFSET,
};

/// Branch to `miss` unless the realm whose proof `iteration` names is active.
/// Clobbers `x11`.
fn emit_iteration_realm_guard(
    ops: &mut Assembler,
    iteration: &otter_vm::jit::JitArrayIteration,
    miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x11, [x20, THREAD_OFFSET]
        ; ldr x11, [x11, VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
        ; cbz x11, =>miss
        ; ldr w11, [x11]
    );
    emit_load_u64(ops, 12, u64::from(iteration.realm));
    dynasm!(ops ; .arch aarch64 ; cmp w11, w12 ; b.ne =>miss);
}

/// `GetIterator` from `source` into the record at `record`. While the baked
/// Array iteration proof holds, an ordinary Array opens a fast record in
/// place: the array, then cursor 0. Anything else runs the VM operation.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_get_iterator(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    record: u16,
    source: u16,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if let Some(iteration) = &view.array_iteration {
        emit_load_reg(ops, 9, source)?;
        emit_cell_test(ops, 9, CellTest::IsNotCell, slow);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x9, =>slow
            ; ldrb w11, [x9]
            ; cmp w11, u32::from(iteration.array_type_tag)
            ; b.ne =>slow
            ; ldr w11, [x9, iteration.array_exotic_byte]
            ; cbnz w11, =>slow
        );
        emit_iteration_realm_guard(ops, iteration, slow);
        emit_prototype_validity_guard(ops, relocations, iteration.iterable, 11, slow);
        emit_prototype_validity_guard(ops, relocations, iteration.iterator, 11, slow);
        emit_store_reg(ops, 9, record)?;
        dynasm!(ops ; .arch aarch64 ; movz x11, NUMBER_TAG_HI16, lsl #48);
        emit_store_reg(ops, 11, record + 1)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>slow);
    emit_iterator_op(
        ops,
        relocations,
        transitions,
        Op::GetIterator as u8,
        u64::from(record),
        u64::from(source),
        0,
        bail,
        threw,
        fatal,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// `IteratorClose` / `IteratorCloseThrow` (`opcode`) of the record at
/// `record`. An exhausted fast record is done, and a live one runs nothing
/// while the baked proof keeps `return` absent; anything else runs the VM
/// operation.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_iterator_close(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    opcode: Op,
    record: u16,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_load_reg(ops, 9, record + 1)?;
    emit_load_u64(ops, 11, VALUE_TRUE);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>done);
    if let Some(iteration) = view.array_iteration.as_ref().filter(|it| it.close) {
        dynasm!(ops
            ; .arch aarch64
            ; lsr x11, x9, #48
            ; movz x12, NUMBER_TAG_HI16
            ; cmp x11, x12
            ; b.ne =>slow
        );
        emit_iteration_realm_guard(ops, iteration, slow);
        emit_prototype_validity_guard(ops, relocations, iteration.iterator, 11, slow);
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>slow);
    emit_iterator_op(
        ops,
        relocations,
        transitions,
        opcode as u8,
        u64::from(record),
        0,
        0,
        bail,
        threw,
        fatal,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// `IteratorNext` over the record at `record`. With the site's element
/// feedback, a fast Array record (see `otter_vm::iterator_record`) reads the
/// element at its cursor exactly as `LoadElement` reads an index; a generic
/// record, a cursor at the end, a hole or any failed guard runs the VM step.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_iterator_next(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    value_dst: u16,
    done_dst: u16,
    record: u16,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if let Some(access) = element_access_for(view, byte_pc) {
        emit_element_address(
            ops,
            relocations,
            view,
            access,
            |ops, register| emit_load_reg(ops, register, record),
            |ops, register| emit_load_reg(ops, register, record + 1),
            slow,
        )?;
        // The cursor's successor must stay a non-negative int32.
        emit_load_reg(ops, 13, record + 1)?;
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, #1
            ; tst w13, #0x8000_0000
            ; b.ne =>slow
        );
        emit_element_read(ops, access.element, slow);
        emit_store_reg(ops, 9, value_dst)?;
        emit_load_u64(ops, 9, VALUE_FALSE);
        emit_store_reg(ops, 9, done_dst)?;
        emit_store_reg(ops, 13, record + 1)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>slow);
    emit_iterator_op(
        ops,
        relocations,
        transitions,
        Op::IteratorNext as u8,
        u64::from(value_dst),
        u64::from(done_dst),
        u64::from(record),
        bail,
        threw,
        fatal,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_iterator_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, arg1);
    emit_load_u64(ops, 4, arg2);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_ITERATOR_OP),
        abi::STUB_JIT_ITERATOR_OP,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
    super::transitions::emit_status_word_result(ops, Some(bail), threw, fatal);
}
