//! Generated nursery allocation shared by the AArch64 tiers.
//!
//! # Contents
//! - [`emit_create_context`]: `CreateContext` carved from a
//!   [`otter_vm::jit::JitContextAllocationPlan`].
//! - [`emit_copy_context`]: `CopyContext` carved from the live source cell.
//! - [`emit_closure`]: `MakeClosure` / `MakeFunction` carved from a
//!   [`otter_vm::jit::JitClosureAllocationPlan`].
//! - [`emit_bigint64`]: a one-digit BigInt carved from an `i64`.
//! - [`emit_grow_tagged_slab`]: an Array's grown tagged element storage.
//!
//! - Dynamic callee receivers use the VM-owned current family preparation.
//!
//! # Invariants
//! - A carve reads the linear allocation buffer through the entry context's
//!   allocation window. A disabled window names an empty buffer whose bump
//!   always misses, so the heap turns generated allocation off by emptying
//!   it (marking, stress, tenuring and heap caps all do).
//! - Every word of a cell is written before the bump cursor is published,
//!   and nothing between the bump probe and the publication can collect.
//! - A miss branches before any effect, with the caller's input registers
//!   and every register outside the documented clobber set intact.
//! - Allocation clients use declared temporaries and explicit value recipes;
//!   their candidates remain separate from the committed result.
//! - A fresh cell is young and unmarked, so its initializing stores need no
//!   write barrier.
//!
//! # See also
//! - `otter_vm::context` — the context body and its initialization contract.
//! - `otter_gc::heap` — the Rust-side buffer bump these carves mirror.

use crate::allocation::{AllocationValue, LabRegisters};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JIT_CLOSURE_CELL_BYTES, JIT_CLOSURE_MAP_RARE_WORD, JIT_YOUNG_CLOSURE_HEADER_WORD,
    JIT_YOUNG_CONTEXT_HEADER_WORD, JitClosureAllocationPlan, JitContextAllocationPlan,
};

use crate::entry::{ALLOC_WINDOW_LAB_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET, VALUE_UNDEFINED};
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};

mod receiver_dynamic;
pub(crate) use receiver_dynamic::{
    emit_dynamic_construct_receiver, emit_planned_construct_receiver,
};

mod string;
pub(crate) use string::emit_concat;

mod bigint;
pub(crate) use bigint::{emit_bigint64, emit_unbox_bigint64};

mod empty;
pub(crate) use empty::emit_empty_literal;
mod slab;
pub(crate) use slab::emit_grow_tagged_slab;
mod literal;
pub(crate) use literal::{emit_array_literal, emit_boilerplate_array, emit_object_literal};

/// Probe one bounded aligned cell in the nursery buffer; miss precedes
/// every write.
fn emit_bump_probe(
    ops: &mut Assembler,
    context_register: u8,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    emit_bump_probe_in(ops, context_register, ALLOC_WINDOW_LAB_OFFSET, regs, slow);
}

/// [`emit_bump_probe`] in the buffer the window holds at `lab_offset`.
fn emit_bump_probe_in(
    ops: &mut Assembler,
    context_register: u8,
    lab_offset: u32,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    let LabRegisters {
        buffer,
        candidate,
        end,
        scratch,
        size,
    } = regs;
    let registers = [buffer, candidate, end, scratch, size];
    debug_assert!(registers.iter().enumerate().all(|(index, register)| {
        !registers[..index].contains(register) && *register != context_register
    }));
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(buffer), [X(context_register), lab_offset]
        ; ldr X(candidate), [X(buffer), LAB_TOP_OFFSET]
        ; ldr X(scratch), [X(buffer), LAB_LIMIT_OFFSET]
        ; add X(end), X(candidate), X(size)
        ; cmp X(end), X(scratch)
        ; b.hi =>slow
    );
}

/// Paint the old cell `cell` black while marking runs: no visit follows a
/// cell born during a cycle. Its initializing edges are barriered by the
/// caller or proven marked. Clobbers x16.
pub(crate) fn emit_black_when_marking(ops: &mut Assembler, context: u8, cell: u8) {
    use otter_vm::jit::{JIT_GC_BLACK_COLOR, JIT_HEADER_FLAGS_BYTE};
    dynasm!(ops ; .arch aarch64
        ; ldr x16, [X(context), crate::entry::THREAD_OFFSET]
        ; ldr x16, [x16, crate::entry::VM_THREAD_MARKING_FLAG_CELL_OFFSET]
        ; ldrb w16, [x16]
        ; cbz w16, >unmarked
        ; ldrb w16, [X(cell), JIT_HEADER_FLAGS_BYTE]
        ; orr w16, w16, u32::from(JIT_GC_BLACK_COLOR)
        ; strb w16, [X(cell), JIT_HEADER_FLAGS_BYTE]
        ; unmarked:);
}

/// Publish the fully initialized cell; candidate survives. The heap derives
/// the allocated bytes from the buffer's progress.
fn emit_publish(ops: &mut Assembler, regs: LabRegisters) {
    let LabRegisters { buffer, end, .. } = regs;
    dynasm!(ops ; .arch aarch64 ; str X(end), [X(buffer), LAB_TOP_OFFSET]);
}

mod context;
pub(crate) use context::{emit_copy_context, emit_create_context};
mod closure;
pub(crate) use closure::emit_closure;

/// Read one explicit value without allocating or changing any other register.
pub(crate) fn emit_value(ops: &mut Assembler, destination: u8, value: AllocationValue) {
    match value {
        AllocationValue::Constant(bits) => emit_load_u64(ops, destination, bits),
        AllocationValue::Register(register) => {
            dynasm!(ops ; .arch aarch64 ; mov X(destination), X(register));
        }
        AllocationValue::StackByte(byte) if byte <= 32760 && byte.is_multiple_of(8) => {
            dynasm!(ops ; .arch aarch64 ; ldr X(destination), [sp, byte]);
        }
        AllocationValue::StackByte(byte) => {
            emit_load_u64(ops, destination, u64::from(byte));
            dynasm!(ops ; .arch aarch64 ; add XSP(destination), sp, X(destination) ; ldr X(destination), [X(destination)]);
        }
    }
}

fn validate_inputs(regs: LabRegisters, values: &[AllocationValue]) {
    for value in values {
        if let AllocationValue::Register(register) = value {
            assert!(
                ![
                    regs.buffer,
                    regs.candidate,
                    regs.end,
                    regs.scratch,
                    regs.size
                ]
                .contains(register)
            );
        }
    }
}

/// Publish the actual own DerivedThis identity after complete context allocation.
/// The immutable plan proves the descriptor; the frame check excludes inline
/// descendants and ordinary calls. No heap read, safepoint or barrier occurs.
pub(crate) fn emit_publish_derived_this_context(
    ops: &mut Assembler,
    frame: u8,
    result: u8,
    source_function_id: u32,
    scratch: [u8; 2],
) {
    use otter_vm::native_abi as abi;
    debug_assert!(!scratch.contains(&result) && !scratch.contains(&frame));
    let [a, b] = scratch;
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldrb W(a), [X(frame), crate::entry::NATIVE_FRAME_FLAGS_OFFSET]
        ; tst W(a), u32::from(abi::NativeFrameFlags::DERIVED_CONSTRUCTOR)
        ; b.eq =>done
        ; ldr W(a), [X(frame)]);
    emit_load_u64(ops, b, u64::from(source_function_id));
    dynasm!(ops ; .arch aarch64 ; cmp W(a), W(b) ; b.ne =>done
        ; str W(result), [X(frame), abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET]
        ; =>done);
}

mod group;
pub(crate) use group::emit_fixed_group;
