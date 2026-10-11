//! Executable proof of one complete whole-lookup null word.
//!
//! # Contents
//! - One aligned word read rejects either compressed handle while preserving
//!   the original receiver body.
//!
//! # Invariants
//! - The read stays inside one initialized host word and never calls or collects.
//! - Either 32-bit edge changing makes the proof miss before any effect.
//!
//! # See also
//! - `super::emit_body_guard` owns the production body reads.

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
