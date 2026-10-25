//! The x86-64 `ToBoolean` routine optimized code shares for a value that is
//! not one of the immediates it tests inline.
//!
//! # Contents
//! - [`emit_routine`] — the truthiness of a double, a string or another cell,
//!   and of every remaining immediate through the VM's leaf predicate.
//! - [`TRUTHY_VALUE`] — the register carrying the value in and the answer out.
//!
//! # Invariants
//! - The value arrives in [`TRUTHY_VALUE`], which leaves holding one for a
//!   truthy value and zero for a falsy one. The routine writes `r10` and
//!   `r11` only (a call to it loads its address into `r11`): the leaf call saves and restores every allocatable register
//!   around itself, on a stack it realigns.
//! - A double is falsy at ±0 and NaN, a string at length zero; every other
//!   cell is an object, truthy, except a native function (the only possible
//!   `[[IsHTMLDDA]]`) and a BigInt, which the leaf decides.
//! - Nothing allocates or collects.
//!
//! # See also
//! - [`crate::routines`] — where the routine is placed and shared.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi as abi;
use otter_vm::value::tag;

use crate::artifact::relocation::RelocationCapture;
use crate::routines::RoutineOffsets;
use crate::x86_64::values::{emit_load_runtime_stub, emit_load_u64};

/// The value's register on entry and the answer's on return.
pub(crate) const TRUTHY_VALUE: u8 = 10;

/// General registers the leaf call may write that optimized code allocates.
const SAVED_GENERAL: [u8; 9] = [0, 1, 2, 3, 6, 7, 8, 9, 12];
/// Floating registers optimized code allocates.
const SAVED_FLOATING: u8 = 15;

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
    let string_tag = view.string_layout.string_type_tag as i8;
    let length = view.string_layout.string_len_byte as i32;
    emit_load_u64(ops, 11, tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(TRUTHY_VALUE), r11 ; jz =>cell
        // A boxed double carries number tag bits; other immediates do not.
        ; mov r11, Rq(TRUTHY_VALUE) ; shr r11, 49 ; jz =>slow
    );
    emit_load_u64(ops, 11, tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops ; .arch x64
        ; sub Rq(TRUTHY_VALUE), r11
        ; mov r11, Rq(TRUTHY_VALUE) ; shl r11, 1 ; jz =>falsy
    );
    // NaN boxes as the one canonical pattern.
    emit_load_u64(ops, 11, tag::CANONICAL_NAN);
    dynasm!(ops ; .arch x64
        ; cmp Rq(TRUTHY_VALUE), r11 ; je =>falsy
        ; =>truthy
        ; mov Rd(TRUTHY_VALUE), 1
        ; ret
        ; =>falsy
        ; xor Rd(TRUTHY_VALUE), Rd(TRUTHY_VALUE)
        ; ret
        ; =>cell
        ; test Rq(TRUTHY_VALUE), Rq(TRUTHY_VALUE) ; jz =>slow
        ; cmp BYTE [Rq(TRUTHY_VALUE)], string_tag ; jne =>object
        ; cmp DWORD [Rq(TRUTHY_VALUE) + length], 0 ; je =>falsy ; jmp =>truthy
        ; =>object
        ; cmp BYTE [Rq(TRUTHY_VALUE)], otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG as i8
        ; je =>slow
        ; cmp BYTE [Rq(TRUTHY_VALUE)], otter_vm::bigint::BIG_INT_BODY_TYPE_TAG as i8
        ; je =>slow
        ; jmp =>truthy
        ; =>slow
    );
    // The caller's stack is aligned before its call: the return address and
    // an odd number of words realign it for the leaf.
    let words = SAVED_GENERAL.len() as i32 + i32::from(SAVED_FLOATING);
    let bytes = (words + 1 - words % 2) * 8;
    dynasm!(ops ; .arch x64 ; sub rsp, bytes);
    for (index, register) in SAVED_GENERAL.iter().copied().enumerate() {
        let offset = index as i32 * 8;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(register));
    }
    let floating_base = SAVED_GENERAL.len() as i32 * 8;
    for register in 0..SAVED_FLOATING {
        let offset = floating_base + i32::from(register) * 8;
        dynasm!(ops ; .arch x64 ; movsd [rsp + offset], Rx(register));
    }
    dynasm!(ops ; .arch x64
        ; mov rsi, Rq(TRUTHY_VALUE)
        ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
        ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
        abi::STUB_TO_BOOLEAN_LEAF,
    );
    crate::x86_64::call_abi::emit_runtime_call(ops, abi::STUB_TO_BOOLEAN_LEAF);
    dynasm!(ops ; .arch x64
        ; xor Rd(TRUTHY_VALUE), Rd(TRUTHY_VALUE)
        ; cmp rax, DWORD tag::VALUE_TRUE as i32
        ; sete Rb(TRUTHY_VALUE)
    );
    for register in 0..SAVED_FLOATING {
        let offset = floating_base + i32::from(register) * 8;
        dynasm!(ops ; .arch x64 ; movsd Rx(register), [rsp + offset]);
    }
    for (index, register) in SAVED_GENERAL.iter().copied().enumerate() {
        let offset = index as i32 * 8;
        dynasm!(ops ; .arch x64 ; mov Rq(register), [rsp + offset]);
    }
    dynasm!(ops ; .arch x64 ; add rsp, bytes ; ret);
    RoutineOffsets {
        entry,
        second: entry,
    }
}
