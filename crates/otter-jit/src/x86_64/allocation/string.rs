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
//! An old cons leaves its two child edges to its caller, which takes the
//! write barrier for each (the remembered set for a young child, the
//! insertion barrier for a white one while marking runs) before anything
//! else runs. A tenured site read at run time misses before any write.
//!
//! # See also
//! - `otter_vm::jit::JitStringLayout` owns the current physical string recipe.
//! - `super` owns the shared LAB probe, publication and per-type accounting.

use super::{emit_bump_probe_in, emit_publish, emit_value, validate_inputs};
use crate::allocation::{AllocationValue, ConcatPlacement, LabRegisters};
use crate::entry::{ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_OLD_LAB_OFFSET};
use crate::x86_64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::jit::{
    JIT_SITE_CREATED_OFFSET, JIT_SITE_DECIDED_BIT, JIT_SITE_DECISION_OFFSET, JitSiteMode,
    JitStringLayout,
};

const SITE_ID_OFFSET: i32 = otter_vm::jit::JIT_SITE_ID_OFFSET as i32;
const HEADER_SITE_BYTES: i32 = otter_vm::jit::JIT_HEADER_SITE_BYTES as i32;
const TENURE: i8 = otter_vm::jit::JIT_SITE_TENURE as i8;

fn lab_offset(placement: ConcatPlacement) -> u32 {
    if placement.old() {
        ALLOC_WINDOW_OLD_LAB_OFFSET
    } else {
        ALLOC_WINDOW_LAB_OFFSET
    }
}

/// Candidate is the result on fit/identity; every miss is pre-effect.
///
/// For an old placement, returns the label the cons path reaches with the
/// new cell in `Rq(r.candidate)` and its child edges not yet barriered: the
/// caller binds it, takes the write barrier for both children, and joins
/// the other results.
#[must_use]
pub(crate) fn emit_concat(
    ops: &mut Assembler,
    context: u8,
    p: JitStringLayout,
    inputs: [AllocationValue; 2],
    r: LabRegisters,
    placement: ConcatPlacement,
    slow: DynamicLabel,
) -> Option<DynamicLabel> {
    validate_inputs(r, &inputs);
    debug_assert_eq!(r.size, 11);
    debug_assert!(
        [r.buffer, r.candidate, r.end, r.scratch]
            .iter()
            .all(|v| ![10, 11].contains(v))
    );
    if let ConcatPlacement::Record(record) = placement {
        debug_assert!(
            ![r.buffer, r.candidate, r.end, r.scratch, 10, 11, context].contains(&record)
        );
        dynasm!(ops ; .arch x64 ; test Rq(record),Rq(record) ; jz >placed
            ; cmp BYTE [Rq(record)+JIT_SITE_DECISION_OFFSET as i32],TENURE ; je =>slow
            ; placed:);
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
        ; cmp Rd(r.scratch),p.short_concat_flat as i32 ; ja =>cons);
    emit_flat(ops, context, p, inputs, r, placement, false, slow);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>latin
        ; cmp Rd(r.scratch),p.short_concat_latin1 as i32 ; ja =>cons);
    emit_flat(ops, context, p, inputs, r, placement, true, slow);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>cons);
    // Depth refusal precedes all candidate stores; canonical concat may flatten.
    emit_depth(ops, p, a, r.scratch, r.buffer);
    emit_depth(ops, p, b, r.end, r.buffer);
    dynasm!(ops ; .arch x64 ; cmp Rd(r.scratch),Rd(r.end) ; cmovb Rd(r.scratch),Rd(r.end)
        ; inc Rd(r.scratch) ; cmp Rd(r.scratch),p.max_rope_depth as i32 ; ja =>slow);
    emit_load_u64(ops, r.size, u64::from(p.pair_cell_bytes));
    initialize(ops, context, p, inputs, r, placement, p.cons_tag, slow);
    emit_value(ops, 10, a);
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)+p.cons_left_byte as i32],r10d);
    emit_value(ops, 10, b);
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)+p.cons_right_byte as i32],r10d);
    emit_depth(ops, p, a, r.scratch, r.end);
    emit_depth(ops, p, b, r.buffer, r.end);
    dynasm!(ops ; .arch x64 ; cmp Rd(r.scratch),Rd(r.buffer) ; cmovb Rd(r.scratch),Rd(r.buffer)
        ; inc Rd(r.scratch) ; mov [Rq(r.candidate)+p.cons_depth_byte as i32],Rb(r.scratch));
    publish(ops, context, r, placement);
    let old_cons = placement.old().then(|| ops.new_dynamic_label());
    let joined = old_cons.unwrap_or(done);
    dynasm!(ops ; .arch x64 ; jmp =>joined ; =>empty_a ; mov Rq(r.candidate),Rq(r.end)
        ; jmp =>done ; =>empty_b ; mov Rq(r.candidate),Rq(r.buffer) ; =>done);
    old_cons
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
        ; cmp BYTE [Rq(source)+p.string_repr_byte as i32],p.seq_latin1_tag as i8 ; je =>latin
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

/// Carve a cell of `Rq(r.size)` bytes and write its fixed part: the sized
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
    dynasm!(ops ; .arch x64 ; mov QWORD [Rq(r.candidate)+Rq(r.size)-8],0);
    emit_load_u64(ops, 10, placement.header_word(p.young_header_word));
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)],r10
        ; mov [Rq(r.candidate)+otter_vm::jit::JIT_HEADER_SIZE_BYTE as i32],Rd(r.size));
    if placement.old() {
        super::emit_black_when_marking(ops, context, r.candidate);
    }
    emit_load_u64(ops, 10, p.map_atom_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(r.candidate)+8],r10);
    emit_value(ops, 10, inputs[0]);
    dynasm!(ops ; .arch x64 ; mov Rd(r.scratch),[r10+p.string_len_byte as i32]);
    emit_value(ops, 10, inputs[1]);
    // The length's word also holds the zero hash.
    dynasm!(ops ; .arch x64 ; add Rd(r.scratch),[r10+p.string_len_byte as i32]
        ; mov [Rq(r.candidate)+p.string_len_byte as i32],Rq(r.scratch)
        ; mov DWORD [Rq(r.candidate)+p.string_repr_byte as i32],i32::from(kind));
}

fn publish(ops: &mut Assembler, context: u8, r: LabRegisters, placement: ConcatPlacement) {
    // The cell's size is its header's.
    dynasm!(ops ; .arch x64 ; mov Rq(r.buffer),[Rq(context)+lab_offset(placement) as i32]
        ; mov r10d,[Rq(r.candidate)+otter_vm::jit::JIT_HEADER_SIZE_BYTE as i32]
        ; lea Rq(r.end),[Rq(r.candidate)+r10]);
    emit_publish(ops, r);
    // A tracked birth counts toward the site's survival ratio.
    match placement {
        ConcatPlacement::Site(site) if site.mode == JitSiteMode::Track => {
            emit_load_u64(ops, 10, site.record);
            dynasm!(ops ; .arch x64 ; add DWORD [r10+JIT_SITE_CREATED_OFFSET as i32],1);
        }
        ConcatPlacement::Site(_) => {}
        ConcatPlacement::Record(record) => {
            dynasm!(ops ; .arch x64 ; test Rq(record),Rq(record) ; jz >counted
                ; test BYTE [Rq(record)+JIT_SITE_DECISION_OFFSET as i32],JIT_SITE_DECIDED_BIT as i8
                ; jnz >counted
                ; movzx r10d,WORD [Rq(record)+SITE_ID_OFFSET]
                ; mov WORD [Rq(r.candidate)+HEADER_SITE_BYTES],r10w
                ; add DWORD [Rq(record)+JIT_SITE_CREATED_OFFSET as i32],1
                ; counted:);
        }
    }
}

/// A sequential result of the summed length in `Rd(r.scratch)`: Latin-1
/// when both sides are, else WTF-16.
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
    let align = (otter_vm::jit::JIT_CELL_SIZE - 1) as i32;
    if latin {
        dynasm!(ops ; .arch x64 ; lea Rd(r.size),[Rq(r.scratch)+p.string_body_size as i32+align]);
    } else {
        dynasm!(ops ; .arch x64
            ; lea Rd(r.size),[Rq(r.scratch)+Rq(r.scratch)+p.string_body_size as i32+align]);
    }
    dynasm!(ops ; .arch x64 ; and Rd(r.size),!align);
    let tag = if latin {
        p.seq_latin1_tag
    } else {
        p.seq_flat_tag
    };
    initialize(ops, context, p, inputs, r, placement, tag, slow);
    dynasm!(ops ; .arch x64 ; movq xmm15,Rq(r.candidate)
        ; lea Rq(r.buffer),[Rq(r.candidate)+p.string_body_size as i32]);
    for input in inputs {
        let wide = ops.new_dynamic_label();
        let next = ops.new_dynamic_label();
        emit_value(ops, r.end, input);
        dynasm!(ops ; .arch x64 ; mov Rd(r.size),[Rq(r.end)+p.string_len_byte as i32]
            ; cmp BYTE [Rq(r.end)+p.string_repr_byte as i32],p.seq_flat_tag as i8
            ; lea Rq(r.end),[Rq(r.end)+p.string_body_size as i32]
            ; je =>wide);
        emit_copy_loop(ops, r, false, latin);
        dynasm!(ops ; .arch x64 ; jmp =>next ; =>wide);
        emit_copy_loop(ops, r, true, latin);
        dynasm!(ops ; .arch x64 ; =>next);
    }
    dynasm!(ops ; .arch x64 ; movq Rq(r.candidate),xmm15);
    publish(ops, context, r, placement);
}

/// Copy `Rd(r.size)` units from `Rq(r.end)` to `Rq(r.buffer)`: one byte or
/// two per source unit, one or two per destination unit.
fn emit_copy_loop(ops: &mut Assembler, r: LabRegisters, wide: bool, latin: bool) {
    let copying = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; test Rd(r.size),Rd(r.size) ; jz =>done ; =>copying
        ; movzx r10d,BYTE [Rq(r.end)] ; mov [Rq(r.buffer)],r10b
        ; inc Rq(r.end) ; inc Rq(r.buffer));
    if wide {
        dynasm!(ops ; .arch x64 ; movzx r10d,BYTE [Rq(r.end)] ; inc Rq(r.end));
    } else {
        dynasm!(ops ; .arch x64 ; xor r10d,r10d);
    }
    if !latin {
        dynasm!(ops ; .arch x64 ; mov [Rq(r.buffer)],r10b ; inc Rq(r.buffer));
    }
    dynasm!(ops ; .arch x64 ; dec Rd(r.size) ; jnz =>copying ; =>done);
}
