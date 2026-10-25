//! Exact numeric integer projections shared by native JIT tiers.
//!
//! # Contents
//! - [`emit_number_index`] accepts either legal numeric Value encoding.
//! - [`emit_exact_int32`] and [`emit_float64_int32`] preserve semantic Int32 facts.
//!
//! # Invariants
//! - Index projection accepts `0..=i32::MAX`, treating negative zero as zero.
//! - Signed projection accepts the full Int32 range and rejects negative zero.
//! - The original key survives misses and hits with a distinct destination.
//! - Only explicit outputs/FP scratches and reserved r10/r11 change.
//! - Neither projection proves the physical Int32 tag of its input Value.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::value::tag::{DOUBLE_ENCODE_OFFSET, NUMBER_TAG};

/// Project an exact signed Int32 Number, excluding negative zero. Only the
/// explicit destination/roundtrip FP register and r10/r11 change. The source
/// survives and the destination is written only after every guard passes.
pub(crate) fn emit_float64_int32(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    roundtrip: u8,
    miss: DynamicLabel,
) {
    debug_assert_ne!(source, roundtrip);
    let nonzero = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; cvttsd2si r10d, Rx(source) ; cvtsi2sd Rx(roundtrip), r10d
        ; ucomisd Rx(source), Rx(roundtrip) ; jp =>miss ; jne =>miss
        ; test r10d, r10d ; jnz =>nonzero
        ; movq r11, Rx(source) ; test r11, r11 ; js =>miss
        ; =>nonzero ; mov Rd(destination), r10d);
}

/// Project either legal Number encoding to an exact signed Int32. Unlike
/// element-index projection, -0 is refused. The boxed source survives a miss
/// and, when distinct from the destination, a hit. Only `fp`, xmm15, r10/r11
/// and the destination may change; no physical Int32-tag proof is produced.
pub(crate) fn emit_exact_int32(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    fp: u8,
    miss: DynamicLabel,
) {
    debug_assert!(![10, 11].contains(&source));
    debug_assert_ne!(fp, 15);
    let integer = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r10, Rq(source) ; sar r10, 49 ; cmp r10, -1 ; je =>integer
        ; mov r10, QWORD NUMBER_TAG as i64 ; test Rq(source), r10 ; jz =>miss
        ; mov r10, QWORD DOUBLE_ENCODE_OFFSET as i64
        ; neg r10 ; add r10, Rq(source) ; movq Rx(fp), r10);
    emit_float64_int32(ops, fp, destination, 15, miss);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>integer
        ; mov Rd(destination), Rd(source) ; =>done);
}

pub(crate) fn emit_number_index(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    fp: u8,
    miss: DynamicLabel,
) {
    debug_assert_ne!(source, destination);
    debug_assert!(![10, 11].contains(&source) && ![10, 11].contains(&destination));
    let integer = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r10, Rq(source) ; sar r10, 49 ; cmp r10, -1 ; je =>integer
        ; mov r10, QWORD NUMBER_TAG as i64 ; test Rq(source), r10 ; jz =>miss
        ; mov r10, QWORD DOUBLE_ENCODE_OFFSET as i64
        ; neg r10 ; add r10, Rq(source) ; movq Rx(fp), r10
        ; cvttsd2si Rd(destination), Rx(fp)
        ; test Rd(destination), Rd(destination) ; js =>miss
        ; cvtsi2sd Rx(fp), Rd(destination) ; movq r11, Rx(fp)
        ; btr r10, 63 ; cmp r10, r11 ; jne =>miss ; jmp =>done
        ; =>integer ; mov Rd(destination), Rd(source)
        ; test Rd(destination), Rd(destination) ; js =>miss
        ; =>done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::Value;

    #[test]
    fn native_exact_number_int32_accepts_double_and_preserves_source() {
        let mut ops = Assembler::new().unwrap();
        let entry = ops.offset();
        let miss = ops.new_dynamic_label();
        let check = ops.new_dynamic_label();
        let corrupt = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; mov r8, rdi ; mov r9, rsi ; movq xmm0, rsi);
        emit_exact_int32(&mut ops, 7, 2, 4, miss);
        dynasm!(ops ; .arch x64 ; jmp =>check ; =>miss ; mov rdx, -1
            ; =>check ; cmp rdi, r8 ; jne =>corrupt ; cmp rsi, r9 ; jne =>corrupt
            ; movq rax, xmm0 ; cmp rax, r9 ; jne =>corrupt ; mov rax, rdx ; ret
            ; =>corrupt ; mov rax, -2 ; ret);
        let code =
            crate::CompiledCode::new(&crate::CodeMemory::new(), ops.finalize().unwrap(), entry)
                .unwrap();
        // SAFETY: a noncollecting scalar leaf with two word inputs.
        let run: extern "sysv64" fn(u64, u64) -> u64 =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        let mut aliased = Assembler::new().unwrap();
        let alias_entry = aliased.offset();
        let alias_miss = aliased.new_dynamic_label();
        emit_exact_int32(&mut aliased, 7, 7, 4, alias_miss);
        dynasm!(aliased ; .arch x64 ; =>alias_miss ; mov rax, rdi ; ret);
        let alias_code = crate::CompiledCode::new(
            &crate::CodeMemory::new(),
            aliased.finalize().unwrap(),
            alias_entry,
        )
        .unwrap();
        // SAFETY: the same leaf, reusing a dead input register for its result.
        let run_alias: extern "sysv64" fn(u64) -> u64 =
            unsafe { std::mem::transmute(alias_code.entry_ptr()) };
        let mut cases = Vec::new();
        for number in [i32::MIN, -71, -1, 0, 71, i32::MAX] {
            let expected = u64::from(number as u32);
            cases.push((Value::number_i32(number), expected));
            cases.push((Value::double(f64::from(number)), expected));
        }
        for number in [
            -0.0,
            -0.5,
            0.5,
            -2147483649.0,
            2147483648.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            cases.push((Value::double(number), u64::MAX));
        }
        cases.extend([
            (Value::undefined(), u64::MAX),
            (Value::null(), u64::MAX),
            (Value::boolean(true), u64::MAX),
            (Value::from_bits(0x1000), u64::MAX),
        ]);
        for (value, expected) in cases {
            assert_eq!(run(value.to_bits(), 0x1234_5678_9abc_def0), expected);
            assert_eq!(
                run_alias(value.to_bits()),
                if expected == u64::MAX {
                    value.to_bits()
                } else {
                    expected
                },
            );
        }
    }

    #[test]
    fn native_number_index_accepts_double_and_preserves_key_on_miss() {
        let mut ops = Assembler::new().unwrap();
        let entry = ops.offset();
        let miss = ops.new_dynamic_label();
        let check = ops.new_dynamic_label();
        let corrupt = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; mov r8, rdi ; mov r9, rsi ; movq xmm0, rsi);
        emit_number_index(&mut ops, 7, 2, 4, miss);
        dynasm!(ops ; .arch x64 ; jmp =>check ; =>miss ; mov rdx, -1
            ; =>check ; cmp rdi, r8 ; jne =>corrupt ; cmp rsi, r9 ; jne =>corrupt
            ; movq rax, xmm0 ; cmp rax, r9 ; jne =>corrupt ; mov rax, rdx ; ret
            ; =>corrupt ; mov rax, -2 ; ret);
        let code =
            crate::CompiledCode::new(&crate::CodeMemory::new(), ops.finalize().unwrap(), entry)
                .unwrap();
        // SAFETY: a noncollecting scalar leaf with two word inputs.
        let run: extern "sysv64" fn(u64, u64) -> u64 =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        let mut cases = vec![(Value::number_i32(0), 0), (Value::number_i32(71), 71)];
        for number in [0.0, -0.0, 71.0, f64::from(i32::MAX)] {
            cases.push((Value::double(number), number as u64));
        }
        for number in [
            -1.0,
            -0.5,
            0.5,
            2147483648.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            cases.push((Value::double(number), u64::MAX));
        }
        cases.extend([
            (Value::number_i32(-1), u64::MAX),
            (Value::undefined(), u64::MAX),
            (Value::null(), u64::MAX),
            (Value::boolean(true), u64::MAX),
            (Value::from_bits(0x1000), u64::MAX),
        ]);
        for (value, expected) in cases {
            assert_eq!(run(value.to_bits(), 0x1234_5678_9abc_def0), expected);
        }
    }
}
