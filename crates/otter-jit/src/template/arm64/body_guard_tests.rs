//! Executable proofs of whole-lookup words and numeric element indices.
//!
//! # Contents
//! - One aligned word read rejects either compressed handle while preserving
//!   the original receiver body.
//! - Numeric index projection retains its validated base and original key.
//!
//! # Invariants
//! - The read stays inside one initialized host word and never calls or collects.
//! - Either 32-bit edge changing makes the proof miss before any effect.
//! - Integral Double keys and signed zero use the same addressed element as Int32.
//!
//! # See also
//! - `super::emit_body_guard` owns the production body reads.
//! - `super::emit_element_address_from_dense_view` owns element addressing.

use super::*;

#[test]
fn whole_lookup_null_word_rejects_either_compressed_edge() {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x13, x0);
    let guard_start = ops.offset().0;
    emit_body_guard(
        &mut ops,
        13,
        JitBodyGuard::clear(8, JitGuardWidth::Word64),
        miss,
    );
    assert_eq!(
        ops.offset().0 - guard_start,
        8,
        "one load and one null branch"
    );
    dynasm!(ops ; .arch aarch64
        ; mov x0, x13 ; b =>done
        ; =>miss ; mov x0, xzr ; =>done ; ret);
    let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the C fixture reads one aligned initialized word inside `body`.
    let call: extern "C" fn(*const u64) -> usize = unsafe { std::mem::transmute(code.entry_ptr()) };
    let mut body = [u64::MAX, 0, u64::MAX];
    assert_eq!(
        call(std::hint::black_box(&body).as_ptr()),
        body.as_ptr() as usize
    );
    for edges in [1, 1 << 31, 1 << 32, 1 << 63, (1 << 32) | 1] {
        body[1] = edges;
        assert_eq!(
            call(std::hint::black_box(&body).as_ptr()),
            0,
            "either compressed edge must make the complete null word miss"
        );
    }
    body[1] = 0;
    assert_eq!(
        call(std::hint::black_box(&body).as_ptr()),
        body.as_ptr() as usize
    );
}

#[test]
fn template_element_address_accepts_numeric_indices_and_preserves_miss_key() {
    use otter_vm::Value;

    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let check = ops.new_dynamic_label();
    let corrupt = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; mov x16, x0 ; mov x14, x1 ; mov x4, x2 ; fmov d0, x2);
    emit_element_address_from_dense_view(
        &mut ops,
        &JitElementAccess {
            length_width: JitGuardWidth::Word64,
            element: JitElementRepr::Boxed,
            ..Default::default()
        },
        |ops, register| {
            dynasm!(ops ; .arch aarch64 ; mov X(register), x2);
            Ok(())
        },
        miss,
    )
    .unwrap();
    dynasm!(ops ; .arch aarch64
        ; mov x0, x16 ; b =>check
        ; =>miss ; mov x0, xzr
        ; =>check
        ; str x15, [x3]
        ; cmp x2, x4 ; b.ne =>corrupt
        ; fmov x5, d0 ; cmp x5, x4 ; b.ne =>corrupt
        ; ret
        ; =>corrupt ; movn x0, #0 ; ret);
    let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the leaf computes one in-range address or returns zero. The
    // caller owns the initialized output slot; it neither calls nor collects.
    let call: extern "C" fn(*const u64, u64, u64, *mut u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let elements = [17_u64, 31, 47, 71];
    for (key, index) in [
        (Value::number_i32(0), Some(0)),
        (Value::double(0.0), Some(0)),
        (Value::double(-0.0), Some(0)),
        (Value::number_i32(3), Some(3)),
        (Value::double(3.0), Some(3)),
        (Value::double(4.0), None),
        (Value::number_i32(-1), None),
        (Value::double(-1.0), None),
        (Value::double(-0.5), None),
        (Value::double(0.5), None),
        (Value::double(2147483648.0), None),
        (Value::double(f64::NAN), None),
        (Value::double(f64::INFINITY), None),
        (Value::undefined(), None),
        (Value::TRUE, None),
    ] {
        let mut preserved = 0;
        let actual = call(
            elements.as_ptr(),
            elements.len() as u64,
            key.to_bits(),
            &mut preserved,
        );
        let expected = index.map_or(0, |index| elements.as_ptr().wrapping_add(index) as usize);
        assert_eq!(actual, expected, "key {key:?}");
        assert_eq!(preserved, key.to_bits(), "committed miss retains its key");
    }
}
