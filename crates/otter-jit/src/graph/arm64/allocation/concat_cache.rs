//! Read and publish ephemeral concat results on AArch64.
//!
//! # Contents
//! - `emit_hit` accepts an exact ordered pair.
//! - `emit_record` publishes a fully initialized result after its barriers.
//!
//! # Invariants
//! The heap clears this table before moving or reclaiming cells. No helper
//! call or safepoint is allowed between selecting a line and using it.
//! The table remains empty during marking.
//!
//! # See also
//! - `otter_vm::string::concat_cache` owns the key and entry layout.

use crate::allocation::{AllocationValue, LabRegisters};
use crate::arm64::allocation::emit_value;
use crate::entry::{
    THREAD_OFFSET, VM_THREAD_CONCAT_CACHE_OFFSET, VM_THREAD_MARKING_FLAG_CELL_OFFSET,
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::string::concat_cache::*;

pub(super) fn emit_hit(
    ops: &mut Assembler,
    values: [AllocationValue; 2],
    r: LabRegisters,
    destination: u8,
    done: DynamicLabel,
) {
    let miss = ops.new_dynamic_label();
    emit_line(ops, values, r, miss, false);
    dynasm!(ops ; .arch aarch64
        ; ldp x16,x17,[X(r.scratch),CACHE_KEYS_BYTE as i32]
        ; cmp X(r.buffer),x16 ; b.ne =>miss
        ; cmp X(r.end),x17 ; b.ne =>miss
        ; ldr x16,[X(r.scratch),CACHE_RESULT_BYTE]
        ; cbz x16,=>miss
        ; mov X(destination),x16 ; b =>done
        ; =>miss);
}

pub(super) fn emit_record(
    ops: &mut Assembler,
    values: [AllocationValue; 2],
    r: LabRegisters,
    result: u8,
) {
    let skip = ops.new_dynamic_label();
    emit_line(ops, values, r, skip, true);
    dynasm!(ops ; .arch aarch64
        ; stp X(r.buffer),X(r.end),[X(r.scratch),CACHE_KEYS_BYTE as i32]
        ; str X(result),[X(r.scratch),CACHE_RESULT_BYTE]
        ; =>skip);
}

fn emit_line(
    ops: &mut Assembler,
    [left, right]: [AllocationValue; 2],
    r: LabRegisters,
    skip: DynamicLabel,
    publishing: bool,
) {
    dynasm!(ops ; .arch aarch64
        ; ldr x16,[x20,THREAD_OFFSET]
        ; ldr X(r.scratch),[x16,VM_THREAD_CONCAT_CACHE_OFFSET]
        ; cbz X(r.scratch),=>skip);
    if publishing {
        dynasm!(ops ; .arch aarch64
            ; ldr x16,[x16,VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb w16,[x16] ; cbnz w16,=>skip);
    }
    emit_value(ops, r.buffer, left);
    emit_value(ops, r.end, right);
    dynasm!(ops ; .arch aarch64
        ; eor x17,X(r.buffer),X(r.end),ror CACHE_RIGHT_ROTATION
        ; ubfx x17,x17,CACHE_INDEX_START,CACHE_INDEX_BITS
        ; add X(r.scratch),X(r.scratch),x17,lsl CACHE_ENTRY_SHIFT);
}
