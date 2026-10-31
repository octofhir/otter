//! x86-64 code-unit reads of flat strings, shared by every tier.
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
//! - `crate::graph::x86_64` lowers `charCodeAt` through [`emit_code_unit`].

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;

/// `Rd(destination)` = the code unit of the string `Rq(receiver)` at the
/// Int32 index `Rd(index)`, unsigned-compared against its length. The
/// receiver must already be known to be a string. Anything but a sequential
/// string or a slice of one jumps to `miss` before `Rd(destination)`
/// changes. Clobbers `Rq(base)`, r10 and r11.
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
    dynasm!(ops ; .arch x64
        ; cmp Rd(index), [Rq(receiver) + p.string_len_byte as i32] ; jae =>miss
        ; mov Rq(base), Rq(receiver)
        ; mov r11d, Rd(index)
        ; movzx r10d, BYTE [Rq(receiver) + p.string_repr_byte as i32]
        ; cmp r10d, i32::from(p.sliced_tag) ; jne =>flat
        // A slice views its flat parent from a start offset.
        ; add r11d, [Rq(receiver) + p.sliced_start_byte as i32]
        ; mov r10d, [Rq(receiver) + p.sliced_parent_byte as i32]
        ; mov Rq(base), QWORD 0xffff_ffff_0000_0000_u64 as i64
        ; and Rq(base), Rq(receiver) ; add Rq(base), r10
        ; movzx r10d, BYTE [Rq(base) + p.string_repr_byte as i32]
        ; =>flat
        ; cmp r10d, i32::from(p.seq_latin1_tag) ; je =>seq_latin1
        ; cmp r10d, i32::from(p.seq_flat_tag) ; jne =>miss
        ; movzx Rd(destination), WORD [Rq(base) + r11 * 2 + p.string_body_size as i32]
        ; jmp =>done
        ; =>seq_latin1
        ; movzx Rd(destination), BYTE [Rq(base) + r11 + p.string_body_size as i32]
        ; =>done);
}

/// `Rq(destination)` = `Rq(receiver)[Rq(key)]` for a string receiver and a
/// nonnegative Int32 key below its length whose code unit is Latin-1: the
/// shared one-unit string (`r15` holds the live `JitCtx`). Any other
/// receiver, key or unit jumps to `miss` with only the temporaries, r10 and
/// r11 changed. `destination` is written last, so it may alias any operand.
pub(crate) fn emit_unit_string_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    [receiver, key]: [u8; 2],
    [index, base]: [u8; 2],
    destination: u8,
    miss: DynamicLabel,
) {
    debug_assert!(![receiver, key].contains(&index) && ![receiver, key].contains(&base));
    dynasm!(ops ; .arch x64
        ; mov r10, QWORD otter_vm::value::tag::NOT_CELL_MASK as i64
        ; test Rq(receiver), r10 ; jnz =>miss
        ; test Rq(receiver), Rq(receiver) ; jz =>miss
        ; cmp BYTE [Rq(receiver)], view.string_layout.string_type_tag as i8 ; jne =>miss
        ; mov r10, Rq(key) ; sar r10, 49 ; cmp r10, -1 ; jne =>miss
        ; mov Rd(index), Rd(key)
    );
    // A negative key compares above every length.
    emit_code_unit(ops, view, [receiver, index, base], index, miss);
    dynasm!(ops ; .arch x64
        ; cmp Rd(index), 0xff ; ja =>miss
        ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
        ; mov r10, [r10 + crate::entry::VM_THREAD_UNIT_STRINGS_OFFSET as i32]
        ; test r10, r10 ; jz =>miss
        ; mov Rq(destination), [r10 + Rq(index) * 8]
    );
}
