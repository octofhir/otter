//! Descriptor-resolved runtime transitions for the template backend.
//!
//! # Contents
//! - Reentrant transition emitters completing whole opcodes in the VM.
//! - Direct reads from baked, non-moving global lexical cells and guarded
//!   global-object property records.
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
//! - Runtime entries, cage bases, and plan-owned operand slices are recorded
//!   with stable semantic identities during the existing emission pass.
//! - Baked global lexical cells are permanent old-space objects; their live
//!   value is read on every execution, while a TDZ hole uses the canonical
//!   throwing transition.
//! - A baked global-object load proves the realm epoch, dictionary shape, and
//!   property slot before reading the current value; any mismatch re-enters
//!   the canonical global lookup.
//!
//! # See also
//! - `crates/otter-vm/src/native_abi/runtime_stubs.rs` — the authoritative
//!   descriptor inventory.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi::{self as abi};
use otter_vm::runtime_stubs::alloc_value_stub_by_id;

use super::values::{
    emit_decompress_slot, emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64,
    emit_load_u64, emit_slab_base, emit_store_reg,
};
pub(super) use crate::entry::TransitionTable;
use otter_vm::JitCompileSnapshot;

use crate::artifact::relocation::{
    RelocationCapture, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
};
use crate::entry::{
    ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET,
    ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET, GLOBAL_THIS_OFFSET_PTR_OFFSET,
    NATIVE_FRAME_OFFSET, NATIVE_FRAME_UPVALUE_BASE_OFFSET, NUMBER_TAG_HI16, THREAD_OFFSET,
    Unsupported, VALUE_HOLE, VALUE_UNDEFINED, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
};
use crate::template::TemplateTail;

/// `blr` to a resolved transition entry and branch to `threw` on a nonzero
/// status in `x0`. Argument registers must already be set.
fn emit_transition_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    entry: u64,
    descriptor: abi::RuntimeStubDescriptor,
    threw: DynamicLabel,
) {
    emit_load_runtime_stub(ops, relocations, 16, entry, descriptor);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
    );
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
    );
}

pub(super) fn emit_load_string(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_STRING),
        abi::STUB_JIT_LOAD_STRING,
        threw,
    );
}

pub(super) fn emit_load_regexp(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_REGEXP),
        abi::STUB_JIT_LOAD_REGEXP,
        threw,
    );
}

pub(super) fn emit_load_global(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    name: u32,
    code_block_id: u32,
    byte_pc: u32,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if let Some(target) = view.global_lexical_loads.get(&byte_pc)
        && let Some(cell_addr) = view.cage_base.checked_add(target.cell_offset as usize)
    {
        emit_load_symbol_u64(
            ops,
            relocations,
            13,
            cell_addr as u64,
            RelocationTarget::GlobalLexicalCell { byte_pc },
        );
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x13, view.upvalue_value_byte]);
        emit_load_u64(ops, 11, VALUE_HOLE);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x9, x11
            ; b.eq =>miss
        );
        emit_store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    } else if let Some(target) = view.global_object_loads.get(&byte_pc) {
        dynasm!(ops
            ; .arch aarch64
            ; ldr x14, [x20, THREAD_OFFSET]
            ; ldr x14, [x14, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET]
            ; cbz x14, =>miss
            ; ldr x15, [x14]
        );
        emit_load_u64(ops, 11, target.global_lexical_epoch);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x15, x11
            ; b.ne =>miss
            ; ldr x14, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET]
            ; ldr w12, [x14]
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            14,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x14, x12
            ; ldr w14, [x13, view.object_shape_byte]
        );
        if target.dictionary {
            dynasm!(ops
                ; .arch aarch64
                ; cbnz w14, =>miss
                ; ldr x14, [x13, view.object_dictionary_shape_id_byte]
            );
            emit_load_u64(ops, 11, target.shape);
            dynasm!(ops
                ; .arch aarch64
                ; cmp x14, x11
                ; b.ne =>miss
            );
        } else {
            emit_load_u64(ops, 11, target.shape);
            dynasm!(ops
                ; .arch aarch64
                ; cmp w14, w11
                ; b.ne =>miss
            );
        }
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x13, =>miss
            ; ldr w9, [x13, target.value_byte]
        );
        emit_decompress_slot(ops, relocations, view.cage_base as u64, miss);
        emit_store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(name));
    emit_load_u64(ops, 3, u64::from(code_block_id));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_GLOBAL),
        abi::STUB_JIT_LOAD_GLOBAL,
        threw,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

pub(super) fn emit_load_builtin_error(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_BUILTIN_ERROR),
        abi::STUB_JIT_LOAD_BUILTIN_ERROR,
        threw,
    );
}

pub(super) fn emit_new_object(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_NEW_OBJECT),
        abi::STUB_JIT_NEW_OBJECT,
        threw,
    );
}

pub(super) fn emit_new_array(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    elements: &[u16],
    elements_tail: TemplateTail,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_operand_slice_address(
        ops,
        relocations,
        2,
        elements.as_ptr() as u64,
        TemplateOperandArena::Registers,
        TemplateOperandRole::NewArrayElements,
        elements_tail,
    );
    emit_load_u64(ops, 3, elements.len() as u64);
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_NEW_ARRAY),
        abi::STUB_JIT_NEW_ARRAY,
        threw,
    );
}

pub(super) fn emit_math_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    method: u32,
    arguments: &[u16],
    arguments_tail: TemplateTail,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    if arguments.is_empty()
        && otter_bytecode::method_id::MathMethod::from_u32(method)
            == Some(otter_bytecode::method_id::MathMethod::Random)
    {
        // Value-producing leaf entry: no context, no status, result in x0.
        emit_load_runtime_stub(
            ops,
            relocations,
            16,
            table.nullary_value_entry(abi::STUB_JIT_MATH_RANDOM),
            abi::STUB_JIT_MATH_RANDOM,
        );
        dynasm!(ops ; .arch aarch64 ; blr x16);
        return emit_store_reg(ops, 0, dst);
    }
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(method));
    emit_operand_slice_address(
        ops,
        relocations,
        3,
        arguments.as_ptr() as u64,
        TemplateOperandArena::Registers,
        TemplateOperandRole::MathArguments,
        arguments_tail,
    );
    emit_load_u64(ops, 4, arguments.len() as u64);
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_MATH_CALL),
        abi::STUB_JIT_MATH_CALL,
        threw,
    );
    Ok(())
}

pub(super) fn emit_fresh_upvalue(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    index: i32,
    threw: DynamicLabel,
) {
    emit_ctx_arg(ops);
    emit_load_u64(ops, 1, u64::from(index as u32));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_FRESH_UPVALUE),
        abi::STUB_JIT_FRESH_UPVALUE,
        threw,
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
    );
}

/// Emit the ordinary-dense-array fast-path guards for one element access:
/// heap cell → `ArrayBody` tag → no exotic sidecar → int32 index inside the
/// dense bounds. On the hit path the element address is left in `x16` and the
/// code falls through; any failed guard branches to `miss`. Addresses go
/// through the VM-maintained `(elements_ptr, dense_len)` body cache, so `Vec`
/// layout stays unobserved. Nothing here allocates, so no safepoint is owed.
///
/// Clobbers `x9`, `x11`-`x16`.
fn emit_dense_element_guards(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    receiver: u16,
    index: u16,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let layout = view.array_layout;
    emit_load_reg(ops, 9, receiver)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2       // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9              // low-32 Gc offset
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12        // x13 = GcHeader ptr
        ; ldrb w14, [x13]
        ; cmp w14, layout.type_tag as u32
        ; b.ne =>miss
        ; ldr x14, [x13, layout.exotic_byte]
        ; cbnz x14, =>miss         // exotic sidecar: the stub owns semantics
    );
    emit_load_reg(ops, 15, index)?;
    dynasm!(ops
        ; .arch aarch64
        ; lsr x11, x15, #48
        ; movz x12, NUMBER_TAG_HI16
        ; cmp x11, x12
        ; b.ne =>miss              // index is not an int32 payload
        ; ldr w16, [x13, layout.dense_len_byte]
        ; cmp w15, w16
        ; b.hs =>miss              // unsigned: negative indices miss too
        ; ldr x16, [x13, layout.elements_ptr_byte]
        ; add x16, x16, w15, uxtw #3
    );
    Ok(())
}

pub(super) fn emit_load_element(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    receiver: u16,
    index: u16,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Dense fast path: read the element straight from the buffer. A hole is an
    // absent property — the prototype chain answers — so it misses like every
    // other failed guard.
    if view.cage_base != 0 {
        emit_dense_element_guards(ops, relocations, view, receiver, index, miss)?;
        emit_load_u64(ops, 11, VALUE_HOLE);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x16]
            ; cmp x9, x11
            ; b.eq =>miss
        );
        emit_store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, dst as u32
        ; movz x2, receiver as u32
        ; movz x3, index as u32
    );
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_ELEMENT),
        abi::STUB_JIT_LOAD_ELEMENT,
        threw,
    );
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
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Dense fast path for a primitive value: an in-bounds overwrite of a
    // non-hole element with a non-cell value owes no generational write
    // barrier and cannot allocate. A cell value takes the stub (barrier), a
    // hole takes the stub (a prototype setter may observe the store).
    if view.cage_base != 0 {
        emit_dense_element_guards(ops, relocations, view, receiver, index, miss)?;
        emit_load_u64(ops, 11, VALUE_HOLE);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x16]
            ; cmp x9, x11
            ; b.eq =>miss
        );
        emit_load_reg(ops, 9, value)?;
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #0x2       // NOT_CELL_MASK
            ; tst x9, x11
            ; b.eq =>miss              // heap cell: the stub owns the barrier
            ; str x9, [x16]
            ; b =>done
        );
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    emit_ctx_arg(ops);
    dynasm!(ops
        ; .arch aarch64
        ; movz x1, receiver as u32
        ; movz x2, index as u32
        ; movz x3, value as u32
    );
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_STORE_ELEMENT),
        abi::STUB_JIT_STORE_ELEMENT,
        threw,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

pub(super) fn emit_load_upvalue(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    index: i32,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Inline captured-binding read: the spine holds 4-byte compressed cell
    // handles, and the cell's captured Value sits at a fixed offset. Only a
    // TDZ hole misses — the stub raises the ReferenceError with the right
    // identity. Nothing here allocates.
    if view.cage_base != 0 && index >= 0 {
        let spine_offset = (index as u32) * 4;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
            ; ldr x9, [x10, NATIVE_FRAME_UPVALUE_BASE_OFFSET]
            ; cbz x9, =>miss
            ; ldr w9, [x9, spine_offset]
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            13,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x9
            ; ldr x9, [x13, view.upvalue_value_byte]
        );
        emit_load_u64(ops, 11, crate::entry::VALUE_HOLE);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x9, x11
            ; b.eq =>miss
        );
        emit_store_reg(ops, 9, dst)?;
        dynasm!(ops ; .arch aarch64 ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>miss);
    emit_ctx_arg(ops);
    dynasm!(ops ; .arch aarch64 ; movz x1, dst as u32);
    emit_load_u64(ops, 2, u64::from(index as u32));
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(abi::STUB_JIT_LOAD_UPVALUE),
        abi::STUB_JIT_LOAD_UPVALUE,
        threw,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

pub(super) fn emit_store_upvalue(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
    emit_transition_call(
        ops,
        relocations,
        table.variadic_entry(descriptor),
        descriptor,
        threw,
    );
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
        ; and x1, x1, #0xff
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}
