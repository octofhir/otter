//! Generated growth of an Array's tagged element storage.
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
//! - `otter_vm::jit::JitArrayStorage` for the slab and Array words.
//! - `otter_vm::array::reserve_dense_capacity` for the runtime's growth.

use super::{LabRegisters, emit_bump_probe, emit_publish};
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::JitArrayStorage;

/// Carve a young tagged slab of `W(regs.size)` words and copy the `W(length)`
/// live words of the Array `X(receiver)`'s storage into it. Leaves the cell
/// in `X(regs.candidate)` and its element base in `X(regs.size)`; misses to
/// `slow` with every input intact for a capacity past a young cell or a full
/// buffer. Clobbers the other `regs`, `x16` and `x17`.
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
    let data_byte = storage.slab_header_from_base.unsigned_abs();
    let numeric = storage.numeric;
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    emit_load_u64(ops, 16, u64::from(storage.max_young_capacity));
    dynasm!(ops
        ; .arch aarch64
        ; cmp W(size), w16
        ; b.hi =>slow
        ; lsl X(size), X(size), #3
        ; add XSP(size), XSP(size), data_byte
    );
    emit_bump_probe(ops, context, regs, slow);
    emit_load_u64(ops, scratch, storage.young_slab_header_word);
    dynasm!(ops
        ; .arch aarch64
        ; orr X(scratch), X(scratch), X(size), lsl 32
        ; str X(scratch), [X(candidate)]
        ; sub XSP(scratch), XSP(size), data_byte
        ; lsr X(scratch), X(scratch), #3
        ; add XSP(size), XSP(candidate), data_byte
        ; stur W(scratch), [X(size), numeric.capacity_byte]
        ; stur W(length), [X(size), storage.slab_length_from_base]
    );
    // The hole count, the kind byte and its reserved padding are one word.
    emit_load_u64(ops, scratch, u64::from(storage.tagged_kind) << 32);
    dynasm!(ops
        ; .arch aarch64
        ; stur X(scratch), [X(size), numeric.hole_count_byte]
        ; stur wzr, [X(size), storage.slab_dirty_start_from_base]
        ; add WSP(scratch), WSP(length), #1
        ; stur W(scratch), [X(size), storage.slab_dirty_end_from_base]
        ; cbz W(length), =>copied
        ; ldr X(scratch), [X(receiver), storage.base_byte]
        ; mov w17, wzr
        ; =>copy
        ; ldr x16, [X(scratch), x17, lsl #3]
        ; str x16, [X(size), x17, lsl #3]
        ; add w17, w17, #1
        ; cmp w17, W(length)
        ; b.lo =>copy
        ; =>copied
    );
    emit_publish(ops, regs);
}
