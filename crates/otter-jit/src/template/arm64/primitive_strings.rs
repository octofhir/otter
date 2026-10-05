//! Baseline primitive string fits and pure ordering over existing operands.
//!
//! # Contents
//! - The shared target LAB concat helper, with a late canonical register commit.
//! - VM-owned UTF-16/numeric ordering through the exact NoAlloc Probe leaf.
//!
//! # Invariants
//! No interior character pointer crosses an operation or collecting boundary.
//! Fits use the same complete initializer and publication owner as Graph.
//! Unsupported primitives reach the existing canonical completion once; pure
//! ordering cannot run user conversion hooks or publish a safepoint.
//!
//! # See also
//! - `crate::arm64::allocation::emit_concat` owns native payload initialization.
//! - `otter_vm::runtime_stubs::PRIMITIVE_STRING_ORDER` owns ordering semantics.

use super::values::{emit_load_reg, emit_load_runtime_stub, emit_store_reg};
use crate::allocation::{AllocationValue, LabRegisters};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{THREAD_OFFSET, Unsupported, VM_THREAD_GC_HEAP_OFFSET};
use crate::template::CompareKind;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

pub(super) fn emit_concat_fit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    lhs: u16,
    rhs: u16,
    slow: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    let regs = LabRegisters {
        buffer: 11,
        candidate: 12,
        end: 13,
        scratch: 15,
        size: 17,
    };
    crate::arm64::allocation::emit_concat(
        ops,
        20,
        view.string_layout,
        [AllocationValue::Register(9), AllocationValue::Register(10)],
        regs,
        slow,
    );
    emit_store_reg(ops, regs.candidate, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

pub(super) fn emit_order(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    kind: CompareKind,
    miss: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; mov x1,x9 ; mov x2,x10
        ; ldr x0,[x20,THREAD_OFFSET] ; ldr x0,[x0,VM_THREAD_GC_HEAP_OFFSET]);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::PRIMITIVE_STRING_ORDER.entry_addr() as u64,
        abi::STUB_PRIMITIVE_STRING_ORDER,
    );
    let success = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; blr x16 ; cbz x1,=>success
        ; cmp x1,#abi::NativeResultStatus::SideExit as u32 ; b.eq =>miss
        ; b =>fatal ; =>success);
    // Unordered +2 satisfies only !=. Unsigned <=1 excludes both -1 and +2.
    match kind {
        CompareKind::Eq => dynasm!(ops ; .arch aarch64 ; cmp w0,#0 ; cset w13,eq),
        CompareKind::Ne => dynasm!(ops ; .arch aarch64 ; cmp w0,#0 ; cset w13,ne),
        CompareKind::Lt => dynasm!(ops ; .arch aarch64 ; cmp w0,#0 ; cset w13,lt),
        CompareKind::Le => dynasm!(ops ; .arch aarch64 ; cmp w0,#0 ; cset w13,le),
        CompareKind::Gt => dynasm!(ops ; .arch aarch64 ; cmp w0,#1 ; cset w13,eq),
        CompareKind::Ge => dynasm!(ops ; .arch aarch64 ; cmp w0,#1 ; cset w13,ls),
    }
}
