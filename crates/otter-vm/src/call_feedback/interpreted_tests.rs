//! Intrinsic `.call` observations before any compiled body executes.
//!
//! # Contents
//! - Exact bytecode targets recorded before a collecting, throwing callee.
//! - Own-method replacements that must not acquire an intrinsic target fact.
//!
//! # Invariants
//! - Each fixture uses verified bytecode and one cold source invocation.
//! - The hook panics if compilation is requested; it supplies no native body.
//! - The observed distribution stores only function ids, never moving handles.
//! - Existing runtime regressions separately prove exact generated linkage.

use super::*;
use crate::{ExecutionContext, NativeError, Value, VmError, jit};
use otter_bytecode::{Constant, Function, FunctionCodeBuilder, Op, Operand};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct ColdObserverHook;

impl jit::JitCompilerHook for ColdObserverHook {
    fn compile_function(
        &self,
        _request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        panic!("one cold call must record feedback before compilation");
    }
}

fn reg(index: u16) -> Operand {
    Operand::Register(index)
}

fn fixture(closure: bool, shadow: bool) -> (Interpreter, ExecutionContext, u32) {
    let mut module = crate::test_support::minimal_bytecode_module("interpreted-call-feedback.js");
    module.constants = vec![
        Constant::FunctionId { index: 1 },
        Constant::String {
            utf16: "call".encode_utf16().collect(),
        },
        Constant::String {
            utf16: "observeColdCall".encode_utf16().collect(),
        },
        Constant::FunctionId { index: 2 },
    ];
    let mut main = FunctionCodeBuilder::new();
    main.push(Op::LoadUndefined, &[reg(0)]);
    if closure {
        main.push(Op::MakeClosure, &[reg(1), Operand::ConstIndex(0), reg(0)]);
    } else {
        main.push(Op::MakeFunction, &[reg(1), Operand::ConstIndex(0)]);
    }
    main.push(Op::LoadNull, &[reg(2)]);
    if shadow {
        main.push(Op::MakeFunction, &[reg(4), Operand::ConstIndex(3)]);
        main.push(
            Op::StoreProperty,
            &[reg(1), Operand::ConstIndex(1), reg(4), reg(5)],
        );
    }
    let call_pc = if shadow { 5 } else { 3 };
    main.push(
        Op::CallMethodValue,
        &[
            reg(3),
            reg(1),
            Operand::ConstIndex(1),
            Operand::ConstIndex(1),
            reg(2),
        ],
    );
    main.push(Op::ReturnValue, &[reg(3)]);
    let mut leaf = FunctionCodeBuilder::new();
    leaf.push(Op::LoadGlobalOrThrow, &[reg(0), Operand::ConstIndex(2)]);
    leaf.push(Op::Call, &[reg(1), reg(0), Operand::ConstIndex(0)]);
    leaf.push(Op::LoadInt32, &[reg(1), Operand::Imm32(91)]);
    leaf.push(Op::Throw, &[reg(1)]);
    let mut replacement = FunctionCodeBuilder::new();
    replacement.push(Op::LoadInt32, &[reg(0), Operand::Imm32(42)]);
    replacement.push(Op::ReturnValue, &[reg(0)]);
    module.functions = vec![
        Function {
            id: 0,
            name: "coldCaller".into(),
            locals: 6,
            is_strict: true,
            code: main.finish(),
            ..Function::default()
        },
        Function {
            id: 1,
            name: "collectingThrower".into(),
            locals: 2,
            is_strict: true,
            code: leaf.finish(),
            ..Function::default()
        },
        Function {
            id: 2,
            name: "replacementCall".into(),
            locals: 1,
            is_strict: true,
            code: replacement.finish(),
            ..Function::default()
        },
    ];
    let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = vm
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verified cold method-call fixture");
    vm.jit_hook = Some(Arc::new(ColdObserverHook));
    (vm, context, call_pc)
}

#[test]
fn intrinsic_call_records_exact_target_before_collecting_throw() {
    for closure in [false, true] {
        let (mut vm, context, call_pc) = fixture(closure, false);
        let caller = context.code_block_arc(context.function_base()).unwrap();
        assert!(caller.call_distribution_at(call_pc as usize).is_none());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let expected =
            crate::feedback::CallSiteDistribution::Mono(crate::feedback::CallTargetCount {
                target: OrdinaryCallTarget::FunctionPrototypeCall(context.function_base() + 1),
                hits: 1,
            });
        let observations = Arc::new(Mutex::new(Vec::new()));
        let native_observations = observations.clone();
        let observed_caller = caller.clone();
        let observer = crate::native_function::native_value(
            vm.gc_heap_mut(),
            "observeColdCall",
            move |ctx, args, _captures| {
                assert!(args.is_empty());
                let before = observed_caller.call_distribution_at(call_pc as usize);
                observed_calls.fetch_add(1, Ordering::Relaxed);
                let minor = ctx.heap_mut().gc_stats().minor_gc_cycles;
                ctx.heap_mut().set_gc_stress(1, false);
                ctx.scope(|mut scope| {
                    let child = scope.array(0)?;
                    Ok::<_, NativeError>(scope.finish(child))
                })?;
                native_observations.lock().unwrap().push((
                    before,
                    observed_caller.call_distribution_at(call_pc as usize),
                    minor,
                    ctx.heap_mut().gc_stats().minor_gc_cycles,
                ));
                Ok(Value::undefined())
            },
        )
        .unwrap();
        vm.set_global("observeColdCall", observer).unwrap();
        let error = vm.run(&context).expect_err("callee throws once");
        assert!(matches!(error.error, VmError::Uncaught));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        let (before, after, minor_before, minor_after) = &observations[0];
        assert_eq!(
            before,
            &Some(expected.clone()),
            "the actual intrinsic target is published before callee effects"
        );
        assert_eq!(
            after,
            &Some(expected.clone()),
            "moving collection cannot erase the scalar target fact"
        );
        assert_eq!(*minor_after, minor_before + 1);
        assert_eq!(
            caller.call_distribution_at(call_pc as usize),
            Some(expected)
        );
        assert!(vm.jit_code_generation_snapshot().is_empty());
        assert_eq!(vm.jit_code_residency().pending_compilations, 0);
    }
}

#[test]
fn shadowed_call_does_not_record_the_intrinsic_receiver_target() {
    for closure in [false, true] {
        let (mut vm, context, call_pc) = fixture(closure, true);
        let caller = context.code_block_arc(context.function_base()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let observer = crate::native_function::native_value(
            vm.gc_heap_mut(),
            "observeColdCall",
            move |_ctx, _args, _captures| {
                observed_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Value::undefined())
            },
        )
        .unwrap();
        vm.set_global("observeColdCall", observer).unwrap();
        assert_eq!(vm.run(&context).unwrap(), Value::number_i32(42));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        match caller.call_distribution_at(call_pc as usize) {
            None => {}
            Some(crate::feedback::CallSiteDistribution::Mono(target)) => {
                assert_eq!(
                    target.target,
                    OrdinaryCallTarget::Bytecode(context.function_base() + 2)
                );
                assert_eq!(target.hits, 1);
            }
            unexpected => panic!("one cold replacement cannot have other targets: {unexpected:?}"),
        }
        assert!(vm.jit_code_generation_snapshot().is_empty());
        assert_eq!(vm.jit_code_residency().pending_compilations, 0);
    }
}
