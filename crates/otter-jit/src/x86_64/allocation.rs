//! Generated nursery allocation shared by the x86-64 tiers.
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
//! - Empty/static literal shells and dense slabs use the same LAB probe.
//! - Constructor receivers prove their live finalized family and current
//!   prototype, then use the shared LAB owner before callee publication.
//! - Dynamic callee prefixes consult the same VM-owned preparation proof.
//!
//! # Invariants
//! - `r15` holds the entry context. A carve reads the linear allocation
//!   buffer through its allocation window; a disabled window names an empty
//!   buffer whose bump always misses, so the heap turns generated allocation
//!   off by emptying it (marking, stress, tenuring and heap caps all do).
//! - Every word of a cell is written before the bump cursor is published,
//!   and nothing between the bump probe and the publication can collect.
//! - Context/closure misses precede effects and clobber only declared temporaries.
//! - Receiver misses preserve `rsi`/`rcx` and return undefined/zero in
//!   `rax`/`rdx`; their profile counters remain observable. Literal misses
//!   clobber only their declared recipe and reserved scratch registers.
//! - A fresh cell is young and unmarked, so its initializing stores need no
//!   write barrier.
//! - Literal candidates use four allocator-declared temporaries; reserved
//!   r10/r11 and XMM15 own value initialization, never an early result.
//!
//! # See also
//! - `crate::arm64::allocation` — the peer AArch64 carves.
//! - `otter_vm::context` — the context body and its initialization contract.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JIT_CLOSURE_CELL_BYTES, JIT_CONTEXT_HAS_EXTENSION_BIT, JIT_INLINE_CONTEXT_MAX_WORDS,
    JIT_TYPE_STATS_ALLOC_BYTES_OFFSET, JIT_TYPE_STATS_ALLOC_COUNT_OFFSET,
    JIT_TYPE_STATS_LIVE_BYTES_OFFSET, JIT_TYPE_STATS_ROW_BYTES, JIT_YOUNG_CLOSURE_HEADER_WORD,
    JIT_YOUNG_CONTEXT_HEADER_WORD, JitClosureAllocationPlan, JitContextAllocationPlan,
};

use crate::allocation::{AllocationValue, LabRegisters};

use crate::entry::{
    ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_TYPE_STATS_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET,
    VALUE_UNDEFINED,
};

/// Account one generated allocation of `Rq(size)` bytes against the
/// statistics row of `type_tag`. Clobbers `Rq(base)`.
pub(crate) fn emit_count_allocation(
    ops: &mut Assembler,
    context: u8,
    type_tag: u8,
    size: u8,
    base: u8,
) {
    let row = u32::from(type_tag) * JIT_TYPE_STATS_ROW_BYTES;
    let live = (row + JIT_TYPE_STATS_LIVE_BYTES_OFFSET) as i32;
    let count = (row + JIT_TYPE_STATS_ALLOC_COUNT_OFFSET) as i32;
    let bytes = (row + JIT_TYPE_STATS_ALLOC_BYTES_OFFSET) as i32;
    dynasm!(ops
        ; .arch x64
        ; mov Rq(base), [Rq(context) + ALLOC_WINDOW_TYPE_STATS_OFFSET as i32]
        ; add [Rq(base) + live], Rq(size)
        ; add QWORD [Rq(base) + count], 1
        ; add [Rq(base) + bytes], Rq(size)
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
mod receiver;
pub(crate) use receiver::{
    emit_receiver_bump, emit_receiver_candidate_probe, emit_receiver_fit, emit_receiver_guards,
    emit_receiver_publication_effect,
};

#[cfg(test)]
#[path = "allocation/tests.rs"]
mod tests;

/// Probe a declared bounded cell. The limit miss precedes every heap write.
fn emit_bump_probe(ops: &mut Assembler, context: u8, regs: LabRegisters, slow: DynamicLabel) {
    let LabRegisters {
        buffer,
        candidate,
        end,
        scratch,
        size,
    } = regs;
    let registers = [buffer, candidate, end, scratch, size];
    debug_assert!(registers.iter().enumerate().all(|(index, register)| {
        !registers[..index].contains(register) && *register != context
    }));
    dynasm!(ops ; .arch x64
        ; mov Rq(buffer), [Rq(context) + ALLOC_WINDOW_LAB_OFFSET as i32]
        ; mov Rq(candidate), [Rq(buffer) + LAB_TOP_OFFSET as i32]
        ; lea Rq(end), [Rq(candidate) + Rq(size)]
        ; cmp Rq(end), [Rq(buffer) + LAB_LIMIT_OFFSET as i32]
        ; ja =>slow
    );
}

/// Publish the fully initialized cell and account it; candidate survives.
fn emit_publish(ops: &mut Assembler, context: u8, type_tag: u8, regs: LabRegisters) {
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.buffer) + LAB_TOP_OFFSET as i32], Rq(regs.end));
    emit_count_allocation(ops, context, type_tag, regs.size, regs.buffer);
}

mod context;
pub(crate) use context::{emit_copy_context, emit_create_context};
mod closure;
pub(crate) use closure::emit_closure;

/// Read one explicit value without allocating or changing any other register.
pub(crate) fn emit_value(ops: &mut Assembler, destination: u8, value: AllocationValue) {
    match value {
        AllocationValue::Constant(bits) => {
            crate::x86_64::values::emit_load_u64(ops, destination, bits)
        }
        AllocationValue::Register(register) => {
            dynasm!(ops ; .arch x64 ; mov Rq(destination), Rq(register));
        }
        AllocationValue::StackByte(byte) => {
            let byte = i32::try_from(byte).expect("validated canonical allocation input offset");
            dynasm!(ops ; .arch x64 ; mov Rq(destination), [rsp + byte]);
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

/// Publish the exact initialized own DerivedThis context identity. The VM plan
/// proves the scope; the physical-frame guards exclude inline/ordinary clients.
/// Both declared scratch registers and flags are dead here; result is preserved.
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
    dynasm!(ops ; .arch x64
        ; test BYTE [Rq(frame) + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32],
            abi::NativeFrameFlags::DERIVED_CONSTRUCTOR as i8
        ; jz =>done
        ; mov Rd(a), [Rq(frame)]
        ; mov Rd(b), source_function_id as i32
        ; cmp Rd(a), Rd(b) ; jne =>done
        ; mov DWORD [Rq(frame) + abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET as i32], Rd(result)
        ; =>done);
}

mod group;
pub(crate) use group::emit_fixed_group;
