//! Dense spread staging parity, publication and moving-root ownership.
//!
//! # Contents
//! - Exact tagged words for numeric, tagged and holey dense prefixes.
//! - Reusable staging storage and unchanged request publication on errors.
//! - Collector rewriting of the staged actual window before native entry.
//!
//! # Invariants
//! - The old borrowed-slice view is the raw staging semantic oracle.
//! - No logical-length expansion, hole normalization or numeric re-encoding.
//! - Only scalar bits and offsets cross collection in test-owned observations.
//!
//! # See also
//! - `JitCtx::stage_spread_arguments` owns the noncollecting snapshot.
//! - `ActivationStack::trace_roots` owns the pending actual values.

use super::{JitCtx, Value, VmError, VmThread, context};
use crate::{ActivationStack, Interpreter, NativeCallInfo, NativeCtx, NativeError, array};

fn actual_bits(ctx: &JitCtx) -> Vec<u64> {
    // SAFETY: successful staging publishes initialized words retained by the
    // live ActivationStack. This read does not allocate in the VM or collect.
    let values = unsafe {
        std::slice::from_raw_parts(
            ctx.pending_call.arguments,
            ctx.pending_call.argument_count as usize,
        )
    };
    values.iter().map(|value| value.to_bits()).collect()
}

#[test]
fn spread_staging_preserves_dense_representation_bits_and_capacity() {
    let mut vm = Interpreter::new().unwrap();
    let mut stack = ActivationStack::new();
    vm.with_runtime_turn(&mut stack, |turn| {
        let (vm, stack) = turn.into_parts();
        for count in [0, 1, 8, 9, 256] {
            for kind in 0..4 {
                let values = (0..count).map(|index| match kind {
                    0 => Value::number_i32(index as i32 - 17),
                    1 => match index % 6 {
                        0 => Value::number_f64(-0.0),
                        1 => {
                            // Preserve a valid quiet-NaN payload on entry to
                            // numeric storage. Its ordinary read must purify
                            // it exactly like the borrowed-slice oracle.
                            const NAN_BITS: u64 = 0x7ff8_0000_0000_0317;
                            let value = Value::from_bits(crate::value::tag::box_double(NAN_BITS));
                            assert_eq!(value.as_f64().unwrap().to_bits(), NAN_BITS);
                            value
                        }
                        2 => Value::number_f64(f64::INFINITY),
                        3 => Value::number_f64(f64::NEG_INFINITY),
                        4 => Value::number_f64(f64::from_bits(1)),
                        _ => Value::number_f64(index as f64 + 0.25),
                    },
                    2 => {
                        if index % 3 == 0 {
                            Value::hole()
                        } else {
                            Value::number_f64(-0.25)
                        }
                    }
                    _ => match index % 3 {
                        0 => Value::boolean(true),
                        1 => Value::hole(),
                        _ => Value::UNDEFINED,
                    },
                });
                let array = array::from_elements_old_for_fixture(vm.gc_heap_mut(), values).unwrap();
                let expected = array::with_elements(array, vm.gc_heap(), |values| {
                    values
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>()
                });
                assert_eq!(expected.len(), count);
                vm.gc_heap().read_payload(array, |body| {
                    assert!(body.element_cache_is_current());
                    if count != 0 {
                        assert_eq!(
                            body.dense_kind(),
                            match kind {
                                0 | 1 => array::DenseElementKind::PackedDouble,
                                2 => array::DenseElementKind::HoleyDouble,
                                _ => array::DenseElementKind::Tagged,
                            }
                        );
                    }
                });
                let mut activation = crate::VmRuntimeActivation::new(vm, stack, None);
                let mut thread = VmThread::empty();
                thread.runtime_context = std::ptr::from_mut(&mut activation) as u64;
                let mut error = None;
                let mut ctx = context(&mut thread, &mut error);
                ctx.stage_spread_arguments(Value::array(array)).unwrap();
                assert_eq!(actual_bits(&ctx), expected, "kind={kind}, count={count}");
                let buffer = ctx.pending_call.arguments;
                ctx.stage_spread_arguments(Value::array(array)).unwrap();
                assert_eq!(
                    ctx.pending_call.arguments, buffer,
                    "staging capacity is reused"
                );
                assert_eq!(actual_bits(&ctx), expected);
            }
        }
    });
}

#[test]
fn spread_staging_uses_dense_prefix_and_preserves_publication_on_early_errors() {
    let mut vm = Interpreter::new().unwrap();
    let mut stack = ActivationStack::new();
    vm.with_runtime_turn(&mut stack, |turn| {
        let (vm, stack) = turn.into_parts();
        let array =
            array::from_elements_old_for_fixture(vm.gc_heap_mut(), (0..9).map(Value::number_i32))
                .unwrap();
        array::set_length(array, vm.gc_heap_mut(), u32::MAX as usize).unwrap();
        let mut activation = crate::VmRuntimeActivation::new(vm, stack, None);
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut activation) as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        ctx.stage_spread_arguments(Value::array(array)).unwrap();
        assert_eq!(ctx.pending_call.argument_count, 9);
        let published = (ctx.pending_call.arguments, ctx.pending_call.argument_count);
        let expected = actual_bits(&ctx);
        for missing in 0..3 {
            let saved_thread = ctx.thread;
            let saved_vm = activation.vm;
            let saved_stack = activation.stack;
            match missing {
                0 => ctx.thread = std::ptr::null_mut(),
                1 => activation.vm = std::ptr::null_mut(),
                _ => activation.stack = std::ptr::null_mut(),
            }
            assert!(matches!(
                ctx.stage_spread_arguments(Value::array(array)),
                Err(VmError::InvalidOperand)
            ));
            assert_eq!(
                (ctx.pending_call.arguments, ctx.pending_call.argument_count),
                published
            );
            assert_eq!(actual_bits(&ctx), expected);
            ctx.thread = saved_thread;
            activation.vm = saved_vm;
            activation.stack = saved_stack;
        }
        activation.stack = std::ptr::null_mut();
        assert!(activation.stack.is_null());
        assert!(matches!(
            ctx.stage_spread_arguments(Value::UNDEFINED),
            Err(VmError::TypeMismatch)
        ));
        assert_eq!(
            (ctx.pending_call.arguments, ctx.pending_call.argument_count),
            published
        );
    });
}

#[test]
fn spread_staging_actual_roots_are_rewritten_before_entry() {
    let mut vm = Interpreter::new().unwrap();
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let array = scope.array(9)?;
            let layout = scope.object_layout(&["marker"])?;
            for index in 0..9 {
                let marker = scope.number(index as f64 + 731.0);
                let child = scope.object_with_layout(layout, &[marker])?;
                scope.set_index(array, index, child)?;
            }
            scope.with_turn_parts(|vm, stack| {
                let array = vm.escape_scoped(array).as_array().unwrap();
                let mut activation = crate::VmRuntimeActivation::new(vm, stack, None);
                let mut thread = VmThread::empty();
                thread.runtime_context = std::ptr::from_mut(&mut activation) as u64;
                let mut error = None;
                let mut ctx = context(&mut thread, &mut error);
                ctx.stage_spread_arguments(Value::array(array)).unwrap();
                let before = actual_bits(&ctx);
                let cycles = vm.gc_heap_mut().gc_stats().minor_gc_cycles;
                vm.force_gc().unwrap();
                let after = actual_bits(&ctx);
                assert_eq!(after.len(), 9);
                assert_ne!(
                    before[8], after[8],
                    "the youngest actual moves in the staged root window"
                );
                assert!(vm.gc_heap_mut().gc_stats().minor_gc_cycles > cycles);
                for (index, bits) in after.into_iter().enumerate() {
                    let child = Value::from_bits(bits).as_object().unwrap();
                    assert_eq!(
                        crate::object::get(child, vm.gc_heap(), "marker"),
                        Some(Value::number_i32(index as i32 + 731))
                    );
                }
            });
            Ok::<_, NativeError>(())
        })
    })
    .unwrap();
}
