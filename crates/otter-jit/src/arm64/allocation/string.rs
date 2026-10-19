//! Native primitive string concatenation on the target's one nursery LAB.
//!
//! # Contents
//! - Empty identity, short width-preserving flat concatenation and bounded Cons.
//! - VM-owned payload length/hash and complete initialized cell publication.
//! - Allocation-site placement: tracked young cells, old cells, or a site
//!   record read at run time by code many sites share.
//!
//! # Invariants
//! All tag/length/depth/limit misses precede candidate writes. Inputs are explicit
//! immutable homes/constants or registers outside the declared temporary bank.
//! Fits never collect, call Rust, widen a source body or retain an interior data
//! pointer beyond this operation. Every byte/padding word is initialized before
//! the single top store; the result is committed late by the tier's caller.
//! Only four declared GP temporaries plus the target's reserved GP/FP scratch
//! registers and flags are clobbered. String geometry/hash/header bits are VM data.
//! An old string is never born pointing at a young child: such a cons misses
//! to the canonical allocator, which remembers the edge. A tenured site read
//! at run time misses before any write.
//!
//! # See also
//! - `otter_vm::jit::JitStringLayout` owns the current physical string recipe.
//! - `super` owns the shared LAB probe, publication and per-type accounting.

use super::{emit_bump_probe_in, emit_publish, emit_value, validate_inputs};
use crate::allocation::{AllocationValue, ConcatPlacement, LabRegisters};
use crate::entry::{ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_OLD_LAB_OFFSET};
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::{
    JIT_GC_YOUNG_FLAG, JIT_HEADER_FLAGS_BYTE, JIT_HEADER_SITE_BYTES, JIT_SITE_CREATED_OFFSET,
    JIT_SITE_DECIDED_BIT, JIT_SITE_DECISION_OFFSET, JIT_SITE_ID_OFFSET, JitSiteMode,
    JitStringLayout,
};

const TENURE: u32 = otter_vm::jit::JIT_SITE_TENURE as u32;

fn lab_offset(placement: ConcatPlacement) -> u32 {
    if placement.old() {
        ALLOC_WINDOW_OLD_LAB_OFFSET
    } else {
        ALLOC_WINDOW_LAB_OFFSET
    }
}

/// Candidate is the result on fit/identity; every miss is pre-effect.
pub(crate) fn emit_concat(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    placement: ConcatPlacement,
    slow: DynamicLabel,
) {
    validate_inputs(r, &inputs);
    debug_assert_eq!(r.size, 17);
    debug_assert!(
        [r.buffer, r.candidate, r.end, r.scratch]
            .iter()
            .all(|v| ![16, 17].contains(v))
    );
    if let ConcatPlacement::Record(record) = placement {
        debug_assert!(
            ![r.buffer, r.candidate, r.end, r.scratch, 16, 17, context].contains(&record)
        );
        dynasm!(ops ; .arch aarch64 ; cbz X(record), >placed
            ; ldrb w17, [X(record), JIT_SITE_DECISION_OFFSET]
            ; cmp w17, TENURE ; b.eq =>slow ; placed:);
    }
    let [a, b] = inputs;
    let latin = ops.new_dynamic_label();
    let flat = ops.new_dynamic_label();
    let cons = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let empty_a = ops.new_dynamic_label();
    let empty_b = ops.new_dynamic_label();
    for input in inputs {
        emit_value(ops, r.buffer, input);
        dynasm!(ops ; .arch aarch64 ; tst X(r.buffer), #0xfffe_0000_0000_0007 ; b.ne =>slow
            ; cbz X(r.buffer),=>slow ; ldrb w17,[X(r.buffer)]
            ; cmp WSP(17),p.string_type_tag as u32 ; b.ne =>slow);
    }
    emit_value(ops, r.buffer, a);
    emit_value(ops, r.end, b);
    dynasm!(ops ; .arch aarch64
        ; ldr W(r.scratch),[X(r.buffer),p.string_len_byte] ; cbz W(r.scratch),=>empty_a
        ; ldr w17,[X(r.end),p.string_len_byte] ; cbz w17,=>empty_b
        ; adds W(r.scratch),W(r.scratch),w17 ; b.cs =>slow);
    let latin_a = ops.new_dynamic_label();
    let wide_a = ops.new_dynamic_label();
    classify(ops, p, r.buffer, latin_a, wide_a, cons);
    dynasm!(ops ; .arch aarch64 ; =>latin_a);
    classify(ops, p, r.end, latin, flat, cons);
    dynasm!(ops ; .arch aarch64 ; =>wide_a);
    classify(ops, p, r.end, flat, flat, cons);
    dynasm!(ops ; .arch aarch64 ; =>flat
        ; cmp WSP(r.scratch),p.short_concat_flat as u32 ; b.hi =>cons);
    emit_flat(ops, context, p, inputs, r, placement, false, slow);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>latin
        ; cmp WSP(r.scratch),p.short_concat_latin1 as u32 ; b.hi =>cons);
    emit_flat(ops, context, p, inputs, r, placement, true, slow);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>cons);
    emit_depth(ops, p, a, r.scratch, r.buffer);
    emit_depth(ops, p, b, r.end, r.buffer);
    dynasm!(ops ; .arch aarch64 ; cmp W(r.scratch),W(r.end)
        ; csel W(r.scratch),W(r.scratch),W(r.end),hs
        ; add WSP(r.scratch),WSP(r.scratch),1
        ; cmp WSP(r.scratch),p.max_rope_depth as u32 ; b.hi =>slow);
    if placement.old() {
        // An old cons is born black while marking runs, so a white child would
        // be an edge no visit follows: the canonical allocator shades it.
        dynasm!(ops ; .arch aarch64
            ; ldr x16, [X(context), crate::entry::THREAD_OFFSET]
            ; ldr x16, [x16, crate::entry::VM_THREAD_MARKING_FLAG_CELL_OFFSET]
            ; ldrb W(r.scratch), [x16]);
        for input in inputs {
            let next = ops.new_dynamic_label();
            emit_value(ops, 16, input);
            dynasm!(ops ; .arch aarch64 ; ldrb w17, [x16, JIT_HEADER_FLAGS_BYTE]
                ; tbnz w17, JIT_GC_YOUNG_FLAG.trailing_zeros(), =>slow
                ; cbz W(r.scratch), =>next
                ; tst w17, #3 ; b.eq =>slow
                ; =>next);
        }
    }
    emit_load_u64(ops, r.size, u64::from(p.pair_cell_bytes));
    initialize(ops, context, p, inputs, r, placement, p.cons_tag, slow);
    emit_value(ops, 16, a);
    dynasm!(ops ; .arch aarch64 ; str w16,[X(r.candidate),p.cons_left_byte]);
    emit_value(ops, 16, b);
    dynasm!(ops ; .arch aarch64 ; str w16,[X(r.candidate),p.cons_right_byte]);
    emit_depth(ops, p, a, r.scratch, r.end);
    emit_depth(ops, p, b, r.buffer, r.end);
    dynasm!(ops ; .arch aarch64 ; cmp W(r.scratch),W(r.buffer)
        ; csel W(r.scratch),W(r.scratch),W(r.buffer),hs
        ; add WSP(r.scratch),WSP(r.scratch),1
        ; strb W(r.scratch),[X(r.candidate),p.cons_depth_byte]);
    publish(ops, context, r, placement);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>empty_a ; mov X(r.candidate),X(r.end)
        ; b =>done ; =>empty_b ; mov X(r.candidate),X(r.buffer) ; =>done);
}
fn classify(
    ops: &mut Assembler,
    p: JitStringLayout,
    source: u8,
    latin: DynamicLabel,
    wide: DynamicLabel,
    other: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; ldrb w17,[X(source),p.string_repr_byte]
        ; cmp WSP(17),p.seq_latin1_tag as u32 ; b.eq =>latin
        ; cmp WSP(17),p.seq_flat_tag as u32 ; b.eq =>wide ; b =>other);
}
fn emit_depth(
    ops: &mut Assembler,
    p: JitStringLayout,
    input: AllocationValue,
    dst: u8,
    source: u8,
) {
    let done = ops.new_dynamic_label();
    emit_value(ops, source, input);
    dynasm!(ops ; .arch aarch64 ; mov W(dst),wzr ; ldrb w17,[X(source),p.string_repr_byte]
        ; cmp WSP(17),p.cons_tag as u32 ; b.ne =>done
        ; ldrb W(dst),[X(source),p.cons_depth_byte] ; =>done);
}
/// Carve a cell of `X(r.size)` bytes and write its fixed part: the sized
/// header, the map and a zero atom, the summed length and a zero hash, and
/// `kind` with a zero depth. The last word is zeroed first, so trailing
/// padding is initialized before any payload byte lands.
fn initialize(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    placement: ConcatPlacement,
    kind: u8,
    slow: DynamicLabel,
) {
    emit_bump_probe_in(ops, context, lab_offset(placement), r, slow);
    dynasm!(ops ; .arch aarch64 ; add x16, X(r.candidate), X(r.size) ; stur xzr, [x16, -8]);
    emit_load_u64(ops, 16, placement.header_word(p.young_header_word));
    dynasm!(ops ; .arch aarch64 ; orr x16, x16, X(r.size), lsl u32::from(p.cell_size_shift)
        ; str x16,[X(r.candidate)]);
    if placement.old() {
        super::emit_black_when_marking(ops, context, r.candidate);
    }
    emit_load_u64(ops, 16, p.map_atom_word);
    dynasm!(ops ; .arch aarch64 ; str x16,[X(r.candidate),8]);
    emit_value(ops, 16, inputs[0]);
    dynasm!(ops ; .arch aarch64 ; ldr W(r.scratch),[x16,p.string_len_byte]);
    emit_value(ops, 16, inputs[1]);
    // The length's word also holds the zero hash.
    dynasm!(ops ; .arch aarch64 ; ldr w17,[x16,p.string_len_byte]
        ; add W(r.scratch),W(r.scratch),w17 ; str X(r.scratch),[X(r.candidate),p.string_len_byte]
        ; movz w16, u32::from(kind) ; str w16,[X(r.candidate),p.string_repr_byte]);
}
fn publish(ops: &mut Assembler, context: u8, r: LabRegisters, placement: ConcatPlacement) {
    // The cell's size is its header's.
    dynasm!(ops ; .arch aarch64 ; ldr X(r.buffer),[X(context),lab_offset(placement)]
        ; ldr w16,[X(r.candidate), otter_vm::jit::JIT_HEADER_SIZE_BYTE]
        ; add X(r.end),X(r.candidate),x16);
    emit_publish(ops, r);
    if placement.old() {
        // The initialized cell is visible before any store publishes it to
        // the concurrent marker.
        dynasm!(ops ; .arch aarch64 ; dmb ishst);
    }
    // A tracked birth counts toward the site's survival ratio.
    match placement {
        ConcatPlacement::Site(site) if site.mode == JitSiteMode::Track => {
            emit_load_u64(ops, 16, site.record);
            dynasm!(ops ; .arch aarch64 ; ldr w17, [x16, JIT_SITE_CREATED_OFFSET]
                ; add w17, w17, #1 ; str w17, [x16, JIT_SITE_CREATED_OFFSET]);
        }
        ConcatPlacement::Site(_) => {}
        ConcatPlacement::Record(record) => {
            dynasm!(ops ; .arch aarch64 ; cbz X(record), >counted
                ; ldrb w17, [X(record), JIT_SITE_DECISION_OFFSET]
                ; tbnz w17, JIT_SITE_DECIDED_BIT.trailing_zeros(), >counted
                ; ldrh w17, [X(record), JIT_SITE_ID_OFFSET]
                ; strh w17, [X(r.candidate), JIT_HEADER_SITE_BYTES]
                ; ldr w17, [X(record), JIT_SITE_CREATED_OFFSET]
                ; add w17, w17, #1 ; str w17, [X(record), JIT_SITE_CREATED_OFFSET]
                ; counted:);
        }
    }
}
/// A sequential result of the summed length in `W(r.scratch)`: Latin-1 when
/// both sides are, else WTF-16.
fn emit_flat(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    placement: ConcatPlacement,
    latin: bool,
    slow: DynamicLabel,
) {
    // Cell size: the fixed part and the units, rounded up to a cell.
    let align = otter_gc_cell_mask();
    if latin {
        dynasm!(ops ; .arch aarch64 ; add WSP(r.size), WSP(r.scratch), p.string_body_size + align);
    } else {
        dynasm!(ops ; .arch aarch64 ; lsl W(r.size), W(r.scratch), #1
            ; add WSP(r.size), WSP(r.size), p.string_body_size + align);
    }
    dynasm!(ops ; .arch aarch64 ; and WSP(r.size), W(r.size), !align);
    let tag = if latin {
        p.seq_latin1_tag
    } else {
        p.seq_flat_tag
    };
    initialize(ops, context, p, inputs, r, placement, tag, slow);
    dynasm!(ops ; .arch aarch64 ; fmov d31,X(r.candidate)
        ; add XSP(r.buffer),XSP(r.candidate),p.string_body_size);
    for input in inputs {
        let wide = ops.new_dynamic_label();
        let next = ops.new_dynamic_label();
        emit_value(ops, r.end, input);
        dynasm!(ops ; .arch aarch64 ; ldr W(r.size),[X(r.end),p.string_len_byte]
            ; ldrb w16,[X(r.end),p.string_repr_byte]
            ; add XSP(r.end),XSP(r.end),p.string_body_size
            ; cmp WSP(16),p.seq_flat_tag as u32 ; b.eq =>wide);
        emit_copy_loop(ops, r, false, latin);
        dynasm!(ops ; .arch aarch64 ; b =>next ; =>wide);
        emit_copy_loop(ops, r, true, latin);
        dynasm!(ops ; .arch aarch64 ; =>next);
    }
    dynasm!(ops ; .arch aarch64 ; fmov X(r.candidate),d31);
    publish(ops, context, r, placement);
}
/// The cell-size rounding mask.
const fn otter_gc_cell_mask() -> u32 {
    (otter_vm::jit::JIT_CELL_SIZE - 1) as u32
}
/// Copy `W(r.size)` units from `X(r.end)` to `X(r.buffer)`: one byte or two
/// per source unit, one or two per destination unit.
fn emit_copy_loop(ops: &mut Assembler, r: LabRegisters, wide: bool, latin: bool) {
    let copying = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; cbz W(r.size),=>done ; =>copying
        ; ldrb w16,[X(r.end)] ; strb w16,[X(r.buffer)]
        ; add XSP(r.end),XSP(r.end),1 ; add XSP(r.buffer),XSP(r.buffer),1);
    if wide {
        dynasm!(ops ; .arch aarch64 ; ldrb w16,[X(r.end)] ; add XSP(r.end),XSP(r.end),1);
    } else {
        dynasm!(ops ; .arch aarch64 ; mov w16,wzr);
    }
    if !latin {
        dynasm!(ops ; .arch aarch64 ; strb w16,[X(r.buffer)] ; add XSP(r.buffer),XSP(r.buffer),1);
    }
    dynasm!(ops ; .arch aarch64 ; subs W(r.size),WSP(r.size),1 ; b.ne =>copying ; =>done);
}
