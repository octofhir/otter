//! Field-store observation through defining shape cells.
//!
//! # Contents
//! - Captured historical targets bypass observation for unchanged membership.
//! - Inline Top and exact-shape membership probes with scalar widening.
//! - A cold noncollecting VM leaf that monotonically widens the owner word.
//!
//! # Invariants
//! - The caller has completed every store guard and supplies the defining owner.
//! - Zero owners denote absorbing Top summaries. The owner temporary is consumed.
//! - Captured targets are rooted ordinary shapes whose identities already
//!   belong to the monotone owner word. They need not remain stable: the probe
//!   compares the RHS's current shape and never infers a loaded value's shape.
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
    FIRST_SHAPE_SHIFT, NULL, SECOND_SHAPE_SHIFT, SHAPE_MASK, TOP, UNDEFINED,
};
use otter_vm::value::tag;
use otter_vm::{JitCompileSnapshot, native_abi as abi, object::ShapeState};

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_NULL, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
};
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};

pub(crate) fn emit_observe_known(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    owner: u8,
    value: u8,
    known: (&[u32], u8),
) {
    debug_assert!(owner != value && !matches!(owner, 16 | 17 | 19 | 20 | 21 | 29 | 30 | 31));
    debug_assert!(!matches!(value, 16 | 17));
    let (stored_shapes, nullish_mask) = known;
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
    debug_assert!(owner != value && !matches!(owner, 16 | 17 | 19 | 20 | 21 | 29 | 30 | 31));
    debug_assert!(!matches!(value, 16 | 17));
    let done = ops.new_dynamic_label();
    let scalar = ops.new_dynamic_label();
    let top = ops.new_dynamic_label();
    let null = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
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
    emit_load_u64(ops, 16, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp X(value), x16 ; b.eq =>null);
    emit_load_u64(ops, 16, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64
        ; cmp X(value), x16 ; b.eq =>undefined ; b =>top
        ; =>null ; tbnz x17, NULL.trailing_zeros(), =>done
        ; orr x17, x17, NULL
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>undefined ; tbnz x17, UNDEFINED.trailing_zeros(), =>done
        ; orr x17, x17, UNDEFINED
        ; str x17, [X(owner), view.shape_field_type_byte] ; b =>done
        ; =>top);
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
