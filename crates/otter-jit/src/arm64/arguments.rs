//! Direct reads of unmaterialized native argument windows.
//!
//! # Contents
//! - `emit` proves the frame cache/window and returns a tagged length or value.
//!
//! # Invariants
//! - A cache hit means observable object semantics and branches to the cold path.
//! - Only exact nonnegative Int32 indices inside the actual window are loaded.
//! - Every frame names its actual span explicitly.
//! - No raw window address survives this probe, a call or a backedge.
//! - Scratch registers are x9, x10, x15, x16 and x17; the result is x16.
//!
//! # See also
//! - `otter_vm::arguments_access` owns complete property semantics.

use crate::entry::NUMBER_TAG_HI16;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

pub(crate) fn emit(ops: &mut Assembler, frame: u8, key: Option<u8>, miss: DynamicLabel) {
    if let Some(key) = key {
        dynasm!(ops ; .arch aarch64 ; mov x10, X(key));
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov x15, X(frame)
        ; ldr w16, [x15, abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET]
        ; cbnz w16, =>miss
        ; ldr w16, [x15, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
    );
    if key.is_some() {
        dynasm!(ops
            ; .arch aarch64
            ; lsr x17, x10, 48
            ; movz x9, NUMBER_TAG_HI16
            ; cmp x17, x9
            ; b.ne =>miss
            ; cmp w10, w16
            ; b.hs =>miss
            ; mov w10, w10
            ; ldr x17, [x15, abi::NATIVE_FRAME_ACTUALS_OFFSET]
            ; ldr x16, [x17, x10, lsl #3]
        );
    } else {
        dynasm!(ops
            ; .arch aarch64
            ; tbnz w16, 31, =>miss
            ; movz x17, NUMBER_TAG_HI16, lsl #48
            ; orr x16, x16, x17
        );
    }
}
