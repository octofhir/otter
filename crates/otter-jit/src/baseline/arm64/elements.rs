//! Dense Array and TypedArray AArch64 fast-path emitters.
//!
//! # Contents
//! - Host Vec layout probing for frozen typed-array offsets.
//! - Receiver decompression and integer-index guards.
//! - Dense Array and typed backing load/store templates.
//!
//! # Invariants
//! - Live backing lengths bound every native memory access.
//! - Movable object pointers never survive a safepoint.
//! - Observable holes, prototypes, accessors, and exotic cases miss to the VM.

use super::*;

/// Probe the `Vec<u8>` field layout — which std does **not** guarantee — by
/// value-identity, returning `(data_pointer_byte_offset, length_byte_offset)`
/// of the two words within a `Vec<u8>`. Computed once and cached. The inline
/// typed-array element path reads the backing buffer's data pointer and its
/// live byte length (the memory-safety bound) at these offsets.
pub(in super::super) fn vec_layout_offsets() -> (u32, u32) {
    use std::sync::OnceLock;
    static CACHE: OnceLock<(u32, u32)> = OnceLock::new();
    *CACHE.get_or_init(|| {
        // capacity 4, length 1: cap, len, and the (large) data pointer are
        // three distinct values, so each machine word is identified
        // unambiguously by equality.
        let mut v: Vec<u8> = Vec::with_capacity(4);
        v.push(0xA5);
        let ptr = v.as_ptr() as usize;
        let len = v.len();
        assert_eq!(
            std::mem::size_of::<Vec<u8>>(),
            24,
            "Vec<u8> is not three machine words"
        );
        // SAFETY: copy the three machine words of the Vec by value; they are
        // only compared to the public pointer/length, never dereferenced.
        let words: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
        let mut ptr_off = None;
        let mut len_off = None;
        for (i, &w) in words.iter().enumerate() {
            if w == ptr {
                ptr_off = Some((i * 8) as u32);
            } else if w == len {
                len_off = Some((i * 8) as u32);
            }
        }
        (
            ptr_off.expect("Vec<u8> data-pointer word not found"),
            len_off.expect("Vec<u8> length word not found"),
        )
    })
}
/// Shared element-access prelude: load the receiver `Value` from its frame
/// slot, guard the pointer-object tag, decompress to its GC body pointer,
/// and read its header type tag. Leaves `x9` = body pointer, `x11` =
/// cage base, `w10` = header type tag. A non-pointer receiver misses to
/// `el_miss`. No safepoint, so the pointer is recomputed from the rooted
/// frame slot every time and never held across a move.
pub(super) fn emit_recv_decompress(
    ops: &mut Assembler,
    cage_base: usize,
    recv_off: u32,
    el_miss: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, recv_off]      // receiver Value
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; orr x15, x15, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x15
        ; b.ne =>el_miss
        ; mov w12, w9                  // low-32 Gc offset (zero-ext, scratch)
    );
    emit_load_u64(ops, 11, cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x9, x11, x12             // x9 = body GcHeader ptr
        ; ldrb w10, [x9]               // w10 = header type tag
    );
}

/// Shared element-access prelude: load the index `Value`, guard it is an
/// int32, and leave the zero-extended `u32` payload in `x12`. A non-int32
/// index misses to `el_miss`.
pub(super) fn emit_idx_int32(ops: &mut Assembler, idx_off: u32, el_miss: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x12, [x19, idx_off]      // index Value
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x12, x15
        ; cmp x14, x15
        ; b.ne =>el_miss               // non-int32 index → stub
        ; and x12, x12, #0xffffffff    // index = zero-extended u32 payload
    );
}

/// Typed-array backing resolution. Assumes the prelude already set `x9` =
/// typed-array body ptr, `x11` = cage base, `x12` = int32 index. Guards
/// not-length-tracking → index in `[0, cached length)` → `Local`
/// (non-shared) backing → local buffer body tag, then dispatches on element
/// kind to `f64_path` / `i32_path` leaving `x13` = buffer data pointer,
/// `x16` = view byte offset, `x17` = live `Vec<u8>` byte length (the
/// detach/resize memory-safety bound). Any miss → `el_miss`.
pub(super) fn emit_ta_backing(
    ops: &mut Assembler,
    ta: &JitTypedArrayLayout,
    el_miss: DynamicLabel,
    f64_path: DynamicLabel,
    i32_path: DynamicLabel,
) {
    let local_buf_type_tag = u32::from(ta.local_buffer_type_tag);
    let local_tag = ta.buffer_local_tag;
    let kind_f64 = ta.kind_float64;
    let kind_i32 = ta.kind_int32;
    let length_tracking_byte = ta.ta_length_tracking_byte;
    let length_byte = ta.ta_length_byte;
    let byte_offset_byte = ta.ta_byte_offset_byte;
    let buffer_disc_byte = ta.buffer_disc_byte;
    let buffer_handle_byte = ta.buffer_handle_byte;
    // The std `Vec` field order is not guaranteed, so the buffer body
    // carries only the Vec base; add the probed data-pointer / length word
    // sub-offsets here.
    let (ptr_word, len_word) = vec_layout_offsets();
    let bytes_ptr_byte = ta.buf_bytes_byte + ptr_word;
    let bytes_len_byte = ta.buf_bytes_byte + len_word;
    let kind_byte = ta.ta_kind_byte;
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [x9, length_tracking_byte]
        ; cbnz w14, =>el_miss          // length-tracking view → stub
        ; ldr x14, [x9, length_byte]   // cached element length
        ; cmp x12, x14
        ; b.hs =>el_miss               // index >= length (unsigned) → stub
        ; ldr w14, [x9, buffer_disc_byte]
        ; cmp w14, local_tag
        ; b.ne =>el_miss               // Shared backing → stub
        ; ldr w15, [x9, buffer_handle_byte]
        ; add x10, x11, x15            // x10 = local buffer GcHeader ptr
        ; ldrb w14, [x10]
        ; cmp w14, local_buf_type_tag
        ; b.ne =>el_miss
        ; ldr x13, [x10, bytes_ptr_byte]   // Vec<u8> data pointer
        ; ldr x17, [x10, bytes_len_byte]   // live Vec<u8> byte length
        ; ldr x16, [x9, byte_offset_byte]  // view byte offset
        ; ldr w14, [x9, kind_byte]         // element kind
        ; cmp w14, kind_f64
        ; b.eq =>f64_path
        ; cmp w14, kind_i32
        ; b.eq =>i32_path
        ; b =>el_miss                  // other kinds → stub
    );
}

/// Typed-array store guard chain: prelude + `Float64Array`/`Int32Array`
/// backing dispatch.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_ta_guard_chain(
    ops: &mut Assembler,
    ta: &JitTypedArrayLayout,
    cage_base: usize,
    recv_off: u32,
    idx_off: u32,
    el_miss: DynamicLabel,
    f64_path: DynamicLabel,
    i32_path: DynamicLabel,
) {
    let ta_type_tag = u32::from(ta.ta_type_tag);
    emit_recv_decompress(ops, cage_base, recv_off, el_miss);
    dynasm!(ops ; .arch aarch64 ; cmp w10, ta_type_tag ; b.ne =>el_miss);
    emit_idx_int32(ops, idx_off, el_miss);
    emit_ta_backing(ops, ta, el_miss, f64_path, i32_path);
}

/// Inline dense `Array` element store for the narrow non-observable case:
/// default prototype, no exotic sidecar, intact array-index accessor
/// protector, int32 index inside both logical `length` and the dense
/// elements vector. Misses route to the existing typed-array/runtime path.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_array_store(
    ops: &mut Assembler,
    layout: &JitTypedArrayLayout,
    cage_base: usize,
    recv_off: u32,
    idx_off: u32,
    src_off: u32,
    el_miss: DynamicLabel,
    el_done: DynamicLabel,
    threw: DynamicLabel,
    recv_reg: u16,
    src_reg: u16,
) {
    let array_tag = u32::from(layout.array_type_tag);
    let (ptr_word, len_word) = vec_layout_offsets();
    let arr_ptr_byte = layout.array_elements_byte + ptr_word;
    let arr_len_byte = layout.array_elements_byte + len_word;
    let length_byte = layout.array_length_byte;
    let exotic_byte = layout.array_exotic_byte;

    emit_recv_decompress(ops, cage_base, recv_off, el_miss);
    emit_idx_int32(ops, idx_off, el_miss);
    dynasm!(ops
        ; .arch aarch64
        ; cmp w10, array_tag
        ; b.ne =>el_miss
        ; ldr x14, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
        ; ldrb w14, [x14]
        ; cbnz w14, =>el_miss              // indexed proto/accessor hazard
        ; ldr x14, [x9, exotic_byte]
        ; cbnz x14, =>el_miss              // custom proto/accessor/flags/source
        ; ldr x17, [x9, arr_len_byte]      // elements Vec length
        ; cmp x12, x17
        ; b.hs =>el_miss
        ; ldr x16, [x9, length_byte]       // logical length
        ; cmp x12, x16
        ; b.hs =>el_miss                   // would need length update
        ; ldr x13, [x9, arr_ptr_byte]      // elements Vec data pointer
        ; lsl x14, x12, #3
        ; add x14, x13, x14                // element address
        ; ldr x9, [x19, src_off]
        ; str x9, [x14]
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>el_done                   // primitive value, no barrier
        ; mov x0, x20
        ; movz x1, recv_reg as u32
        ; movz x2, src_reg as u32
    );
    emit_call_stub(ops, jit_write_barrier_stub as *const () as usize, threw);
    dynasm!(ops ; .arch aarch64 ; b =>el_done);
}

/// Unified inline `LoadElement`: one receiver decompress + one index guard,
/// then a header-type-tag dispatch to the dense-`Array` path (raw `Value`
/// with a hole-sentinel guard) or the typed-array path (`Float64Array` /
/// `Int32Array`, box/unbox). Anything else — other kinds, a hole, an
/// out-of-bounds or non-int32 index, a non-array/typed-array receiver —
/// misses to `el_miss` (the runtime stub, which owns the spec-correct
/// prototype / sparse / accessor / string semantics). No safepoint.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_element_load(
    ops: &mut Assembler,
    layout: &JitTypedArrayLayout,
    cage_base: usize,
    recv_off: u32,
    idx_off: u32,
    dst_off: u32,
    el_miss: DynamicLabel,
    el_done: DynamicLabel,
) {
    let array_tag = u32::from(layout.array_type_tag);
    let ta_tag = u32::from(layout.ta_type_tag);
    let (ptr_word, len_word) = vec_layout_offsets();
    let arr_ptr_byte = layout.array_elements_byte + ptr_word;
    let arr_len_byte = layout.array_elements_byte + len_word;
    let hole_bits = VALUE_HOLE;
    let array_path = ops.new_dynamic_label();
    let ta_path = ops.new_dynamic_label();
    let f64_path = ops.new_dynamic_label();
    let i32_path = ops.new_dynamic_label();

    emit_recv_decompress(ops, cage_base, recv_off, el_miss);
    emit_idx_int32(ops, idx_off, el_miss);
    dynasm!(ops
        ; .arch aarch64
        ; cmp w10, array_tag
        ; b.eq =>array_path
        ; cmp w10, ta_tag
        ; b.eq =>ta_path
        ; b =>el_miss
    );

    // Dense Array: element is a raw 8-byte Value. Bounds-check against the
    // live `elements` Vec length, then a hole sentinel → stub (the stub
    // walks the prototype / sparse / accessor, all spec-owned there).
    dynasm!(ops
        ; .arch aarch64
        ; =>array_path
        ; ldr x17, [x9, arr_len_byte]      // elements Vec length
        ; cmp x12, x17
        ; b.hs =>el_miss                   // index >= length → stub
        ; ldr x13, [x9, arr_ptr_byte]      // elements Vec data pointer
        ; lsl x14, x12, #3                 // index * sizeof(Value)
        ; add x14, x13, x14                // element address
        ; ldr x13, [x14]                   // the Value
    );
    emit_load_u64(ops, 15, hole_bits);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x13, x15
        ; b.eq =>el_miss                   // hole → stub
        ; str x13, [x19, dst_off]
        ; b =>el_done
    );

    // Typed array: resolve backing, then per-kind load + box.
    dynasm!(ops ; .arch aarch64 ; =>ta_path);
    emit_ta_backing(ops, layout, el_miss, f64_path, i32_path);
    dynasm!(ops
        ; .arch aarch64
        ; =>f64_path
        ; lsl x14, x12, #3                 // index * 8
        ; add x14, x14, x16                // + byte_offset
        ; add x15, x14, #8                 // + element size (bound)
        ; cmp x15, x17
        ; b.hi =>el_miss
        ; add x14, x13, x14                // element address
        ; ldr d0, [x14]
    );
    emit_box_double(ops, 0, 15);
    dynasm!(ops
        ; .arch aarch64
        ; str x15, [x19, dst_off]
        ; b =>el_done
        ; =>i32_path
        ; lsl x14, x12, #2                 // index * 4
        ; add x14, x14, x16                // + byte_offset
        ; add x15, x14, #4                 // + element size (bound)
        ; cmp x15, x17
        ; b.hi =>el_miss
        ; add x14, x13, x14                // element address
        ; ldr w13, [x14]                   // signed int32 (low-32)
    );
    box_int32!(ops, 13, 15);
    dynasm!(ops
        ; .arch aarch64
        ; str x13, [x19, dst_off]
        ; b =>el_done
    );
}
