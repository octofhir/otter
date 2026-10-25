//! The `ToBoolean` routine optimized code shares for a value that is not one
//! of the immediates it tests inline.
//!
//! # Contents
//! - [`emit_routine`] — the truthiness of a double, a string or another cell,
//!   and of every remaining immediate through the VM's leaf predicate.
//! - [`TRUTHY_VALUE`] — the register carrying the value in and the answer out.
//!
//! # Invariants
//! - The value arrives in [`TRUTHY_VALUE`], which leaves holding one for a
//!   truthy value and zero for a falsy one. The routine writes `x16` and
//!   `x17` only: the leaf call saves and restores every allocatable register
//!   and `x30` around itself.
//! - A double is falsy at ±0 and NaN, a string at length zero; every other
//!   cell is an object, truthy, except a native function (the only possible
//!   `[[IsHTMLDDA]]`) and a BigInt, which the leaf decides.
//! - Nothing allocates or collects.
//!
//! # See also
//! - [`crate::routines`] — where the routine is placed and shared.

use dynasmrt::{DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;
use otter_vm::value::tag;

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{THREAD_OFFSET, VM_THREAD_GC_HEAP_OFFSET};
use crate::routines::RoutineOffsets;
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};

/// The value's register on entry and the answer's on return.
pub(crate) const TRUTHY_VALUE: u8 = 17;

/// General registers the leaf call may write that optimized code allocates.
const SAVED_GENERAL: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
/// Floating registers the leaf call may write that optimized code allocates.
const SAVED_FLOATING: [u8; 23] = [
    0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
];

/// Emit the routine, returning its entry offsets.
pub(crate) fn emit_routine(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
) -> RoutineOffsets {
    let entry = ops.offset().0;
    let cell = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let string_tag = u32::from(view.string_layout.string_type_tag);
    let length = view.string_layout.string_len_byte;
    dynasm!(ops ; .arch aarch64
        ; tst X(TRUTHY_VALUE), #0xfffe_0000_0000_0007
        ; b.eq =>cell
        // A boxed double carries number tag bits; other immediates do not.
        ; lsr x16, X(TRUTHY_VALUE), #49
        ; cbz x16, =>slow
    );
    emit_load_u64(ops, 16, tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops ; .arch aarch64
        ; sub x16, X(TRUTHY_VALUE), x16
        ; lsl x17, x16, #1
        ; cbz x17, =>falsy
    );
    // NaN boxes as the one canonical pattern.
    emit_load_u64(ops, 17, tag::CANONICAL_NAN);
    dynasm!(ops ; .arch aarch64
        ; cmp x16, x17
        ; b.eq =>falsy
        ; =>truthy
        ; movz X(TRUTHY_VALUE), 1
        ; ret
        ; =>falsy
        ; mov X(TRUTHY_VALUE), xzr
        ; ret
        ; =>cell
        ; cbz X(TRUTHY_VALUE), =>slow
        ; ldrb w16, [X(TRUTHY_VALUE)]
        ; cmp w16, string_tag
        ; b.ne =>object
        ; ldr w16, [X(TRUTHY_VALUE), length]
        ; cbz w16, =>falsy
        ; b =>truthy
        ; =>object
        ; cmp w16, u32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG)
        ; b.eq =>slow
        ; cmp w16, u32::from(otter_vm::bigint::BIG_INT_BODY_TYPE_TAG)
        ; b.eq =>slow
        ; b =>truthy
        ; =>slow
    );
    let bytes = ((SAVED_GENERAL.len() + 1 + SAVED_FLOATING.len()) as u32 * 8).next_multiple_of(16);
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, bytes);
    for (index, register) in SAVED_GENERAL.iter().copied().chain([30]).enumerate() {
        let offset = index as u32 * 8;
        dynasm!(ops ; .arch aarch64 ; str X(register), [sp, offset]);
    }
    let floating_base = (SAVED_GENERAL.len() as u32 + 1) * 8;
    for (index, register) in SAVED_FLOATING.iter().copied().enumerate() {
        let offset = floating_base + index as u32 * 8;
        dynasm!(ops ; .arch aarch64 ; str D(register), [sp, offset]);
    }
    dynasm!(ops ; .arch aarch64
        ; mov x1, X(TRUTHY_VALUE)
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
        RelocationTarget::runtime_stub(abi::STUB_TO_BOOLEAN_LEAF),
    );
    dynasm!(ops ; .arch aarch64
        ; blr x16
        ; cmp x0, tag::VALUE_TRUE as u32
        ; cset X(TRUTHY_VALUE), eq
    );
    for (index, register) in SAVED_FLOATING.iter().copied().enumerate() {
        let offset = floating_base + index as u32 * 8;
        dynasm!(ops ; .arch aarch64 ; ldr D(register), [sp, offset]);
    }
    for (index, register) in SAVED_GENERAL.iter().copied().chain([30]).enumerate() {
        let offset = index as u32 * 8;
        dynasm!(ops ; .arch aarch64 ; ldr X(register), [sp, offset]);
    }
    dynasm!(ops ; .arch aarch64 ; add sp, sp, bytes ; ret);
    RoutineOffsets {
        entry,
        second: entry,
    }
}
