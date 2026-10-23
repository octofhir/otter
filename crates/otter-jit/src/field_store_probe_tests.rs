//! Executable scalar field-observation probes over owned metadata fixtures.
//!
//! # Contents
//! - One scalar widening matrix shared by both native encoders.
//! - Dynamic owner words and valid captured-constant histories.
//!
//! # Invariants
//! - Fixture watch bytes are clear, so scalar probes never enter a VM leaf.
//! - Owned aligned storage remains live throughout each noncollecting call.
//! - Captured facts precede the live word in the monotone observation history.
//!
//! # See also
//! - `super::FieldStoreOwner` owns captured membership.
//! - `otter_vm::object::field_type` owns the canonical scalar contract.

use super::FieldStoreOwner;
use crate::artifact::relocation::RelocationCapture;
use dynasmrt::{AssemblyOffset, DynasmApi, ExecutableBuffer, dynasm};
use otter_vm::object::field_type::{self, DOUBLE, INT32, NULL, TOP, UNDEFINED};
use otter_vm::{JitCompileSnapshot, Value};

#[cfg(target_arch = "aarch64")]
fn compile(view: &JitCompileSnapshot, captured: Option<&FieldStoreOwner>) -> ExecutableBuffer {
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    match captured {
        Some(record) => crate::arm64::field_type::emit_observe_known(
            &mut ops,
            &mut relocations,
            view,
            0,
            1,
            record,
        ),
        None => crate::arm64::field_type::emit_observe(&mut ops, &mut relocations, view, 0, 1),
    }
    dynasm!(ops ; .arch aarch64
        ; ldr x0, [x2, view.shape_field_type_byte] ; ret);
    ops.finalize().unwrap()
}

#[cfg(target_arch = "x86_64")]
fn compile(view: &JitCompileSnapshot, captured: Option<&FieldStoreOwner>) -> ExecutableBuffer {
    let mut ops = dynasmrt::x64::Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    match captured {
        Some(record) => crate::x86_64::field_type::emit_observe_known(
            &mut ops,
            &mut relocations,
            view,
            7,
            6,
            record,
        ),
        None => crate::x86_64::field_type::emit_observe(&mut ops, &mut relocations, view, 7, 6),
    }
    dynasm!(ops ; .arch x64
        ; mov rax, [rdx + view.shape_field_type_byte as i32] ; ret);
    ops.finalize().unwrap()
}

#[test]
fn native_scalar_probes_preserve_constants_and_erase_payload_on_widening() {
    let mut storage = vec![0_u64; 256];
    let address = storage.as_mut_ptr() as u64;
    let owner = address as u32;
    assert_ne!(owner, 0);
    let mut view = JitCompileSnapshot::without_feedback(0, 0, 0, vec![]);
    view.cage_base = (address & !u64::from(u32::MAX)) as usize;
    // Isolated snapshots carry no baked offsets. Give the fixture distinct
    // summary and watch locations before executing either native encoder.
    view.shape_field_type_byte = 16;
    view.shape_watched_byte = 24;
    assert!((view.shape_field_type_byte as usize) + 8 <= storage.len() * 8);
    assert!((view.shape_watched_byte as usize) < storage.len() * 8);
    let constant = field_type::int32_sample;
    let cases = [
        (0, Value::number_i32(7), constant(7)),
        (0, Value::number_i32(i32::MIN), constant(i32::MIN)),
        (0, Value::number_i32(i32::MAX), constant(i32::MAX)),
        (constant(7), Value::number_i32(7), constant(7)),
        (constant(7), Value::number_i32(8), INT32),
        (INT32, Value::number_i32(7), INT32),
        (constant(i32::MIN), Value::number_i32(i32::MAX), INT32),
        (
            constant(i32::MAX),
            Value::number_i32(i32::MAX),
            constant(i32::MAX),
        ),
        (constant(7), Value::NULL, INT32 | NULL),
        (constant(i32::MIN), Value::UNDEFINED, INT32 | UNDEFINED),
        (NULL, Value::number_i32(i32::MIN), INT32 | NULL),
        (
            NULL | UNDEFINED,
            Value::number_i32(i32::MAX),
            INT32 | NULL | UNDEFINED,
        ),
        (constant(7), Value::number_f64(-0.0), INT32 | DOUBLE),
        (constant(7), Value::number_f64(3.5), INT32 | DOUBLE),
        (0, otter_vm::Value::double(0.0), DOUBLE),
        (DOUBLE, Value::number_i32(7), INT32 | DOUBLE),
        (DOUBLE, Value::number_f64(f64::NAN), DOUBLE),
        (DOUBLE, otter_vm::Value::double(0.0), DOUBLE),
        (TOP, Value::number_i32(7), TOP),
    ];
    for (captured, captured_double) in [
        (None, false),
        (Some(7), false),
        (Some(i32::MIN), false),
        (Some(i32::MAX), false),
        (None, true),
    ] {
        let record = (captured.is_some() || captured_double).then(|| FieldStoreOwner {
            owner_shape: owner,
            stored_shapes: smallvec::SmallVec::new(),
            nullish_mask: 0,
            int32: captured.is_some(),
            double: captured_double,
            int32_constant: captured,
        });
        let code = compile(&view, record.as_ref());
        // SAFETY: the native probe uses the platform's three integer argument
        // registers and returns one word. Every reached path is noncollecting.
        #[cfg(target_arch = "aarch64")]
        let probe: unsafe extern "C" fn(u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(code.ptr(AssemblyOffset(0))) };
        #[cfg(target_arch = "x86_64")]
        let probe: unsafe extern "sysv64" fn(u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(code.ptr(AssemblyOffset(0))) };
        for (initial, value, expected) in cases {
            if captured_double && initial & DOUBLE == 0 {
                continue;
            }
            if captured.is_some_and(|value| field_type::merge(constant(value), initial) != initial)
            {
                continue;
            }
            storage.fill(0);
            // SAFETY: the aligned summary word lies in this owned fixture.
            unsafe {
                ((address + u64::from(view.shape_field_type_byte)) as *mut u64).write(initial);
            }
            // SAFETY: scalar inputs with clear watch bits stay in generated
            // code and only touch this initialized fixture; no VM entry occurs.
            assert_eq!(
                unsafe { probe(u64::from(owner), value.to_bits(), address) },
                expected,
                "captured={captured:?}, initial={initial:#x}, value={value:?}"
            );
        }
    }
}
