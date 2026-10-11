//! Callable receiver accessor regressions for fused method lookup.
//!
//! # Contents
//! - Native callable own getters and their returned method binding.
//! - Compiled method resolution after an own getter moves its receiver.
//!
//! # Invariants
//! - JavaScript captures and receiver values remain in traced handles.
//! - Getter and method each execute once with the original receiver.
//!
//! # See also
//! - [`super::Interpreter::get_method_value_for_call`]
//! - [`crate::object_internal_ops`]

use std::sync::Arc;

use smallvec::SmallVec;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::activation_stack::ActivationStack;
use crate::native_function::NativeFunction;
use crate::object::PropertyDescriptor;
use crate::{Interpreter, IntrinsicCallKind, Value};

#[test]
fn native_own_getter_returns_the_called_method_with_original_receiver() {
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    let mut module = crate::test_support::minimal_bytecode_module("native-get-method.js");
    module.constants.push(otter_bytecode::Constant::String {
        utf16: "isArray".encode_utf16().collect(),
    });
    module.functions[0].locals = 3;
    let mut code = otter_bytecode::FunctionCodeBuilder::new();
    code.push(
        otter_bytecode::Op::CallMethodValue,
        &[
            otter_bytecode::Operand::Register(1),
            otter_bytecode::Operand::Register(0),
            otter_bytecode::Operand::ConstIndex(0),
            otter_bytecode::Operand::ConstIndex(1),
            otter_bytecode::Operand::Register(2),
        ],
    );
    code.push(
        otter_bytecode::Op::Return,
        &[otter_bytecode::Operand::Register(1)],
    );
    module.functions[0].code = code.finish();
    let context = interp
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("valid accessor fixture");
    let method_key = context.property_atom(0).expect("resolved method atom");
    let getter_calls = Arc::new(AtomicUsize::new(0));
    let method_calls = Arc::new(AtomicUsize::new(0));
    let mut stack = ActivationStack::new();

    interp.with_runtime_turn(&mut stack, |turn| {
        let (interp, stack) = turn.into_parts();
        interp.with_handle_scope(|interp, scope| {
            let receiver = interp
                .realm_intrinsics
                .callable(IntrinsicCallKind::Array)
                .expect("canonical Array callable");
            let receiver_root = interp.scoped_value(scope, Value::native_function(receiver));
            let calls = Arc::clone(&method_calls);
            let method_captures = smallvec::smallvec![interp.escape_scoped(receiver_root)];
            let method = NativeFunction::with_captures(
                &mut interp.gc_heap,
                "replacement-method",
                move |ctx, args, captures| {
                    assert_eq!(*ctx.this_value(), captures[0]);
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(args[0])
                },
                method_captures,
            )
            .expect("replacement method");
            let method_root = interp.scoped_value(scope, Value::native_function(method));
            let calls = Arc::clone(&getter_calls);
            let getter_captures = smallvec::smallvec![
                interp.escape_scoped(receiver_root),
                interp.escape_scoped(method_root)
            ];
            let getter = NativeFunction::with_captures(
                &mut interp.gc_heap,
                "method-getter",
                move |ctx, args, captures| {
                    assert!(args.is_empty());
                    assert_eq!(*ctx.this_value(), captures[0]);
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(captures[1])
                },
                getter_captures,
            )
            .expect("method getter");
            let getter_root = interp.scoped_value(scope, Value::native_function(getter));
            let receiver = interp
                .escape_scoped(receiver_root)
                .as_native_function()
                .expect("rooted callable receiver");
            let descriptor = PropertyDescriptor::accessor(
                Some(interp.escape_scoped(getter_root)),
                None,
                false,
                true,
            );
            assert!(
                receiver
                    .define_own_property(&mut interp.gc_heap, "isArray", descriptor)
                    .expect("own accessor installation")
            );

            let loaded = interp
                .get_method_value_for_call(
                    &context,
                    stack,
                    interp.escape_scoped(receiver_root),
                    method_key,
                )
                .expect("getter completion")
                .expect("method property");
            assert_eq!(loaded, interp.escape_scoped(method_root));
            let result = interp
                .run_callable_sync_rooted(
                    stack,
                    Some(&context),
                    &loaded,
                    interp.escape_scoped(receiver_root),
                    smallvec::smallvec![Value::number_i32(91)],
                )
                .expect("returned method completion");
            assert_eq!(result, Value::number_i32(91));
        });
    });
    assert_eq!(getter_calls.load(Ordering::Relaxed), 1);
    assert_eq!(method_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn compiled_method_getter_moves_receiver_before_resolved_call() {
    use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};

    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    let mut module = crate::test_support::minimal_bytecode_module("moving-method-getter.js");
    module.constants.push(Constant::String {
        utf16: "method".encode_utf16().collect(),
    });
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::CallMethodValue,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::ConstIndex(0),
        ],
    );
    code.push(Op::ReturnUndefined, &[]);
    module.functions[0].code = code.finish();
    let context = interp
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("valid method source");
    let getter_calls = Arc::new(AtomicUsize::new(0));
    let method_calls = Arc::new(AtomicUsize::new(0));
    let mut stack = ActivationStack::new();
    interp.with_runtime_turn(&mut stack, |turn| {
        let (interp, stack) = turn.into_parts();
        interp.with_handle_scope(|interp, scope| {
            let calls = Arc::clone(&method_calls);
            let method = NativeFunction::with_captures(
                &mut interp.gc_heap,
                "resolved-method",
                move |ctx, args, _| {
                    assert!(args.is_empty());
                    let receiver = *ctx.this_value();
                    ctx.scope(|mut scope| {
                        let receiver = scope.value(receiver);
                        let marker = scope.get(receiver, "marker")?;
                        assert_eq!(scope.raw(marker), Value::number_i32(71));
                        Ok::<(), crate::NativeError>(())
                    })?;
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(Value::number_i32(91))
                },
                SmallVec::new(),
            )
            .expect("resolved method");
            let method = interp.scoped_value(scope, Value::native_function(method));
            let calls = Arc::clone(&getter_calls);
            let getter_captures = smallvec::smallvec![interp.escape_scoped(method)];
            let getter = NativeFunction::with_captures(
                &mut interp.gc_heap,
                "moving-getter",
                move |ctx, args, captures| {
                    assert!(args.is_empty());
                    let receiver = *ctx.this_value();
                    let method = captures[0];
                    ctx.scope(|mut scope| {
                        let receiver = scope.value(receiver);
                        let method = scope.value(method);
                        let before = scope.raw(receiver).to_bits();
                        scope.with_turn_parts(|interp, _| interp.force_gc())?;
                        assert_ne!(
                            scope.raw(receiver).to_bits(),
                            before,
                            "getter moved its young receiver"
                        );
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(scope.raw(method))
                    })
                },
                getter_captures,
            )
            .expect("moving getter");
            let getter = interp.scoped_value(scope, Value::native_function(getter));
            let receiver = interp.scoped_object(scope).expect("young receiver");
            let marker = interp.scoped_value(scope, Value::number_i32(71));
            interp
                .scoped_set(scope, receiver, "marker", marker)
                .unwrap();
            interp
                .scoped_set_call_native(scope, receiver, method)
                .unwrap();
            let undefined = interp.scoped_undefined(scope);
            interp
                .scoped_define_accessor(
                    scope,
                    receiver,
                    "method",
                    getter,
                    undefined,
                    crate::object::PropertyFlags::new(false, false, true),
                )
                .unwrap();
            let loaded = interp
                .jit_runtime_resolve_method(
                    &context,
                    stack,
                    context.function_base(),
                    0,
                    interp.escape_scoped(receiver),
                    0,
                    0,
                )
                .expect("compiled method resolution through collecting getter");
            assert_eq!(loaded, interp.escape_scoped(method));
            let result = interp
                .run_callable_sync_rooted(
                    stack,
                    Some(&context),
                    &loaded,
                    interp.escape_scoped(receiver),
                    SmallVec::new(),
                )
                .expect("call resolved method once");
            assert_eq!(result, Value::number_i32(91));
        });
    });
    assert_eq!(getter_calls.load(Ordering::Relaxed), 1);
    assert_eq!(method_calls.load(Ordering::Relaxed), 1);
}
