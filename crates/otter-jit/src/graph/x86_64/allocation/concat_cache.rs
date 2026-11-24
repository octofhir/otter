//! Read and publish ephemeral concat results on x86-64.
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
use crate::entry::{
    THREAD_OFFSET, VM_THREAD_CONCAT_CACHE_OFFSET, VM_THREAD_MARKING_FLAG_CELL_OFFSET,
};
use crate::x86_64::allocation::emit_value;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
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
    dynasm!(ops ; .arch x64
        ; cmp Rq(r.buffer),[Rq(r.scratch)+CACHE_KEYS_BYTE as i32] ; jne =>miss
        ; cmp Rq(r.end),[Rq(r.scratch)+(CACHE_KEYS_BYTE + 8) as i32] ; jne =>miss
        ; mov r10,[Rq(r.scratch)+CACHE_RESULT_BYTE as i32]
        ; test r10,r10 ; jz =>miss
        ; mov Rq(destination),r10 ; jmp =>done
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
    dynasm!(ops ; .arch x64
        ; mov [Rq(r.scratch)+CACHE_KEYS_BYTE as i32],Rq(r.buffer)
        ; mov [Rq(r.scratch)+(CACHE_KEYS_BYTE + 8) as i32],Rq(r.end)
        ; mov [Rq(r.scratch)+CACHE_RESULT_BYTE as i32],Rq(result)
        ; =>skip);
}

fn emit_line(
    ops: &mut Assembler,
    [left, right]: [AllocationValue; 2],
    r: LabRegisters,
    skip: DynamicLabel,
    publishing: bool,
) {
    dynasm!(ops ; .arch x64
        ; mov r10,[r15+THREAD_OFFSET as i32]
        ; mov Rq(r.scratch),[r10+VM_THREAD_CONCAT_CACHE_OFFSET as i32]
        ; test Rq(r.scratch),Rq(r.scratch) ; jz =>skip);
    if publishing {
        dynasm!(ops ; .arch x64
            ; mov r10,[r10+VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
            ; cmp BYTE [r10],0 ; jne =>skip);
    }
    emit_value(ops, r.buffer, left);
    emit_value(ops, r.end, right);
    dynasm!(ops ; .arch x64
        ; mov r11,Rq(r.end) ; ror r11,CACHE_RIGHT_ROTATION as i8
        ; xor r11,Rq(r.buffer) ; shr r11,CACHE_INDEX_START as i8
        ; and r11d,((1 << CACHE_INDEX_BITS) - 1) as i32
        ; shl r11,CACHE_ENTRY_SHIFT as i8
        ; add Rq(r.scratch),r11);
}
