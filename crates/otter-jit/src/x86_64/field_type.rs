//! Field-store observation through defining shape cells.
//!
//! # Contents
//! - Captured historical targets bypass observation for unchanged membership.
//! - Inline Top and exact-shape membership probes with scalar widening.
//! - A cold noncollecting VM leaf through the platform C boundary.
//!
//! # Invariants
//! - Every store guard precedes observation. A store whose selected record has
//!   owner zero emits no observation; the dynamic probe's zero owner still
//!   denotes an absorbing Top summary.
//! - Rooted captured shapes already belong to the monotone owner word, even
//!   after losing stability. Their offsets prove only current RHS membership,
//!   never the shape of a value read from a field.
//! - The owner temporary is consumed; the RHS and other registers survive.
//! - Only r10/r11 and flags are scratch on a hit. Cold calls save every
//!   volatile GP and scalar FP register without a collecting root recipe.
//! - One VM thread owns widening. Scalar stores match relaxed atomic writes;
//!   the leaf cannot allocate, collect, throw or reenter and needs no safepoint.
//!
//! # See also
//! - `otter_vm::object::field_type` owns classification and monotone widening.
//! - [`super::call_abi`] owns platform C arguments and result normalization.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::object::field_type::{
    FIRST_SHAPE_SHIFT, NULL, SECOND_SHAPE_SHIFT, SHAPE_MASK, TOP, UNDEFINED,
};
use otter_vm::value::tag;
use otter_vm::{JitCompileSnapshot, native_abi as abi, object::ShapeState};

use super::values::{emit_load_symbol_u64, emit_load_u64};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_NULL, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
};
use crate::field_store::FieldStoreOwner;

/// Observe a store through `record`'s nonzero defining owner, already
/// materialized in `Rq(owner)`: captured membership first, then the live word.
pub(crate) fn emit_observe_known(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
    record: &FieldStoreOwner,
) {
    debug_assert!(owner != value && !matches!(owner, 4 | 5 | 10 | 11 | 13 | 14 | 15));
    debug_assert!(!matches!(value, 10 | 11));
    debug_assert!(record.observes());
    let (stored_shapes, nullish_mask) = (record.stored_shapes.as_slice(), record.nullish_mask);
    debug_assert!(stored_shapes.len() <= 2 && stored_shapes.iter().all(|shape| *shape != 0));
    let nullish_mask = u64::from(nullish_mask);
    debug_assert!(nullish_mask & !(NULL | UNDEFINED) == 0);
    if stored_shapes.is_empty() && nullish_mask == 0 {
        emit_observe(ops, relocations, view, owner, value);
        return;
    }
    let done = ops.new_dynamic_label();
    let scalar = ops.new_dynamic_label();
    let fallback = ops.new_dynamic_label();
    if !stored_shapes.is_empty() {
        emit_load_u64(ops, 10, 0xfffe_0000_0000_0007);
        dynasm!(ops ; .arch x64
            ; test Rq(value), r10 ; jnz =>scalar
            ; test Rq(value), Rq(value) ; jz =>fallback
            ; cmp BYTE [Rq(value)], OBJECT_BODY_TYPE_TAG as i8 ; jne =>fallback
            ; mov r10d, [Rq(value) + view.object_shape_byte as i32]);
        for &shape in stored_shapes {
            dynasm!(ops ; .arch x64 ; cmp r10d, shape as i32 ; je =>done);
        }
        dynasm!(ops ; .arch x64 ; jmp =>fallback);
    }
    dynasm!(ops ; .arch x64 ; =>scalar);
    if nullish_mask == NULL | UNDEFINED {
        dynasm!(ops ; .arch x64
            ; mov r10, Rq(value) ; or r10, tag::UNDEFINED_TAG as i32
            ; cmp r10, VALUE_UNDEFINED as i32 ; je =>done);
    } else if nullish_mask != 0 {
        let nullish = if nullish_mask == NULL {
            VALUE_NULL
        } else {
            VALUE_UNDEFINED
        };
        dynasm!(ops ; .arch x64 ; cmp Rq(value), nullish as i32 ; je =>done);
    }
    dynasm!(ops ; .arch x64 ; =>fallback);
    emit_observe(ops, relocations, view, owner, value);
    dynasm!(ops ; .arch x64 ; =>done);
}

pub(crate) fn emit_observe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
) {
    debug_assert!(owner != value && !matches!(owner, 4 | 5 | 10 | 11 | 13 | 14 | 15));
    debug_assert!(!matches!(value, 10 | 11));
    let done = ops.new_dynamic_label();
    let scalar = ops.new_dynamic_label();
    let top = ops.new_dynamic_label();
    let null = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; test Rd(owner), Rd(owner) ; jz =>done);
    emit_load_symbol_u64(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch x64
        ; add Rq(owner), r10
        ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], TOP as i8 ; jnz =>done);
    emit_load_u64(ops, 10, 0xfffe_0000_0000_0007);
    dynasm!(ops ; .arch x64
        ; test Rq(value), r10 ; jnz =>scalar ; test Rq(value), Rq(value) ; jz =>top
        ; cmp BYTE [Rq(value)], OBJECT_BODY_TYPE_TAG as i8 ; jne =>top
        ; mov r10d, [Rq(value) + view.object_shape_byte as i32] ; test r10d, r10d ; jz =>top);
    emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(value) ; add r10, r11
        ; test BYTE [r10 + view.shape_state_byte as i32], (ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK) as i8 ; jnz =>top
        ; mov r10, [r10 + view.shape_id_byte as i32]
        ; mov r11, r10 ; shr r11, SHAPE_MASK.count_ones() as i8 ; test r11, r11 ; jnz =>top
        ; test r10, r10 ; jz =>top
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; shr r11, FIRST_SHAPE_SHIFT as i8 ; and r11d, SHAPE_MASK as i32
        ; cmp r10, r11 ; je =>done
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; shr r11, SECOND_SHAPE_SHIFT as i8
        ; cmp r10, r11 ; je =>done ; jmp =>slow
        ; =>scalar);
    emit_load_u64(ops, 10, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; cmp Rq(value), r10 ; je =>null);
    emit_load_u64(ops, 10, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64
        ; cmp Rq(value), r10 ; je =>undefined ; jmp =>top
        ; =>null ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], NULL as i8 ; jnz =>done
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32] ; or r11, NULL as i32
        ; mov [Rq(owner) + view.shape_field_type_byte as i32], r11 ; jmp =>done
        ; =>undefined ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], UNDEFINED as i8 ; jnz =>done
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32] ; or r11, UNDEFINED as i32
        ; mov [Rq(owner) + view.shape_field_type_byte as i32], r11 ; jmp =>done
        ; =>top ; mov QWORD [Rq(owner) + view.shape_field_type_byte as i32], TOP as i32 ; jmp =>done
        ; =>slow ; sub rsp, 208);
    let registers = [0u8, 1, 2, 6, 7, 8, 9, 10, 11];
    for (index, register) in registers.iter().copied().enumerate() {
        dynasm!(ops ; .arch x64 ; mov [rsp + index as i32 * 8], Rq(register));
    }
    for register in 0u8..16 {
        dynasm!(ops ; .arch x64 ; movsd [rsp + 72 + i32::from(register) * 8], Rx(register));
    }
    if let Some(index) = registers.iter().position(|&register| register == value) {
        dynasm!(ops ; .arch x64 ; mov rdx, [rsp + index as i32 * 8]);
    } else {
        dynasm!(ops ; .arch x64 ; mov rdx, Rq(value));
    }
    if let Some(index) = registers.iter().position(|&register| register == owner) {
        dynasm!(ops ; .arch x64 ; mov esi, [rsp + index as i32 * 8]);
    } else {
        dynasm!(ops ; .arch x64 ; mov esi, Rd(owner));
    }
    dynasm!(ops ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]);
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        otter_vm::runtime_stubs::FIELD_TYPE_OBSERVE.entry_addr() as u64,
        RelocationTarget::runtime_stub(abi::STUB_FIELD_TYPE_OBSERVE),
    );
    super::call_abi::emit_runtime_call(ops, abi::STUB_FIELD_TYPE_OBSERVE);
    for register in 0u8..16 {
        dynasm!(ops ; .arch x64 ; movsd Rx(register), [rsp + 72 + i32::from(register) * 8]);
    }
    for (index, register) in registers.iter().copied().enumerate() {
        dynasm!(ops ; .arch x64 ; mov Rq(register), [rsp + index as i32 * 8]);
    }
    dynasm!(ops ; .arch x64 ; add rsp, 208 ; =>done);
}
