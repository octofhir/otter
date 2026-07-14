//! Optimizing entry, branches, loop phis, polls, and in-place deopt parity.
//!
//! # Contents
//! - Hand-authored arithmetic, max-diamond, and int32-loop functions matching
//!   the optimizer's deliberately narrow production subset.
//! - Hot optimized return and non-int32 guard-deopt comparisons against
//!   [`JitSelection::InterpreterOnly`].
//! - Cooperative cancellation after a hot loop has entered optimized code.
//!
//! # Invariants
//! - Both tier selections execute the identical linked bytecode module.
//! - The tiered run must prove machine-code entry and reconstructed deopt via
//!   isolate-owned execution statistics.

use std::{sync::Arc, time::Duration};

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

    let mut max = FunctionCodeBuilder::new();
    max.push(Op::LoadInt32, &[Operand::Register(2), Operand::Imm32(0)]);
    max.push(
        Op::GreaterThan,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::Register(1),
        ],
    );
    max.push(Op::JumpIfFalse, &[Operand::Imm32(2), Operand::Register(3)]);
    max.push(
        Op::Add,
        &[
            Operand::Register(4),
            Operand::Register(0),
            Operand::Register(2),
        ],
    );
    max.push(Op::Jump, &[Operand::Imm32(1)]);
    max.push(
        Op::Add,
        &[
            Operand::Register(4),
            Operand::Register(1),
            Operand::Register(2),
        ],
    );
    max.push(Op::ReturnValue, &[Operand::Register(4)]);

    let mut sum = FunctionCodeBuilder::new();
    sum.push(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(0)]);
    sum.push(Op::LoadInt32, &[Operand::Register(2), Operand::Imm32(0)]);
    sum.push(Op::LoadInt32, &[Operand::Register(3), Operand::Imm32(1)]);
    sum.push(
        Op::LessThan,
        &[
            Operand::Register(4),
            Operand::Register(1),
            Operand::Register(0),
        ],
    );
    sum.push(Op::JumpIfFalse, &[Operand::Imm32(3), Operand::Register(4)]);
    sum.push(
        Op::Add,
        &[
            Operand::Register(2),
            Operand::Register(2),
            Operand::Register(1),
        ],
    );
    sum.push(
        Op::Add,
        &[
            Operand::Register(1),
            Operand::Register(1),
            Operand::Register(3),
        ],
    );
    sum.push(Op::Jump, &[Operand::Imm32(-5)]);
    sum.push(Op::ReturnValue, &[Operand::Register(2)]);

    let mut count = FunctionCodeBuilder::new();
    count.push(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(0)]);
    count.push(Op::LoadInt32, &[Operand::Register(2), Operand::Imm32(1)]);
    count.push(
        Op::LessThan,
        &[
            Operand::Register(3),
            Operand::Register(1),
            Operand::Register(0),
        ],
    );
    count.push(Op::JumpIfFalse, &[Operand::Imm32(2), Operand::Register(3)]);
    count.push(
        Op::Add,
        &[
            Operand::Register(1),
            Operand::Register(1),
            Operand::Register(2),
        ],
    );
    count.push(Op::Jump, &[Operand::Imm32(-4)]);
    count.push(Op::ReturnValue, &[Operand::Register(1)]);

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
            Function {
                id: 2,
                name: "max".to_string(),
                param_count: 2,
                scratch: 3,
                code: max.finish(),
                ..Function::default()
            },
            Function {
                id: 3,
                name: "sum".to_string(),
                param_count: 1,
                scratch: 4,
                code: sum.finish(),
                ..Function::default()
            },
            Function {
                id: 4,
                name: "count".to_string(),
                param_count: 1,
                scratch: 3,
                code: count.finish(),
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

fn call_max(
    interp: &mut Interpreter,
    context: &otter_vm::ExecutionContext,
    left: Value,
    right: Value,
) -> Value {
    let args: SmallVec<[Value; 8]> = smallvec![left, right];
    interp
        .run_callable_sync(context, &Value::function_id(2), Value::undefined(), args)
        .expect("max call")
}

fn call_int(
    interp: &mut Interpreter,
    context: &otter_vm::ExecutionContext,
    function_id: u32,
    value: i32,
) -> Result<Value, otter_vm::VmError> {
    let args: SmallVec<[Value; 8]> = smallvec![Value::number_i32(value)];
    interp.run_callable_sync(
        context,
        &Value::function_id(function_id),
        Value::undefined(),
        args,
    )
}

fn run(selection: JitSelection) -> (Value, Value, Value, Value, Value, JitRuntimeStats) {
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
        assert_eq!(
            call_max(
                &mut interp,
                &context,
                Value::number_i32(31),
                Value::number_i32(12),
            )
            .as_i32(),
            Some(31)
        );
        assert_eq!(
            call_int(&mut interp, &context, 3, 10)
                .expect("sum call")
                .as_i32(),
            Some(45)
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
    let max_left = call_max(
        &mut interp,
        &context,
        Value::number_i32(19),
        Value::number_i32(7),
    );
    let max_right = call_max(
        &mut interp,
        &context,
        Value::number_i32(-4),
        Value::number_i32(12),
    );
    let sum = call_int(&mut interp, &context, 3, 100).expect("optimized sum call");
    (
        optimized,
        deoptimized,
        max_left,
        max_right,
        sum,
        interp.jit_runtime_stats(),
    )
}

#[test]
fn optimized_return_and_deopt_match_interpreter() {
    let (oracle_return, oracle_deopt, oracle_max_left, oracle_max_right, oracle_sum, _) =
        run(JitSelection::InterpreterOnly);
    let (tiered_return, tiered_deopt, tiered_max_left, tiered_max_right, tiered_sum, stats) =
        run(JitSelection::Baseline);

    assert_eq!(oracle_return.as_i32(), Some(16));
    assert_eq!(tiered_return.to_bits(), oracle_return.to_bits());
    assert_eq!(tiered_deopt.to_bits(), oracle_deopt.to_bits());
    assert_eq!(tiered_deopt.to_bits(), Value::number_f64(3.5).to_bits());
    assert_eq!(oracle_max_left.as_i32(), Some(19));
    assert_eq!(oracle_max_right.as_i32(), Some(12));
    assert_eq!(tiered_max_left.to_bits(), oracle_max_left.to_bits());
    assert_eq!(tiered_max_right.to_bits(), oracle_max_right.to_bits());
    assert_eq!(oracle_sum.as_i32(), Some(4_950));
    assert_eq!(tiered_sum.to_bits(), oracle_sum.to_bits());
    assert!(
        stats.optimized_entries >= 5,
        "arithmetic, diamond, and loop fixtures must enter optimized code: {stats:?}"
    );
    assert!(
        stats.optimized_deopts >= 1,
        "non-int32 argument must take a reconstructed deopt exit: {stats:?}"
    );
}

#[test]
fn optimized_long_loop_terminates_through_interrupt_path() {
    let mut interp = Interpreter::new();
    interp.set_jit_compiler(Some(Arc::new(BaselineJitCompiler::new())));
    let context = interp.link_module(fixture_module());
    for _ in 0..4010 {
        assert_eq!(
            call_int(&mut interp, &context, 4, 10)
                .expect("count warmup")
                .as_i32(),
            Some(10)
        );
    }
    let entries_before = interp.jit_runtime_stats().optimized_entries;
    assert!(
        entries_before > 0,
        "count loop must be optimized after warmup"
    );

    let interrupt = interp.interrupt_handle();
    let interrupter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        interrupt.interrupt();
    });
    let result = call_int(&mut interp, &context, 4, i32::MAX);
    interrupter.join().expect("interrupt setter");

    assert_eq!(result, Err(otter_vm::VmError::Interrupted));
    let stats = interp.jit_runtime_stats();
    assert!(stats.optimized_entries > entries_before, "{stats:?}");
    assert!(
        stats.optimized_deopts > 0,
        "poll must reconstruct the loop: {stats:?}"
    );
}
