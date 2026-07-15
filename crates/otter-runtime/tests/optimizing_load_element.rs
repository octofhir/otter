//! Optimizing-tier element-load execution and throw parity.
//!
//! # Contents
//! - Hand-authored element-load and integer-array summation functions matching
//!   the optimizing subset without unrelated source-lowering scaffolding.
//! - Source-lowered `values[index]` coverage for the compiler's unreachable
//!   post-return tail and an observable optimizing-entry assertion.
//! - Hot optimized execution through the production runtime transition.
//! - Interpreter-oracle comparison for returned elements and sums, plus a
//!   null-receiver throw through the optimized entry.
//!
//! # Invariants
//! - Both tier selections execute the identical linked bytecode module.
//! - The tiered run must prove machine-code entry through isolate-owned stats.
//! - Array storage remains owned by the VM; compiled code reaches it only
//!   through `STUB_JIT_LOAD_ELEMENT`.
//!
//! # See also
//! - `crates/otter-difftest/corpus/arrays_typed.js` exercises element loads
//!   across the moving-GC stride matrix.

use std::sync::Arc;

use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, Operand, SourceKind};
use otter_jit::BaselineJitCompiler;
use otter_runtime::{JitSelection, Runtime, SourceInput};
use otter_vm::{ExecutionContext, Interpreter, JitRuntimeStats, Value};
use smallvec::{SmallVec, smallvec};

fn fixture_module() -> BytecodeModule {
    let mut main = FunctionCodeBuilder::new();
    main.push(Op::ReturnUndefined, &[]);

    let mut load = FunctionCodeBuilder::new();
    load.push(
        Op::LoadElement,
        &[
            Operand::Register(2),
            Operand::Register(0),
            Operand::Register(1),
        ],
    );
    load.push(Op::ReturnValue, &[Operand::Register(2)]);

    let mut sum = FunctionCodeBuilder::new();
    sum.push(Op::LoadInt32, &[Operand::Register(2), Operand::Imm32(0)]);
    sum.push(Op::LoadInt32, &[Operand::Register(3), Operand::Imm32(0)]);
    sum.push(Op::LoadInt32, &[Operand::Register(4), Operand::Imm32(1)]);
    sum.push(
        Op::LessThan,
        &[
            Operand::Register(5),
            Operand::Register(2),
            Operand::Register(1),
        ],
    );
    sum.push(Op::JumpIfFalse, &[Operand::Imm32(4), Operand::Register(5)]);
    sum.push(
        Op::LoadElement,
        &[
            Operand::Register(6),
            Operand::Register(0),
            Operand::Register(2),
        ],
    );
    sum.push(
        Op::Add,
        &[
            Operand::Register(3),
            Operand::Register(3),
            Operand::Register(6),
        ],
    );
    sum.push(
        Op::Add,
        &[
            Operand::Register(2),
            Operand::Register(2),
            Operand::Register(4),
        ],
    );
    sum.push(Op::Jump, &[Operand::Imm32(-6)]);
    sum.push(Op::ReturnValue, &[Operand::Register(3)]);

    BytecodeModule {
        module: "optimizing-load-element.js".to_string(),
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
                name: "load".to_string(),
                param_count: 2,
                scratch: 1,
                code: load.finish(),
                ..Function::default()
            },
            Function {
                id: 2,
                name: "sum".to_string(),
                param_count: 2,
                scratch: 5,
                code: sum.finish(),
                ..Function::default()
            },
        ],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

fn call(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    function_id: u32,
    args: SmallVec<[Value; 8]>,
) -> Result<Value, otter_vm::VmError> {
    interp.run_callable_sync(
        context,
        &Value::function_id(function_id),
        Value::undefined(),
        args,
    )
}

fn run(selection: JitSelection) -> (Value, Value, String, JitRuntimeStats) {
    let mut interp = Interpreter::new();
    if !matches!(selection, JitSelection::InterpreterOnly) {
        interp.set_jit_compiler(Some(Arc::new(BaselineJitCompiler::new())));
    }
    let context = interp.link_module(fixture_module());
    let array = Value::array(
        interp
            .array_from_elements_host_rooted(
                [
                    Value::number_i32(1),
                    Value::number_i32(2),
                    Value::number_i32(3),
                    Value::number_i32(4),
                ],
                &[],
                &[],
            )
            .expect("array allocation"),
    );

    for _ in 0..4010 {
        assert_eq!(
            call(
                &mut interp,
                &context,
                1,
                smallvec![array, Value::number_i32(1)],
            )
            .expect("load warmup")
            .to_bits(),
            Value::number_i32(2).to_bits()
        );
        assert_eq!(
            call(
                &mut interp,
                &context,
                2,
                smallvec![array, Value::number_i32(4)],
            )
            .expect("sum warmup")
            .to_bits(),
            Value::number_i32(10).to_bits()
        );
    }

    let loaded = call(
        &mut interp,
        &context,
        1,
        smallvec![array, Value::number_i32(3)],
    )
    .expect("optimized load");
    let summed = call(
        &mut interp,
        &context,
        2,
        smallvec![array, Value::number_i32(4)],
    )
    .expect("optimized sum");
    let thrown = format!(
        "{:?}",
        call(
            &mut interp,
            &context,
            1,
            smallvec![Value::null(), Value::number_i32(0)],
        )
        .expect_err("null element load must throw")
    );
    (loaded, summed, thrown, interp.jit_runtime_stats())
}

#[test]
fn optimized_element_values_and_throw_match_interpreter() {
    let (oracle_load, oracle_sum, oracle_throw, _) = run(JitSelection::InterpreterOnly);
    let (tiered_load, tiered_sum, tiered_throw, stats) = run(JitSelection::Baseline);

    assert_eq!(oracle_load.to_bits(), Value::number_i32(4).to_bits());
    assert_eq!(oracle_sum.to_bits(), Value::number_i32(10).to_bits());
    assert_eq!(tiered_load.to_bits(), oracle_load.to_bits());
    assert_eq!(tiered_sum.to_bits(), oracle_sum.to_bits());
    assert!(
        oracle_throw.contains("Uncaught") || oracle_throw.contains("TypeError"),
        "interpreter null load must throw: {oracle_throw}"
    );
    assert!(
        tiered_throw.contains("Uncaught") || tiered_throw.contains("TypeError"),
        "optimized null load must throw through the parked exception channel: {tiered_throw}"
    );
    assert!(
        stats.optimized_entries >= 3,
        "load, sum, and throwing load must enter optimized code: {stats:?}"
    );
}

#[test]
fn source_element_load_enters_optimized_code() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::Baseline)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                    function hotElement(values, index) {
                      return values[index];
                    }
                    globalThis.values = [1, 2.5, 3, 4.5];
                    let warmSource = "";
                    for (let i = 0; i < 4010; i++) {
                      warmSource += "hotElement(values, 0);";
                    }
                    eval(warmSource);
                "#,
            ),
            "optimizing-source-load-element.js",
        )
        .expect("define source element load");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(
                "hotElement(values, 0) + hotElement(values, 1)\
                 + hotElement(values, 2) + hotElement(values, 3);",
            ),
            "optimizing-source-load-element-result.js",
        )
        .expect("source element load result")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();

    assert_eq!(completion, "11");
    assert!(
        stats.jit_optimized_entries > 0,
        "source-lowered element load must enter optimized code: {stats:?}"
    );
}
