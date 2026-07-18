//! AArch64 guarded collection-method calls for the template compiler.
//!
//! # Contents
//! - Identity guards proving a monomorphic `Map`/`Set` builtin method site.
//! - The leaf (`LeafValue2`) call for non-allocating collection reads.
//! - The allocating (`AllocValue3`) call publishing a concrete safepoint.
//!
//! # Invariants
//! - Guards prove receiver type tag, clean guard flags, prototype identity,
//!   prototype shape, and the exact static builtin address before any typed
//!   entry runs; every miss falls back to the shared method-call path.
//! - Leaf entries receive only the opaque heap pointer plus boxed operand
//!   bits and can neither allocate nor re-enter JS.
//! - The allocating entry runs over the frozen call packet with the
//!   snapshot-assigned safepoint; operands re-load from the rooted frame.
//! - Prototype offsets/shapes, builtin identities, cage bases, and stub entries
//!   are captured with the source byte-PC and feedback class when diagnostics
//!   are enabled.
//!
//! # See also
//! - `otter_vm::runtime_stubs` — the compile-time-resolved typed entries.
//! - [`super::calls`] — the shared method-call fallback these fast paths
//!   precede.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::runtime_stubs::{alloc_value_stub_by_id, leaf_no_alloc_stub2_by_id};
use otter_vm::{JitCollectionAllocMethod, JitCollectionLeafMethod, JitCompileSnapshot};

use super::values::{
    emit_decompress_slot, emit_load_reg, emit_load_runtime_stub, emit_load_symbol_u64,
    emit_load_u64, emit_store_reg,
};
use crate::artifact::relocation::{
    CollectionFeedbackKind, CollectionHeapComponent, RelocationCapture, RelocationTarget,
};
use crate::entry::{
    ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET,
    ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG,
    THREAD_OFFSET, Unsupported, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
};

/// Method-call operand bundle shared by the guarded fast paths.
pub(super) struct MethodSite {
    pub(super) dst: u16,
    pub(super) receiver: u16,
    pub(super) argc: u16,
    pub(super) arg0: Option<u16>,
    pub(super) arg1: Option<u16>,
}

/// Emit the receiver + prototype + builtin identity guard chain shared by the
/// leaf and allocating collection calls. On success `x15` holds the
/// prototype's value-slab pointer; every failed guard branches to `miss`.
#[allow(clippy::too_many_arguments)]
fn emit_receiver_guards(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    receiver: u16,
    receiver_type_tag: u32,
    proto_offset: u32,
    proto_shape: u32,
    feedback_kind: CollectionFeedbackKind,
    byte_pc: u32,
    runtime_stub_id: u32,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let guard_flags_byte = view.collection_layout.guard_flags_byte;
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    emit_load_reg(ops, 9, receiver)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2       // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
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
        ; cmp w14, receiver_type_tag
        ; b.ne =>miss
        ; ldr w14, [x13, guard_flags_byte]
        ; cbnz w14, =>miss
    );
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
        RelocationTarget::CollectionHeapReference {
            component: CollectionHeapComponent::Prototype,
            feedback_kind,
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
        RelocationTarget::CollectionHeapReference {
            component: CollectionHeapComponent::PrototypeShape,
            feedback_kind,
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
    Ok(())
}

/// Guard the method slot against the exact static builtin address. Expects
/// the prototype slab pointer in `x15`; leaves nothing live.
fn emit_builtin_identity_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    method_value_byte: u32,
    builtin_fn_addr: usize,
    decompress_via_slot: bool,
    feedback_kind: CollectionFeedbackKind,
    byte_pc: u32,
    runtime_stub_id: u32,
    miss: DynamicLabel,
) {
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    let native_static_fn_byte = view.native_static_fn_byte;
    if decompress_via_slot {
        dynasm!(ops
            ; .arch aarch64
            ; ldr w17, [x15, method_value_byte]
        );
        emit_decompress_slot(ops, relocations, view.cage_base as u64, miss);
        dynasm!(ops
            ; .arch aarch64
            ; mov x9, x17
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #0x2   // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9
        );
    } else {
        // The value slab holds 4-byte compressed slots. The method must be a
        // cell (low-3 tag `000`, nonzero); its zero-extended offset is the
        // bare cage offset. Anything else misses.
        dynasm!(ops
            ; .arch aarch64
            ; ldr w9, [x15, method_value_byte]
            ; ands w11, w9, #0x7
            ; b.ne =>miss
            ; cbz w9, =>miss
            ; mov w12, w9
        );
    }
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
        ; ldr x14, [x13, native_static_fn_byte]
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        15,
        builtin_fn_addr as u64,
        RelocationTarget::CollectionBuiltinFunction {
            feedback_kind,
            byte_pc,
            runtime_stub_id,
        },
    );
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, x15
        ; b.ne =>miss
    );
}

/// Emit the guarded non-allocating collection read (`map.get`/`map.has`/
/// `set.has`) through the compile-time-resolved `LeafValue2` entry. Returns
/// `false` when the site cannot take the fast path at all.
pub(super) fn emit_leaf_method_guarded_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    leaf: &JitCollectionLeafMethod,
    byte_pc: u32,
    site: &MethodSite,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 {
        return Ok(false);
    }
    let Some(stub) = leaf_no_alloc_stub2_by_id(leaf.leaf_stub_id) else {
        return Ok(false);
    };
    debug_assert!(stub.is_valid());
    emit_receiver_guards(
        ops,
        relocations,
        view,
        site.receiver,
        u32::from(leaf.receiver_type_tag),
        leaf.proto_offset,
        leaf.proto_shape,
        CollectionFeedbackKind::Leaf,
        byte_pc,
        leaf.leaf_stub_id,
        miss,
    )?;
    emit_builtin_identity_guard(
        ops,
        relocations,
        view,
        leaf.method_value_byte,
        leaf.builtin_fn_addr,
        true,
        CollectionFeedbackKind::Leaf,
        byte_pc,
        leaf.leaf_stub_id,
        miss,
    );
    // Leaf machine call: `(heap, receiver bits, key bits) -> pair`.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
    );
    emit_load_reg(ops, 1, site.receiver)?;
    if let Some(key) = site.arg0 {
        emit_load_reg(ops, 2, key)?;
    } else {
        emit_load_u64(ops, 2, VALUE_UNDEFINED);
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        stub.entry_addr() as u64,
        stub.descriptor,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; cbnz x1, =>miss
    );
    emit_store_reg(ops, 0, site.dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

/// Emit the guarded allocating collection write (`map.set`/`set.add`/
/// `map.delete`/…) through the resolved `AllocValue3` entry over the frozen
/// call packet, publishing the snapshot-assigned safepoint. Returns `false`
/// when the site cannot take the fast path at all.
pub(super) fn emit_alloc_method_guarded_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    alloc: &JitCollectionAllocMethod,
    byte_pc: u32,
    site: &MethodSite,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 || alloc.value_arg_count != 3 {
        return Ok(false);
    }
    let Some(stub) = alloc_value_stub_by_id(alloc.alloc_stub_id) else {
        return Ok(false);
    };
    let Some(stub_addr) = stub.entry_addr() else {
        return Ok(false);
    };
    emit_receiver_guards(
        ops,
        relocations,
        view,
        site.receiver,
        u32::from(alloc.receiver_type_tag),
        alloc.proto_offset,
        alloc.proto_shape,
        CollectionFeedbackKind::Alloc,
        byte_pc,
        alloc.alloc_stub_id,
        miss,
    )?;
    emit_builtin_identity_guard(
        ops,
        relocations,
        view,
        alloc.method_value_byte,
        alloc.builtin_fn_addr,
        false,
        CollectionFeedbackKind::Alloc,
        byte_pc,
        alloc.alloc_stub_id,
        miss,
    );
    let arg1 = if site.argc <= 1
        || alloc.alloc_stub_id == otter_vm::native_abi::STUB_COLLECTION_SET_ADD_ALLOC.id
    {
        None
    } else {
        site.arg1
    };
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; movz w9, alloc.safepoint_id
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(alloc.safepoint_id));
    emit_load_reg(ops, 2, site.receiver)?;
    if let Some(arg0) = site.arg0 {
        emit_load_reg(ops, 3, arg0)?;
    } else {
        emit_load_u64(ops, 3, VALUE_UNDEFINED);
    }
    if let Some(arg1) = arg1 {
        emit_load_reg(ops, 4, arg1)?;
    } else {
        emit_load_u64(ops, 4, VALUE_UNDEFINED);
    }
    emit_load_runtime_stub(ops, relocations, 16, stub_addr as u64, stub.descriptor);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    emit_store_reg(ops, 0, site.dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}
