//! Descriptor-resolved runtime transitions for the template backend.
//!
//! # Contents
//! - Reentrant transition emitters completing whole opcodes in the VM.
//! - The allocating call-packet emitter publishing a concrete safepoint.
//!
//! # Invariants
//! - Every baked entry is resolved by descriptor id and validated against the
//!   descriptor's signature family before emission; raw addresses are never
//!   consumed without that check.
//! - Register-window reentrant transitions receive the entry context and report
//!   status in `x0`. Computed element transitions use the fixed boxed-value
//!   `NativeResultPair` ABI (`x0` payload, canonical whole-word `x1` status);
//!   Status-word calls decode the sole `NativeResultStatus` alphabet; unknown
//!   words go directly to the structural fatal epilogue.
//! - Allocating calls build the frozen call-packet layout on the machine
//!   stack, name a concrete safepoint, and are followed by no derived-pointer
//!   reuse — operands re-load from the rooted frame window.
//! - Runtime entries, cage bases, and plan-owned operand slices are recorded
//!   with stable semantic identities during the existing emission pass.
//!
//! # See also
//! - `crates/otter-vm/src/native_abi/runtime_stubs.rs` — the authoritative
//!   descriptor inventory.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi::{self as abi};
use otter_vm::runtime_stubs::alloc_value_stub_by_id;

use super::ic_probe::{
    DenseIndexForm, element_access_for, emit_element_address, emit_element_read, emit_element_write,
};
use super::values::{
    emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64, emit_load_u64, emit_store_reg,
};
pub(super) use crate::entry::TransitionTable;
use otter_vm::JitCompileSnapshot;

use crate::artifact::relocation::{
    RelocationCapture, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
};
use crate::entry::{
    ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET,
    ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET, THREAD_OFFSET, Unsupported, VALUE_UNDEFINED,
};
use crate::template::TemplateTail;

/// Decode the sole status-word alphabet after a runtime call. Unknown words
/// are structural ABI failures and go directly to the compiled fatal exit.
pub(super) fn emit_status_word_result(
    ops: &mut Assembler,
    side_exit: Option<DynamicLabel>,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    let success = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cmp x0, abi::NativeResultStatus::Success as u32
        ; b.eq =>success
    );
    if let Some(side_exit) = side_exit {
        dynasm!(ops
            ; .arch aarch64
            ; cmp x0, abi::NativeResultStatus::SideExit as u32
            ; b.eq =>side_exit
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; cmp x0, abi::NativeResultStatus::Throw as u32
        ; b.eq =>threw
        ; b =>fatal
        ; =>success
    );
}

/// `blr` to a resolved transition entry and validate its Success/Throw result.
/// Argument registers must already be set.
fn emit_transition_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
    descriptor: abi::RuntimeStubDescriptor,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_load_runtime_stub(ops, relocations, 16, entry, descriptor);
    dynasm!(ops ; .arch aarch64 ; blr x16);
    emit_status_word_result(ops, None, threw, fatal);
}

fn emit_operand_slice_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    address: u64,
    arena: TemplateOperandArena,
    role: TemplateOperandRole,
    tail: TemplateTail,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        register,
        address,
        RelocationTarget::TemplateOperandSlice {
            arena,
            role,
            start: u32::try_from(tail.start).expect("template operand offset fits u32"),
            len: u32::try_from(tail.len).expect("template operand length fits u32"),
        },
    );
}

/// Stage the entry context into `x0` (the first transition argument).
fn emit_ctx_arg(ops: &mut Assembler) {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
}

pub(super) fn emit_make_function(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(constant));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_MAKE_FN),
        abi::STUB_JIT_MAKE_FN,
        threw,
        fatal,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_make_closure(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    code_block_id: u32,
    dst: u16,
    function: u32,
    parents: &[u32],
    parents_tail: TemplateTail,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    emit_load_u64(ops, 1, u64::from(code_block_id));
    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
    emit_load_u64(ops, 3, u64::from(function));
    emit_operand_slice_address(
        ops,
        relocations,
        4,
        parents.as_ptr() as u64,
        TemplateOperandArena::Indices,
        TemplateOperandRole::ClosureParents,
        parents_tail,
    );
    emit_load_u64(ops, 5, parents.len() as u64);
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_MAKE_CLOSURE),
        abi::STUB_JIT_MAKE_CLOSURE,
        threw,
        fatal,
    );
}

pub(super) fn emit_load_regexp(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(constant));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_REGEXP),
        abi::STUB_JIT_LOAD_REGEXP,
        threw,
        fatal,
    );
}

pub(super) fn emit_load_builtin_error(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(constant));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_BUILTIN_ERROR),
        abi::STUB_JIT_LOAD_BUILTIN_ERROR,
        threw,
        fatal,
    );
}

pub(super) fn emit_new_object(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    super::value_packet::emit_value_packet_transition(
        ops,
        relocations,
        table,
        abi::STUB_JIT_NEW_OBJECT,
        &[],
        dst,
        throw_value,
        fatal,
    )
}

pub(super) fn emit_collect_arguments(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_COLLECT_ARGUMENTS),
        abi::STUB_JIT_COLLECT_ARGUMENTS,
        threw,
        fatal,
    );
}

pub(super) fn emit_new_array(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    elements: &[u16],
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let words = elements
        .iter()
        .copied()
        .map(super::value_packet::PacketWord::Register)
        .collect::<Vec<_>>();
    super::value_packet::emit_value_packet_transition(
        ops,
        relocations,
        table,
        abi::STUB_JIT_NEW_ARRAY,
        &words,
        dst,
        throw_value,
        fatal,
    )
}

pub(super) fn emit_fresh_upvalue(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    index: i32,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    emit_load_u64(ops, 1, u64::from(index as u32));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_FRESH_UPVALUE),
        abi::STUB_JIT_FRESH_UPVALUE,
        threw,
        fatal,
    );
}

pub(super) fn emit_define_data_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    object: u16,
    key: u16,
    value: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, object as u32
        ; movz x2, key as u32
        ; movz x3, value as u32
    );
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_DEFINE_DATA_PROPERTY),
        abi::STUB_JIT_DEFINE_DATA_PROPERTY,
        threw,
        fatal,
    );
}

pub(super) fn emit_define_own_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    target: u16,
    key: u16,
    descriptor: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, target as u32
        ; movz x2, key as u32
        ; movz x3, descriptor as u32
    );
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_DEFINE_OWN_PROPERTY),
        abi::STUB_JIT_DEFINE_OWN_PROPERTY,
        threw,
        fatal,
    );
}

pub(super) fn emit_load_element(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    receiver: u16,
    index: u16,
    byte_pc: u32,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Dense fast path: read the element straight from the buffer. A hole is an
    // absent property — the prototype chain answers — so it misses like every
    // other failed guard.
    if let Some(access) = element_access_for(view, byte_pc) {
        emit_element_address(
            ops,
            relocations,
            view,
            access,
            |ops, register| emit_load_reg(ops, register, receiver),
            |ops, register| emit_load_reg(ops, register, index),
            DenseIndexForm::Tagged,
            miss,
        )?;
        emit_element_read(ops, access.element, miss);
        emit_store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    emit_ctx_arg(ops);
    emit_load_reg(ops, 1, receiver)?;
    emit_load_reg(ops, 2, index)?;
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_LOAD_ELEMENT),
        abi::STUB_JIT_LOAD_ELEMENT,
    );
    let completed = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x1, =>completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; =>completed
    );
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

pub(super) fn emit_store_element(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    receiver: u16,
    index: u16,
    value: u16,
    byte_pc: u32,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Dense fast path for a primitive value: an in-bounds overwrite of a
    // non-hole element with a non-cell value owes no generational write
    // barrier and cannot allocate. A cell value takes the stub (barrier), a
    // hole takes the stub (a prototype setter may observe the store).
    if let Some(access) = element_access_for(view, byte_pc) {
        emit_element_address(
            ops,
            relocations,
            view,
            access,
            |ops, register| emit_load_reg(ops, register, receiver),
            |ops, register| emit_load_reg(ops, register, index),
            DenseIndexForm::Tagged,
            miss,
        )?;
        emit_element_read(ops, access.element, miss);
        emit_load_reg(ops, 9, value)?;
        emit_element_write(ops, access.element, miss);
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    emit_ctx_arg(ops);
    emit_load_reg(ops, 1, receiver)?;
    emit_load_reg(ops, 2, index)?;
    emit_load_reg(ops, 3, value)?;
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_STORE_ELEMENT),
        abi::STUB_JIT_STORE_ELEMENT,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x1, =>done
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Interpreter-completing `+` delegate for coercive operands.
pub(super) fn emit_add_delegate(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    lhs: u16,
    rhs: u16,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, dst as u32
        ; movz x2, lhs as u32
        ; movz x3, rhs as u32
    );
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_ADD),
        abi::STUB_JIT_ADD,
        threw,
        fatal,
    );
}

/// Allocating string-concat call through the isolate-resolved `AllocValue3`
/// entry: build the frozen call packet on the machine stack, name the
/// concrete `safepoint`, call, and on `Ok` store the result. Any non-`Ok`
/// status branches to `miss` (the interpreter-completing delegate path).
pub(super) fn emit_string_concat_alloc_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    lhs: u16,
    rhs: u16,
    safepoint: otter_vm::native_abi::SafepointId,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let Some(stub_addr) =
        alloc_value_stub_by_id(abi::STUB_STRING_CONCAT_ALLOC.id).and_then(|stub| stub.entry_addr())
    else {
        return Ok(());
    };
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; movz w9, safepoint
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(safepoint));
    emit_load_reg(ops, 2, lhs)?;
    emit_load_reg(ops, 3, rhs)?;
    emit_load_u64(ops, 4, VALUE_UNDEFINED);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        stub_addr as u64,
        abi::STUB_STRING_CONCAT_ALLOC,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

/// Allocate `Array()` or `Array(Int32)` through the shared `AllocValue3`
/// boundary. The VM stub rejects a non-Int32 or negative length before any
/// allocation; every non-success status therefore exits at the original
/// `ArrayConstruct` and lets the canonical interpreter own wide/non-number
/// semantics, `RangeError`, and OOM reporting.
pub(super) fn emit_array_construct_alloc_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    length: Option<u16>,
    safepoint: otter_vm::native_abi::SafepointId,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let descriptor = abi::STUB_ARRAY_CONSTRUCT_ALLOC;
    let stub_addr = alloc_value_stub_by_id(descriptor.id)
        .and_then(|stub| stub.entry_addr())
        .ok_or(Unsupported::OperandShape(
            "ArrayConstruct allocating stub entry",
        ))?;
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; movz w9, safepoint
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(safepoint));
    if let Some(length) = length {
        emit_load_reg(ops, 2, length)?;
    } else {
        emit_load_u64(ops, 2, otter_vm::Value::number_i32(0).to_bits());
    }
    emit_load_u64(ops, 3, VALUE_UNDEFINED);
    emit_load_u64(ops, 4, VALUE_UNDEFINED);
    emit_load_runtime_stub(ops, relocations, 16, stub_addr as u64, descriptor);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>bail
    );
    emit_store_reg(ops, 0, dst)
}
