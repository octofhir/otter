//! Descriptor-resolved runtime transitions for the template backend.
//!
//! # Contents
//! - [`TransitionTable`] — per-compile resolution of classified stub
//!   descriptors to validated machine entries.
//! - Reentrant transition emitters completing whole opcodes in the VM.
//! - The allocating call-packet emitter publishing a concrete safepoint.
//!
//! # Invariants
//! - Every baked entry is resolved by descriptor id and validated against the
//!   descriptor's signature family before emission; raw addresses are never
//!   consumed without that check.
//! - Reentrant transitions receive the entry context and report status in
//!   `x0`; nonzero branches to the shared throw epilogue.
//! - Allocating calls build the frozen call-packet layout on the machine
//!   stack, name a concrete safepoint, and are followed by no derived-pointer
//!   reuse — operands re-load from the rooted frame window.
//!
//! # See also
//! - `crates/otter-vm/src/native_abi/runtime_stubs.rs` — the authoritative
//!   descriptor inventory.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi::{self as abi, RuntimeStubDescriptor, RuntimeStubSignature};
use otter_vm::runtime_stubs::alloc_value_stub_by_id;

use super::values::{emit_load_reg, emit_load_u64, emit_store_reg};
use crate::entry::{
    ALLOC_CTX_CODE_OBJECT_ID_OFFSET, ALLOC_CTX_FRAME_OFFSET, ALLOC_CTX_RESERVED0_OFFSET,
    ALLOC_CTX_RESERVED1_OFFSET, ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
    ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
    NATIVE_FRAME_CODE_OBJECT_ID_OFFSET, NATIVE_FRAME_OFFSET, THREAD_OFFSET, Unsupported,
    VALUE_UNDEFINED,
};

/// Per-compile resolution of the JIT-owned transition inventory.
///
/// Entries are resolved by descriptor id at compile time and baked into the
/// emitted code; the table itself never outlives one compilation.
pub(super) struct TransitionTable {
    bindings: Vec<otter_vm::JitRuntimeStubBinding>,
}

impl TransitionTable {
    pub(super) fn resolve() -> Self {
        Self {
            bindings: crate::entry::runtime_stub_bindings(),
        }
    }

    /// Validated machine entry for `descriptor`.
    ///
    /// Panics on an unknown id or a signature-family mismatch: both are
    /// compiler-construction bugs, not runtime conditions.
    pub(super) fn entry(&self, descriptor: RuntimeStubDescriptor) -> u64 {
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.id == descriptor.id)
            .unwrap_or_else(|| panic!("runtime stub {} has no JIT binding", descriptor.id));
        assert_eq!(
            binding.signature, descriptor.signature,
            "runtime stub {} bound with a different signature family",
            descriptor.id
        );
        assert_ne!(binding.entry_addr, 0);
        binding.entry_addr as u64
    }

    /// Validated machine entry for a status-reporting `Variadic` transition.
    fn variadic_entry(&self, descriptor: RuntimeStubDescriptor) -> u64 {
        assert_eq!(descriptor.signature, RuntimeStubSignature::Variadic);
        self.entry(descriptor)
    }

    /// Validated machine entry for a `NullaryValue` producer.
    fn nullary_value_entry(&self, descriptor: RuntimeStubDescriptor) -> u64 {
        assert_eq!(descriptor.signature, RuntimeStubSignature::NullaryValue);
        self.entry(descriptor)
    }
}

/// `blr` to a resolved transition entry and branch to `threw` on a nonzero
/// status in `x0`. Argument registers must already be set.
fn emit_transition_call(ops: &mut Assembler, entry: u64, threw: DynamicLabel) {
    emit_load_u64(ops, 16, entry);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
    );
}

/// Stage the entry context into `x0` (the first transition argument).
fn emit_ctx_arg(ops: &mut Assembler) {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
}

pub(super) fn emit_make_function(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(constant));
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_MAKE_FN), threw);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_make_closure(
    ops: &mut Assembler,
    table: &TransitionTable,
    code_block_id: u32,
    dst: u16,
    function: u32,
    parents: &[u32],
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    emit_load_u64(ops, 1, u64::from(code_block_id));
    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
    emit_load_u64(ops, 3, u64::from(function));
    emit_load_u64(ops, 4, parents.as_ptr() as u64);
    emit_load_u64(ops, 5, parents.len() as u64);
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_MAKE_CLOSURE), threw);
}

pub(super) fn emit_load_string(
    ops: &mut Assembler,
    table: &TransitionTable,
    code_block_id: u32,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    emit_load_u64(ops, 1, u64::from(code_block_id));
    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
    emit_load_u64(ops, 3, u64::from(constant));
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_LOAD_STRING), threw);
}

pub(super) fn emit_load_global(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    name: u32,
    code_block_id: u32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(name));
    emit_load_u64(ops, 3, u64::from(code_block_id));
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_LOAD_GLOBAL), threw);
}

pub(super) fn emit_load_builtin_error(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    constant: u32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(constant));
    emit_transition_call(
        ops,
        table.variadic_entry(abi::STUB_JIT_LOAD_BUILTIN_ERROR),
        threw,
    );
}

pub(super) fn emit_new_object(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_NEW_OBJECT), threw);
}

pub(super) fn emit_new_array(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    elements: &[u16],
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, elements.as_ptr() as u64);
    emit_load_u64(ops, 3, elements.len() as u64);
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_NEW_ARRAY), threw);
}

pub(super) fn emit_math_call(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    method: u32,
    arguments: &[u16],
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    if arguments.is_empty()
        && otter_bytecode::method_id::MathMethod::from_u32(method)
            == Some(otter_bytecode::method_id::MathMethod::Random)
    {
        // Value-producing leaf entry: no context, no status, result in x0.
        emit_load_u64(
            ops,
            16,
            table.nullary_value_entry(abi::STUB_JIT_MATH_RANDOM),
        );
        dynasm!(ops ; .arch aarch64 ; blr x16);
        return emit_store_reg(ops, 0, dst);
    }
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(method));
    emit_load_u64(ops, 3, arguments.as_ptr() as u64);
    emit_load_u64(ops, 4, arguments.len() as u64);
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_MATH_CALL), threw);
    Ok(())
}

pub(super) fn emit_fresh_upvalue(
    ops: &mut Assembler,
    table: &TransitionTable,
    index: i32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    emit_load_u64(ops, 1, u64::from(index as u32));
    emit_transition_call(
        ops,
        table.variadic_entry(abi::STUB_JIT_FRESH_UPVALUE),
        threw,
    );
}

pub(super) fn emit_define_data_property(
    ops: &mut Assembler,
    table: &TransitionTable,
    object: u16,
    key: u16,
    value: u16,
    threw: DynamicLabel,
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
        table.variadic_entry(abi::STUB_JIT_DEFINE_DATA_PROPERTY),
        threw,
    );
}

pub(super) fn emit_define_own_property(
    ops: &mut Assembler,
    table: &TransitionTable,
    target: u16,
    key: u16,
    descriptor: u16,
    threw: DynamicLabel,
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
        table.variadic_entry(abi::STUB_JIT_DEFINE_OWN_PROPERTY),
        threw,
    );
}

pub(super) fn emit_load_element(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    receiver: u16,
    index: u16,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, dst as u32
        ; movz x2, receiver as u32
        ; movz x3, index as u32
    );
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_LOAD_ELEMENT), threw);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_element(
    ops: &mut Assembler,
    table: &TransitionTable,
    receiver: u16,
    index: u16,
    value: u16,
    scratch: u16,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, receiver as u32
        ; movz x2, index as u32
        ; movz x3, value as u32
        ; movz x4, scratch as u32
    );
    emit_transition_call(
        ops,
        table.variadic_entry(abi::STUB_JIT_STORE_ELEMENT),
        threw,
    );
}

pub(super) fn emit_load_upvalue(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    index: i32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(index as u32));
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_LOAD_UPVALUE), threw);
}

pub(super) fn emit_store_upvalue(
    ops: &mut Assembler,
    table: &TransitionTable,
    src: u16,
    index: i32,
    checked: bool,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, src as u32);
    emit_load_u64(ops, 2, u64::from(index as u32));
    let descriptor = if checked {
        abi::STUB_JIT_STORE_UPVALUE_CHECKED
    } else {
        abi::STUB_JIT_STORE_UPVALUE
    };
    emit_transition_call(ops, table.variadic_entry(descriptor), threw);
}

/// Interpreter-completing `+` delegate for coercive operands.
pub(super) fn emit_add_delegate(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    lhs: u16,
    rhs: u16,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, dst as u32
        ; movz x2, lhs as u32
        ; movz x3, rhs as u32
    );
    emit_transition_call(ops, table.variadic_entry(abi::STUB_JIT_ADD), threw);
}

/// Allocating string-concat call through the isolate-resolved `AllocValue3`
/// entry: build the frozen call packet on the machine stack, name the
/// concrete `safepoint`, call, and on `Ok` store the result. Any non-`Ok`
/// status branches to `miss` (the interpreter-completing delegate path).
pub(super) fn emit_string_concat_alloc_call(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    safepoint: otter_vm::SafepointId,
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
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
        ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
        ; movz w9, safepoint
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
        ; movz w9, #0
        ; strh w9, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(safepoint));
    emit_load_reg(ops, 2, lhs)?;
    emit_load_reg(ops, 3, rhs)?;
    emit_load_u64(ops, 4, VALUE_UNDEFINED);
    emit_load_u64(ops, 16, stub_addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}
