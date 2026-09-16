//! No-call x86-64 proofs for tagged ECMAScript loose equality.
//!
//! # Contents
//! - Identity, homogeneous Number, nullish-pair and ordinary object-pair hits.
//! - A pure miss for coercion, different primitive cells and uncertain values.
//!
//! # Invariants
//! - The probe allocates, calls, deoptimizes, and runs user code nowhere.
//! - All allocator-visible registers are restored before either output is
//!   defined; `r11` is the target-reserved scratch register.
//! - Same-pointer objects never coerce, while mixed nullish/object pairs miss
//!   so the committed sibling can preserve HTMLDDA semantics.

use super::*;

pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    inputs: [u8; 2],
    outputs: [u8; 2],
    equal: bool,
) {
    let [left, right] = inputs;
    let [result, hit] = outputs;
    let identical = ops.new_dynamic_label();
    let integer = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    let nullish = ops.new_dynamic_label();
    let yes = ops.new_dynamic_label();
    let no = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    // The opcode declares no clobbers. Preserve the two temporary registers,
    // then snapshot both inputs before either temporary is overwritten.
    dynasm!(ops
        ; .arch x64
        ; push rax
        ; push r10
        ; push Rq(left)
        ; push Rq(right)
        ; mov rax, [rsp + 8]
        ; mov r10, [rsp]
        ; cmp rax, r10
        ; je =>identical
        ; mov r11, rax
        ; shr r11, 48
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; je =>integer
        ; test r11w, NUMBER_TAG_HI16 as i16
        ; jnz =>double
    );
    for value in [Value::null(), Value::undefined()] {
        load64(ops, 11, value.to_bits());
        dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>nullish);
    }
    load64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test rax, r11
        ; jnz =>miss
        ; test r10, r11
        ; jnz =>miss
    );
    if view.cage_base == 0 {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
    } else {
        for source in [0_u8, 10_u8] {
            // A tagged cell carries the cage offset in its low word; discard
            // the upper payload bits before adding the process-local base.
            dynasm!(ops ; .arch x64 ; mov Rd(source), Rd(source));
            symbolic(
                ops,
                relocations,
                11,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops
                ; .arch x64
                ; add r11, Rq(source)
                ; movzx r11d, BYTE [r11]
            );
            for primitive_tag in view.primitive_cell_type_tags {
                dynasm!(ops ; .arch x64 ; cmp r11d, i32::from(primitive_tag) ; je =>miss);
            }
        }
        dynasm!(ops ; .arch x64 ; jmp =>no);
    }

    dynasm!(ops
        ; .arch x64
        ; =>integer
        ; mov r11, r10
        ; shr r11, 48
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; je =>no
        ; jmp =>miss
        ; =>double
        ; mov r11, r10
        ; shr r11, 48
        ; test r11w, NUMBER_TAG_HI16 as i16
        ; jz =>miss
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; je =>miss
    );
    load64(ops, 11, DOUBLE_OFFSET);
    dynasm!(ops
        ; .arch x64
        ; btr rax, 63
        ; cmp rax, r11
        ; jne =>no
        ; btr r10, 63
        ; cmp r10, r11
        ; je =>yes
        ; jmp =>no
        ; =>nullish
    );
    for value in [Value::null(), Value::undefined()] {
        load64(ops, 11, value.to_bits());
        dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>yes);
    }
    dynasm!(ops ; .arch x64 ; jmp =>miss ; =>identical);
    load64(
        ops,
        11,
        Value::number(otter_vm::NumberValue::Double(f64::NAN)).to_bits(),
    );
    dynasm!(ops ; .arch x64 ; cmp rax, r11 ; je =>no ; =>yes);
    restore_temporaries(ops);
    load64(ops, result, Value::boolean(equal).to_bits());
    dynasm!(ops ; .arch x64 ; mov Rd(hit), 1 ; jmp =>done ; =>no);
    restore_temporaries(ops);
    load64(ops, result, Value::boolean(!equal).to_bits());
    dynasm!(ops ; .arch x64 ; mov Rd(hit), 1 ; jmp =>done ; =>miss);
    restore_temporaries(ops);
    load64(ops, result, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch x64 ; xor Rd(hit), Rd(hit) ; =>done);
}

fn restore_temporaries(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch x64
        ; add rsp, 16
        ; pop r10
        ; pop rax
    );
}
