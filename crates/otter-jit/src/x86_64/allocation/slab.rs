//! x86-64 generated growth of an Array's tagged element storage.
//!
//! # Contents
//! - [`emit_grow_tagged_slab`] carves a young tagged element slab and copies
//!   the live prefix of an Array's current storage into it.
//!
//! # Invariants
//! - The capacity bound and the buffer limit miss before any heap write.
//! - Header, capacity, length, kind, dirty interval and the copied words are
//!   written before the bump cursor publishes the cell; the caller installs
//!   it and fills the slot after the prefix before any collection point.
//! - The dirty interval covers the copied words and the slot after them, as
//!   the VM's own growth and append record them.
//!
//! # See also
//! - `crate::arm64::allocation::slab` for the same carve on AArch64.
//! - `otter_vm::jit::JitArrayStorage` for the slab and Array words.

use super::{LabRegisters, emit_bump_probe, emit_publish};
use crate::x86_64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::jit::JitArrayStorage;

/// Carve a young tagged slab of `regs.size` words and copy the `length`
/// live words of the Array `receiver`'s storage into it. Leaves the cell in
/// `regs.candidate` and its element base in `regs.size`; misses to `slow`
/// with every input intact for a capacity past a young cell or a full
/// buffer. Clobbers the other `regs`, `r10` and `r11`.
pub(crate) fn emit_grow_tagged_slab(
    ops: &mut Assembler,
    context: u8,
    storage: JitArrayStorage,
    [receiver, length]: [u8; 2],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    let LabRegisters {
        candidate,
        scratch,
        size,
        ..
    } = regs;
    let data_byte = storage.slab_header_from_base.unsigned_abs() as i32;
    let numeric = storage.numeric;
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; cmp Rd(size), storage.max_young_capacity as i32 ; ja =>slow
        ; lea Rq(size), [Rq(size) * 8 + data_byte]);
    emit_bump_probe(ops, context, regs, slow);
    emit_load_u64(ops, scratch, storage.young_slab_header_word);
    dynasm!(ops ; .arch x64
        ; mov r10, Rq(size) ; shl r10, 32 ; or Rq(scratch), r10
        ; mov [Rq(candidate)], Rq(scratch)
        ; lea Rq(scratch), [Rq(size) - data_byte] ; shr Rq(scratch), 3
        ; lea Rq(size), [Rq(candidate) + data_byte]
        ; mov [Rq(size) + numeric.capacity_byte], Rd(scratch)
        ; mov [Rq(size) + storage.slab_length_from_base], Rd(length));
    // The hole count, the kind byte and its reserved padding are one word.
    emit_load_u64(ops, scratch, u64::from(storage.tagged_kind) << 32);
    dynasm!(ops ; .arch x64
        ; mov [Rq(size) + numeric.hole_count_byte], Rq(scratch)
        ; mov DWORD [Rq(size) + storage.slab_dirty_start_from_base], 0
        ; lea Rd(scratch), [Rq(length) + 1]
        ; mov [Rq(size) + storage.slab_dirty_end_from_base], Rd(scratch)
        ; test Rd(length), Rd(length) ; jz =>copied
        ; mov Rq(scratch), [Rq(receiver) + storage.base_byte as i32]
        ; xor r11d, r11d
        ; =>copy
        ; mov r10, [Rq(scratch) + r11 * 8]
        ; mov [Rq(size) + r11 * 8], r10
        ; add r11d, 1
        ; cmp r11d, Rd(length) ; jb =>copy
        ; =>copied);
    emit_publish(ops, regs);
}
