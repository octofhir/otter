//! AArch64 lowering of one cache program, shared by every tier that probes a
//! property site.
//!
//! # Contents
//! - [`emit_load_header`] / [`emit_check_shape`] / [`emit_load_field`] — the
//!   shared object, shape, and field primitives.
//! - [`emit_property_ic_load`] / [`emit_property_ic_store_guard`] — execute an
//!   immutable CacheIR snapshot for the Template tier.
//! - Add-property programs — guard the complete no-allocation append contract
//!   and publish its child shape/length before the caller commits barriers.
//! - [`emit_exotic_length_fast`] — the `.length` reads no cache program can
//!   describe.
//! - [`emit_element_address`] / [`emit_element_read`] / [`emit_element_write`]
//!   — the indexed element access program.
//! - [`emit_native_leaf_guard`] / [`emit_native_leaf_call`] — prove a static
//!   callee's bootstrap identity, then optionally run its declared leaf entry
//!   without materializing a frame.
//! - [`emit_guarded_method_guard`] / [`emit_guarded_method_call`] — the same
//!   split for `receiver.method(args…)`, over either a template frame register
//!   or an optimizing-tier tagged machine register; the preserving guard keeps
//!   the exotic body available for immediate allocation-free completion.
//! - [`emit_native_entry_call`] — one call sequence per declared ABI family.
//!
//! # Invariants
//! - Every tier emits property probes from here. A cache program has exactly one
//!   machine lowering, so a tier cannot disagree with the interpreter about
//!   what a site caches.
//! - A CacheIR snapshot is self-contained compile metadata. Every receiver
//!   shape, atom slot, prototype link, and publication effect is explicit; an
//!   unsupported program branches to the caller's pre-effect miss as a whole.
//! - The call protocol is chosen by the family the entry id resolves in, never
//!   by which builtin a site named. A read, an in-place mutation and an
//!   allocating write reach the same sequence from one description.
//! - Split call guards are the exact shared prefix of their corresponding
//!   declared-entry calls. They load no arguments and perform no effects, so a
//!   proven operation may instead continue into equivalent machine lowering.
//!   The preserving method form keeps a raw body pointer only across the same
//!   allocation-free guard sequence and never across a transition.
//! - Way stride is [`WHISKER_IC_WAY_BYTES`], asserted against the cell's own
//!   layout where the cell is defined.
//! - Register contract on entry: `x15` holds the cell address, `w14` the
//!   receiver shape handle, `x13` the receiver's `GcHeader`. On a matched way
//!   `w17` holds the slot byte offset, `w7` the guarded holder shape, and `w6`
//!   the transition child shape while the probe runs; `w10` carries the
//!   prototype proof kind until the transition guard consumes it. A store
//!   probe returns that child in non-allocatable `w16` (`0` for an existing-slot program);
//!   after [`emit_resolve_holder`], `x13` is the holder's header and `w14` its
//!   shape.
//! - The hop reads the receiver's `[[Prototype]]` at run time. The receiver
//!   shape cannot stand in for it: the prototype lives in the object body, so
//!   `setPrototypeOf` changes it while the shape stays put.
//! - A matching shape is insufficient by itself. Receiver and data-holder
//!   guards also prove live fast mode and no overridden slot metadata.
//!   Property and native-method probes admit benign symbol/native metadata
//!   while rejecting opaque host-backed or virtual lookup. Missing-key chain
//!   links use their narrower absence proof.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitBodyGuard, JitCompileSnapshot, JitElementAccess, JitElementBase, JitElementRepr,
    JitGuardWidth, JitGuardedMethodCall, JitGuardedReceiver,
};

use otter_vm::native_abi::{NO_SAFEPOINT, RuntimeStubId, SafepointId, runtime_stub_name};
use otter_vm::runtime_stubs::{
    LeafNoAllocStub2, MutatingLeafStub2, MutatingLeafStub3, alloc_value_stub_by_id,
    leaf_no_alloc_stub2_by_id, mutating_leaf_stub2_by_id, mutating_leaf_stub3_by_id,
};

use super::values::{
    CellTest, emit_box_int32, emit_box_number_with_scratch, emit_cell_test, emit_load_reg,
    emit_load_symbol_u64, emit_load_u64,
};
use crate::artifact::relocation::{GuardedHeapComponent, RelocationCapture, RelocationTarget};
use crate::entry::{
    ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET,
    ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET, DOUBLE_OFFSET_HI16, NUMBER_TAG_HI16,
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, Unsupported, VALUE_HOLE, VALUE_UNDEFINED,
    VM_THREAD_GC_HEAP_OFFSET,
};

/// Prove the live object can still participate in an immutable hidden-class
/// proof. A separate atom-slot node guards descriptor overrides; benign
/// sidecars do not change named slots. This shape node rejects dictionary-
/// compatible history and opaque chain links.
pub(crate) fn emit_shape_state_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    miss: DynamicLabel,
) {
    let scratch = if header == 14 { 11 } else { 14 };
    let mode_byte = view.object_shape_cache_mode_byte;
    let fast_mode = u32::from(view.object_shape_cache_fast);
    let opaque_byte = view.object_chain_link_opaque_byte;
    dynasm!(ops ; .arch aarch64 ; ldrb W(scratch), [X(header), mode_byte]);
    if fast_mode == 0 {
        dynasm!(ops ; .arch aarch64 ; cbnz W(scratch), =>miss);
    } else {
        emit_load_u64(ops, 10, u64::from(fast_mode));
        dynasm!(ops ; .arch aarch64 ; cmp W(scratch), w10 ; b.ne =>miss);
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldrb W(scratch), [X(header), opaque_byte]
        ; cbnz W(scratch), =>miss
    );
}

/// Prove shape-derived named lookup remains authoritative without rejecting
/// benign symbol or native-call sidecars. Clobbers `x10` and `x14` (`x11`
/// instead of `x14` when that register holds the header).
pub(crate) fn emit_ordinary_lookup_state_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    miss: DynamicLabel,
) {
    emit_shape_state_guard(ops, view, header, miss);
    let scratch = if header == 14 { 11 } else { 14 };
    dynasm!(ops
        ; .arch aarch64
        ; ldrb W(scratch), [X(header), view.object_slot_attrs_overridden_byte]
        ; cbnz W(scratch), =>miss
    );
}

/// Prove that one prototype-chain link still supports the missing-key proof a
/// generated add-transition carries.
///
/// A link needs less than a receiver: its shape fixes which keys it owns, so
/// the guard requires only that hidden-class ICs may still trust that shape
/// (fast mode) and that the link is not opaque — a Proxy or non-object
/// `[[Prototype]]` leaves the flat mirror null without ending the chain, and a
/// String wrapper owns keys its shape does not list. Sidecars, overridden
/// attributes, and extensibility do not affect whether a key is absent, so a
/// prototype such as `Object.prototype` that carries a sidecar stays
/// guardable.
fn emit_chain_link_state_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    miss: DynamicLabel,
) {
    emit_shape_state_guard(ops, view, header, miss);
}

/// Resolve the object that owns the slot.
///
/// A way with holder shape `0` owns its slot on the receiver and this is a
/// single branch. Otherwise the receiver's `[[Prototype]]` is loaded and its
/// shape guarded before the caller touches the slab.
pub(crate) fn emit_resolve_holder(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) {
    let shape_byte = view.object_shape_byte;
    let resolved = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz w7, =>resolved
        ; ldr w12, [x13, view.jit_proto_byte]
        ; cbz w12, =>miss
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
        ; add x13, x13, x12          // x13 = prototype GcHeader ptr
        ; ldrb w14, [x13]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x13, shape_byte] // holder shape handle
        ; cmp w14, w7
        ; b.ne =>miss
    );
    emit_ordinary_lookup_state_guard(ops, view, 13, miss);
    dynasm!(ops ; .arch aarch64 ; =>resolved);
}

/// Prove the receiver is an ordinary object cell with a non-empty hidden
/// class, leaving its `GcHeader` in `x13` and that class handle in `w14`.
///
/// `load_receiver` materializes the receiver `Value` into the register it is
/// handed and is the only thing a tier supplies. Every failed guard branches to
/// `miss`.
/// Prove the receiver is a heap cell carrying an ordinary object body, leaving
/// that body's `GcHeader` address in `header`.
///
/// The address is a raw interior pointer: a moving collection invalidates it, so
/// a caller may keep it only until its next safepoint.
pub(crate) fn emit_load_header<R>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    load_receiver: R,
    header: u8,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    emit_load_object_header(ops, relocations, view, load_receiver, header, miss)?;
    emit_ordinary_lookup_state_guard(ops, view, header, miss);
    Ok(())
}

/// Prove only that a tagged value is an ordinary object and decompress its
/// header. CacheIR Machine nodes use this after an explicit metadata proof so
/// each node reads only its declared alias class.
pub(crate) fn emit_load_object_header<R>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    load_receiver: R,
    header: u8,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    load_receiver(ops, 9)?;
    super::values::emit_cell_test(ops, 9, 11, super::values::CellTest::IsNotCell, miss);
    dynasm!(ops
        ; .arch aarch64
        ; mov w12, w9              // low-32 Gc offset (zero-ext)
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
        ; add X(header), x13, x12  // header = GcHeader ptr
        ; ldrb w14, [X(header)]    // header type tag
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
    );
    Ok(())
}

/// Prove the holder `header` names still carries the compile-time hidden class
/// `shape`.
///
/// A snapshot shape is an immediate, so the guard needs no mutable cell lookup.
/// Clobbers `w12` and `w14`.
pub(crate) fn emit_check_shape(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    shape: u32,
    miss: DynamicLabel,
) {
    emit_ordinary_lookup_state_guard(ops, view, header, miss);
    emit_check_shape_identity(ops, view, header, shape, miss);
}

/// Compare only the immutable hidden-class token. The caller owns any separate
/// object-local descriptor/exotic proof.
pub(crate) fn emit_check_shape_identity(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    shape: u32,
    miss: DynamicLabel,
) {
    let shape_byte = view.object_shape_byte;
    dynasm!(ops
        ; .arch aarch64
        ; ldr w14, [X(header), shape_byte]
        ; cbz w14, =>miss          // empty-cell sentinel
    );
    emit_load_u64(ops, 12, u64::from(shape));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w14, w12
        ; b.ne =>miss
    );
}

/// Read the own data slot at `value_byte` from the holder `header` names,
/// leaving the boxed `Value` in `x9`.
///
/// The slab base is computed in `x13` — `dynasm` cannot add a constant offset to
/// a dynamic base register — so a holder held anywhere else is moved there
/// first.
pub(crate) fn emit_load_field(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    value_byte: u32,
    miss: DynamicLabel,
) {
    if header != 13 {
        dynasm!(ops ; .arch aarch64 ; mov x13, X(header));
    }
    super::values::emit_slab_base(ops, view, 13, 14);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x13, =>miss
        ; ldr x9, [x13, value_byte]
    );
}

/// Prove the tagged receiver in `x9` and load its pinned realm prototype into
/// `x15`. The following CacheIR nodes prove live shape/descriptor/slot state.
/// Clobbers `x11..x15`; neither a miss nor a hit enters the runtime.
pub(crate) fn emit_intrinsic_prototype_header(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    target: otter_vm::jit::JitIntrinsicPrototype,
    byte_pc: u32,
    miss: DynamicLabel,
) {
    if !target.is_property_receiver() {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return;
    }
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, miss);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x9, =>miss
        ; mov x13, x9
        ; ldrb w14, [x13]
        ; cmp w14, u32::from(target.type_tag)
        ; b.ne =>miss
    );
    if let Some(guard) = target.guard {
        emit_body_guard(ops, guard, miss);
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        15,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        12,
        u64::from(target.proto_offset),
        RelocationTarget::GuardedHeapReference {
            component: GuardedHeapComponent::Prototype,
            byte_pc,
            runtime_stub_id: otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY.id,
        },
    );
    dynasm!(ops ; .arch aarch64 ; add x15, x15, x12);
}

/// Probe a named-property load site and leave the loaded `Value` in `x9`.
///
/// The Template tier consumes the same immutable CacheIR DTO that Machine
/// transpiles to SSA. Every failed guard branches to `miss`, where the fixed
/// committed boundary owns full `[[Get]]` semantics exactly once.
///
/// `load_receiver` materializes the receiver `Value` into the register it is
/// handed and is the only thing a tier supplies. On return the complete value
/// is in `x9`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_property_ic_load<R>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    byte_pc: u32,
    mut load_receiver: R,
    _cell_addr: usize,
    _cell_ordinal: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnMut(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    let Some(programs) = programs.filter(|programs| !programs.is_empty()) else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return Ok(());
    };
    let intrinsic = programs.iter().any(|program| {
        matches!(
            program.ops.first(),
            Some(otter_vm::JitCacheIrOp::LoadIntrinsicPrototype { .. })
        )
    });
    if !intrinsic {
        emit_load_header(ops, relocations, view, &mut load_receiver, 13, miss)?;
    }
    let done = ops.new_dynamic_label();
    for program in programs {
        let next = ops.new_dynamic_label();
        if matches!(
            program.ops.first(),
            Some(otter_vm::JitCacheIrOp::LoadIntrinsicPrototype { .. })
        ) {
            load_receiver(ops, 9)?;
        } else if intrinsic {
            emit_load_header(ops, relocations, view, &mut load_receiver, 13, next)?;
        }
        let mut terminal = false;
        for op in program.ops.iter() {
            match *op {
                otter_vm::JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target,
                } => {
                    emit_intrinsic_prototype_header(ops, relocations, view, target, byte_pc, next);
                }
                otter_vm::JitCacheIrOp::LoadIntrinsicPrototype { .. } => {
                    return Err(Unsupported::OperandShape(
                        "CacheIR intrinsic prototype operands",
                    ));
                }
                otter_vm::JitCacheIrOp::GuardShape { object, shape } => {
                    let header = match object {
                        0 => 13,
                        1 => 15,
                        _ => return Err(Unsupported::OperandShape("CacheIR object operand")),
                    };
                    emit_check_shape(ops, view, header, shape, next);
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot {
                    object,
                    writable: false,
                    ..
                } => {
                    // The immediately preceding shape guard already checks the
                    // object-local descriptor/exotic override bits. The atom
                    // and slot mapping itself is immutable in that shape.
                    let _ = match object {
                        0 => 13,
                        1 => 15,
                        _ => return Err(Unsupported::OperandShape("CacheIR atom-slot object")),
                    };
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot { .. } => {
                    return Err(Unsupported::OperandShape(
                        "writable atom-slot guard in load CacheIR",
                    ));
                }
                otter_vm::JitCacheIrOp::LoadPrototype {
                    object: 0,
                    result: 1,
                } => {
                    dynasm!(ops ; .arch aarch64 ; ldr w12, [x13, view.jit_proto_byte] ; cbz w12, =>next);
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        15,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops ; .arch aarch64 ; add x15, x15, x12 ; ldrb w14, [x15] ; cmp w14, OBJECT_BODY_TYPE_TAG ; b.ne =>next);
                    emit_ordinary_lookup_state_guard(ops, view, 15, next);
                }
                otter_vm::JitCacheIrOp::LoadPrototype { .. } => {
                    return Err(Unsupported::OperandShape("CacheIR prototype operands"));
                }
                otter_vm::JitCacheIrOp::LoadField { object, value_byte } => {
                    let header = match object {
                        0 => 13,
                        1 => 15,
                        _ => return Err(Unsupported::OperandShape("CacheIR field object")),
                    };
                    emit_load_field(ops, view, header, value_byte, next);
                    dynasm!(ops ; .arch aarch64 ; b =>done);
                    terminal = true;
                }
                otter_vm::JitCacheIrOp::StoreField { .. } => {
                    return Err(Unsupported::OperandShape("store op in load CacheIR"));
                }
                otter_vm::JitCacheIrOp::GuardPrototypeNull { .. }
                | otter_vm::JitCacheIrOp::GuardExtensible { .. }
                | otter_vm::JitCacheIrOp::PublishShape { .. } => {
                    return Err(Unsupported::OperandShape("store-only op in load CacheIR"));
                }
            }
        }
        if !terminal {
            return Err(Unsupported::OperandShape("unterminated load CacheIR"));
        }
        dynasm!(ops ; .arch aarch64 ; =>next);
    }
    dynasm!(ops ; .arch aarch64 ; b =>miss ; =>done);
    Ok(())
}

/// Prove a named-property store site's receiver and resolve its slot, leaving
/// the receiver in `x12`, its value slab in `x13`, the slot byte in `x17`, and
/// the transition child shape in non-allocatable `w16` (`0` for an
/// existing-slot program).
///
/// Existing-slot programs name one guarded own field. Add-transition programs
/// additionally prove their complete prototype contract, extensibility, exact
/// append length, and existing storage capacity before publishing the child
/// shape, new length, and (for slot zero) inline values pointer.
///
/// The caller owns everything after the slot is resolved: loading and storing
/// the complete value word, then running the child-shape barrier when `w16` is
/// nonzero and the value barrier when needed. No branch to `miss` is legal
/// after the helper publishes a transition.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_property_ic_store_guard<R>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    programs: Option<&[otter_vm::JitCacheIrProgram]>,
    load_receiver: R,
    _cell_addr: usize,
    _cell_ordinal: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    let Some(programs) = programs.filter(|programs| !programs.is_empty()) else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return Ok(());
    };
    emit_load_header(ops, relocations, view, load_receiver, 13, miss)?;
    let matched = ops.new_dynamic_label();
    let existing = ops.new_dynamic_label();
    for program in programs {
        let next = ops.new_dynamic_label();
        let mut terminal = false;
        let add_transition = program.ops.iter().any(|op| {
            matches!(
                op,
                otter_vm::JitCacheIrOp::GuardExtensible { .. }
                    | otter_vm::JitCacheIrOp::PublishShape { .. }
            )
        });
        for (index, op) in program.ops.iter().enumerate() {
            match *op {
                otter_vm::JitCacheIrOp::GuardShape { object, shape } => {
                    let header = match object {
                        0 => 13,
                        1 => 15,
                        _ => return Err(Unsupported::OperandShape("store CacheIR shape object")),
                    };
                    if add_transition && object == 1 {
                        emit_chain_link_state_guard(ops, view, header, next);
                        emit_check_shape_identity(ops, view, header, shape, next);
                    } else {
                        emit_check_shape(ops, view, header, shape, next);
                    }
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot {
                    object,
                    writable: true,
                    ..
                } => {
                    // See the load-side note: GuardShape proves the immutable
                    // atom/slot mapping and the live override state together.
                    if object > 1 {
                        return Err(Unsupported::OperandShape("store CacheIR atom-slot object"));
                    }
                }
                otter_vm::JitCacheIrOp::LoadPrototype { object, result: 1 } => {
                    let header = match object {
                        0 => 13,
                        1 => 15,
                        _ => {
                            return Err(Unsupported::OperandShape(
                                "store CacheIR prototype object",
                            ));
                        }
                    };
                    dynasm!(ops ; .arch aarch64 ; ldr w12, [X(header), view.jit_proto_byte] ; cbz w12, =>next);
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        15,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops ; .arch aarch64 ; add x15, x15, x12 ; ldrb w14, [x15] ; cmp w14, OBJECT_BODY_TYPE_TAG ; b.ne =>next);
                    if add_transition {
                        emit_chain_link_state_guard(ops, view, 15, next);
                    } else {
                        emit_ordinary_lookup_state_guard(ops, view, 15, next);
                    }
                }
                otter_vm::JitCacheIrOp::GuardPrototypeNull { object } => {
                    let header = match object {
                        0 => 13,
                        1 => 15,
                        _ => {
                            return Err(Unsupported::OperandShape(
                                "store CacheIR null-prototype object",
                            ));
                        }
                    };
                    dynasm!(ops ; .arch aarch64 ; ldr w12, [X(header), view.jit_proto_byte] ; cbnz w12, =>next);
                }
                otter_vm::JitCacheIrOp::GuardExtensible {
                    object: 0,
                    value_byte,
                } => {
                    let inline_storage = ops.new_dynamic_label();
                    let storage_fits = ops.new_dynamic_label();
                    emit_load_u64(ops, 17, u64::from(value_byte));
                    dynasm!(ops
                        ; .arch aarch64
                        ; tst w17, #7
                        ; b.ne =>next
                        ; ldr w16, [x13, view.object_slab_handle_byte]
                        ; cbz w16, =>inline_storage
                    );
                    emit_load_symbol_u64(
                        ops,
                        relocations,
                        15,
                        view.cage_base as u64,
                        RelocationTarget::GcCageBase,
                    );
                    dynasm!(ops
                        ; .arch aarch64
                        ; add x15, x15, x16
                        ; ldr w14, [x15, view.object_slab_capacity_byte]
                        ; lsr w16, w17, #3
                        ; cmp w16, w14
                        ; b.hs =>next
                        ; b =>storage_fits
                        ; =>inline_storage
                        ; lsr w16, w17, #3
                        ; cmp w16, view.object_inline_slot_cap
                        ; b.hs =>next
                        ; =>storage_fits
                        ; ldrb w16, [x13, view.object_extensible_byte]
                        ; cbz w16, =>next
                        ; ldrh w16, [x13, view.object_slab_len_byte]
                        ; lsr w15, w17, #3
                        ; cmp w16, w15
                        ; b.ne =>next
                    );
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot { .. }
                | otter_vm::JitCacheIrOp::LoadPrototype { .. }
                | otter_vm::JitCacheIrOp::LoadIntrinsicPrototype { .. }
                | otter_vm::JitCacheIrOp::GuardExtensible { .. } => {
                    return Err(Unsupported::OperandShape("store CacheIR guard operands"));
                }
                otter_vm::JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte,
                } => {
                    emit_load_u64(ops, 17, u64::from(value_byte));
                    terminal = true;
                    if !matches!(
                        program.ops.get(index + 1),
                        Some(otter_vm::JitCacheIrOp::PublishShape { .. })
                    ) {
                        dynasm!(ops ; .arch aarch64 ; b =>existing);
                    }
                }
                otter_vm::JitCacheIrOp::PublishShape {
                    object: 0,
                    shape,
                    new_len,
                    initialize_inline,
                } if terminal => {
                    if initialize_inline {
                        super::values::emit_initialize_inline_values_ptr(ops, view, 13, 16);
                    }
                    emit_load_u64(ops, 14, u64::from(new_len));
                    emit_load_u64(ops, 16, u64::from(shape));
                    dynasm!(ops
                        ; .arch aarch64
                        ; strh w14, [x13, view.object_slab_len_byte]
                        ; str w16, [x13, view.object_shape_byte]
                        ; b =>matched
                    );
                }
                otter_vm::JitCacheIrOp::StoreField { .. }
                | otter_vm::JitCacheIrOp::LoadField { .. }
                | otter_vm::JitCacheIrOp::PublishShape { .. } => {
                    return Err(Unsupported::OperandShape("store CacheIR terminal"));
                }
            }
        }
        if !terminal {
            return Err(Unsupported::OperandShape("unterminated store CacheIR"));
        }
        dynasm!(ops ; .arch aarch64 ; =>next);
    }
    dynasm!(ops ; .arch aarch64 ; b =>miss ; =>existing ; mov w16, wzr ; =>matched ; mov x12, x13);
    super::values::emit_slab_base(ops, view, 13, 14);
    dynasm!(ops ; .arch aarch64 ; cbz x13, =>miss);
    Ok(())
}

/// Run the child-shape edge barrier after an add-transition value commit.
///
/// `w16` is the child shape (`0` for an existing-slot store), `x12` the
/// receiver header, and `x9` the already-stored value. The slow marking path is
/// a native call, so preserve the parent/value pair needed by the following
/// value barrier. There is deliberately no miss edge: structural publication
/// and the value store have already committed.
pub(crate) fn emit_property_transition_shape_barrier(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    context: u8,
) {
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz w16, =>done
        ; stp x12, x9, [sp, #-16]!
    );
    super::values::emit_write_barrier_with_context(ops, relocations, view, 12, 16, context);
    dynasm!(ops
        ; .arch aarch64
        ; ldp x12, x9, [sp], #16
        ; =>done
    );
}

/// Serve `receiver.length` for the two receivers whose length is not an own
/// data slot: a dense array's exotic `length`, and a primitive string's.
///
/// Neither can be expressed as a cache program — an array's length lives in
/// its body rather than the value slab, and a string is not an object at all,
/// so no stub is ever installed for it. Without this arm every `s.length` in a
/// loop leaves generated code for the runtime.
///
/// On entry `x9` holds the receiver `Value`. On a match the boxed length is in
/// `x9` and control branches to `have_length`; otherwise control branches to
/// `not_length` and the caller emits its ordinary property probe.
pub(crate) fn emit_exotic_length_fast(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    have_length: DynamicLabel,
    not_length: DynamicLabel,
) {
    let array_tag = u32::from(view.array_layout.type_tag);
    let array_length_byte = view.array_layout.length_byte;
    let string_tag = u32::from(view.string_layout.string_type_tag);
    let string_len_byte = view.string_layout.string_len_byte;
    let try_string = ops.new_dynamic_label();

    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, not_length);
    dynasm!(ops ; .arch aarch64 ; mov w12, w9); // low-32 Gc offset
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
        ; cmp w14, array_tag
        ; b.ne =>try_string
        ; ldr x9, [x13, array_length_byte]
    );
    // An array length beyond i32 cannot box as a small integer here.
    emit_load_u64(ops, 12, i32::MAX as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x12
        ; b.hi =>not_length
    );
    emit_box_int32(ops, 9, 12);
    dynasm!(ops
        ; .arch aarch64
        ; b =>have_length
        ; =>try_string
        ; cmp w14, string_tag
        ; b.ne =>not_length
        ; ldr w9, [x13, string_len_byte]
    );
    // Ropes carry a u32 length and can exceed the tagged int32 range without
    // allocating a contiguous multi-gigabyte buffer. Let the canonical
    // property operation box those lengths as Number instead of wrapping.
    emit_load_u64(ops, 12, i32::MAX as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x12
        ; b.hi =>not_length
    );
    emit_box_int32(ops, 9, 12);
    dynasm!(ops ; .arch aarch64 ; b =>have_length);
}

/// Whether the index operand a site supplies still carries a `Value` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DenseIndexForm {
    /// A boxed `Value` whose int32 payload the guard must still prove.
    Tagged,
}

/// Whether a family is baked for the indexed-element program.
///
/// The declaration is the whole gate: without a cage base the guard cannot
/// reach a body at all, and a zero cell tag means no element-bearing family was
/// described, so the site keeps the runtime path.
pub(crate) fn element_access_for(
    view: &JitCompileSnapshot,
    byte_pc: u32,
) -> Option<&JitElementAccess> {
    (view.cage_base != 0)
        .then(|| view.element_accesses.get(&byte_pc))
        .flatten()
        .filter(|access| access.type_tag != 0)
}

/// Prove the receiver and immutable body guards for one in-body dense view.
///
/// On success `x16` contains the current element base and `x14` contains the
/// zero-extended live length. The returned pair may be cached only while the
/// caller proves that no allocation, reentry, or representation-changing
/// effect can occur. Nothing here publishes a GC root: `x16` is a raw host
/// address and must not survive such a boundary.
///
/// Clobbers `x9`, `x11`-`x16`.
pub(crate) fn emit_dense_element_view<R>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    access: &JitElementAccess,
    load_receiver: R,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    let JitElementBase::InBody { byte: base_byte } = access.base else {
        return Err(Unsupported::OperandShape("dense element view base"));
    };
    load_receiver(ops, 9)?;
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64 ; mov w12, w9); // low-32 Gc offset
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
        ; cmp w14, access.type_tag as u32
        ; b.ne =>miss
    );
    for guard in access.guards.iter().flatten() {
        emit_body_guard(ops, *guard, miss);
    }
    match access.length_width {
        JitGuardWidth::Byte => dynasm!(ops
            ; .arch aarch64
            ; ldrb w14, [x13, access.length_byte]
        ),
        JitGuardWidth::Word32 => dynasm!(ops
            ; .arch aarch64
            ; ldr w14, [x13, access.length_byte]
        ),
        JitGuardWidth::Word64 => dynasm!(ops
            ; .arch aarch64
            ; ldr x14, [x13, access.length_byte]
        ),
    }
    dynasm!(ops ; .arch aarch64 ; ldr x16, [x13, base_byte]);
    Ok(())
}

/// Prove an indexed receiver and materialize its current raw element view.
///
/// On success `x16` is the element base and `x14` is the zero-extended live
/// element count. The selected index is deliberately not inspected here:
/// Machine lowering represents bounds as a separate dependent operation.
///
/// Clobbers `x9`, `x11`-`x16`.
pub(crate) fn emit_element_view<R>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    access: &JitElementAccess,
    load_receiver: R,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    if matches!(access.base, JitElementBase::InBody { .. }) {
        return emit_dense_element_view(ops, relocations, view, access, load_receiver, miss);
    }
    load_receiver(ops, 9)?;
    emit_cell_test(ops, 9, 11, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64 ; mov w12, w9);
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, access.type_tag as u32
        ; b.ne =>miss
    );
    for guard in access.guards.iter().flatten() {
        emit_body_guard(ops, *guard, miss);
    }
    match access.length_width {
        JitGuardWidth::Byte => dynasm!(ops ; .arch aarch64 ; ldrb w14, [x13, access.length_byte]),
        JitGuardWidth::Word32 => dynasm!(ops ; .arch aarch64 ; ldr w14, [x13, access.length_byte]),
        JitGuardWidth::Word64 => dynasm!(ops ; .arch aarch64 ; ldr x14, [x13, access.length_byte]),
    }
    let shift = access.element.stride_shift();
    match access.base {
        JitElementBase::None | JitElementBase::InBody { .. } => {
            return Err(Unsupported::OperandShape("element view base"));
        }
        JitElementBase::ThroughLocalBuffer {
            storage_tag_byte,
            local_tag,
            handle_byte,
            detached_byte,
            data_ptr_byte,
            byte_len_byte,
            view_offset_byte,
        } => {
            dynasm!(ops ; .arch aarch64 ; ldr w15, [x13, storage_tag_byte]);
            emit_load_u64(ops, 12, u64::from(local_tag));
            dynasm!(ops
                ; .arch aarch64
                ; cmp w15, w12
                ; b.ne =>miss
                ; ldr w12, [x13, handle_byte]
                ; cbz w12, =>miss
            );
            emit_load_symbol_u64(
                ops,
                relocations,
                11,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops
                ; .arch aarch64
                ; add x11, x11, x12
                ; ldrb w15, [x11, detached_byte]
                ; cbnz w15, =>miss
                ; ldr x15, [x13, view_offset_byte]
            );
            match shift {
                2 => dynasm!(ops
                    ; .arch aarch64
                    ; lsr x12, x14, #62
                    ; cbnz x12, =>miss
                    ; lsl x12, x14, #2
                ),
                _ => dynasm!(ops
                    ; .arch aarch64
                    ; lsr x12, x14, #61
                    ; cbnz x12, =>miss
                    ; lsl x12, x14, #3
                ),
            }
            dynasm!(ops
                ; .arch aarch64
                ; adds x12, x15, x12
                ; b.cs =>miss
                ; ldr x15, [x11, byte_len_byte]
                ; cmp x12, x15
                ; b.hi =>miss
                ; ldr x16, [x11, data_ptr_byte]
                ; cbz x16, =>miss
                ; ldr x15, [x13, view_offset_byte]
                ; add x16, x16, x15
            );
        }
    }
    Ok(())
}

/// Prove one index against an already validated in-body dense view.
///
/// The caller supplies the raw base in `x16` and normalized length in `x14`.
/// On success `x16` is advanced to the addressed element. The tagged form
/// first proves the exact int32 number tag; the unsigned bounds comparison then
/// rejects negative indices without a separate branch.
///
/// Clobbers `x11`, `x12`, `x15`, and `x16`.
pub(crate) fn emit_element_address_from_dense_view<I>(
    ops: &mut Assembler,
    access: &JitElementAccess,
    load_index: I,
    index_form: DenseIndexForm,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    I: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    load_index(ops, 15)?;
    if index_form == DenseIndexForm::Tagged {
        dynasm!(ops
            ; .arch aarch64
            ; lsr x11, x15, #48
            ; movz x12, NUMBER_TAG_HI16
            ; cmp x11, x12
            ; b.ne =>miss
        );
    }
    match access.length_width {
        JitGuardWidth::Byte | JitGuardWidth::Word32 => dynasm!(ops
            ; .arch aarch64
            ; cmp w15, w14
            ; b.hs =>miss
        ),
        JitGuardWidth::Word64 => dynasm!(ops
            ; .arch aarch64
            ; mov w15, w15
            ; cmp x15, x14
            ; b.hs =>miss
        ),
    }
    match access.element.stride_shift() {
        2 => dynasm!(ops ; .arch aarch64 ; add x16, x16, w15, uxtw #2),
        _ => dynasm!(ops ; .arch aarch64 ; add x16, x16, w15, uxtw #3),
    }
    Ok(())
}

/// Prove an in-bounds indexed element access, leaving the element's address in
/// `x16`.
///
/// The guard is one program over the declared family: the receiver is a heap
/// cell carrying that cell tag, its latch reads clean, the index is a
/// non-negative int32 below the body's live element count, a fixed typed view's
/// complete construction-time extent still fits its live backing buffer, and
/// the address comes from the body's element base pointer so the backing
/// container's layout stays unobserved. Nothing here allocates, so no
/// safepoint is owed.
///
/// `load_receiver` and `load_index` materialize their operand into the register
/// they are handed and run inside the guard sequence, so they must touch no
/// other register. That is the only thing a tier supplies: the guard itself is
/// written once, and the family is [`JitElementAccess`] data. Clobbers `x9`,
/// `x11`–`x16`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_element_address<R, I>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    access: &JitElementAccess,
    load_receiver: R,
    load_index: I,
    index_form: DenseIndexForm,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    R: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
    I: FnOnce(&mut Assembler, u8) -> Result<(), Unsupported>,
{
    emit_element_view(ops, relocations, view, access, load_receiver, miss)?;
    emit_element_address_from_dense_view(ops, access, load_index, index_form, miss)
}

/// Read the element whose address [`emit_element_address`] left in `x16` into
/// `x9` as a boxed `Value`, branching to `miss` when the declared
/// representation says the slot holds no value.
///
/// A boxed hole is an absent property — the prototype chain answers a read, and
/// a prototype setter may observe a write — so it is a guard failure. A scalar
/// element has no such state and always produces a value. Clobbers `x10`–`x12`,
/// `x14`, `x15`, `d30`, and `d31`.
pub(crate) fn emit_element_read(ops: &mut Assembler, element: JitElementRepr, miss: DynamicLabel) {
    match element {
        JitElementRepr::Boxed => {
            emit_load_u64(ops, 11, VALUE_HOLE);
            dynasm!(ops
                ; .arch aarch64
                ; ldr x9, [x16]
                ; cmp x9, x11
                ; b.eq =>miss
            );
        }
        JitElementRepr::Int32 => {
            // `ldr w` zero-extends, which is what the int32 box wants: the
            // payload is the low 32 bits and the tag occupies the top.
            dynasm!(ops ; .arch aarch64 ; ldr w9, [x16]);
            emit_box_int32(ops, 9, 11);
        }
        JitElementRepr::Float64 => {
            // Match `NumberValue::from_f64`: exact int32 values use the Smi
            // representation, while -0, fractions, infinities and values
            // outside int32 remain doubles. This is required before a later
            // representation guard observes the loaded value.
            dynasm!(ops ; .arch aarch64 ; ldr d31, [x16]);
            emit_box_number_with_scratch(ops, 31, 9, 30);
        }
    }
}

/// Overwrite the element whose address [`emit_element_address`] left in `x16`
/// with the boxed `Value` in `x9`, unboxing it into the declared
/// representation.
///
/// The value is *guarded*, never coerced: `ToNumber` on a non-numeric value can
/// call user code, which cannot run inside a guard sequence, so a value that is
/// not already in the view's representation leaves the fast path and the
/// runtime stub performs the whole observable store. A boxed element takes any
/// non-cell value, because a cell would owe the generational write barrier that
/// only the stub runs. Clobbers `x11`, `x12`, `x14`.
pub(crate) fn emit_element_write(ops: &mut Assembler, element: JitElementRepr, miss: DynamicLabel) {
    emit_element_write_guard(ops, element, miss);
    emit_element_write_proven(ops, element);
}

/// Prove that boxed value `x9` is directly storable in `element`.
///
/// This helper has no heap effect. It is the reusable guard half used by
/// decomposed Machine stores before their no-fail write node.
pub(crate) fn emit_element_write_guard(
    ops: &mut Assembler,
    element: JitElementRepr,
    miss: DynamicLabel,
) {
    match element {
        JitElementRepr::Boxed => {
            // A heap cell would owe the generational barrier only the stub runs.
            emit_cell_test(ops, 9, 11, CellTest::IsCell, miss);
        }
        JitElementRepr::Int32 => {
            // Only a value already boxed as an int32 stores exactly. A double
            // would owe `ToInt32`, whose modular truncation is not `fcvtzs`.
            dynasm!(ops
                ; .arch aarch64
                ; lsr x11, x9, #48
                ; movz x12, NUMBER_TAG_HI16
                ; cmp x11, x12
                ; b.ne =>miss
            );
        }
        JitElementRepr::Float64 => {
            // A number that is not an int32 is a boxed double, so undoing the
            // encode offset yields the raw bit pattern. An int32 would owe an
            // integer-to-double conversion, which needs an FP register neither
            // tier reserves here.
            dynasm!(ops
                ; .arch aarch64
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; tst x9, x11
                ; b.eq =>miss              // not a number at all
                ; lsr x12, x9, #48
                ; movz x14, NUMBER_TAG_HI16
                ; cmp x12, x14
                ; b.eq =>miss              // int32-boxed, not a double
            );
        }
    }
}

/// Store boxed value `x9` after [`emit_element_write_guard`] succeeded.
///
/// The operation contains no condition or exit. `x16` is the proved address.
pub(crate) fn emit_element_write_proven(ops: &mut Assembler, element: JitElementRepr) {
    match element {
        JitElementRepr::Boxed => dynasm!(ops ; .arch aarch64 ; str x9, [x16]),
        JitElementRepr::Int32 => dynasm!(ops ; .arch aarch64 ; str w9, [x16]),
        JitElementRepr::Float64 => dynasm!(ops
            ; .arch aarch64
            ; movz x11, DOUBLE_OFFSET_HI16, lsl #48
            ; sub x9, x9, x11
            ; str x9, [x16]
        ),
    }
}

/// Whether a declared leaf entry can be called inline at a site of this arity.
///
/// Support follows the declaration: an entry is callable exactly when it is a
/// guarded callable builtin and the site passes the argument count that entry
/// implements. No builtin is named here, so a new one costs a declaration and
/// no generated code.
pub(crate) fn native_leaf_call_is_supported(
    view: &JitCompileSnapshot,
    stub_id: RuntimeStubId,
    argc: usize,
) -> bool {
    let Some(declaration) = otter_vm::jit_static_native::jit_leaf_builtin(stub_id) else {
        return false;
    };
    view.native_ref_byte != 0
        && argc == usize::from(declaration.argument_count)
        && leaf_no_alloc_stub2_by_id(stub_id).is_some()
}

/// Whether a declared entry has machine-callable code for the protocol its
/// family implies, with the safepoint the site would publish.
fn native_entry_call_is_supported(stub_id: RuntimeStubId, safepoint_id: SafepointId) -> bool {
    if safepoint_id == NO_SAFEPOINT {
        return leaf_no_alloc_stub2_by_id(stub_id).is_some_and(LeafNoAllocStub2::is_valid)
            || mutating_leaf_stub2_by_id(stub_id).is_some_and(MutatingLeafStub2::is_valid)
            || mutating_leaf_stub3_by_id(stub_id).is_some_and(MutatingLeafStub3::is_valid);
    }
    alloc_value_stub_by_id(stub_id)
        .is_some_and(|stub| stub.is_valid_for_safepoint(safepoint_id) && stub.has_entry())
}

/// Whether a guarded method call can be lowered at all.
///
/// The layout words the guards read come from the compile snapshot; without
/// them the site keeps the ordinary path instead of failing the whole compile.
pub(crate) fn guarded_method_call_is_supported(
    view: &JitCompileSnapshot,
    call: &JitGuardedMethodCall,
) -> bool {
    view.cage_base != 0
        && view.native_ref_byte != 0
        && native_entry_call_is_supported(call.entry_stub_id, call.safepoint_id)
}

/// Guard a callee's exact bootstrap identity, then run its declared leaf entry.
///
/// Every guard runs *before* any argument is materialized, and arguments are
/// loaded straight into the ABI registers the entry reads. An argument
/// therefore never occupies a register the guard sequence owns, which is the
/// whole clobber class that parking inputs ahead of the guards makes
/// expressible.
///
/// `callee_x` holds the callee `Value` and must not name guard scratch.
/// `load_argument(ops, index, register)` materializes one argument and runs
/// only once every guard has passed, so it may freely use `x10`–`x15`. The
/// boxed result is left in `x0`; every miss branches to `bail`.
/// `context_x` names the tier-owned native context register.
pub(crate) fn emit_native_leaf_call<F>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    stub_id: RuntimeStubId,
    builtin_native_ref: u32,
    callee_x: u8,
    context_x: u8,
    load_argument: F,
    bail: DynamicLabel,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    emit_native_leaf_guard(ops, view, builtin_native_ref, callee_x, bail)?;

    let Some(declaration) = otter_vm::jit_static_native::jit_leaf_builtin(stub_id) else {
        return Err(Unsupported::OperandShape("native leaf entry"));
    };
    emit_native_entry_call(
        ops,
        relocations,
        stub_id,
        NO_SAFEPOINT,
        declaration.argument_count,
        context_x,
        load_argument,
        bail,
    )
}

/// Guard one static native callee without choosing how its declared operation
/// is lowered afterwards.
///
/// The optimizing tier uses the same bootstrap identity proof before replacing
/// selected numeric leaf entries with equivalent machine instructions. Other
/// callers continue from this guard into [`emit_native_entry_call`].
pub(crate) fn emit_native_leaf_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    builtin_native_ref: u32,
    callee_x: u8,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    debug_assert!(
        !(12..=16).contains(&callee_x),
        "the callee must survive the guard, which owns x12..x16"
    );

    let native_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    emit_cell_test(ops, callee_x, 12, CellTest::IsNotCell, bail);
    dynasm!(ops
        ; .arch aarch64
        ; cbz X(callee_x), =>bail
        ; ldrb w14, [X(callee_x)]
        ; cmp w14, native_type_tag
        ; b.ne =>bail
        ; ldr w14, [X(callee_x), view.native_ref_byte]
    );
    emit_native_ref_compare(ops, builtin_native_ref);
    dynasm!(ops
        ; .arch aarch64
        ; b.ne =>bail
    );
    Ok(())
}

/// Call a declared entry whose identity a caller has already guarded.
///
/// The family the id resolves in picks the protocol and nothing else does. A
/// leaf or mutating-leaf entry runs `(heap, value0, value1) -> pair` and
/// publishes no safepoint, because it can neither allocate, collect, nor
/// re-enter JS. An allocating entry builds its allocation context on the stack
/// and runs `(ctx, safepoint, value0, value1, value2) -> pair`.
///
/// This runs only once every guard has passed, so nothing it writes is live
/// across a miss and `load_value` may freely use `x10`–`x15`. `value_count` is
/// how many of the family's operand words the site fills; the rest are
/// `undefined`. The boxed result is left in `x0`; a miss branches to `bail`.
/// The caller supplies its native context register as `context_x`.
pub(crate) fn emit_native_entry_call<F>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    stub_id: RuntimeStubId,
    safepoint_id: SafepointId,
    value_count: u8,
    context_x: u8,
    mut load_value: F,
    bail: DynamicLabel,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    if safepoint_id == NO_SAFEPOINT {
        // Both leaf families take the heap and their operand words directly;
        // they differ only in how many words they read, so the width is the
        // whole distinction.
        let pair = leaf_no_alloc_stub2_by_id(stub_id)
            .filter(|stub| stub.is_valid())
            .map(|stub| (stub.entry_addr(), stub.descriptor, 2))
            .or_else(|| {
                mutating_leaf_stub2_by_id(stub_id)
                    .filter(|stub| stub.is_valid())
                    .map(|stub| (stub.entry_addr(), stub.descriptor, 2))
            })
            .or_else(|| {
                mutating_leaf_stub3_by_id(stub_id)
                    .filter(|stub| stub.is_valid())
                    .map(|stub| (stub.entry_addr(), stub.descriptor, 3))
            });
        let Some((entry_addr, descriptor, last_register)) = pair else {
            return Err(Unsupported::OperandShape("native leaf entry"));
        };
        dynasm!(ops
            ; .arch aarch64
            ; ldr x0, [X(context_x), THREAD_OFFSET]
            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        );
        emit_entry_values(ops, 1, last_register, value_count, &mut load_value)?;
        emit_load_symbol_u64(
            ops,
            relocations,
            16,
            entry_addr as u64,
            RelocationTarget::runtime_stub(descriptor),
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x1, =>bail
        );
        return Ok(());
    }

    let Some((entry_addr, descriptor)) = alloc_value_stub_by_id(stub_id)
        .filter(|stub| stub.is_valid_for_safepoint(safepoint_id))
        .and_then(|stub| Some((stub.entry_addr()?, stub.descriptor)))
    else {
        return Err(Unsupported::OperandShape("native allocating entry"));
    };
    // The context lives in the caller's own stack, so the entry's rooting
    // packet is torn down by the same `add sp` that unwinds it, on both the
    // taken and the missed path.
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [X(context_x), THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; movz w9, safepoint_id
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(safepoint_id));
    emit_entry_values(ops, 2, 4, value_count, &mut load_value)?;
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        entry_addr as u64,
        RelocationTarget::runtime_stub(descriptor),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>bail
    );
    Ok(())
}

/// Fill the operand registers `first..=last` a declared family reads, padding
/// the words the site does not fill with `undefined`.
fn emit_entry_values<F>(
    ops: &mut Assembler,
    first: u8,
    last: u8,
    value_count: u8,
    load_value: &mut F,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    for register in first..=last {
        let index = register - first;
        if index < value_count {
            load_value(ops, index, register)?;
        } else {
            emit_load_u64(ops, register, VALUE_UNDEFINED);
        }
    }
    Ok(())
}

/// Declared entry name for one guarded callable builtin, for diagnostics.
///
/// Process-independent: the id names a declaration, never an address.
pub(crate) fn native_leaf_call_name(stub_id: RuntimeStubId) -> &'static str {
    runtime_stub_name(stub_id)
}

/// Emit `dst = receiver.method(args…)` where the method is a declared native
/// entry, guarding the receiver, the method slot's identity, and nothing else.
///
/// The receiver layout is baked rather than probed: the site's feedback already
/// resolved how the receiver is named, where the method lives and at which slot
/// byte, so this needs no cache cell and no way walk. Both receiver forms —
/// an ordinary object named by its hidden class, and an exotic body named by
/// its cell type tag — end at the same value slab, so one sequence lowers them.
///
/// `load_argument(ops, index, register)` materializes one JavaScript argument
/// and runs only after every guard. An exotic receiver is passed to the entry
/// ahead of those arguments, because the operation is on that body. The boxed
/// result is left in `x0`; every miss branches to `miss`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_guarded_method_call<F>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    call: &JitGuardedMethodCall,
    receiver: u16,
    byte_pc: u32,
    mut load_argument: F,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    // An exotic receiver is passed to the entry as its first operand, because
    // the operation is on that body; a shaped receiver only supplies arguments.
    let receiver_is_operand = matches!(call.receiver, JitGuardedReceiver::Exotic(_));
    emit_guarded_method_guard(ops, relocations, view, call, receiver, byte_pc, miss)?;
    // An exotic receiver occupies the entry's first operand word, so the call's
    // own arguments shift one place along.
    let receiver_word = u8::from(receiver_is_operand);
    let value_count = receiver_word + call.argument_count;
    emit_native_entry_call(
        ops,
        relocations,
        call.entry_stub_id,
        call.safepoint_id,
        value_count,
        20,
        |ops, index, register| {
            if receiver_is_operand && index == 0 {
                return emit_load_reg(ops, register, receiver);
            }
            load_argument(ops, index - receiver_word, register)
        },
        miss,
    )
}

/// Guard the receiver, pinned method holder, and exact builtin identity while
/// leaving completion of the already-proven operation to the caller.
///
/// This is the shared prefix for declared method entries and optimizing-tier
/// machine intrinsics. It performs no argument loads and has no effects before
/// a miss.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_guarded_method_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    call: &JitGuardedMethodCall,
    receiver: u16,
    byte_pc: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_guarded_method_guard_impl(
        ops,
        relocations,
        view,
        call,
        receiver,
        None,
        byte_pc,
        miss,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_guarded_method_guard_impl(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    call: &JitGuardedMethodCall,
    receiver: u16,
    tagged_receiver: Option<u8>,
    byte_pc: u32,
    miss: DynamicLabel,
    preserve_receiver: bool,
) -> Result<(), Unsupported> {
    if view.cage_base == 0 || view.native_ref_byte == 0 {
        return Err(Unsupported::OperandShape("guarded method call layout"));
    }
    match call.receiver {
        // A cell carrying an ordinary object body whose shape is the one the
        // site recorded. The shape pins the slot offset; the identity guard
        // below still pins which function occupies it, since assigning over an
        // existing property leaves the shape alone.
        JitGuardedReceiver::Shape { shape } => {
            let shape_byte = view.object_shape_byte;
            emit_receiver_type_guard_impl(
                ops,
                relocations,
                view,
                receiver,
                tagged_receiver,
                OBJECT_BODY_TYPE_TAG,
                miss,
            )?;
            emit_ordinary_lookup_state_guard(ops, view, 13, miss);
            if preserve_receiver {
                dynasm!(ops ; .arch aarch64 ; mov x8, x13);
            }
            dynasm!(ops
                ; .arch aarch64
                ; ldr w14, [x13, shape_byte]
                ; cbz w14, =>miss
            );
            emit_load_u64(ops, 12, u64::from(shape));
            dynasm!(ops
                ; .arch aarch64
                ; cmp w14, w12
                ; b.ne =>miss
            );
            // `0` means the receiver owns the slot. Otherwise the way's guarded
            // prototype hop runs, which reads `[[Prototype]]` at run time
            // because `setPrototypeOf` moves it while the shape stays put.
            if call.holder_shape != 0 {
                emit_load_u64(ops, 7, u64::from(call.holder_shape));
                emit_resolve_holder(ops, relocations, view, miss);
            }
            super::values::emit_slab_base(ops, view, 13, 14);
            dynasm!(ops
                ; .arch aarch64
                ; cbz x13, =>miss
                ; mov x15, x13
            );
        }
        // A cell carrying the recorded exotic body, whose builtin lives on a
        // pinned realm prototype. A latched body must additionally read clean:
        // an expando, an overridden method or a custom descriptor makes the
        // prototype's slot the wrong answer even though the prototype itself is
        // unchanged.
        JitGuardedReceiver::Exotic(otter_vm::jit::JitIntrinsicPrototype {
            type_tag,
            guard,
            proto_offset,
        }) => {
            emit_receiver_type_guard_impl(
                ops,
                relocations,
                view,
                receiver,
                tagged_receiver,
                u32::from(type_tag),
                miss,
            )?;
            if preserve_receiver {
                dynasm!(ops ; .arch aarch64 ; mov x8, x13);
            }
            if let Some(guard) = guard {
                emit_body_guard(ops, guard, miss);
            }
            emit_prototype_guard(
                ops,
                relocations,
                view,
                proto_offset,
                call.holder_shape,
                byte_pc,
                call.entry_stub_id,
                miss,
            );
        }
    }
    emit_builtin_identity_guard(
        ops,
        relocations,
        view,
        call.method_value_byte,
        call.builtin_native_ref,
        miss,
    );
    if preserve_receiver {
        dynasm!(ops ; .arch aarch64 ; mov x13, x8);
    }
    Ok(())
}

fn emit_receiver_type_guard_impl(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    receiver: u16,
    tagged_receiver: Option<u8>,
    receiver_type_tag: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let tagged = if let Some(tagged) = tagged_receiver {
        tagged
    } else {
        emit_load_reg(ops, 9, receiver)?;
        9
    };
    let tag_scratch = if tagged == 11 { 10 } else { 11 };
    emit_cell_test(ops, tagged, tag_scratch, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64 ; mov w12, W(tagged));
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, receiver_type_tag
        ; b.ne =>miss
    );
    Ok(())
}

/// Prove one declared body word still holds the value the layout depends on.
///
/// Expects the body header in `x13`. There is one sequence per declared
/// *width*, never one per receiver family: a collection's flags word, an
/// array's exotic sidecar and a typed view's kind discriminant are the same
/// two instructions at different offsets. Clobbers `x14`.
fn emit_body_guard(ops: &mut Assembler, guard: JitBodyGuard, miss: DynamicLabel) {
    let byte = guard.byte;
    match guard.width {
        JitGuardWidth::Byte => dynasm!(ops ; .arch aarch64 ; ldrb w14, [x13, byte]),
        JitGuardWidth::Word32 => dynasm!(ops ; .arch aarch64 ; ldr w14, [x13, byte]),
        JitGuardWidth::Word64 => dynasm!(ops ; .arch aarch64 ; ldr x14, [x13, byte]),
    }
    if guard.expect == 0 {
        dynasm!(ops ; .arch aarch64 ; cbnz x14, =>miss);
    } else {
        emit_load_u64(ops, 12, u64::from(guard.expect));
        dynasm!(ops ; .arch aarch64 ; cmp x14, x12 ; b.ne =>miss);
    }
}

/// Prove the realm prototype still has the expected identity and shape. On
/// success `x15` holds its value-slab pointer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_prototype_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    proto_offset: u32,
    proto_shape: u32,
    byte_pc: u32,
    runtime_stub_id: RuntimeStubId,
    miss: DynamicLabel,
) {
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    emit_load_symbol_u64(
        ops,
        relocations,
        15,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        12,
        u64::from(proto_offset),
        RelocationTarget::GuardedHeapReference {
            component: GuardedHeapComponent::Prototype,
            byte_pc,
            runtime_stub_id,
        },
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x15, x15, x12
        ; ldrb w14, [x15]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x15, object_shape_byte]
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        12,
        u64::from(proto_shape),
        RelocationTarget::GuardedHeapReference {
            component: GuardedHeapComponent::PrototypeShape,
            byte_pc,
            runtime_stub_id,
        },
    );
    dynasm!(ops
        ; .arch aarch64
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldr x15, [x15, object_values_ptr_byte]
        ; cbz x15, =>miss
    );
}

/// Guard the method slot against the exact static builtin address. Expects
/// the holder's slab pointer in `x15`; leaves nothing live.
///
/// Every holder — an ordinary receiver's own slab and a pinned realm
/// prototype's alike — stores the same 8-byte `Value` representation.
pub(crate) fn emit_builtin_identity_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    method_value_byte: u32,
    builtin_native_ref: u32,
    miss: DynamicLabel,
) {
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    let native_ref_byte = view.native_ref_byte;
    dynasm!(ops
        ; .arch aarch64
        ; ldr w9, [x15, method_value_byte]
        ; ands w11, w9, #0x7
        ; b.ne =>miss
        ; cbz w9, =>miss
        ; mov w12, w9
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
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, native_function_type_tag
        ; b.ne =>miss
        ; ldr w14, [x13, native_ref_byte]
    );
    emit_native_ref_compare(ops, builtin_native_ref);
    dynasm!(ops
        ; .arch aarch64
        ; b.ne =>miss
    );
}

/// Compare the external-reference index already loaded into `w14` against the
/// one the guard demands, leaving the result in the flags.
///
/// The index is isolate-local and assigned in bootstrap install order, so
/// unlike the entry address it used to replace it is a plain build-stable
/// constant: no relocation, and a table small enough that the common case is a
/// single `cmp` against a 12-bit immediate.
fn emit_native_ref_compare(ops: &mut Assembler, native_ref: u32) {
    if native_ref <= 0xfff {
        dynasm!(ops
            ; .arch aarch64
            ; cmp w14, native_ref
        );
        return;
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w15, native_ref & 0xffff
        ; movk w15, native_ref >> 16, lsl 16
        ; cmp w14, w15
    );
}
