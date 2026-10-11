//! Executable proof of live Array named-storage projection.
//!
//! # Contents
//! - Actual receiver-tag and nullable edge checks before the storage read.
//! - A changed compressed edge is read live without replacing the receiver.
//!
//! # Invariants
//! - Aligned host cells are a bounded machine-code fixture, never GC objects.
//! - Both cells share one address cage; no collector or helper can run.
//!
//! # See also
//! - `super::emit_array_expando` owns the production projection.

use super::*;
use crate::CompiledCode;

#[path = "../property_projection_tests.rs"]
mod fixture;

#[test]
fn projected_store_returns_storage_owner_and_misses_preserve_source() {
    use fixture::Invocation;
    let model = fixture::Fixture::new();
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let stored = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let mut relocations = crate::artifact::relocation::RelocationCapture::default();
    dynasm!(ops ; .arch aarch64
        ; mov x6, x0
        ; ldr x0, [x6, std::mem::offset_of!(Invocation, receiver) as u32]
        ; ldr x1, [x6, std::mem::offset_of!(Invocation, value) as u32]
        ; ldr x2, [x6, std::mem::offset_of!(Invocation, slot) as u32]);
    emit_slot_store(
        &mut ops,
        &mut relocations,
        &model.view,
        0,
        1,
        2,
        [3, 4, 5],
        miss,
        miss,
        stored,
    );
    dynasm!(ops ; .arch aarch64
        ; =>stored ; mov x7, #1 ; b =>done
        ; =>miss ; mov x7, xzr
        ; =>done
        ; str x0, [x6, std::mem::offset_of!(Invocation, returned_receiver) as u32]
        ; str x2, [x6, std::mem::offset_of!(Invocation, returned_slot) as u32]
        ; str x5, [x6, std::mem::offset_of!(Invocation, child) as u32]
        ; mov x0, x7 ; ret);
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the fixture uses initialized host cells, a zero observation owner,
    // and the generated C signature; no helper or collection can run.
    let call: extern "C" fn(*mut Invocation) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    fixture::check_store_contract(|invocation| call(invocation));
}

#[test]
fn array_expando_projection_reads_live_edge_and_preserves_source_receiver() {
    let mut words = [0u64; 8];
    let array = words.as_mut_ptr() as usize;
    let bag_a = array + 32;
    let bag_b = array + 48;
    assert_eq!(array >> 32, bag_b >> 32);
    words[0] = u64::from(otter_vm::array::ARRAY_BODY_TYPE_TAG);
    words[1] = bag_a as u32 as u64;
    let layout = otter_vm::jit::JitArrayLayout {
        type_tag: otter_vm::array::ARRAY_BODY_TYPE_TAG,
        length_byte: 16,
        expando_byte: 8,
    };
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_array_expando(&mut ops, layout, 0, 2, miss);
    dynasm!(ops ; .arch aarch64 ; mov x0, x2 ; b =>done ; =>miss ; mov x0, xzr ; =>done ; ret);
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the generated C fixture reads only initialized bounded host cells.
    let call: extern "C" fn(usize) -> usize = unsafe { std::mem::transmute(code.entry_ptr()) };
    assert_eq!(call(std::hint::black_box(&words).as_ptr() as usize), bag_a);
    words[1] = bag_b as u32 as u64;
    assert_eq!(call(std::hint::black_box(&words).as_ptr() as usize), bag_b);
    words[1] = 0;
    assert_eq!(call(std::hint::black_box(&words).as_ptr() as usize), 0);
    words[1] = bag_a as u32 as u64;
    words[0] = crate::entry::OBJECT_BODY_TYPE_TAG as u64;
    assert_eq!(
        call(std::hint::black_box(&words).as_ptr() as usize),
        0,
        "an ordinary object never supplies an Array bag"
    );
    assert_eq!(call(0), 0);
    assert_eq!(call(crate::entry::VALUE_UNDEFINED as usize), 0);
}
