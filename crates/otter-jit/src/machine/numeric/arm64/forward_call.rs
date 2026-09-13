//! Allocator-visible forwarding through the shared native call lifecycle.
//!
//! # Contents
//! - Pre-effect source admission and current-target native dispatch.
//! - Operand and mapped-binding reads from precise moving-root homes.
//!
//! # Invariants
//! - The ordinary call descriptor already owns clobbers, roots and exception CFG.
//! - Source materialization exits precede call effects and restore caller roots.
//! - A native target miss reaches the single committed boxed-value cold sibling.
//! - Dynamic linkage uses the recovered caller base without changing SP over a
//!   live private callee. Mapped bindings are loaded after capture allocation.

use super::*;

pub(super) fn emit(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    transitions: &TransitionTable,
    instruction: &crate::machine::MachineInstruction,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    locations: &[AllocatedLocation],
    result_index: usize,
    logical_pc: u32,
    byte_pc: u32,
    cold: DynamicLabel,
    finish_error: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_u64(ops, 15, u64::from(logical_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
    );
    crate::arm64::emit_runtime_forward(
        ops,
        relocations,
        view,
        transitions,
        [
            u16::try_from(result_index)
                .map_err(|_| Unsupported::OperandShape("forward result index"))?,
            0,
            1,
            2,
        ],
        logical_pc,
        byte_pc,
        None,
        cold,
        finish_error,
        threw,
        fatal,
        done,
        19,
        |ops, source, target, bias| {
            let value = instruction
                .operands
                .get(usize::from(source))
                .ok_or(Unsupported::OperandShape("forward source operand"))?
                .value;
            emit_load_safepoint_root(
                ops,
                frame,
                site,
                value,
                target,
                bias.checked_add(MACHINE_ROOT_RECORD_SIZE)
                    .ok_or(Unsupported::OperandShape("forward root bias"))?,
            )
        },
        |ops, destination, source, bias| {
            let location = *locations
                .get(usize::from(destination))
                .ok_or(Unsupported::OperandShape("forward result location"))?;
            emit_store_allocated_tagged(ops, frame, location, source, bias)
        },
        |ops| {
            dynasm!(ops ; .arch aarch64 ; mov x17, x0);
            emit_clear_machine_roots(ops);
            emit_reload_safepoint_roots(ops, frame, site)?;
            dynasm!(ops ; .arch aarch64 ; mov x0, x17);
            Ok(())
        },
        |ops, register, base| {
            let index = view
                .code_block
                .forwarded_argument_bindings()
                .filter_map(|(_, storage)| match storage {
                    otter_bytecode::ArgumentBindingStorage::Register { reg } => Some(reg),
                    _ => None,
                })
                .position(|reg| reg == register)
                .ok_or(Unsupported::OperandShape("forward binding operand"))?
                + 3;
            let value = instruction
                .operands
                .get(index)
                .ok_or(Unsupported::OperandShape("forward binding input"))?
                .value;
            let root = site
                .roots
                .iter()
                .find(|root| root.value == value)
                .ok_or(Unsupported::OperandShape("forward binding root"))?;
            let offset = root_offset(frame, root.save_slot)?
                .checked_add(MACHINE_ROOT_RECORD_SIZE)
                .ok_or(Unsupported::OperandShape("forward binding home offset"))?;
            emit_load_u64(ops, 16, u64::from(offset));
            dynasm!(ops ; .arch aarch64 ; add x16, X(base), x16 ; ldr x14, [x16]);
            Ok(())
        },
    )
}

/// Only a native miss needs caller-materialization admission. The native plan
/// has already proved an elided intrinsic source, so a hit needs no second probe.
pub(super) fn emit_cold_source_admission(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    instruction: &crate::machine::MachineInstruction,
    frame: MachineFrameLayout,
    site: &MachineSafepointSite,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let abi_source = otter_vm::native_abi::STUB_JIT_FORWARD_SOURCE_READY;
    emit_load_safepoint_root(
        ops,
        frame,
        site,
        instruction.operands[0].value,
        1,
        MACHINE_ROOT_RECORD_SIZE,
    )?;
    dynasm!(ops ; .arch aarch64 ; mov x0, x19);
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        transitions.entry(abi_source),
        RelocationTarget::runtime_stub(abi_source),
    );
    let admitted = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; blr x16 ; cbnz x0, =>admitted);
    emit_clear_machine_roots(ops);
    emit_reload_safepoint_roots(ops, frame, site)?;
    dynasm!(ops ; .arch aarch64 ; b =>bail ; =>admitted);
    Ok(())
}
