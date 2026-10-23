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
//! - Only explicit outputs/FP scratches and reserved x16/x17 change.
//! - Neither projection proves the physical Int32 tag of its input Value.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::value::tag::{DOUBLE_ENCODE_OFFSET, NUMBER_TAG};

/// Project an exact signed Int32 Number, excluding negative zero. Only the
/// explicit destination/roundtrip FP register and x16/x17 change; the FP
/// source survives. The destination is written only after every guard passes.
pub(crate) fn emit_float64_int32(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    roundtrip: u8,
    miss: DynamicLabel,
) {
    debug_assert_ne!(source, roundtrip);
    if crate::arm64::has_javascript_conversion() {
        crate::arm64::emit_fjcvtzs(ops, source, 16);
        dynasm!(ops ; .arch aarch64 ; b.ne =>miss ; mov W(destination), w16);
        return;
    }
    let nonzero = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; fcvtzs w16, D(source) ; scvtf D(roundtrip), w16
        ; fcmp D(source), D(roundtrip) ; b.ne =>miss
        ; cbnz w16, =>nonzero ; fmov x17, D(source)
        ; tbnz x17, #63, =>miss ; =>nonzero
        ; mov W(destination), w16);
}

/// Project either legal Number encoding to an exact signed Int32. Unlike
/// element-index projection, -0 is refused. The boxed source survives a miss
/// and, when distinct from the destination, a hit. Only `fp`, d31, x16/x17
/// and the destination may change; no physical Int32-tag proof is produced.
pub(crate) fn emit_exact_int32(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    fp: u8,
    miss: DynamicLabel,
) {
    debug_assert!(![16, 17].contains(&source));
    debug_assert_ne!(fp, 31);
    let integer = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; asr x16, X(source), #49 ; cmn x16, #1 ; b.eq =>integer
        ; movz x16, (NUMBER_TAG >> 48) as u32, lsl #48
        ; tst X(source), x16 ; b.eq =>miss
        ; movz x17, (DOUBLE_ENCODE_OFFSET >> 48) as u32, lsl #48
        ; sub x17, X(source), x17 ; fmov D(fp), x17);
    emit_float64_int32(ops, fp, destination, 31, miss);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>integer
        ; mov W(destination), W(source) ; =>done);
}

pub(crate) fn emit_number_index(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    fp: u8,
    miss: DynamicLabel,
) {
    debug_assert_ne!(source, destination);
    debug_assert!(![16, 17].contains(&source) && ![16, 17].contains(&destination));
    let integer = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; asr x16, X(source), #49 ; cmn x16, #1 ; b.eq =>integer
        ; movz x16, (NUMBER_TAG >> 48) as u32, lsl #48
        ; tst X(source), x16 ; b.eq =>miss
        ; movz x16, (DOUBLE_ENCODE_OFFSET >> 48) as u32, lsl #48
        ; sub x16, X(source), x16 ; fmov D(fp), x16
        ; fcvtzs W(destination), D(fp)
        ; tbnz W(destination), #31, =>miss
        ; scvtf D(fp), W(destination) ; fmov x17, D(fp)
        // A nonnegative exact round trip differs only for -0. Clearing its
        // sign also leaves negative fractions distinguishable from zero.
        ; and x16, x16, #0x7fff_ffff_ffff_ffff
        ; cmp x16, x17 ; b.ne =>miss ; b =>done
        ; =>integer
        ; mov W(destination), W(source) ; tbnz W(destination), #31, =>miss
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
        dynasm!(ops ; .arch aarch64 ; mov x3, x0 ; mov x5, x1 ; fmov d0, x1);
        emit_exact_int32(&mut ops, 0, 2, 4, miss);
        dynasm!(ops ; .arch aarch64 ; b =>check ; =>miss ; movn x2, #0
            ; =>check ; cmp x0, x3 ; b.ne =>corrupt ; cmp x1, x5 ; b.ne =>corrupt
            ; fmov x6, d0 ; cmp x6, x5 ; b.ne =>corrupt ; mov x0, x2 ; ret
            ; =>corrupt ; movn x0, #1 ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
        // SAFETY: a noncollecting scalar leaf with two word inputs.
        let run: extern "C" fn(u64, u64) -> u64 = unsafe { std::mem::transmute(code.entry_ptr()) };
        let mut aliased = Assembler::new().unwrap();
        let alias_entry = aliased.offset();
        let alias_miss = aliased.new_dynamic_label();
        emit_exact_int32(&mut aliased, 0, 0, 4, alias_miss);
        dynasm!(aliased ; .arch aarch64 ; =>alias_miss ; ret);
        let alias_code = crate::CompiledCode::new(aliased.finalize().unwrap(), alias_entry);
        // SAFETY: the same leaf, reusing a dead input register for its result.
        let run_alias: extern "C" fn(u64) -> u64 =
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
        dynasm!(ops ; .arch aarch64 ; mov x3, x0 ; mov x5, x1 ; fmov d0, x1);
        emit_number_index(&mut ops, 0, 2, 4, miss);
        dynasm!(ops ; .arch aarch64 ; b =>check ; =>miss ; movn x2, #0
            ; =>check ; cmp x0, x3 ; b.ne =>corrupt ; cmp x1, x5 ; b.ne =>corrupt
            ; fmov x6, d0 ; cmp x6, x5 ; b.ne =>corrupt ; mov x0, x2 ; ret
            ; =>corrupt ; movn x0, #1 ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
        // SAFETY: a noncollecting scalar leaf with two word inputs.
        let run: extern "C" fn(u64, u64) -> u64 = unsafe { std::mem::transmute(code.entry_ptr()) };
        for (value, expected) in cases() {
            assert_eq!(run(value.to_bits(), 0x1234_5678_9abc_def0), expected);
        }
    }

    fn cases() -> Vec<(Value, u64)> {
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
        cases
    }
}
