//! Iterator-lifecycle transition emission.
//!
//! # Contents
//! - `IteratorNext` over a fast Array record, stepped in place.
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

use dynasmrt::{DynamicLabel, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::ic_probe::{element_access_for, emit_element_address, emit_element_read};
use super::values::{emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg};
use crate::Unsupported;
use crate::artifact::relocation::RelocationCapture;
use crate::entry::VALUE_FALSE;

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
