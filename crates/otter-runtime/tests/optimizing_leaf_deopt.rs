//! Optimizing-leaf entry and in-place deopt reconstruction parity.
//!
//! # Contents
//! - A hand-authored bytecode leaf matching the optimizer's deliberately narrow
//!   production subset.
//! - Hot optimized return and non-int32 guard-deopt comparisons against
//!   [`JitSelection::InterpreterOnly`].
//!
//! # Invariants
//! - Both tier selections execute the identical linked bytecode module.
//! - The tiered run must prove machine-code entry and reconstructed deopt via
//!   isolate-owned execution statistics.

use std::sync::Arc;

use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, Operand, SourceKind};
use otter_jit::BaselineJitCompiler;
use otter_runtime::JitSelection;
use otter_vm::{Interpreter, JitRuntimeStats, Value};
use smallvec::{SmallVec, smallvec};

fn fixture_module() -> BytecodeModule {
    let mut main = FunctionCodeBuilder::new();
    main.push(Op::ReturnUndefined, &[]);

    let mut leaf = FunctionCodeBuilder::new();
    leaf.push(
        Op::Add,
        &[
            Operand::Register(2),
            Operand::Register(0),
            Operand::Register(1),
        ],
    );
    leaf.push(Op::ReturnValue, &[Operand::Register(2)]);

    BytecodeModule {
        module: "optimizing-leaf-deopt.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![
            Function {
                id: 0,
                name: "<main>".to_string(),
                code: main.finish(),
                ..Function::default()
            },
            Function {
                id: 1,
                name: "add".to_string(),
                param_count: 2,
                scratch: 1,
                code: leaf.finish(),
                ..Function::default()
            },
        ],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

fn call_add(
    interp: &mut Interpreter,
    context: &otter_vm::ExecutionContext,
    left: Value,
    right: Value,
) -> Value {
    let args: SmallVec<[Value; 8]> = smallvec![left, right];
    interp
        .run_callable_sync(context, &Value::function_id(1), Value::undefined(), args)
        .expect("add call")
}

fn run(selection: JitSelection) -> (Value, Value, JitRuntimeStats) {
    let mut interp = Interpreter::new();
    if !matches!(selection, JitSelection::InterpreterOnly) {
        interp.set_jit_compiler(Some(Arc::new(BaselineJitCompiler::new())));
    }
    let context = interp.link_module(fixture_module());
    for _ in 0..4010 {
        assert_eq!(
            call_add(
                &mut interp,
                &context,
                Value::number_i32(20),
                Value::number_i32(22),
            )
            .as_i32(),
            Some(42)
        );
    }
    let optimized = call_add(
        &mut interp,
        &context,
        Value::number_i32(7),
        Value::number_i32(9),
    );
    let deoptimized = call_add(
        &mut interp,
        &context,
        Value::number_f64(1.5),
        Value::number_i32(2),
    );
    (optimized, deoptimized, interp.jit_runtime_stats())
}

#[test]
fn optimized_return_and_deopt_match_interpreter() {
    let (oracle_return, oracle_deopt, _) = run(JitSelection::InterpreterOnly);
    let (tiered_return, tiered_deopt, stats) = run(JitSelection::Baseline);

    assert_eq!(oracle_return.as_i32(), Some(16));
    assert_eq!(tiered_return.to_bits(), oracle_return.to_bits());
    assert_eq!(tiered_deopt.to_bits(), oracle_deopt.to_bits());
    assert_eq!(tiered_deopt.to_bits(), Value::number_f64(3.5).to_bits());
    assert!(
        stats.optimized_entries >= 2,
        "fixture must enter optimized code for success and deopt: {stats:?}"
    );
    assert!(
        stats.optimized_deopts >= 1,
        "non-int32 argument must take a reconstructed deopt exit: {stats:?}"
    );
}
