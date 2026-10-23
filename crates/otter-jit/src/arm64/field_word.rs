//! Explicit-register operations on the engine's Value-valid numeric words.
//!
//! # Contents
//! - Pure Double encoding, including integral values, with NaN purification.
//! - Summary-proven Int32/Double decoding into a floating-point register.
//!
//! # Invariants
//! - Int32 and integral Double are both legal Values in every value location.
//! - Numeric loads require a live defining field-summary proof.
//! - Encoding preserves the FP source and uses only explicit GP scratches.
//! - Helpers never call, allocate, write the heap or overwrite implicit registers.
//!
//! # See also
//! - `otter_vm::Value::double` owns the matching VM producer.
//! - `crate::field_store` owns physical summary observation membership.

use dynasmrt::{DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::value::tag::{CANONICAL_NAN, DOUBLE_ENCODE_OFFSET};

pub(crate) fn emit_encode_double(ops: &mut Assembler, source: u8, destination: u8, scratch: u8) {
    debug_assert_ne!(destination, scratch);
    let ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; fmov X(destination), D(source)
        ; fcmp D(source), D(source)
        ; b.vc =>ready
        ; movz X(destination), (CANONICAL_NAN >> 48) as u32, lsl #48
        ; =>ready
        ; movz X(scratch), (DOUBLE_ENCODE_OFFSET >> 48) as u32, lsl #48
        ; add X(destination), X(destination), X(scratch)
    );
}

/// Decode a Number under its live physical summary proof.
pub(crate) fn emit_decode_number(
    ops: &mut Assembler,
    source: u8,
    destination: u8,
    scratch: u8,
    double_only: bool,
) {
    debug_assert_ne!(source, scratch);
    let done = ops.new_dynamic_label();
    if !double_only {
        let double = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64
            ; asr X(scratch), X(source), #49 ; cmn XSP(scratch), #1
            ; b.ne =>double ; scvtf D(destination), W(source) ; b =>done
            ; =>double);
    }
    dynasm!(ops ; .arch aarch64
        ; movz X(scratch), (DOUBLE_ENCODE_OFFSET >> 48) as u32, lsl #48
        ; sub X(scratch), X(source), X(scratch)
        ; fmov D(destination), X(scratch)
        ; =>done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::Value;

    #[test]
    fn native_field_word_double_encoding_preserves_source_and_numeric_decode() {
        let mut ops = Assembler::new().unwrap();
        let entry = ops.offset();
        dynasm!(ops ; .arch aarch64 ; mov x2, x0 ; fmov d4, x0);
        emit_encode_double(&mut ops, 4, 0, 16);
        let done = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; fmov x1, d4 ; cmp x1, x2 ; b.eq =>done
            ; movz x0, 10 ; =>done ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
        // SAFETY: this leaf receives IEEE bits and returns one Value word.
        let run: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(code.entry_ptr()) };
        for number in numbers() {
            assert_eq!(run(number.to_bits()), Value::double(number).to_bits());
        }
        for double_only in [false, true] {
            let mut ops = Assembler::new().unwrap();
            let entry = ops.offset();
            emit_decode_number(&mut ops, 0, 0, 16, double_only);
            dynasm!(ops ; .arch aarch64 ; fmov x0, d0 ; ret);
            let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
            // SAFETY: this leaf receives a summary-proven numeric Value.
            let run: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(code.entry_ptr()) };
            for number in numbers() {
                let words = [Value::double(number), Value::number_f64(number)];
                for value in words.into_iter().take(if double_only { 1 } else { 2 }) {
                    let actual = f64::from_bits(run(value.to_bits()));
                    assert!(
                        actual.is_nan() && number.is_nan() || actual.to_bits() == number.to_bits()
                    );
                }
            }
        }
    }
    fn numbers() -> [f64; 12] {
        [
            0.0,
            -0.0,
            7.0,
            -31.0,
            f64::from(i32::MIN),
            f64::from(i32::MAX),
            1.25,
            2147483648.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from_bits(0xfff8_0012_3456_7890),
        ]
    }
}
