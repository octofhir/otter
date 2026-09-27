//! Direct reads of unmaterialized native argument windows.
//!
//! # Contents
//! - `emit` proves the cache/window and returns a tagged length or actual value.
//!
//! # Invariants
//! - r11 contains the published NativeFrame; scratch is r8, r9, r10 and r11.
//! - The result is r8; all guards precede its publication.
//! - A materialized object, non-Int32 key or out-of-range index uses the cold path.
//! - No moving pointer or argument-window address survives the probe.
//!
//! # See also
//! - `otter_vm::arguments_access` owns complete property semantics.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{Value, native_abi as abi};

pub(crate) fn emit(ops: &mut Assembler, key: Option<u8>, miss: DynamicLabel) {
    if let Some(key) = key {
        dynasm!(ops ; .arch x64 ; mov r9, Rq(key));
    }
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [r11 + abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET as i32], 0
        ; jne =>miss
        ; test BYTE [r11 + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], abi::NativeFrameFlags::INCOMING_ARGUMENTS as i8
        ; jz =>miss
        ; mov r8d, [r11 + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
    );
    if key.is_some() {
        let tag = (Value::number_i32(0).to_bits() >> 48) as i32;
        dynasm!(ops
            ; .arch x64
            ; mov r10, r9
            ; shr r10, 48
            ; cmp r10d, tag
            ; jne =>miss
            ; cmp r9d, r8d
            ; jae =>miss
            ; mov r9d, r9d
            ; movzx r10d, WORD [r11 + crate::entry::NATIVE_FRAME_REGISTER_COUNT_OFFSET as i32]
            ; mov r11, [r11 + crate::entry::NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
            ; lea r11, [r11 + r10 * 8]
            ; mov r8, [r11 + r9 * 8]
        );
    } else {
        let tag = Value::number_i32(0).to_bits() as i64;
        dynasm!(ops
            ; .arch x64
            ; test r8d, r8d
            ; js =>miss
            ; mov r10, QWORD tag
            ; or r8, r10
        );
    }
}
