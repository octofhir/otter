//! Direct semantic opcodes retain completed native error disposition.
//!
//! # Contents
//! - Actual RegExp iterator, iterator mapper and Promise Error-allocation OOM.
//! - A terminal mapper failure never invokes its source return/close hook.
//! - A real source handler that could catch a re-materialized RangeError.
//!
//! # Invariants
//! Verified bytecode enters the canonical interpreter/trampoline. The failing
//! native callback returns an oversized real SyntaxError;
//! there is no fabricated allocator refusal or callback assertion. Direct
//! source allocation OOM catchability is unchanged.
//!
//! # See also
//! - `super::native_error_to_committed_with_stack` owns one projection.
//! - `crate::CommittedValueError` owns disposition.

use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, VmError};
use otter_bytecode::{ExceptionHandler, FunctionCodeBuilder, Op, Operand};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn direct_regexp_and_promise_opcodes_never_rebuild_completed_materialization_oom() {
    const CAP: u64 = 4 * 1024 * 1024;
    for mode in [0, 1, 2] {
        let promise = mode == 1;
        let mut vm = Interpreter::with_string_heap_cap(CAP).expect("semantic cap bootstrap");
        vm.gc_heap_mut().set_gc_stress(0, true);
        let mut module = crate::test_support::minimal_bytecode_module("completed-semantic-opcode");
        let function = &mut module.functions[0];
        function.param_count = 1;
        function.locals = 3;
        let mut code = FunctionCodeBuilder::new();
        if promise {
            code.push(
                Op::PromiseCall,
                &[
                    Operand::Register(1),
                    Operand::ConstIndex(otter_bytecode::method_id::PromiseMethod::Try as u32),
                    Operand::ConstIndex(1),
                    Operand::Register(0),
                ],
            );
        } else {
            code.push(
                Op::IteratorNext,
                &[
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            );
        }
        code.push(Op::Return, &[Operand::Register(1)]);
        code.push(Op::Return, &[Operand::Register(3)]);
        function.code = code.finish();
        function.handlers = vec![ExceptionHandler {
            start: 0,
            end: 1,
            target: 2,
            exception: 3,
        }];
        let context = vm
            .link_module(module, crate::source_registry::SourceRegistry::default())
            .expect("verified current semantic opcode and handler");
        let calls = Arc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        let closes = Arc::new(AtomicUsize::new(0));
        let close_sink = closes.clone();
        let failure = NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<Value, NativeError> {
                    let callback = scope.native_closure(
                        "oversized semantic error",
                        0,
                        &[],
                        move |_, _, _| {
                            sink.fetch_add(1, Ordering::SeqCst);
                            Err(NativeError::SyntaxError {
                                name: "oversized semantic error",
                                reason: "x".repeat(8 * 1024 * 1024),
                            })
                        },
                    )?;
                    let argument = if promise {
                        callback
                    } else if mode == 2 {
                        let source = scope.object()?;
                        let next =
                            scope.native_closure("mapper source next", 0, &[], |ctx, _, _| {
                                ctx.scope(|mut scope| {
                                    let result = scope.object()?;
                                    let value = scope.number(1.0);
                                    let done = scope.boolean(false);
                                    scope.set(result, "value", value)?;
                                    scope.set(result, "done", done)?;
                                    Ok(scope.finish(result))
                                })
                            })?;
                        let close = scope.native_closure(
                            "mapper source close",
                            0,
                            &[],
                            move |_, _, _| {
                                close_sink.fetch_add(1, Ordering::SeqCst);
                                Ok(Value::undefined())
                            },
                        )?;
                        scope.set(source, "return", close)?;
                        let source_value = scope.raw(source);
                        let next_value = scope.raw(next);
                        let source = scope.with_turn_parts(|interp, _| {
                            interp
                                .alloc_runtime_rooted_iterator_state(
                                    crate::IteratorState::User {
                                        iterator: source_value,
                                        next_method: Some(next_value),
                                    },
                                    &[&source_value, &next_value],
                                    &[],
                                )
                                .map_err(|error| {
                                    crate::native_function::vm_to_native_error(
                                        interp,
                                        error,
                                        "mapper source setup",
                                    )
                                })
                        })?;
                        let mapper = scope.raw(callback);
                        let iterator = scope.with_turn_parts(|interp, _| {
                            interp
                                .alloc_runtime_rooted_iterator_state(
                                    crate::IteratorState::Map {
                                        source,
                                        mapper,
                                        running: false,
                                        counter: 0,
                                    },
                                    &[&mapper, &Value::iterator(source)],
                                    &[],
                                )
                                .map_err(|error| {
                                    crate::native_function::vm_to_native_error(
                                        interp,
                                        error,
                                        "mapper helper setup",
                                    )
                                })
                        })?;
                        scope.value(Value::iterator(iterator))
                    } else {
                        let constructor =
                            scope.global("RegExp").ok_or(NativeError::InvalidOperand)?;
                        let pattern = scope.string("x")?;
                        let flags = scope.string("g")?;
                        let matcher = scope.construct(constructor, &[pattern, flags])?;
                        scope.set(matcher, "exec", callback)?;
                        let input = scope.string("x")?;
                        let matcher = scope.raw(matcher);
                        let input = scope
                            .raw(input)
                            .as_string(scope.context().heap())
                            .ok_or(NativeError::InvalidOperand)?;
                        let iterator = scope.with_turn_parts(|interp, _| {
                            interp
                                .alloc_runtime_rooted_iterator_state(
                                    crate::IteratorState::RegExpString {
                                        matcher,
                                        input,
                                        global: true,
                                        full_unicode: false,
                                        done: false,
                                    },
                                    &[&matcher, &Value::string(input)],
                                    &[],
                                )
                                .map_err(|error| {
                                    crate::native_function::vm_to_native_error(
                                        interp,
                                        error,
                                        "semantic iterator setup",
                                    )
                                })
                        })?;
                        scope.value(Value::iterator(iterator))
                    };
                    let function = scope.value(Value::function(context.main().id));
                    let receiver = scope.undefined();
                    let returned = scope.call(function, receiver, &[argument])?;
                    Ok(scope.finish(returned))
                })
            },
        )
        .expect_err("completed Error-building OOM bypasses the real local source handler");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one actual callback invocation"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "terminal completion must not invoke source return"
        );
        let NativeError::ExecutionFailure(failure) = failure else {
            panic!("completed semantic failure lost its domain: {failure:?}");
        };
        assert!(
            matches!(failure.error, VmError::OutOfMemory {
            requested_bytes, heap_limit_bytes,
        } if requested_bytes > CAP && heap_limit_bytes == CAP),
            "{failure:?}"
        );
        assert!(failure.is_fatal());
        assert!(
            failure.detail.is_none(),
            "Error build refusal cannot retain SyntaxError detail"
        );
        assert!(vm.pending_uncaught_throw.is_none());
    }
}
