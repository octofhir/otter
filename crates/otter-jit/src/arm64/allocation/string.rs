//! Native primitive string concatenation on the target's one nursery LAB.
//!
//! # Contents
//! - Empty identity, short width-preserving flat concatenation and bounded Cons.
//! - VM-owned payload length/hash and complete initialized young cell publication.
//!
//! # Invariants
//! All tag/length/depth/limit misses precede candidate writes. Inputs are explicit
//! immutable homes/constants or registers outside the declared temporary bank.
//! Fits never collect, call Rust, widen a source body or retain an interior data
//! pointer beyond this operation. Every byte/padding word is initialized before
//! the single top store; the result is committed late by the tier's caller.
//! Only four declared GP temporaries plus the target's reserved GP/FP scratch
//! registers and flags are clobbered. String geometry/hash/header bits are VM data.
//!
//! # See also
//! - `otter_vm::jit::JitStringLayout` owns the current physical string recipe.
//! - `super` owns the shared LAB probe, publication and per-type accounting.

use super::{emit_bump_probe, emit_publish, emit_value, validate_inputs};
use crate::allocation::{AllocationValue, LabRegisters};
use crate::entry::ALLOC_WINDOW_LAB_OFFSET;
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::JitStringLayout;

/// Candidate is the result on fit/identity; every miss is pre-effect.
pub(crate) fn emit_concat(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    slow: DynamicLabel,
) {
    validate_inputs(r, &inputs);
    debug_assert_eq!(r.size, 17);
    debug_assert!(
        [r.buffer, r.candidate, r.end, r.scratch]
            .iter()
            .all(|v| ![16, 17].contains(v))
    );
    let [a, b] = inputs;
    let latin = ops.new_dynamic_label();
    let flat = ops.new_dynamic_label();
    let cons = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let empty_a = ops.new_dynamic_label();
    let empty_b = ops.new_dynamic_label();
    for input in inputs {
        emit_value(ops, r.buffer, input);
        emit_load_u64(ops, 17, otter_vm::value::tag::NOT_CELL_MASK);
        dynasm!(ops ; .arch aarch64 ; tst X(r.buffer),x17 ; b.ne =>slow
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
        ; cmp WSP(r.scratch),p.inline_flat_cap as u32 ; b.hi =>cons);
    emit_flat(ops, context, p, inputs, r, false, slow);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>latin
        ; cmp WSP(r.scratch),p.inline_latin1_cap as u32 ; b.hi =>cons);
    emit_flat(ops, context, p, inputs, r, true, slow);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>cons);
    emit_depth(ops, p, a, r.scratch, r.buffer);
    emit_depth(ops, p, b, r.end, r.buffer);
    dynasm!(ops ; .arch aarch64 ; cmp W(r.scratch),W(r.end)
        ; csel W(r.scratch),W(r.scratch),W(r.end),hs
        ; add WSP(r.scratch),WSP(r.scratch),1
        ; cmp WSP(r.scratch),p.max_rope_depth as u32 ; b.hi =>slow);
    initialize(ops, context, p, inputs, r, slow);
    emit_value(ops, 16, a);
    dynasm!(ops ; .arch aarch64 ; str w16,[X(r.candidate),p.cons_left_byte]
        ; ldr X(r.scratch),[x16,p.hash_byte]);
    emit_value(ops, 16, b);
    dynasm!(ops ; .arch aarch64 ; str w16,[X(r.candidate),p.cons_right_byte]
        ; ldr X(r.end),[x16,p.hash_byte]);
    emit_load_u64(ops, 16, p.fnv_prime);
    dynasm!(ops ; .arch aarch64 ; mul X(r.scratch),X(r.scratch),x16
        ; eor X(r.scratch),X(r.scratch),X(r.end)
        ; str X(r.scratch),[X(r.candidate),p.hash_byte]);
    emit_load_u64(ops, 16, u64::from(p.cons_tag));
    dynasm!(ops ; .arch aarch64 ; strb w16,[X(r.candidate),p.string_repr_byte]);
    emit_depth(ops, p, a, r.scratch, r.end);
    emit_depth(ops, p, b, r.buffer, r.end);
    dynasm!(ops ; .arch aarch64 ; cmp W(r.scratch),W(r.buffer)
        ; csel W(r.scratch),W(r.scratch),W(r.buffer),hs
        ; add WSP(r.scratch),WSP(r.scratch),1
        ; strb W(r.scratch),[X(r.candidate),p.cons_depth_byte]);
    publish(ops, context, p, r);
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
        ; cmp WSP(17),p.inline_latin1_tag as u32 ; b.eq =>latin
        ; cmp WSP(17),p.seq_latin1_tag as u32 ; b.eq =>latin
        ; cmp WSP(17),p.inline_flat_tag as u32 ; b.eq =>wide
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
fn initialize(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    slow: DynamicLabel,
) {
    emit_load_u64(ops, r.size, u64::from(p.cell_bytes));
    emit_bump_probe(ops, context, r, slow);
    for byte in (8..p.cell_bytes).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr,[X(r.candidate),byte]);
    }
    emit_load_u64(ops, 16, p.header_word);
    dynasm!(ops ; .arch aarch64 ; str x16,[X(r.candidate)]);
    emit_value(ops, 16, inputs[0]);
    dynasm!(ops ; .arch aarch64 ; ldr W(r.scratch),[x16,p.string_len_byte]);
    emit_value(ops, 16, inputs[1]);
    dynasm!(ops ; .arch aarch64 ; ldr w17,[x16,p.string_len_byte]
        ; add W(r.scratch),W(r.scratch),w17 ; str W(r.scratch),[X(r.candidate),p.string_len_byte]);
}
fn publish(ops: &mut Assembler, context: u8, p: JitStringLayout, r: LabRegisters) {
    emit_load_u64(ops, r.size, u64::from(p.cell_bytes));
    dynasm!(ops ; .arch aarch64 ; ldr X(r.buffer),[X(context),ALLOC_WINDOW_LAB_OFFSET]
        ; add XSP(r.end),XSP(r.candidate),p.cell_bytes);
    emit_publish(ops, context, p.string_type_tag, r);
}
fn emit_flat(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    latin: bool,
    slow: DynamicLabel,
) {
    initialize(ops, context, p, inputs, r, slow);
    let tag = if latin {
        p.inline_latin1_tag
    } else {
        p.inline_flat_tag
    };
    emit_load_u64(ops, 16, u64::from(tag));
    dynasm!(ops ; .arch aarch64 ; strb w16,[X(r.candidate),p.string_repr_byte]
        ; fmov d31,X(r.candidate)
        ; add XSP(r.buffer),XSP(r.candidate),p.string_repr_payload_byte);
    emit_load_u64(ops, r.scratch, p.fnv_offset);
    emit_load_u64(ops, r.candidate, p.fnv_prime);
    for input in inputs {
        let wide = ops.new_dynamic_label();
        let next = ops.new_dynamic_label();
        emit_value(ops, r.end, input);
        dynasm!(ops ; .arch aarch64 ; ldr W(r.size),[X(r.end),p.string_len_byte]
            ; ldrb w16,[X(r.end),p.string_repr_byte]
            ; cmp WSP(16),p.inline_flat_tag as u32 ; b.eq =>wide
            ; cmp WSP(16),p.seq_flat_tag as u32 ; b.eq =>wide);
        emit_copy_loop(ops, p, r, false, latin);
        dynasm!(ops ; .arch aarch64 ; b =>next ; =>wide);
        emit_copy_loop(ops, p, r, true, latin);
        dynasm!(ops ; .arch aarch64 ; =>next);
    }
    dynasm!(ops ; .arch aarch64 ; fmov X(r.candidate),d31
        ; str X(r.scratch),[X(r.candidate),p.hash_byte]);
    publish(ops, context, p, r);
}
fn emit_copy_loop(
    ops: &mut Assembler,
    p: JitStringLayout,
    r: LabRegisters,
    wide: bool,
    latin: bool,
) {
    let inline = ops.new_dynamic_label();
    let start = ops.new_dynamic_label();
    let copying = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let tag = if wide {
        p.inline_flat_tag
    } else {
        p.inline_latin1_tag
    };
    dynasm!(ops ; .arch aarch64 ; ldrb w16,[X(r.end),p.string_repr_byte]
        ; cmp WSP(16),tag as u32 ; b.eq =>inline
        ; add XSP(r.end),XSP(r.end),p.string_body_size ; b =>start
        ; =>inline ; add XSP(r.end),XSP(r.end),p.string_repr_payload_byte ; =>start
        ; cbz W(r.size),=>done ; =>copying
        ; ldrb w16,[X(r.end)] ; strb w16,[X(r.buffer)]
        ; eor X(r.scratch),X(r.scratch),x16 ; mul X(r.scratch),X(r.scratch),X(r.candidate)
        ; add XSP(r.end),XSP(r.end),1 ; add XSP(r.buffer),XSP(r.buffer),1);
    if wide {
        dynasm!(ops ; .arch aarch64 ; ldrb w16,[X(r.end)] ; add XSP(r.end),XSP(r.end),1);
    } else {
        dynasm!(ops ; .arch aarch64 ; mov w16,wzr);
    }
    if !latin {
        dynasm!(ops ; .arch aarch64 ; strb w16,[X(r.buffer)] ; add XSP(r.buffer),XSP(r.buffer),1);
    }
    dynasm!(ops ; .arch aarch64 ; eor X(r.scratch),X(r.scratch),x16
        ; mul X(r.scratch),X(r.scratch),X(r.candidate)
        ; subs W(r.size),WSP(r.size),1 ; b.ne =>copying ; =>done);
}
