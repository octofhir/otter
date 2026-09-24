//! Shared x86-64 receiver proofs for intrinsic-prototype CacheIR loads.
//!
//! # Contents
//! - [`emit`] validates the cell type and optional instance latch, then loads
//!   the pinned realm prototype used by Template and Machine property programs.
//!
//! # Invariants
//! - Receiver checks precede every body read; misses have no effects.
//! - The result is a complete tagged cell pointer, never a compressed offset.
//! - Prototype shape, descriptor and slot checks remain subsequent CacheIR ops.
//! - No allocation, runtime call, safepoint or moving address retention occurs.
//!
//! # See also
//! - `otter_vm::jit::JitIntrinsicPrototype` for the shared receiver declaration.

use super::*;

/// Guard `receiver`, then leave the full pinned prototype pointer in `r8`.
/// Clobbers `r8..r10`; the receiver must not use these scratch registers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    target: otter_vm::jit::JitIntrinsicPrototype,
    byte_pc: u32,
    receiver: u8,
    miss: DynamicLabel,
) {
    if !target.is_generated_receiver() {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    }
    emit_load_u64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test Rq(receiver), r10
        ; jnz =>miss
        ; mov r8d, Rd(receiver)
        ; test r8d, r8d
        ; jz =>miss
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        9,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r8, r9
        ; cmp BYTE [r8], target.type_tag as i8
        ; jne =>miss
    );
    if let Some(guard) = target.guard {
        match guard.width {
            otter_vm::jit::JitGuardWidth::Byte => {
                dynasm!(ops ; .arch x64 ; movzx r10d, BYTE [r8 + guard.byte as i32]);
            }
            otter_vm::jit::JitGuardWidth::Word32 => {
                dynasm!(ops ; .arch x64 ; mov r10d, [r8 + guard.byte as i32]);
            }
            otter_vm::jit::JitGuardWidth::Word64 => {
                dynasm!(ops ; .arch x64 ; mov r10, [r8 + guard.byte as i32]);
            }
        }
        // Materialize the expected u32 exactly: x86 cmp imm32 sign-extends.
        dynasm!(ops
            ; .arch x64
            ; mov r8d, guard.expect as i32
            ; cmp r10, r8
            ; jne =>miss
        );
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        8,
        u64::from(target.proto_offset),
        RelocationTarget::GuardedHeapReference {
            component: crate::artifact::relocation::GuardedHeapComponent::Prototype,
            byte_pc,
            runtime_stub_id: abi::STUB_JIT_LOAD_PROPERTY.id,
        },
    );
    dynasm!(ops ; .arch x64 ; add r8, r9);
}
