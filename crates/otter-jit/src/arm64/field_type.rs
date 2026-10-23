//! Field-store observation through defining shape cells.
//!
//! # Contents
//! - Captured historical targets bypass observation for unchanged membership.
//! - Inline Top, exact Int32 and category membership with scalar widening.
//! - A cold noncollecting VM leaf that monotonically widens the owner word,
//!   and every widening of a summary code subscribed to.
//!
//! # Invariants
//! - The caller has completed every store guard and supplies the defining owner.
//! - A store whose selected record has owner zero emits no observation at all;
//!   the dynamic probe's zero owner still denotes an absorbing Top summary.
//!   The owner temporary is consumed.
//! - Captured targets are rooted ordinary shapes whose identities already
//!   belong to the monotone owner word. They need not remain stable: the probe
//!   compares the RHS's current shape and never infers a loaded value's shape.
//! - A constant payload never participates in shape membership; an unequal
//!   observation permanently replaces it with a category union.
//! - The boxed RHS and all other registers survive the observer. Only x16/x17
//!   are scratch; the cold leaf saves volatile GP/FP registers and the link.
//! - One VM thread owns widening. Scalar stores match relaxed atomic writes;
//!   observation cannot allocate, collect or reenter and names no safepoint.
//!
//! # See also
//! - `otter_vm::object::field_type` owns classification and monotone widening.
//! - [`super::property_ic`] and [`super::property_actions`] own store authority.

use dynasmrt::{DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::object::field_type::{
    CONST_INT32, DOUBLE, FIRST_SHAPE_SHIFT, INT32, NULL, SECOND_SHAPE_SHIFT, SHAPE_MASK, TOP,
    UNDEFINED,
};
use otter_vm::value::tag;
use otter_vm::{JitCompileSnapshot, native_abi as abi, object::ShapeState};

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_NULL, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
};
use crate::field_store::FieldStoreOwner;
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};

/// Observe a store through `record`'s nonzero defining owner, already
/// materialized in `X(owner)`: captured membership first, then the live word.
pub(crate) fn emit_observe_known(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
    record: &FieldStoreOwner,
) {
    debug_assert!(owner != value && !matches!(owner, 16 | 17 | 19 | 20 | 21 | 29 | 30 | 31));
    debug_assert!(!matches!(value, 16 | 17));
    debug_assert!(record.observes());
    let (stored_shapes, nullish_mask) = (record.stored_shapes.as_slice(), record.nullish_mask);
    debug_assert!(stored_shapes.len() <= 2 && stored_shapes.iter().all(|shape| *shape != 0));
    let nullish_mask = u64::from(nullish_mask);
    debug_assert!(nullish_mask & !(NULL | UNDEFINED) == 0);
    if stored_shapes.is_empty() && nullish_mask == 0 && !record.int32 && !record.double {
        emit_observe(ops, relocations, view, owner, value);
        return;
    }
    let done = ops.new_dynamic_label();
    let scalar = ops.new_dynamic_label();
    let fallback = ops.new_dynamic_label();
    if !stored_shapes.is_empty() {
        dynasm!(ops ; .arch aarch64
            ; tst X(value), #0xfffe_0000_0000_0007 ; b.ne =>scalar
            ; cbz X(value), =>fallback
            ; ldrb w16, [X(value)] ; cmp w16, OBJECT_BODY_TYPE_TAG ; b.ne =>fallback
            ; ldr w16, [X(value), view.object_shape_byte]);
        for &shape in stored_shapes {
            emit_load_u64(ops, 17, u64::from(shape));
            dynasm!(ops ; .arch aarch64 ; cmp w16, w17 ; b.eq =>done);
        }
        dynasm!(ops ; .arch aarch64 ; b =>fallback);
    }
    dynasm!(ops ; .arch aarch64 ; =>scalar);
    if record.int32 {
        dynasm!(ops ; .arch aarch64
            ; asr x16, X(value), tag::NUMBER_TAG.trailing_zeros()
            ; cmn x16, 1);
        if let Some(constant) = record.int32_constant {
            dynasm!(ops ; .arch aarch64 ; b.ne =>fallback);
            emit_load_u64(ops, 17, u64::from(constant as u32));
            dynasm!(ops ; .arch aarch64 ; cmp W(value), w17 ; b.eq =>done);
        } else {
            dynasm!(ops ; .arch aarch64 ; b.eq =>done);
        }
    }
    if record.double {
        dynasm!(ops ; .arch aarch64
            ; asr x16, X(value), #49 ; cmn x16, #1 ; b.eq =>fallback
            ; tst X(value), tag::NUMBER_TAG ; b.ne =>done);
    }
    if nullish_mask == NULL | UNDEFINED {
        dynasm!(ops ; .arch aarch64
            ; orr x16, X(value), tag::UNDEFINED_TAG
            ; cmp x16, VALUE_UNDEFINED as u32 ; b.eq =>done);
    } else if nullish_mask != 0 {
        let nullish = if nullish_mask == NULL {
            VALUE_NULL
        } else {
            VALUE_UNDEFINED
        };
        dynasm!(ops ; .arch aarch64 ; cmp XSP(value), nullish as u32 ; b.eq =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>fallback);
    emit_observe(ops, relocations, view, owner, value);
    dynasm!(ops ; .arch aarch64 ; =>done);
}

pub(crate) fn emit_observe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
) {
    // Code subscribed to the summary: every widening goes through the VM,
    // which fires it.
    const WATCHED: u32 = otter_vm::code_dependencies::ShapeFact::FieldType.bit();
    debug_assert!(owner != value && !matches!(owner, 16 | 17 | 19 | 20 | 21 | 29 | 30 | 31));
    debug_assert!(!matches!(value, 16 | 17));
    let done = ops.new_dynamic_label();
    let scalar = ops.new_dynamic_label();
    let top = ops.new_dynamic_label();
    let null = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
    let int32 = ops.new_dynamic_label();
    let int32_category = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    let double_union = ops.new_dynamic_label();
    let int32_union = ops.new_dynamic_label();
    let int32_store = ops.new_dynamic_label();
    let null_union = ops.new_dynamic_label();
    let undefined_union = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cbz W(owner), =>done);
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64
        ; add X(owner), x16, X(owner)
        ; ldr x17, [X(owner), view.shape_field_type_byte]
        ; tbnz x17, TOP.trailing_zeros(), =>done);
    dynasm!(ops ; .arch aarch64
        ; tst X(value), #0xfffe_0000_0000_0007 ; b.ne =>scalar ; cbz X(value), =>top
        ; tbnz x17, CONST_INT32.trailing_zeros(), =>slow
        ; ldrb w16, [X(value)] ; cmp w16, OBJECT_BODY_TYPE_TAG ; b.ne =>top
        ; ldr w16, [X(value), view.object_shape_byte] ; cbz w16, =>top
        ; and x17, X(value), #0xffff_ffff_0000_0000 ; add x16, x17, x16
        ; ldrb w17, [x16, view.shape_state_byte]
        ; tbnz w17, ShapeState::DICTIONARY_MASK.trailing_zeros(), =>top
        ; tst w17, u32::from(ShapeState::OPAQUE_LOOKUP_MASK) ; b.ne =>top
        ; ldr x16, [x16, view.shape_id_byte]
        ; lsr x17, x16, SHAPE_MASK.count_ones() ; cbnz x17, =>top ; cbz x16, =>top
        ; ldr x17, [X(owner), view.shape_field_type_byte]
        ; ubfx x17, x17, FIRST_SHAPE_SHIFT, SHAPE_MASK.count_ones()
        ; cmp x16, x17 ; b.eq =>done
        ; ldr x17, [X(owner), view.shape_field_type_byte]
        ; lsr x17, x17, SECOND_SHAPE_SHIFT
        ; cmp x16, x17 ; b.eq =>done ; b =>slow
        ; =>scalar);
    dynasm!(ops ; .arch aarch64
        ; asr x16, X(value), tag::NUMBER_TAG.trailing_zeros()
        ; cmn x16, 1 ; b.eq =>int32
        ; tst X(value), tag::NUMBER_TAG ; b.ne =>double);
    emit_load_u64(ops, 16, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp X(value), x16 ; b.eq =>null);
    emit_load_u64(ops, 16, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64
        ; cmp X(value), x16 ; b.eq =>undefined ; b =>top
        ; =>null ; tbnz x17, NULL.trailing_zeros(), =>done
        ; ldrb w16, [X(owner), view.shape_watched_byte] ; tbnz w16, WATCHED, =>slow
        ; tbz x17, CONST_INT32.trailing_zeros(), =>null_union
        ; mov x17, INT32
        ; =>null_union
        ; orr x17, x17, NULL
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>undefined ; tbnz x17, UNDEFINED.trailing_zeros(), =>done
        ; ldrb w16, [X(owner), view.shape_watched_byte] ; tbnz w16, WATCHED, =>slow
        ; tbz x17, CONST_INT32.trailing_zeros(), =>undefined_union
        ; mov x17, INT32
        ; =>undefined_union
        ; orr x17, x17, UNDEFINED
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>int32 ; tbz x17, CONST_INT32.trailing_zeros(), =>int32_category
        ; ubfx x16, x17, FIRST_SHAPE_SHIFT, 32
        ; cmp W(value), w16 ; b.eq =>done
        ; ldrb w16, [X(owner), view.shape_watched_byte] ; tbnz w16, WATCHED, =>slow
        ; mov x17, INT32 ; b =>int32_store
        ; =>int32_category ; tbnz x17, INT32.trailing_zeros(), =>done
        ; ldrb w16, [X(owner), view.shape_watched_byte] ; tbnz w16, WATCHED, =>slow
        ; cbnz x17, =>int32_union
        ; ubfx x17, X(value), 0, 32
        ; lsl x17, x17, FIRST_SHAPE_SHIFT
        ; orr x17, x17, INT32 | CONST_INT32 ; b =>int32_store
        ; =>int32_union
        ; orr x17, x17, INT32
        ; =>int32_store
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>double
        ; tbnz x17, DOUBLE.trailing_zeros(), =>done
        ; ldrb w16, [X(owner), view.shape_watched_byte] ; tbnz w16, WATCHED, =>slow
        ; tbz x17, CONST_INT32.trailing_zeros(), =>double_union
        ; mov x17, INT32
        ; =>double_union
        ; orr x17, x17, DOUBLE
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>top
        ; ldrb w16, [X(owner), view.shape_watched_byte] ; tbnz w16, WATCHED, =>slow);
    emit_load_u64(ops, 17, TOP);
    dynasm!(ops ; .arch aarch64
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>slow ; sub sp, sp, #352);
    for register in 0u8..19 {
        dynasm!(ops ; .arch aarch64 ; str X(register), [sp, u32::from(register) * 8]);
    }
    dynasm!(ops ; .arch aarch64 ; str x30, [sp, #152]);
    for (index, register) in (0u8..8).chain(16..32).enumerate() {
        dynasm!(ops ; .arch aarch64 ; str D(register), [sp, 160 + index as u32 * 8]);
    }
    if value < 19 {
        dynasm!(ops ; .arch aarch64 ; ldr x2, [sp, u32::from(value) * 8]);
    } else {
        dynasm!(ops ; .arch aarch64 ; mov x2, X(value));
    }
    if owner < 19 {
        dynasm!(ops ; .arch aarch64 ; ldr w1, [sp, u32::from(owner) * 8]);
    } else {
        dynasm!(ops ; .arch aarch64 ; mov w1, W(owner));
    }
    dynasm!(ops ; .arch aarch64
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]);
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::FIELD_TYPE_OBSERVE.entry_addr() as u64,
        RelocationTarget::runtime_stub(abi::STUB_FIELD_TYPE_OBSERVE),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
    for (index, register) in (0u8..8).chain(16..32).enumerate() {
        dynasm!(ops ; .arch aarch64 ; ldr D(register), [sp, 160 + index as u32 * 8]);
    }
    for register in 0u8..19 {
        dynasm!(ops ; .arch aarch64 ; ldr X(register), [sp, u32::from(register) * 8]);
    }
    dynasm!(ops ; .arch aarch64 ; ldr x30, [sp, #152] ; add sp, sp, #352 ; =>done);
}
