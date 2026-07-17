//! Optimizing-tier element-load execution and throw parity.
//!
//! # Contents
//! - Hand-authored element-load and integer-array summation functions matching
//!   the optimizing subset without unrelated source-lowering scaffolding.
//! - Source-lowered `values[index]` coverage for the compiler's unreachable
//!   post-return tail and an observable optimizing-entry assertion.
//! - Float-array and many-live-array loops that exercise precise transition
//!   roots while numeric accumulators stay in optimizing machine locations.
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

use otter_bytecode::{
    BytecodeModule, Constant, Function, FunctionCodeBuilder, Op, Operand, SourceKind,
};
use otter_jit::OtterJitCompiler;
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

    let mut float_sum = FunctionCodeBuilder::new();
    float_sum.push(Op::LoadInt32, &[Operand::Register(2), Operand::Imm32(0)]);
    float_sum.push(
        Op::LoadNumber,
        &[Operand::Register(3), Operand::ConstIndex(0)],
    );
    float_sum.push(Op::LoadInt32, &[Operand::Register(4), Operand::Imm32(1)]);
    float_sum.push(
        Op::LessThan,
        &[
            Operand::Register(5),
            Operand::Register(2),
            Operand::Register(1),
        ],
    );
    float_sum.push(Op::JumpIfFalse, &[Operand::Imm32(4), Operand::Register(5)]);
    float_sum.push(
        Op::LoadElement,
        &[
            Operand::Register(6),
            Operand::Register(0),
            Operand::Register(2),
        ],
    );
    float_sum.push(
        Op::Add,
        &[
            Operand::Register(3),
            Operand::Register(3),
            Operand::Register(6),
        ],
    );
    float_sum.push(
        Op::Add,
        &[
            Operand::Register(2),
            Operand::Register(2),
            Operand::Register(4),
        ],
    );
    float_sum.push(Op::Jump, &[Operand::Imm32(-6)]);
    float_sum.push(Op::ReturnValue, &[Operand::Register(3)]);

    let mut many = FunctionCodeBuilder::new();
    many.push(Op::LoadInt32, &[Operand::Register(5), Operand::Imm32(0)]);
    many.push(Op::LoadInt32, &[Operand::Register(6), Operand::Imm32(0)]);
    many.push(Op::LoadInt32, &[Operand::Register(7), Operand::Imm32(1)]);
    many.push(
        Op::LessThan,
        &[
            Operand::Register(8),
            Operand::Register(5),
            Operand::Register(4),
        ],
    );
    many.push(Op::JumpIfFalse, &[Operand::Imm32(10), Operand::Register(8)]);
    for receiver in 0..4 {
        many.push(
            Op::LoadElement,
            &[
                Operand::Register(9),
                Operand::Register(receiver),
                Operand::Register(5),
            ],
        );
        many.push(
            Op::Add,
            &[
                Operand::Register(6),
                Operand::Register(6),
                Operand::Register(9),
            ],
        );
    }
    many.push(
        Op::Add,
        &[
            Operand::Register(5),
            Operand::Register(5),
            Operand::Register(7),
        ],
    );
    many.push(Op::Jump, &[Operand::Imm32(-12)]);
    many.push(Op::ReturnValue, &[Operand::Register(6)]);

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
            Function {
                id: 3,
                name: "floatSum".to_string(),
                param_count: 2,
                scratch: 5,
                code: float_sum.finish(),
                ..Function::default()
            },
            Function {
                id: 4,
                name: "many".to_string(),
                param_count: 5,
                scratch: 5,
                code: many.finish(),
                ..Function::default()
            },
        ],
        constants: vec![Constant::Number {
            bits: 0.25f64.to_bits(),
        }],
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

fn run(selection: JitSelection) -> (Value, Value, Value, Value, String, JitRuntimeStats) {
    let mut interp = Interpreter::new();
    if !matches!(selection, JitSelection::InterpreterOnly) {
        interp.set_jit_compiler(Some(Arc::new(OtterJitCompiler::production_tiered())));
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
    let float_array = Value::array(
        interp
            .array_from_elements_host_rooted(
                [
                    Value::number_f64(1.25),
                    Value::number_f64(2.5),
                    Value::number_f64(3.75),
                    Value::number_f64(5.0),
                ],
                &[],
                &[],
            )
            .expect("float array allocation"),
    );
    let many_arrays =
        [[1, 2, 3, 4], [10, 20, 30, 40], [2, 4, 6, 8], [1, 2, 3, 4]].map(|elements| {
            Value::array(
                interp
                    .array_from_elements_host_rooted(elements.map(Value::number_i32), &[], &[])
                    .expect("many-array allocation"),
            )
        });

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
        assert_eq!(
            call(
                &mut interp,
                &context,
                3,
                smallvec![float_array, Value::number_i32(4)],
            )
            .expect("float sum warmup")
            .to_bits(),
            Value::number_f64(12.75).to_bits()
        );
        assert_eq!(
            call(
                &mut interp,
                &context,
                4,
                smallvec![
                    many_arrays[0],
                    many_arrays[1],
                    many_arrays[2],
                    many_arrays[3],
                    Value::number_i32(4),
                ],
            )
            .expect("many-array warmup")
            .to_bits(),
            Value::number_i32(140).to_bits()
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
    let float_summed = call(
        &mut interp,
        &context,
        3,
        smallvec![float_array, Value::number_i32(4)],
    )
    .expect("optimized float sum");
    let many_summed = call(
        &mut interp,
        &context,
        4,
        smallvec![
            many_arrays[0],
            many_arrays[1],
            many_arrays[2],
            many_arrays[3],
            Value::number_i32(4),
        ],
    )
    .expect("optimized many-array sum");
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
    (
        loaded,
        summed,
        float_summed,
        many_summed,
        thrown,
        interp.jit_runtime_stats(),
    )
}

#[test]
fn optimized_element_values_and_throw_match_interpreter() {
    let (oracle_load, oracle_sum, oracle_float, oracle_many, oracle_throw, _) =
        run(JitSelection::InterpreterOnly);
    let (tiered_load, tiered_sum, tiered_float, tiered_many, tiered_throw, stats) =
        run(JitSelection::ProductionTiered);

    assert_eq!(oracle_load.to_bits(), Value::number_i32(4).to_bits());
    assert_eq!(oracle_sum.to_bits(), Value::number_i32(10).to_bits());
    assert_eq!(tiered_load.to_bits(), oracle_load.to_bits());
    assert_eq!(tiered_sum.to_bits(), oracle_sum.to_bits());
    assert_eq!(oracle_float.to_bits(), Value::number_f64(12.75).to_bits());
    assert_eq!(oracle_many.to_bits(), Value::number_i32(140).to_bits());
    assert_eq!(tiered_float.to_bits(), oracle_float.to_bits());
    assert_eq!(tiered_many.to_bits(), oracle_many.to_bits());
    assert!(
        oracle_throw.contains("Uncaught") || oracle_throw.contains("TypeError"),
        "interpreter null load must throw: {oracle_throw}"
    );
    assert!(
        tiered_throw.contains("Uncaught") || tiered_throw.contains("TypeError"),
        "optimized null load must throw through the parked exception channel: {tiered_throw}"
    );
    assert!(
        stats.optimized_entries >= 5,
        "load, integer loop, float loop, many-array loop, and throwing load must enter optimized code: {stats:?}"
    );
}

#[test]
fn float_array_loop_enters_optimized_code() {
    let mut interp = Interpreter::new();
    interp.set_jit_compiler(Some(Arc::new(OtterJitCompiler::production_tiered())));
    let context = interp.link_module(fixture_module());
    let array = Value::array(
        interp
            .array_from_elements_host_rooted(
                [
                    Value::number_f64(1.25),
                    Value::number_f64(2.5),
                    Value::number_f64(3.75),
                    Value::number_f64(5.0),
                ],
                &[],
                &[],
            )
            .expect("float array allocation"),
    );
    for _ in 0..4010 {
        assert_eq!(
            call(
                &mut interp,
                &context,
                3,
                smallvec![array, Value::number_i32(4)],
            )
            .expect("float loop warmup")
            .to_bits(),
            Value::number_f64(12.75).to_bits()
        );
    }
    let stats = interp.jit_runtime_stats();
    assert!(
        stats.optimized_entries > 0,
        "float array loop must enter optimized code: {stats:?}"
    );
}

#[test]
fn several_live_arrays_enter_optimized_code() {
    let mut interp = Interpreter::new();
    interp.set_jit_compiler(Some(Arc::new(OtterJitCompiler::production_tiered())));
    let context = interp.link_module(fixture_module());
    let arrays = [[1, 2, 3, 4], [10, 20, 30, 40], [2, 4, 6, 8], [1, 2, 3, 4]].map(|elements| {
        Value::array(
            interp
                .array_from_elements_host_rooted(elements.map(Value::number_i32), &[], &[])
                .expect("many-array allocation"),
        )
    });
    for _ in 0..4010 {
        assert_eq!(
            call(
                &mut interp,
                &context,
                4,
                smallvec![
                    arrays[0],
                    arrays[1],
                    arrays[2],
                    arrays[3],
                    Value::number_i32(4),
                ],
            )
            .expect("many-array loop warmup")
            .to_bits(),
            Value::number_i32(140).to_bits()
        );
    }
    let stats = interp.jit_runtime_stats();
    assert!(
        stats.optimized_entries > 0,
        "many-array loop must enter optimized code: {stats:?}"
    );
}

#[test]
fn source_element_load_enters_optimized_code() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
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

#[test]
fn source_tagged_array_chain_enters_optimized_code() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                    function hotTaggedChain(a, b, c, d, index) {
                      const first = a[index];
                      const second = b[first];
                      const third = c[second];
                      return d[third];
                    }
                    globalThis.chainA = [0];
                    globalThis.chainB = [0];
                    globalThis.chainC = [0];
                    globalThis.chainD = [42.5];
                    let warmChain = "";
                    for (let i = 0; i < 4010; i++) {
                      warmChain += "hotTaggedChain(chainA, chainB, chainC, chainD, 0);";
                    }
                    eval(warmChain);
                "#,
            ),
            "optimizing-source-tagged-chain.js",
        )
        .expect("define source tagged-array chain");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript("hotTaggedChain(chainA, chainB, chainC, chainD, 0);"),
            "optimizing-source-tagged-chain-result.js",
        )
        .expect("source tagged-array chain result")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();

    assert_eq!(completion, "42.5");
    assert!(
        stats.jit_optimized_entries > 0,
        "tagged-array chain must enter optimized code: {stats:?}"
    );
}
