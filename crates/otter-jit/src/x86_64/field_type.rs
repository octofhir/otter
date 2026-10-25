//! Field-store observation through defining shape cells.
//!
//! # Contents
//! - Captured historical targets bypass observation for unchanged membership.
//! - Inline Top, exact Int32 and category membership with scalar widening.
//! - A cold noncollecting VM leaf through the platform C boundary, also for
//!   every widening of a summary code subscribed to.
//! - [`emit_routine`] — that classification and leaf once per isolate: an
//!   optimized store tests captured membership and an absorbing Top inline
//!   and calls the routine for the rest, the value pushed above its return
//!   address.
//!
//! # Invariants
//! - Every store guard precedes observation. A store whose selected record has
//!   owner zero emits no observation; the dynamic probe's zero owner still
//!   denotes an absorbing Top summary.
//! - Rooted captured shapes already belong to the monotone owner word, even
//!   after losing stability. Their offsets prove only current RHS membership,
//!   never the shape of a value read from a field.
//! - Constant payloads never participate in shape membership. Any unequal
//!   observation permanently replaces the constant with a category union.
//! - The owner temporary is consumed; the RHS and other registers survive.
//! - Only r10/r11 and flags are scratch on a hit. Cold calls save every
//!   volatile GP and scalar FP register without a collecting root recipe.
//! - One VM thread owns widening. Scalar stores match relaxed atomic writes;
//!   the leaf cannot allocate, collect, throw or reenter and needs no safepoint.
//!
//! # See also
//! - `otter_vm::object::field_type` owns classification and monotone widening.
//! - [`super::call_abi`] owns platform C arguments and result normalization.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::object::field_type::{
    CONST_INT32, DOUBLE, FIRST_SHAPE_SHIFT, INT32, NULL, SECOND_SHAPE_SHIFT, SHAPE_MASK, TOP,
    UNDEFINED,
};
use otter_vm::value::tag;
use otter_vm::{JitCompileSnapshot, native_abi as abi, object::ShapeState};

use super::values::{emit_load_symbol_u64, emit_load_u64};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_NULL, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
};
use crate::field_store::FieldStoreOwner;
use crate::routines::{Routine, RoutineOffsets, RoutineRequest};

/// The register carrying the owner's address into the observation routine.
pub(crate) const OBSERVE_OWNER: u8 = 10;

/// Observe a store through `record`'s nonzero defining owner, already
/// materialized in `Rq(owner)`: captured membership first, then the live word.
pub(crate) fn emit_observe_known(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    routines: &RoutineRequest<'_>,
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
    if stored_shapes.is_empty() && nullish_mask == 0 && !record.int32 && !record.double {
        emit_observe_call(ops, relocations, routines, view, owner, value);
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
    if record.int32 {
        dynasm!(ops ; .arch x64
            ; mov r10, Rq(value) ; sar r10, tag::NUMBER_TAG.trailing_zeros() as i8
            ; cmp r10, -1);
        if let Some(constant) = record.int32_constant {
            dynasm!(ops ; .arch x64
                ; jne =>fallback ; cmp Rd(value), constant ; je =>done);
        } else {
            dynasm!(ops ; .arch x64 ; je =>done);
        }
    }
    if record.double {
        dynasm!(ops ; .arch x64
            ; mov r10, Rq(value) ; sar r10, 49 ; cmp r10, -1 ; je =>fallback);
        emit_load_u64(ops, 10, tag::NUMBER_TAG);
        dynasm!(ops ; .arch x64 ; test Rq(value), r10 ; jnz =>done);
    }
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
    emit_observe_call(ops, relocations, routines, view, owner, value);
    dynasm!(ops ; .arch x64 ; =>done);
}

/// Observe a store through the owner in `Rq(owner)` with the shared routine:
/// the owner's word answers an absorbing Top inline.
fn emit_observe_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    routines: &RoutineRequest<'_>,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
) {
    let done = ops.new_dynamic_label();
    emit_live_word(ops, relocations, view, owner, done);
    // The value travels on the stack: the call's target takes the other
    // scratch register. The routine pops it as it returns.
    dynasm!(ops ; .arch x64
        ; mov Rq(OBSERVE_OWNER), Rq(owner)
        ; push Rq(value));
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        routines.entry(Routine::FieldObserve),
        RelocationTarget::Routine {
            routine: Routine::FieldObserve.name(),
        },
    );
    dynasm!(ops ; .arch x64 ; call r11 ; =>done);
}

/// Emit the observation routine, returning its entry offsets. The owner's
/// address arrives in [`OBSERVE_OWNER`] and the stored value in the word
/// above the return address, which the routine pops; every other register
/// survives.
pub(crate) fn emit_routine(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) -> RoutineOffsets {
    let entry = ops.offset().0;
    let done = ops.new_dynamic_label();
    // The pushed value and the return address keep the stack aligned.
    dynasm!(ops ; .arch x64
        ; sub rsp, 16 ; mov [rsp], rax ; mov [rsp + 8], rcx
        ; mov rax, Rq(OBSERVE_OWNER) ; mov rcx, [rsp + 24]);
    emit_classify(ops, relocations, view, 0, 1, done);
    dynasm!(ops ; .arch x64
        ; =>done
        ; mov rax, [rsp] ; mov rcx, [rsp + 8] ; add rsp, 16
        ; ret 8);
    RoutineOffsets {
        entry,
        second: entry,
    }
}

/// `Rq(owner)` becomes the address of the owner it holds the offset of; a
/// zero owner or a Top summary go to `done`.
fn emit_live_word(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    done: DynamicLabel,
) {
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
}

/// Observe a store through the owner whose offset `Rq(owner)` holds, inline.
pub(crate) fn emit_observe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
) {
    let done = ops.new_dynamic_label();
    emit_live_word(ops, relocations, view, owner, done);
    emit_classify(ops, relocations, view, owner, value, done);
    dynasm!(ops ; .arch x64 ; =>done);
}

/// Widen the summary of the owner at `Rq(owner)` by `value`, going to `done`
/// once it holds the value, or falling through after the cold leaf widened
/// it.
fn emit_classify(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
    done: DynamicLabel,
) {
    // Code subscribed to the summary: every widening goes through the VM,
    // which fires it.
    const WATCHED: i8 = otter_vm::code_dependencies::ShapeFact::FieldType as i8;
    debug_assert!(owner != value && !matches!(owner, 4 | 5 | 10 | 11 | 13 | 14 | 15));
    debug_assert!(!matches!(value, 10 | 11));
    let scalar = ops.new_dynamic_label();
    let top = ops.new_dynamic_label();
    let null = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
    let int32 = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    let double_union = ops.new_dynamic_label();
    let int32_constant = ops.new_dynamic_label();
    let int32_union = ops.new_dynamic_label();
    let int32_store = ops.new_dynamic_label();
    let null_union = ops.new_dynamic_label();
    let undefined_union = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    emit_load_u64(ops, 10, 0xfffe_0000_0000_0007);
    dynasm!(ops ; .arch x64
        ; test Rq(value), r10 ; jnz =>scalar ; test Rq(value), Rq(value) ; jz =>top
        ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], CONST_INT32 as i8 ; jnz =>slow
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
    dynasm!(ops ; .arch x64
        ; mov r10, Rq(value) ; sar r10, tag::NUMBER_TAG.trailing_zeros() as i8
        ; cmp r10, -1 ; je =>int32);
    emit_load_u64(ops, 10, tag::NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; test Rq(value), r10 ; jnz =>double);
    emit_load_u64(ops, 10, VALUE_NULL);
    dynasm!(ops ; .arch x64 ; cmp Rq(value), r10 ; je =>null);
    emit_load_u64(ops, 10, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64
        ; cmp Rq(value), r10 ; je =>undefined ; jmp =>top
        ; =>null ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], NULL as i8 ; jnz =>done
        ; test BYTE [Rq(owner) + view.shape_watched_byte as i32], WATCHED ; jnz =>slow
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; test r11b, CONST_INT32 as i8 ; jz =>null_union
        ; mov r11, INT32 as i32
        ; =>null_union ; or r11, NULL as i32
        ; mov [Rq(owner) + view.shape_field_type_byte as i32], r11 ; jmp =>done
        ; =>undefined ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], UNDEFINED as i8 ; jnz =>done
        ; test BYTE [Rq(owner) + view.shape_watched_byte as i32], WATCHED ; jnz =>slow
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; test r11b, CONST_INT32 as i8 ; jz =>undefined_union
        ; mov r11, INT32 as i32
        ; =>undefined_union ; or r11, UNDEFINED as i32
        ; mov [Rq(owner) + view.shape_field_type_byte as i32], r11 ; jmp =>done
        ; =>int32
        ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], CONST_INT32 as i8 ; jnz =>int32_constant
        ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], INT32 as i8 ; jnz =>done
        ; test BYTE [Rq(owner) + view.shape_watched_byte as i32], WATCHED ; jnz =>slow
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; test r11, r11 ; jnz =>int32_union
        ; mov r11d, Rd(value) ; shl r11, FIRST_SHAPE_SHIFT as i8
        ; or r11, (INT32 | CONST_INT32) as i32 ; jmp =>int32_store
        ; =>int32_union ; or r11, INT32 as i32
        ; =>int32_store
        ; mov [Rq(owner) + view.shape_field_type_byte as i32], r11 ; jmp =>done
        ; =>int32_constant
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; shr r11, FIRST_SHAPE_SHIFT as i8
        ; cmp r11d, Rd(value) ; je =>done
        ; test BYTE [Rq(owner) + view.shape_watched_byte as i32], WATCHED ; jnz =>slow
        ; mov QWORD [Rq(owner) + view.shape_field_type_byte as i32], INT32 as i32 ; jmp =>done
        ; =>double
        ; test BYTE [Rq(owner) + view.shape_field_type_byte as i32], DOUBLE as i8 ; jnz =>done
        ; test BYTE [Rq(owner) + view.shape_watched_byte as i32], WATCHED ; jnz =>slow
        ; mov r11, [Rq(owner) + view.shape_field_type_byte as i32]
        ; test r11b, CONST_INT32 as i8 ; jz =>double_union
        ; mov r11, INT32 as i32
        ; =>double_union
        ; or r11, DOUBLE as i32
        ; mov [Rq(owner) + view.shape_field_type_byte as i32], r11
        ; jmp =>done
        ; =>top
        ; test BYTE [Rq(owner) + view.shape_watched_byte as i32], WATCHED ; jnz =>slow
        ; mov QWORD [Rq(owner) + view.shape_field_type_byte as i32], TOP as i32 ; jmp =>done
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
    dynasm!(ops ; .arch x64 ; add rsp, 208);
}
