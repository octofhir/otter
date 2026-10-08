//! AArch64 named-property IC sites for the template compiler.
//!
//! # Contents
//! - The inline own-field path (JSC's baseline DataIC fast path): the
//!   receiver shape against the slot's own-field pair, then the field read
//!   or write. Every other receiver calls the code object's shared IC routine
//!   ([`super::shared_property`]), V8's `LoadIC` / `StoreIC` over the same
//!   slot, which ends in the committed runtime miss.
//! - Exotic length reads, static intrinsic-prototype reads on non-ordinary
//!   receivers (primitive strings, functions, collections), and the value
//!   barrier after an inline store.
//!
//! # Invariants
//! - Feedback is read from the site's native slot at run time, never baked:
//!   generated code is unchanged when the slot's entries change.
//! - The inline path neither allocates nor calls, so it carries no safepoint;
//!   the receiver pointer is recomputed from the rooted frame slot on every
//!   access and never survives one.
//! - The slot base derives from the fresh header (in-object slots) or the
//!   out-of-line slab handle — never a cached body pointer that the moving
//!   collector could dangle.
//! - Pointer-valued stores run the generational write barrier; primitive
//!   stores skip it. Every slot stores the complete runtime `Value` word.
//! - The active frame publishes and traces the complete register window, so
//!   the shared routine reloads inputs from it and passes boxed values;
//!   setters, proxies, exceptions, reentry and moving GC complete without
//!   replay.
//! - Every miss precedes effects.
//!
//! # See also
//! - [`super::values`] — slot compression/decompression primitives.
//! - `otter_vm::property_ic` — the slot layout the inline path reads.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
use otter_vm::native_abi as abi;

use super::ic_probe;
use super::values::{
    CellTest, emit_cell_test, emit_load_reg, emit_load_symbol_u64, emit_store_reg,
    emit_write_barrier,
};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{OBJECT_BODY_TYPE_TAG, Unsupported, reg_offset};

/// Route the shared routine's `NativeResultPair` status in `x1`.
fn emit_shared_status(ops: &mut Assembler, throw_value: DynamicLabel, fatal: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; cbz x1, >completed
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>throw_value
        ; b =>fatal
        ; completed:
    );
}

/// `X(slot)` = the site's IC slot address.
fn emit_slot_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    ic_slot: u64,
    slot: u8,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        slot,
        ic_slot,
        RelocationTarget::PropertyIcSlot {
            function_id: view.code_block.id,
            byte_pc,
        },
    );
}

/// Inline own-field selection: `X(receiver)` an ordinary object whose shape
/// equals the slot's own-field pair. On a match `x16` is the field's bank
/// base and `x14` its bank-relative index; otherwise branch to `miss`.
/// Clobbers `x11`..`x17` except `X(receiver)` and `X(slot)`.
fn emit_inline_own_field(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    slot: u8,
    miss: DynamicLabel,
) {
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let layout = view.field_layout;
    emit_cell_test(ops, receiver, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz X(receiver), =>miss
        ; ldrb w14, [X(receiver)]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w15, [X(receiver), view.object_shape_byte]
        ; ldp w13, w14, [X(slot), IC.inline_shape_byte as i32]
        ; cmp w15, w13
        ; b.ne =>miss
        ; tbnz w14, 31, =>inline
        ; ldr w16, [X(receiver), layout.slab_handle_byte]
        ; cbz w16, =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; add x16, x17, x16
        ; add x16, x16, #layout.slab_words_byte
        ; b =>ready
        ; =>inline
        ; and w14, w14, #0x7fff_ffff
        ; add x16, XSP(receiver), #layout.inline_values_byte
        ; =>ready
    );
}

/// Static intrinsic-prototype reads: a receiver of an intrinsic body type
/// (primitive string, closure, collection) reading a data slot of its pinned
/// realm prototype. These programs describe realm intrinsics, not site
/// feedback; a miss falls through to the site's IC.
fn emit_intrinsic_loads(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    byte_pc: u32,
    obj_off: u32,
    dst_off: u32,
    done: DynamicLabel,
) {
    use otter_vm::JitCacheIrOp as Op;
    let Some(programs) = view.property_programs.get(&byte_pc) else {
        return;
    };
    for program in programs {
        let Some((
            Op::LoadIntrinsicPrototype {
                object: 0,
                result: 1,
                target,
            },
            rest,
        )) = program.ops.split_first()
        else {
            continue;
        };
        let Some((Op::LoadField { object: 1, .. }, guards)) = rest.split_last() else {
            continue;
        };
        if !guards.iter().all(|op| {
            matches!(
                op,
                Op::GuardShape { object: 1, .. }
                    | Op::GuardDictionaryLayout { object: 1, .. }
                    | Op::GuardAtomSlot {
                        object: 1,
                        writable: false,
                        ..
                    }
                    | Op::GuardPrototypeValidity { .. }
            )
        }) {
            continue;
        }
        let next = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, obj_off]);
        ic_probe::emit_intrinsic_prototype_header(
            ops,
            relocations,
            view,
            *target,
            byte_pc,
            20,
            next,
        );
        for op in rest {
            match *op {
                Op::GuardShape { shape, .. } => {
                    ic_probe::emit_check_shape_identity(ops, view, 15, shape, next);
                }
                Op::GuardDictionaryLayout { layout, .. } => {
                    ic_probe::emit_dictionary_layout_guard(
                        ops,
                        relocations,
                        view,
                        15,
                        layout,
                        next,
                    );
                }
                Op::GuardPrototypeValidity { validity } => {
                    super::values::emit_prototype_validity_guard(
                        ops,
                        relocations,
                        validity,
                        14,
                        next,
                    );
                }
                Op::LoadField { field, .. } => {
                    ic_probe::emit_load_field(ops, relocations, view, 15, field, next);
                    dynasm!(ops ; .arch aarch64
                        ; str x9, [x19, dst_off]
                        ; b =>done);
                }
                _ => {}
            }
        }
        dynasm!(ops ; .arch aarch64 ; =>next);
    }
}

/// Emit `dst = obj.name`: inline own field, then the shared `LoadIC`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_load_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    shared_probes: &mut super::shared_property::SharedPropertyProbes,
    view: &JitCompileSnapshot,
    dst: u16,
    object: u16,
    byte_pc: u32,
    array_length: bool,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    use super::shared_property::{LOAD_RECEIVER, LOAD_SLOT};
    let ic_slot = view
        .property_accesses
        .get(&byte_pc)
        .map(|access| access.ic_slot)
        .filter(|&slot| slot != 0 && view.cage_base != 0)
        .ok_or(Unsupported::OperandShape("named load without an IC slot"))?;
    let obj_off = reg_offset(object)?;
    let dst_off = reg_offset(dst)?;
    let shared = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    if array_length {
        let have_length = ops.new_dynamic_label();
        let not_length = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x19, obj_off]);
        ic_probe::emit_exotic_length_fast(ops, view, have_length, not_length);
        dynasm!(ops
            ; .arch aarch64
            ; =>have_length
            ; str x9, [x19, dst_off]
            ; b =>done
            ; =>not_length
        );
    }

    emit_intrinsic_loads(ops, relocations, view, byte_pc, obj_off, dst_off, done);
    emit_slot_address(ops, relocations, view, byte_pc, ic_slot, LOAD_SLOT);
    dynasm!(ops ; .arch aarch64 ; ldr X(LOAD_RECEIVER), [x19, obj_off]);
    emit_inline_own_field(ops, view, LOAD_RECEIVER, LOAD_SLOT, shared);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x16, x14, lsl #3]
        ; str x9, [x19, dst_off]
        ; b =>done
        ; =>shared
    );
    // The inline miss is pre-effect; the routine re-proves the receiver from
    // the published window.
    let routine = shared_probes.label(ops, false);
    emit_load_reg(ops, LOAD_RECEIVER, object)?;
    dynasm!(ops ; .arch aarch64 ; bl =>routine);
    emit_shared_status(ops, throw_value, fatal);
    emit_store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit `obj.name = value`: inline own field, then the shared `StoreIC`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_property(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    shared_probes: &mut super::shared_property::SharedPropertyProbes,
    view: &JitCompileSnapshot,
    object: u16,
    value: u16,
    byte_pc: u32,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    use super::shared_property::{STORE_RECEIVER, STORE_SLOT, STORE_VALUE};
    let ic_slot = view
        .property_accesses
        .get(&byte_pc)
        .map(|access| access.ic_slot)
        .filter(|&slot| slot != 0 && view.cage_base != 0)
        .ok_or(Unsupported::OperandShape("named store without an IC slot"))?;
    let obj_off = reg_offset(object)?;
    let src_off = reg_offset(value)?;
    let shared = ops.new_dynamic_label();
    let primitive = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    emit_slot_address(ops, relocations, view, byte_pc, ic_slot, STORE_SLOT);
    dynasm!(ops ; .arch aarch64 ; ldr X(STORE_RECEIVER), [x19, obj_off]);
    emit_inline_own_field(ops, view, STORE_RECEIVER, STORE_SLOT, shared);
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(STORE_VALUE), [x19, src_off]
        ; str X(STORE_VALUE), [x16, x14, lsl #3]
    );
    emit_cell_test(ops, STORE_VALUE, CellTest::IsNotCell, primitive);
    emit_write_barrier(ops, relocations, view, STORE_RECEIVER, STORE_VALUE);
    dynasm!(ops
        ; .arch aarch64
        ; =>primitive
        ; b =>done
        ; =>shared
    );
    let routine = shared_probes.label(ops, true);
    emit_load_reg(ops, STORE_RECEIVER, object)?;
    emit_load_reg(ops, STORE_VALUE, value)?;
    dynasm!(ops ; .arch aarch64 ; bl =>routine);
    emit_shared_status(ops, throw_value, fatal);
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}
