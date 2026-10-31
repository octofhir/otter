//! AArch64 code-unit reads of flat strings, shared by every tier.
//!
//! # Contents
//! - [`emit_code_unit`] reads the code unit of a known string at an Int32
//!   index: a sequential Latin-1 or UTF-16 string, or a slice of one.
//! - [`emit_unit_string_load`] is `string[index]` for an element access: the
//!   isolate's shared one-unit string of a Latin-1 unit.
//!
//! # Invariants
//! - Every guard precedes the first write of the destination; a miss leaves
//!   the receiver, the key and the destination as they were.
//! - Ropes, external strings and units above `0xff` miss: their completion
//!   flattens or allocates in the runtime.
//! - The shared unit strings are read through `VmThread::unit_strings`, an
//!   old-space slab whose entries the collector rewrites in place.
//!
//! # See also
//! - `otter_vm::string::JsString::unit_strings_addr` owns the table.
//! - `crate::graph::arm64` lowers `charCodeAt` through [`emit_code_unit`].

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;

/// `W(destination)` = the code unit of the string `X(receiver)` at the Int32
/// index `W(index)`, unsigned-compared against its length. The receiver must
/// already be known to be a string. Anything but a sequential string or a
/// slice of one branches to `miss` before `W(destination)` changes. Clobbers
/// `X(base)`, x16 and x17.
pub(crate) fn emit_code_unit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    [receiver, index, base]: [u8; 3],
    destination: u8,
    miss: DynamicLabel,
) {
    let p = view.string_layout;
    let flat = ops.new_dynamic_label();
    let seq_latin1 = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldr w17, [X(receiver), p.string_len_byte]
        ; cmp W(index), w17
        ; b.hs =>miss
        ; mov X(base), X(receiver)
        ; mov w17, W(index)
        ; ldrb w16, [X(receiver), p.string_repr_byte]
        ; cmp w16, u32::from(p.sliced_tag)
        ; b.ne =>flat
        // A slice views its flat parent from a start offset.
        ; ldr w16, [X(receiver), p.sliced_parent_byte]
        ; ldr w17, [X(receiver), p.sliced_start_byte]
        ; add w17, w17, W(index)
        ; and XSP(base), X(receiver), #0xffff_ffff_0000_0000
        ; add X(base), X(base), x16
        ; ldrb w16, [X(base), p.string_repr_byte]
        ; =>flat
        ; cmp w16, u32::from(p.seq_latin1_tag) ; b.eq =>seq_latin1
        ; cmp w16, u32::from(p.seq_flat_tag) ; b.ne =>miss
        ; add x16, XSP(base), p.string_body_size
        ; ldrh W(destination), [x16, w17, uxtw #1]
        ; b =>done
        ; =>seq_latin1
        ; add x16, XSP(base), p.string_body_size
        ; ldrb W(destination), [x16, w17, uxtw]
        ; =>done
    );
}

/// `X(destination)` = `X(receiver)[X(key)]` for a string receiver and a
/// nonnegative Int32 key below its length whose code unit is Latin-1: the
/// shared one-unit string (`x20` holds the live `JitCtx`). Any other
/// receiver, key or unit branches to `miss` with only the temporaries,
/// x16 and x17 changed. `destination` is written last, so it may alias any
/// operand.
pub(crate) fn emit_unit_string_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    [receiver, key]: [u8; 2],
    [index, base]: [u8; 2],
    destination: u8,
    miss: DynamicLabel,
) {
    debug_assert!(![receiver, key].contains(&index) && ![receiver, key].contains(&base));
    dynasm!(ops ; .arch aarch64
        ; tst X(receiver), #0xfffe_0000_0000_0007 ; b.ne =>miss ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)]
        ; cmp w16, u32::from(view.string_layout.string_type_tag) ; b.ne =>miss
        ; asr x16, X(key), #49 ; cmn x16, #1 ; b.ne =>miss
        ; mov W(index), W(key)
    );
    // A negative key compares above every length.
    emit_code_unit(ops, view, [receiver, index, base], index, miss);
    dynasm!(ops ; .arch aarch64
        ; cmp WSP(index), #0xff ; b.hi =>miss
        ; ldr x16, [x20, crate::entry::THREAD_OFFSET]
        ; ldr x16, [x16, crate::entry::VM_THREAD_UNIT_STRINGS_OFFSET]
        ; cbz x16, =>miss
        ; ldr X(destination), [x16, X(index), lsl #3]
    );
}
