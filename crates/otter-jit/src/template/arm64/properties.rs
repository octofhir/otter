//! AArch64 named-property IC probes for the template compiler.
//!
//! # Contents
//! - Inline guarded own-data loads/stores through self-patching WhiskerIC
//!   cells, with the Array exotic `length` fast path.
//! - Window-transition misses that resolve full load/`[[Set]]` semantics and
//!   self-patch cacheable sites.
//!
//! # Invariants
//! - Inline sequences neither allocate nor call, so they carry no safepoint;
//!   the receiver pointer is recomputed from the rooted frame slot on every
//!   access and never survives one.
//! - The slab base derives from the fresh header (inline slab) or the stable
//!   out-of-line `values_ptr` — never a cached body pointer that the moving
//!   collector could dangle.
//! - Pointer-valued stores run the generational write barrier; primitive
//!   stores skip it. Wide values the compressed slot cannot hold take the
//!   window transition.
//! - Store misses publish the frame window before entering the VM, so setters,
//!   proxies, exceptions, reentry, and moving GC complete without replay.
//!
//! # See also
//! - [`super::values`] — slot compression/decompression primitives.
//! - `crates/otter-jit/src/entry/runtime_ops/vm_ops.rs` — the window
//!   transitions and the authoritative cell layout.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;

use super::transitions::TransitionTable;
use super::values::{
    BoxedSlotSlowPath, emit_box_int32, emit_compress_slot_or_bail, emit_decompress_slot,
    emit_load_u64, emit_slab_base,
};
use crate::entry::{IC_WAYS, NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, Unsupported, reg_offset};

/// Emit `dst = obj.name` with the inline WhiskerIC probe.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_load_property(
    ops: &mut Assembler,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    dst: u16,
    object: u16,
    name: u32,
    site: u64,
    array_length: bool,
    cell_addr: usize,
    boxed_slot_slow_paths: &mut Vec<BoxedSlotSlowPath>,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let cage_base = view.cage_base;
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    if cage_base != 0 && array_length {
        let obj_off = reg_offset(object)?;
        let dst_off = reg_offset(dst)?;
        let array_tag = u32::from(view.array_layout.type_tag);
        let length_byte = view.array_layout.length_byte;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, obj_off]   // receiver Value
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #0x2       // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9              // low-32 Gc offset
        );
        emit_load_u64(ops, 13, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12        // x13 = GcHeader ptr
            ; ldrb w14, [x13]
            ; cmp w14, array_tag
            ; b.ne =>miss
            ; ldr x9, [x13, length_byte]
        );
        emit_load_u64(ops, 12, i32::MAX as u64);
        dynasm!(ops
            ; .arch aarch64
            ; cmp x9, x12
            ; b.hi =>miss
        );
        emit_box_int32(ops, 9, 12);
        dynasm!(ops
            ; .arch aarch64
            ; str x9, [x19, dst_off]
            ; b =>done
        );
    }

    // Inline guarded own-data load through the self-patching cell: guard tag
    // + GC type tag + cell shape, then read the value slab slot at the cell's
    // byte offset. Shape `0` is the empty-cell sentinel, so live shape-0
    // receivers deliberately miss to the transition.
    if cage_base != 0 {
        let obj_off = reg_offset(object)?;
        let dst_off = reg_offset(dst)?;
        let shape_byte = view.object_shape_byte;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, obj_off]   // receiver Value
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #0x2       // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9              // low-32 Gc offset (zero-ext)
        );
        emit_load_u64(ops, 13, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12        // x13 = GcHeader ptr
            ; ldrb w14, [x13]          // header type tag
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x13, shape_byte] // receiver shape handle
            ; cbz w14, =>miss
        );
        emit_load_u64(ops, 15, cell_addr as u64);
        // Walk the IC ways: a hit loads that way's value byte into w17 and
        // shares the slab read.
        let do_load = ops.new_dynamic_label();
        for way in 0..IC_WAYS as u32 {
            let shape_off = way * 8;
            let vbyte_off = shape_off + 4;
            let next = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; ldr w16, [x15, shape_off]
                ; cmp w14, w16
                ; b.ne =>next
                ; ldr w17, [x15, vbyte_off]
                ; b =>do_load
                ; =>next
            );
        }
        dynasm!(ops ; .arch aarch64 ; b =>miss ; =>do_load);
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x13, =>miss
            ; ldr w9, [x13, x17]       // 4-byte compressed slot
        );
        let boxed_entry = ops.new_dynamic_label();
        let continuation = ops.new_dynamic_label();
        boxed_slot_slow_paths.push(BoxedSlotSlowPath {
            entry: boxed_entry,
            continuation,
            miss,
        });
        emit_decompress_slot(ops, cage_base as u64, boxed_entry);
        dynasm!(ops
            ; .arch aarch64
            ; =>continuation
            ; str x9, [x19, dst_off]
            ; b =>done
        );
    }

    // Miss / no cage base: the window transition resolves own-data IC state,
    // self-patches cacheable sites, and completes full `[[Get]]` semantics.
    dynasm!(ops
        ; .arch aarch64
        ; =>miss
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, object as u32
    );
    emit_load_u64(ops, 3, u64::from(name));
    emit_load_u64(ops, 4, site);
    emit_load_u64(ops, 5, cell_addr as u64);
    emit_load_u64(ops, 6, u64::from(view.code_block.id));
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_LOAD_PROPERTY));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; =>done
    );
    Ok(())
}

/// Emit `obj.name = value` with the inline WhiskerIC probe.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_property(
    ops: &mut Assembler,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    object: u16,
    name: u32,
    value: u16,
    site: u64,
    cell_addr: usize,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let cage_base = view.cage_base;
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    // Inline guarded existing-own-data store through the self-patching cell,
    // then a value-tag-gated write barrier (primitive stores skip it).
    if cage_base != 0 {
        let obj_off = reg_offset(object)?;
        let src_off = reg_offset(value)?;
        let shape_byte = view.object_shape_byte;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, obj_off]   // receiver Value
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #0x2       // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>miss
            ; mov w12, w9              // low-32 Gc offset
        );
        emit_load_u64(ops, 13, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x13, x12        // x13 = GcHeader ptr
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x13, shape_byte] // receiver shape handle
            ; cbz w14, =>miss
        );
        emit_load_u64(ops, 15, cell_addr as u64);
        let do_store = ops.new_dynamic_label();
        for way in 0..IC_WAYS as u32 {
            let shape_off = way * 8;
            let vbyte_off = shape_off + 4;
            let next = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; ldr w16, [x15, shape_off]
                ; cmp w14, w16
                ; b.ne =>next
                ; ldr w17, [x15, vbyte_off]
                ; b =>do_store
                ; =>next
            );
        }
        let store_prim = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; b =>miss
            ; =>do_store
            ; ldr x9, [x19, src_off]   // value to store
        );
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; cbz x13, =>miss
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; orr x11, x11, #0x2       // NOT_CELL_MASK
            ; tst x9, x11
            ; b.ne =>store_prim        // primitive → compress, no barrier
            // Cell: the compressed ref is the low-32 8-aligned offset
            // (low-3 tag 000), i.e. the value's low word.
            ; str w9, [x13, x17]
            // Pointer value: card-mark the parent header from the published
            // frame's register window.
            ; mov x0, x20
            ; movz x1, object as u32
            ; movz x2, value as u32
        );
        emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_WRITE_BARRIER));
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbnz x0, =>threw
            ; b =>done
            ; =>store_prim
        );
        // A wide int / double / function id cannot inline-compress (a boxed
        // number allocates); the window transition handles it.
        emit_compress_slot_or_bail(ops, miss);
        dynasm!(ops ; .arch aarch64 ; str w10, [x13, x17] ; b =>done);
    }

    // Miss / no cage base: the window transition resolves the store and
    // self-patches the cell. Accessor/exotic/proxy/primitive semantics complete
    // in place through the VM's single value-level `[[Set]]` funnel.
    dynasm!(ops
        ; .arch aarch64
        ; =>miss
        ; mov x0, x20
        ; movz x1, object as u32
    );
    emit_load_u64(ops, 2, u64::from(name));
    dynasm!(ops ; .arch aarch64 ; movz x3, value as u32);
    emit_load_u64(ops, 4, site);
    emit_load_u64(ops, 5, cell_addr as u64);
    emit_load_u64(ops, 6, u64::from(view.code_block.id));
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_STORE_PROPERTY));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; =>done
    );
    Ok(())
}
