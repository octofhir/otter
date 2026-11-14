//! Complete byte, moving-entry and cancellation proofs for reverse bands.
//!
//! # Contents
//! - Odd/even views across multiple batch boundaries and element widths.
//! - Actual young receiver relocation inside the native entry.
//! - Interruption before the first committed pair.
//!
//! # Invariants
//! - An immutable byte snapshot defines the independent element-reversal oracle.
//! - Every byte inside and outside the view is compared after the operation.
//! - Native wrappers exist only in tests; production invokes `run` directly.
//!
//! # See also
//! - `super::run` owns checkpointing and element-band exchange.

use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, binary::TypedArrayKind};

#[test]
fn reverse_bands_preserve_every_byte_and_odd_middle_after_entry_collection() {
    for kind in [
        TypedArrayKind::Uint8,
        TypedArrayKind::Uint16,
        TypedArrayKind::Float32,
        TypedArrayKind::Float64,
        TypedArrayKind::BigUint64,
    ] {
        for odd in [0, 1] {
            let width = kind.bytes_per_element();
            let len = 131072 / width + 4 + odd;
            let original: Vec<u8> = (0..len * width + 16)
                .map(|i| ((i * 37 + 11) % 251) as u8)
                .collect();
            let mut expected = original.clone();
            for i in 0..len {
                expected[8 + i * width..8 + (i + 1) * width]
                    .copy_from_slice(&original[8 + (len - 1 - i) * width..8 + (len - i) * width]);
            }
            let mut vm = Interpreter::new().unwrap();
            vm.gc_heap.set_gc_stress(0, false);
            let context = vm
                .link_module(
                    crate::test_support::minimal_bytecode_module("reverse-bands"),
                    crate::source_registry::SourceRegistry::default(),
                )
                .unwrap();
            vm.force_gc().unwrap();
            NativeCtx::with_host_context(
                &mut vm,
                NativeCallInfo::default_call(),
                Some(&context),
                |ctx| {
                    ctx.scope(|mut scope| -> Result<(), NativeError> {
                        let buffer = scope.array_buffer_from_bytes(original)?;
                        let view = scope.typed_array_view(buffer, kind, 8, len)?;
                        let method = scope.native_closure(
                            "CollectThenReverse",
                            0,
                            &[],
                            |ctx, args, _| {
                                ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                                super::run(ctx, args)
                            },
                        )?;
                        let before_view = scope
                            .raw(view)
                            .as_typed_array(scope.context().heap())
                            .unwrap()
                            .handle();
                        // SAFETY: the scoped receiver is current; no allocation intervenes.
                        assert!(unsafe { (*before_view.as_header_ptr()).is_young() });
                        let roots = scope.with_turn_parts(|interp, _| {
                            (
                                interp.handle_arena.len(),
                                interp.iteration_anchors_for_trace().len(),
                            )
                        });
                        let cycles = scope.context().heap().gc_cycle_counts();
                        let result = scope.call(method, view, &[])?;
                        assert_eq!(scope.raw(result), scope.raw(view));
                        assert_ne!(scope.raw(view).as_raw_gc().unwrap().0, before_view.offset());
                        assert!(scope.context().heap().gc_cycle_counts().1 > cycles.1);
                        assert_eq!(scope.array_buffer_bytes(buffer).unwrap(), expected);
                        assert_eq!(
                            scope.with_turn_parts(|interp, _| (
                                interp.handle_arena.len(),
                                interp.iteration_anchors_for_trace().len()
                            )),
                            (roots.0 + 1, roots.1)
                        );
                        Ok(())
                    })
                },
            )
            .unwrap();
            assert_eq!(vm.handle_arena.len(), 0);
        }
    }
}

#[test]
fn reverse_checks_an_interrupt_armed_inside_the_native_entry_before_any_swap() {
    let mut vm = Interpreter::new().unwrap();
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("reverse-cancel"),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| -> Result<(), NativeError> {
                let original: Vec<u8> = (0..64).collect();
                let buffer = scope.array_buffer_from_bytes(original.clone())?;
                let view = scope.typed_array_view(buffer, TypedArrayKind::Uint8, 0, 64)?;
                let method =
                    scope.native_closure("InterruptThenReverse", 0, &[], |ctx, args, _| {
                        ctx.interp_mut().interrupt_handle().interrupt();
                        let result = super::run(ctx, args);
                        ctx.interp_mut().interrupt_handle().reset();
                        assert!(matches!(result, Err(NativeError::Interrupted)));
                        Ok(Value::undefined())
                    })?;
                scope.call(method, view, &[])?;
                assert_eq!(scope.array_buffer_bytes(buffer).unwrap(), original);
                Ok(())
            })
        },
    )
    .unwrap();
}
