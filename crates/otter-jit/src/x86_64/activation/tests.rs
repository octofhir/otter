//! Executed unsigned-tag and function-id receiver-classification regressions.
//!
//! # Contents
//! - Exact primitive-byte comparisons on both sides of the signed-imm8 boundary.
//! - Full native object classification for cell bytes, immediates and function ids.
//!
//! # Invariants
//! - Each mapping calls the production emitter and executes its returned branch.
//! - Aligned local header words exercise byte classification only; no allocation,
//!   moving-GC or VM object-lifetime claim is inferred from these fixtures.

use super::*;

#[test]
fn native_primitive_tag_comparisons_preserve_all_unsigned_bytes() {
    for expected in [0_u8, 1, 34, 127, 128, 255] {
        let mut ops = Assembler::new().unwrap();
        let entry = ops.offset();
        dynasm!(ops ; .arch x64 ; mov r10d, edi);
        let start = ops.offset().0;
        emit_compare_primitive_tag(&mut ops, expected);
        let comparison_bytes = ops.offset().0 - start;
        assert_eq!(comparison_bytes, if expected <= 127 { 4 } else { 7 });
        dynasm!(ops ; .arch x64 ; sete al ; movzx eax, al ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
        // SAFETY: this mapping takes one unsigned integer, returns a Boolean,
        // and touches only caller-saved registers with no memory or call edge.
        let run: extern "sysv64" fn(u32) -> u32 = unsafe { std::mem::transmute(code.entry_ptr()) };
        for actual in 0..=255_u32 {
            assert_eq!(run(actual), u32::from(actual == u32::from(expected)));
        }
    }
}

#[test]
fn native_object_test_preserves_low_word_function_identity_and_unsigned_cell_tags() {
    let mut view = JitCompileSnapshot::without_feedback(0, 0, 0, vec![]);
    view.primitive_cell_type_tags = [1, 127, 255];
    for value_register in [0_u8, 2, 6, 8] {
        let mut ops = Assembler::new().unwrap();
        let entry = ops.offset();
        let object = ops.new_dynamic_label();
        let primitive = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; mov Rq(value_register), rdi);
        let object_test_start = ops.offset().0;
        emit_object_test(&mut ops, &view, value_register, object, primitive);
        assert_eq!(
            ops.offset().0 - object_test_start,
            79,
            "the full exact classifier uses compact tag tests"
        );
        dynasm!(ops ; .arch x64
            ; =>object
            ; cmp Rq(value_register), rdi
            ; jne >changed_source
            ; mov eax, 1
            ; ret
            ; =>primitive
            ; cmp Rq(value_register), rdi
            ; jne >changed_source
            ; xor eax, eax
            ; ret
            ; changed_source:
            ; mov eax, 2
            ; ret
        );
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
        // SAFETY: non-cell inputs cannot reach the header read; every cell
        // input below points to a live, aligned local header word. No external
        // call, allocation, stack reservation or callee-saved clobber occurs.
        let run: extern "sysv64" fn(u64) -> u32 = unsafe { std::mem::transmute(code.entry_ptr()) };
        for id in [0_u32, 1, 0xffff, u32::MAX] {
            assert_eq!(run(tag::box_function_id(id)), 1);
            assert_eq!(run(tag::NUMBER_TAG | tag::box_function_id(id)), 0);
        }
        // Every NUMBER_TAG bit rejects a low-word function-id collision.
        // The adjacent bit 48 alone remains outside NUMBER_TAG, exactly as
        // the VM predicate specifies; neither path may read a cell header.
        for bit in 49..64 {
            assert_eq!(run((1_u64 << bit) | tag::FUNCTION_ID_TAG), 0);
        }
        assert_eq!(run((1_u64 << 48) | tag::FUNCTION_ID_TAG), 1);
        for bits in [
            tag::CANONICAL_NAN.wrapping_add(tag::DOUBLE_ENCODE_OFFSET),
            tag::box_double((-0.0_f64).to_bits()),
            tag::VALUE_NULL,
            tag::VALUE_UNDEFINED,
            tag::VALUE_TRUE,
            tag::VALUE_FALSE,
            tag::VALUE_HOLE,
            tag::box_int32(34),
            tag::box_int32(-1),
            tag::box_double(1.25_f64.to_bits()),
        ] {
            assert_eq!(run(bits), 0);
        }
        for actual in 0..=255_u8 {
            let header = [u64::from(actual), 0_u64];
            let pointer = header.as_ptr() as u64;
            assert_eq!(pointer & tag::NOT_CELL_MASK, 0);
            assert_eq!(
                run(pointer),
                u32::from(!view.primitive_cell_type_tags.contains(&actual))
            );
        }
    }
}
