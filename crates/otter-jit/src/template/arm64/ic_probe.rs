//! AArch64 lowering of one cache program, shared by every tier that probes a
//! property site.
//!
//! # Contents
//! - [`emit_way_walk`] — match the receiver shape against the cell's ways.
//! - [`emit_resolve_holder`] — perform the guarded prototype hop a matched way
//!   asks for.
//! - [`emit_refuse_prototype_hop`] — send hop programs to the stub at sites
//!   that cannot execute them.
//! - [`emit_exotic_length_fast`] — the `.length` reads no cache program can
//!   describe.
//! - [`emit_native_leaf_call`] — guard a callee's bootstrap identity and run
//!   its declared leaf entry without materializing a frame.
//!
//! # Invariants
//! - Both tiers emit property probes from here. A cache program has exactly one
//!   machine lowering, so a tier cannot disagree with the interpreter about
//!   what a site caches.
//! - Way stride is [`WHISKER_IC_WAY_BYTES`], asserted against the cell's own
//!   layout where the cell is defined.
//! - Register contract on entry: `x15` holds the cell address, `w14` the
//!   receiver shape handle, `x13` the receiver's `GcHeader`. On a matched way
//!   `w17` holds the slot byte offset and `w7` the guarded holder shape; after
//!   [`emit_resolve_holder`], `x13` is the holder's header and `w14` its shape.
//! - The hop reads the receiver's `[[Prototype]]` at run time. The receiver
//!   shape cannot stand in for it: the prototype lives in the object body, so
//!   `setPrototypeOf` changes it while the shape stays put.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, JitGuardedReceiver};

use otter_vm::native_abi::{RuntimeStubId, runtime_stub_name};
use otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id;

use super::values::{emit_box_int32, emit_load_reg, emit_load_symbol_u64, emit_load_u64};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    IC_WAYS, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, Unsupported, VALUE_UNDEFINED,
    VM_THREAD_GC_HEAP_OFFSET, WHISKER_IC_WAY_BYTES,
};

/// Match `w14` against the cell's ways, branching to `miss` when none hold.
///
/// On a match `w7` carries the way's holder shape and `w17` its slot byte
/// offset, and control falls through to `matched`.
pub(crate) fn emit_way_walk(ops: &mut Assembler, matched: DynamicLabel, miss: DynamicLabel) {
    for way in 0..IC_WAYS as u32 {
        let shape_off = way * WHISKER_IC_WAY_BYTES;
        let holder_off = shape_off + 4;
        let value_byte_off = shape_off + 8;
        let next = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; ldr w16, [x15, shape_off]
            ; cmp w14, w16
            ; b.ne =>next
            ; ldr w7, [x15, holder_off]
            ; ldr w17, [x15, value_byte_off]
            ; b =>matched
            ; =>next
        );
    }
    dynasm!(ops ; .arch aarch64 ; b =>miss ; =>matched);
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
        ; =>resolved
    );
}

/// Send a matched way to `miss` when it asks for a hop this site cannot run.
///
/// Stores write through the holder, which the store sequences do not resolve;
/// such a program stays on the stub rather than writing the wrong object.
pub(crate) fn emit_refuse_prototype_hop(ops: &mut Assembler, miss: DynamicLabel) {
    dynasm!(ops ; .arch aarch64 ; cbnz w7, =>miss);
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

    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2       // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>not_length
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
    emit_box_int32(ops, 9, 12);
    dynasm!(ops ; .arch aarch64 ; b =>have_length);
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
    let Some(declaration) = otter_vm::math::jit_leaf_builtin(stub_id) else {
        return false;
    };
    view.native_static_fn_byte != 0
        && argc == usize::from(declaration.argument_count)
        && leaf_no_alloc_stub2_by_id(stub_id).is_some()
}

/// Whether a guarded leaf method call can be lowered at all.
///
/// The layout words the guards read come from the compile snapshot; without
/// them the site keeps the ordinary path instead of failing the whole compile.
pub(crate) fn native_leaf_method_call_is_supported(
    view: &JitCompileSnapshot,
    call: &otter_vm::JitMethodNativeLeafCall,
) -> bool {
    view.cage_base != 0
        && view.native_static_fn_byte != 0
        && leaf_no_alloc_stub2_by_id(call.leaf_stub_id).is_some()
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
pub(crate) fn emit_native_leaf_call<F>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    stub_id: RuntimeStubId,
    builtin_fn_addr: usize,
    callee_x: u8,
    load_argument: F,
    bail: DynamicLabel,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    debug_assert!(
        !(12..=16).contains(&callee_x),
        "the callee must survive the guard, which owns x12..x16"
    );

    let native_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    dynasm!(ops
        ; .arch aarch64
        ; movz x12, NUMBER_TAG_HI16, lsl #48
        ; orr x12, x12, #0x2       // NOT_CELL_MASK
        ; tst X(callee_x), x12
        ; b.ne =>bail
        ; cbz X(callee_x), =>bail
        ; ldrb w14, [X(callee_x)]
        ; cmp w14, native_type_tag
        ; b.ne =>bail
        ; ldr x14, [X(callee_x), view.native_static_fn_byte]
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        15,
        builtin_fn_addr as u64,
        RelocationTarget::NativeLeafBuiltinFunction { stub_id },
    );
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, x15
        ; b.ne =>bail
    );

    let Some(declaration) = otter_vm::math::jit_leaf_builtin(stub_id) else {
        return Err(Unsupported::OperandShape("native leaf entry"));
    };
    emit_native_leaf_entry_call(
        ops,
        relocations,
        stub_id,
        declaration.argument_count,
        load_argument,
        bail,
    )
}

/// Call a declared leaf entry whose identity a caller has already guarded.
///
/// Runs only once every guard has passed, so nothing it writes is live across a
/// miss and `load_value` may freely use `x10`–`x15`. `(heap, value0, value1) ->
/// pair`, with no safepoint: a leaf entry cannot allocate, collect, or re-enter
/// JS. `value_count` is how many of the two operand words the entry reads; the
/// rest are `undefined`. The boxed result is left in `x0`; a miss branches to
/// `bail`.
pub(crate) fn emit_native_leaf_entry_call<F>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    stub_id: RuntimeStubId,
    value_count: u8,
    mut load_value: F,
    bail: DynamicLabel,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    let Some(stub) = leaf_no_alloc_stub2_by_id(stub_id) else {
        return Err(Unsupported::OperandShape("native leaf entry"));
    };
    dynasm!(ops
        ; .arch aarch64
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
    );
    load_value(ops, 0, 1)?;
    if value_count >= 2 {
        load_value(ops, 1, 2)?;
    } else {
        emit_load_u64(ops, 2, VALUE_UNDEFINED);
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        stub.entry_addr() as u64,
        RelocationTarget::runtime_stub(stub.descriptor),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; cbnz x1, =>bail
    );
    Ok(())
}

/// Declared entry name for one guarded callable builtin, for diagnostics.
///
/// Process-independent: the id names a declaration, never an address.
pub(crate) fn native_leaf_call_name(stub_id: RuntimeStubId) -> &'static str {
    runtime_stub_name(stub_id)
}

/// Emit `dst = receiver.method(args…)` where the method is a declared leaf
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
pub(crate) fn emit_native_leaf_method_call<F>(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    call: &otter_vm::JitMethodNativeLeafCall,
    receiver: u16,
    mut load_argument: F,
    miss: DynamicLabel,
) -> Result<(), Unsupported>
where
    F: FnMut(&mut Assembler, u8, u8) -> Result<(), Unsupported>,
{
    if view.cage_base == 0 || view.native_static_fn_byte == 0 {
        return Err(Unsupported::OperandShape("native leaf method layout"));
    }
    // The identity guard reads a pinned prototype's slot word through the
    // decompressing path and an ordinary receiver's through the bare cage
    // offset, matching how each holder's slab was reached.
    let decompress_via_slot = matches!(call.receiver, JitGuardedReceiver::Exotic { .. });
    match call.receiver {
        // A cell carrying an ordinary object body whose shape is the one the
        // site recorded. The shape pins the slot offset; the identity guard
        // below still pins which function occupies it, since assigning over an
        // existing property leaves the shape alone.
        JitGuardedReceiver::Shape { shape } => {
            let shape_byte = view.object_shape_byte;
            super::collections::emit_receiver_type_guard(
                ops,
                relocations,
                view,
                receiver,
                OBJECT_BODY_TYPE_TAG,
                miss,
            )?;
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
        // an expando or an overridden method makes the prototype's slot the
        // wrong answer even though the prototype itself is unchanged.
        JitGuardedReceiver::Exotic {
            type_tag,
            latched,
            proto_offset,
        } => {
            super::collections::emit_receiver_type_guard(
                ops,
                relocations,
                view,
                receiver,
                u32::from(type_tag),
                miss,
            )?;
            if latched {
                let guard_flags_byte = view.collection_layout.guard_flags_byte;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr w14, [x13, guard_flags_byte]
                    ; cbnz w14, =>miss
                );
            }
            super::collections::emit_prototype_guard(
                ops,
                relocations,
                view,
                proto_offset,
                call.holder_shape,
                crate::artifact::relocation::GuardedBuiltinKind::Leaf,
                0,
                call.leaf_stub_id,
                miss,
            );
        }
    }
    super::collections::emit_builtin_identity_guard(
        ops,
        relocations,
        view,
        call.method_value_byte,
        call.builtin_fn_addr,
        decompress_via_slot,
        crate::artifact::relocation::GuardedBuiltinKind::Leaf,
        0,
        call.leaf_stub_id,
        miss,
    );
    // An exotic receiver occupies the entry's first operand word, so the call's
    // own arguments shift one place along.
    let receiver_word = u8::from(decompress_via_slot);
    let value_count = receiver_word + call.argument_count;
    emit_native_leaf_entry_call(
        ops,
        relocations,
        call.leaf_stub_id,
        value_count.min(2),
        |ops, index, register| {
            if decompress_via_slot && index == 0 {
                return emit_load_reg(ops, register, receiver);
            }
            load_argument(ops, index - receiver_word, register)
        },
        miss,
    )
}
