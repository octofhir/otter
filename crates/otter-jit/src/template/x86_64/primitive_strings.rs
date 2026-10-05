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
//! - `crate::x86_64::allocation::emit_concat` owns native payload initialization.
//! - `otter_vm::runtime_stubs::PRIMITIVE_STRING_ORDER` owns ordering semantics.

use super::*;
use crate::allocation::{AllocationValue, LabRegisters};

pub(super) fn emit_concat_fit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u16,
    lhs: u16,
    rhs: u16,
    slow: DynamicLabel,
    done: DynamicLabel,
) {
    emit_load_reg(ops, 0, lhs);
    emit_load_reg(ops, 8, rhs);
    let regs = LabRegisters {
        buffer: 6,
        candidate: 7,
        end: 9,
        scratch: 1,
        size: 11,
    };
    crate::x86_64::allocation::emit_concat(
        ops,
        15,
        view.string_layout,
        [AllocationValue::Register(0), AllocationValue::Register(8)],
        regs,
        slow,
    );
    emit_store_reg(ops, regs.candidate, dst);
    dynasm!(ops ; .arch x64 ; jmp =>done);
}

pub(super) fn emit_order(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    kind: CompareKind,
    miss: DynamicLabel,
    fatal: DynamicLabel,
) {
    // RAX/R8 still contain the original source words. ABI shuffling finishes
    // before resolving the target; the common C adapter owns Windows sret.
    dynasm!(ops ; .arch x64 ; mov rsi,rax ; mov rdx,r8
        ; mov rdi,[r15+THREAD_OFFSET as i32]
        ; mov rdi,[rdi+VM_THREAD_GC_HEAP_OFFSET as i32]);
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::PRIMITIVE_STRING_ORDER.entry_addr() as u64,
        abi::STUB_PRIMITIVE_STRING_ORDER,
    );
    emit_runtime_call(ops, abi::STUB_PRIMITIVE_STRING_ORDER);
    let success = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; test rdx,rdx ; jz =>success
        ; cmp rdx,abi::NativeResultStatus::SideExit as i32 ; je =>miss
        ; jmp =>fatal ; =>success);
    match kind {
        CompareKind::Eq => dynasm!(ops ; .arch x64 ; cmp eax,0 ; sete r10b),
        CompareKind::Ne => dynasm!(ops ; .arch x64 ; cmp eax,0 ; setne r10b),
        CompareKind::Lt => dynasm!(ops ; .arch x64 ; cmp eax,0 ; setl r10b),
        CompareKind::Le => dynasm!(ops ; .arch x64 ; cmp eax,0 ; setle r10b),
        CompareKind::Gt => dynasm!(ops ; .arch x64 ; cmp eax,1 ; sete r10b),
        CompareKind::Ge => dynasm!(ops ; .arch x64 ; cmp eax,1 ; setbe r10b),
    }
    dynasm!(ops ; .arch x64 ; movzx eax,r10b);
    emit_load_u64(ops, 11, VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; or rax,r11);
}
