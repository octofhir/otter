//! Generated nursery allocation shared by the AArch64 tiers.
//!
//! # Contents
//! - [`emit_count_allocation`]: the per-type statistics row update the Rust
//!   allocator performs for every cell.
//! - [`emit_create_context`]: `CreateContext` carved from a
//!   [`otter_vm::jit::JitContextAllocationPlan`].
//! - [`emit_copy_context`]: `CopyContext` carved from the live source cell.
//! - [`emit_closure`]: `MakeClosure` / `MakeFunction` carved from a
//!   [`otter_vm::jit::JitClosureAllocationPlan`].
//! - [`emit_bigint64`]: a one-digit BigInt carved from an `i64`.
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
    JIT_CLOSURE_CELL_BYTES, JIT_TYPE_STATS_ALLOC_BYTES_OFFSET, JIT_TYPE_STATS_ALLOC_COUNT_OFFSET,
    JIT_TYPE_STATS_LIVE_BYTES_OFFSET, JIT_TYPE_STATS_ROW_BYTES, JIT_YOUNG_CLOSURE_HEADER_WORD,
    JIT_YOUNG_CONTEXT_HEADER_WORD, JitClosureAllocationPlan, JitContextAllocationPlan,
};

use crate::entry::{
    ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_TYPE_STATS_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET,
    VALUE_UNDEFINED,
};
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};

/// Account one generated allocation of `X(size)` bytes against the
/// statistics row of `type_tag`. Clobbers `X(base)` and `X(scratch)`.
pub(crate) fn emit_count_allocation(
    ops: &mut Assembler,
    context_register: u8,
    type_tag: u8,
    size: u8,
    base: u8,
    scratch: u8,
) {
    let row = u32::from(type_tag) * JIT_TYPE_STATS_ROW_BYTES;
    let live = row + JIT_TYPE_STATS_LIVE_BYTES_OFFSET;
    let count = row + JIT_TYPE_STATS_ALLOC_COUNT_OFFSET;
    let bytes = row + JIT_TYPE_STATS_ALLOC_BYTES_OFFSET;
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(base), [X(context_register), ALLOC_WINDOW_TYPE_STATS_OFFSET]
        ; ldr X(scratch), [X(base), live]
        ; add X(scratch), X(scratch), X(size)
        ; str X(scratch), [X(base), live]
        ; ldr X(scratch), [X(base), count]
        ; add XSP(scratch), XSP(scratch), 1
        ; str X(scratch), [X(base), count]
        ; ldr X(scratch), [X(base), bytes]
        ; add X(scratch), X(scratch), X(size)
        ; str X(scratch), [X(base), bytes]
    );
}

mod receiver_dynamic;
pub(crate) use receiver_dynamic::emit_dynamic_construct_receiver;

mod string;
pub(crate) use string::emit_concat;

mod bigint;
pub(crate) use bigint::{emit_bigint64, emit_unbox_bigint64};

mod empty;
pub(crate) use empty::emit_empty_literal;
mod literal;
pub(crate) use literal::{emit_array_literal, emit_object_literal};

/// Probe one bounded aligned cell; miss precedes every write.
fn emit_bump_probe(
    ops: &mut Assembler,
    context_register: u8,
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
        ; ldr X(buffer), [X(context_register), ALLOC_WINDOW_LAB_OFFSET]
        ; ldr X(candidate), [X(buffer), LAB_TOP_OFFSET]
        ; ldr X(scratch), [X(buffer), LAB_LIMIT_OFFSET]
        ; add X(end), X(candidate), X(size)
        ; cmp X(end), X(scratch)
        ; b.hi =>slow
    );
}

/// Publish the fully initialized cell and count it once; candidate survives.
fn emit_publish(ops: &mut Assembler, context_register: u8, type_tag: u8, regs: LabRegisters) {
    let LabRegisters {
        buffer,
        end,
        scratch,
        size,
        ..
    } = regs;
    dynasm!(ops ; .arch aarch64 ; str X(end), [X(buffer), LAB_TOP_OFFSET]);
    emit_count_allocation(ops, context_register, type_tag, size, buffer, scratch);
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
