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
use crate::x86_64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
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
    debug_assert_eq!(r.size, 11);
    debug_assert!(
        [r.buffer, r.candidate, r.end, r.scratch]
            .iter()
            .all(|v| ![10, 11].contains(v))
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
        emit_load_u64(ops, 11, otter_vm::value::tag::NOT_CELL_MASK);
        dynasm!(ops ; .arch x64 ; test Rq(r.buffer),r11 ; jnz =>slow
            ; test Rq(r.buffer),Rq(r.buffer) ; jz =>slow
            ; cmp BYTE [Rq(r.buffer)],p.string_type_tag as i8 ; jne =>slow);
    }
    emit_value(ops, r.buffer, a);
    emit_value(ops, r.end, b);
    dynasm!(ops ; .arch x64
        ; mov Rd(r.scratch),[Rq(r.buffer)+p.string_len_byte as i32]
        ; test Rd(r.scratch),Rd(r.scratch) ; jz =>empty_a
        ; mov r11d,[Rq(r.end)+p.string_len_byte as i32]
        ; test r11d,r11d ; jz =>empty_b
        ; add Rd(r.scratch),r11d ; jc =>slow);
    let latin_a = ops.new_dynamic_label();
    let wide_a = ops.new_dynamic_label();
    classify(ops, p, r.buffer, latin_a, wide_a, cons);
    dynasm!(ops ; .arch x64 ; =>latin_a);
    classify(ops, p, r.end, latin, flat, cons);
    dynasm!(ops ; .arch x64 ; =>wide_a);
    classify(ops, p, r.end, flat, flat, cons);
    dynasm!(ops ; .arch x64 ; =>flat
        ; cmp Rd(r.scratch),p.inline_flat_cap as i32 ; ja =>cons);
    emit_flat(ops, context, p, inputs, r, false, slow);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>latin
        ; cmp Rd(r.scratch),p.inline_latin1_cap as i32 ; ja =>cons);
    emit_flat(ops, context, p, inputs, r, true, slow);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>cons);
    // Depth refusal precedes all candidate stores; canonical concat may flatten.
    emit_depth(ops, p, a, r.scratch, r.buffer);
    emit_depth(ops, p, b, r.end, r.buffer);
    dynasm!(ops ; .arch x64 ; cmp Rd(r.scratch),Rd(r.end) ; cmovb Rd(r.scratch),Rd(r.end)
        ; inc Rd(r.scratch) ; cmp Rd(r.scratch),p.max_rope_depth as i32 ; ja =>slow);
    initialize(ops, context, p, inputs, r, slow);
    emit_value(ops, 10, a);
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)+p.cons_left_byte as i32],r10d
        ; mov Rq(r.scratch),[r10+p.hash_byte as i32]);
    emit_value(ops, 10, b);
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)+p.cons_right_byte as i32],r10d
        ; mov Rq(r.end),[r10+p.hash_byte as i32]);
    emit_load_u64(ops, 10, p.fnv_prime);
    dynasm!(ops ; .arch x64 ; imul Rq(r.scratch),r10 ; xor Rq(r.scratch),Rq(r.end)
        ; mov [Rq(r.candidate)+p.hash_byte as i32],Rq(r.scratch)
        ; mov BYTE [Rq(r.candidate)+p.string_repr_byte as i32],p.cons_tag as i8);
    emit_depth(ops, p, a, r.scratch, r.end);
    emit_depth(ops, p, b, r.buffer, r.end);
    dynasm!(ops ; .arch x64 ; cmp Rd(r.scratch),Rd(r.buffer) ; cmovb Rd(r.scratch),Rd(r.buffer)
        ; inc Rd(r.scratch) ; mov [Rq(r.candidate)+p.cons_depth_byte as i32],Rb(r.scratch));
    publish(ops, context, p, r);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>empty_a ; mov Rq(r.candidate),Rq(r.end)
        ; jmp =>done ; =>empty_b ; mov Rq(r.candidate),Rq(r.buffer) ; =>done);
}

fn classify(
    ops: &mut Assembler,
    p: JitStringLayout,
    source: u8,
    latin: DynamicLabel,
    wide: DynamicLabel,
    other: DynamicLabel,
) {
    dynasm!(ops ; .arch x64
        ; cmp BYTE [Rq(source)+p.string_repr_byte as i32],p.inline_latin1_tag as i8 ; je =>latin
        ; cmp BYTE [Rq(source)+p.string_repr_byte as i32],p.seq_latin1_tag as i8 ; je =>latin
        ; cmp BYTE [Rq(source)+p.string_repr_byte as i32],p.inline_flat_tag as i8 ; je =>wide
        ; cmp BYTE [Rq(source)+p.string_repr_byte as i32],p.seq_flat_tag as i8 ; je =>wide
        ; jmp =>other);
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
    dynasm!(ops ; .arch x64 ; xor Rd(dst),Rd(dst)
        ; cmp BYTE [Rq(source)+p.string_repr_byte as i32],p.cons_tag as i8 ; jne =>done
        ; movzx Rd(dst),BYTE [Rq(source)+p.cons_depth_byte as i32] ; =>done);
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
        dynasm!(ops ; .arch x64 ; mov QWORD [Rq(r.candidate)+byte as i32],0);
    }
    emit_load_u64(ops, 10, p.header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)],r10);
    emit_value(ops, 10, inputs[0]);
    dynasm!(ops ; .arch x64 ; mov Rd(r.scratch),[r10+p.string_len_byte as i32]);
    emit_value(ops, 10, inputs[1]);
    dynasm!(ops ; .arch x64 ; add Rd(r.scratch),[r10+p.string_len_byte as i32]
        ; mov [Rq(r.candidate)+p.string_len_byte as i32],Rd(r.scratch));
}
fn publish(ops: &mut Assembler, context: u8, p: JitStringLayout, r: LabRegisters) {
    emit_load_u64(ops, r.size, u64::from(p.cell_bytes));
    dynasm!(ops ; .arch x64 ; mov Rq(r.buffer),[Rq(context)+ALLOC_WINDOW_LAB_OFFSET as i32]
        ; lea Rq(r.end),[Rq(r.candidate)+p.cell_bytes as i32]);
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
    dynasm!(ops ; .arch x64 ; mov BYTE [Rq(r.candidate)+p.string_repr_byte as i32],tag as i8
        ; movq xmm15,Rq(r.candidate)
        ; lea Rq(r.buffer),[Rq(r.candidate)+p.string_repr_payload_byte as i32]);
    emit_load_u64(ops, r.scratch, p.fnv_offset);
    emit_load_u64(ops, r.candidate, p.fnv_prime);
    for input in inputs {
        let wide = ops.new_dynamic_label();
        let next = ops.new_dynamic_label();
        emit_value(ops, r.end, input);
        dynasm!(ops ; .arch x64 ; mov Rd(r.size),[Rq(r.end)+p.string_len_byte as i32]
            ; cmp BYTE [Rq(r.end)+p.string_repr_byte as i32],p.inline_flat_tag as i8 ; je =>wide
            ; cmp BYTE [Rq(r.end)+p.string_repr_byte as i32],p.seq_flat_tag as i8 ; je =>wide);
        emit_copy_loop(ops, p, r, false, latin);
        dynasm!(ops ; .arch x64 ; jmp =>next ; =>wide);
        emit_copy_loop(ops, p, r, true, latin);
        dynasm!(ops ; .arch x64 ; =>next);
    }
    dynasm!(ops ; .arch x64 ; movq Rq(r.candidate),xmm15
        ; mov [Rq(r.candidate)+p.hash_byte as i32],Rq(r.scratch));
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
    dynasm!(ops ; .arch x64 ; cmp BYTE [Rq(r.end)+p.string_repr_byte as i32],tag as i8 ; je =>inline
        ; add Rq(r.end),p.string_body_size as i32 ; jmp =>start
        ; =>inline ; add Rq(r.end),p.string_repr_payload_byte as i32 ; =>start
        ; test Rd(r.size),Rd(r.size) ; jz =>done ; =>copying
        ; movzx r10d,BYTE [Rq(r.end)] ; mov [Rq(r.buffer)],r10b
        ; xor Rq(r.scratch),r10 ; imul Rq(r.scratch),Rq(r.candidate)
        ; inc Rq(r.end) ; inc Rq(r.buffer));
    if wide {
        dynasm!(ops ; .arch x64 ; movzx r10d,BYTE [Rq(r.end)] ; inc Rq(r.end));
    } else {
        dynasm!(ops ; .arch x64 ; xor r10d,r10d);
    }
    if !latin {
        dynasm!(ops ; .arch x64 ; mov [Rq(r.buffer)],r10b ; inc Rq(r.buffer));
    }
    dynasm!(ops ; .arch x64 ; xor Rq(r.scratch),r10 ; imul Rq(r.scratch),Rq(r.candidate)
        ; dec Rd(r.size) ; jnz =>copying ; =>done);
}
