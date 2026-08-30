//! Interpreter/JIT agreement on one verified bytecode artifact.
//!
//! `otter-difftest` compares tiers from JavaScript source, so it can only
//! reach artifacts the compiler chooses to emit. The admission boundary
//! promises something wider: any artifact the verifier admits behaves the same
//! on every consumer. These cases build the artifact directly, so shapes a
//! compiler would never produce — a loop whose body is a single back edge,
//! arithmetic that leaves the integer range, a function ending in an abrupt
//! completion — still have to agree.
//!
//! `finally` reached through tier-up is covered from source by the
//! `otter-difftest` corpus instead: emitting a correct finally by hand means
//! reproducing the compiler's parked-completion protocol, and a fixture whose
//! normal path skipped the finally would test the fixture, not the engine.
//!
//! # Contents
//! - [`hot_loop_module`] and friends — artifacts built to reach the tier-up
//!   threshold quickly.
//! - [`run_on_each_tier`] — one artifact, interpreter and JIT, same result.
//!
//! # Invariants
//! - The interpreter is the oracle; a tier disagreeing with it is the failure.
//! - Every artifact must actually tier up, or the case proves nothing.

use std::sync::Arc;

use otter_bytecode::wordcode::FunctionCodeBuilder;
use otter_bytecode::{BytecodeModule, Constant, Function, Op, Operand, SourceKind};
use otter_jit::OtterJitCompiler;
use otter_vm::{Interpreter, Value};

/// Rewrite one control-flow operand so it lands on `target`.
///
/// Wordcode targets are relative to the instruction after the one carrying
/// the operand.
fn set_offset(code: &mut FunctionCodeBuilder, at: u32, operand: usize, target: u32) {
    let delta = i64::from(target) - (i64::from(at) + 1);
    let delta = i32::try_from(delta).expect("fixture offset fits i32");
    assert!(
        code.set_operand(at, operand, Operand::Imm32(delta)),
        "operand {operand} at {at} is not writable"
    );
}

fn module_from(name: &str, locals: u16, code: FunctionCodeBuilder) -> BytecodeModule {
    BytecodeModule {
        module: format!("file:///{name}.js"),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            locals,
            code: code.finish(),
            module_url: format!("file:///{name}.js"),
            ..Default::default()
        }],
        function_source: None,
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// `for (i = 0; i < trips; i += 1) { acc += i }`, returning `acc`.
///
/// The back edge is what drives OSR tier-up, so `trips` sets how hot the
/// artifact gets.
fn hot_loop_module(trips: i32) -> BytecodeModule {
    let mut code = FunctionCodeBuilder::new();
    // r0 = accumulator, r1 = induction variable, r2 = predicate, r3 = step.
    code.push(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]);
    code.push(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(0)]);
    // The step lives in its own register: an immediate-right operator may not
    // name one register as both destination and left operand.
    code.push(Op::LoadInt32, &[Operand::Register(3), Operand::Imm32(1)]);
    let header = code.next_pc();
    code.push(
        Op::LessThanImm,
        &[
            Operand::Register(2),
            Operand::Register(1),
            Operand::Imm32(trips),
        ],
    );
    let exit = code.push(Op::JumpIfFalse, &[Operand::Imm32(0), Operand::Register(2)]);
    code.push(
        Op::Add,
        &[
            Operand::Register(0),
            Operand::Register(0),
            Operand::Register(1),
        ],
    );
    code.push(
        Op::Add,
        &[
            Operand::Register(1),
            Operand::Register(1),
            Operand::Register(3),
        ],
    );
    let back = code.push(Op::Jump, &[Operand::Imm32(0)]);
    let done = code.next_pc();
    code.push(Op::Return, &[Operand::Register(0)]);
    set_offset(&mut code, exit, 0, done);
    set_offset(&mut code, back, 0, header);
    module_from("hot_loop", 4, code)
}

/// A hot loop whose accumulator leaves the int32 range, so both consumers
/// must agree on the float representation rather than on a wrapped integer.
fn overflowing_accumulator_module(trips: i32) -> BytecodeModule {
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::LoadInt32,
        &[Operand::Register(0), Operand::Imm32(i32::MAX - 1)],
    );
    code.push(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(0)]);
    // The step lives in its own register: an immediate-right operator may not
    // name one register as both destination and left operand.
    code.push(Op::LoadInt32, &[Operand::Register(3), Operand::Imm32(1)]);
    let header = code.next_pc();
    code.push(
        Op::LessThanImm,
        &[
            Operand::Register(2),
            Operand::Register(1),
            Operand::Imm32(trips),
        ],
    );
    let exit = code.push(Op::JumpIfFalse, &[Operand::Imm32(0), Operand::Register(2)]);
    code.push(
        Op::Add,
        &[
            Operand::Register(0),
            Operand::Register(0),
            Operand::Register(0),
        ],
    );
    code.push(
        Op::Add,
        &[
            Operand::Register(1),
            Operand::Register(1),
            Operand::Register(3),
        ],
    );
    let back = code.push(Op::Jump, &[Operand::Imm32(0)]);
    let done = code.next_pc();
    code.push(Op::Return, &[Operand::Register(0)]);
    set_offset(&mut code, exit, 0, done);
    set_offset(&mut code, back, 0, header);
    module_from("overflow", 4, code)
}

/// The hot loop again, register for register, but ending in a `throw` instead
/// of a `return`. A throwing exit disables the compare/branch fusion the
/// returning case gets, so this is the artifact that exercises the unfused
/// lowering of the loop.
fn throwing_loop_module(trips: i32) -> BytecodeModule {
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]);
    code.push(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(0)]);
    let header = code.next_pc();
    code.push(
        Op::LessThanImm,
        &[
            Operand::Register(2),
            Operand::Register(1),
            Operand::Imm32(trips),
        ],
    );
    let exit = code.push(Op::JumpIfFalse, &[Operand::Imm32(0), Operand::Register(2)]);
    code.push(
        Op::Add,
        &[
            Operand::Register(0),
            Operand::Register(0),
            Operand::Register(1),
        ],
    );
    code.push(
        Op::Add,
        &[
            Operand::Register(1),
            Operand::Register(1),
            Operand::Register(3),
        ],
    );
    let back = code.push(Op::Jump, &[Operand::Imm32(0)]);
    let done = code.next_pc();
    code.push(
        Op::LoadString,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    code.push(Op::Throw, &[Operand::Register(0)]);
    set_offset(&mut code, exit, 0, done);
    set_offset(&mut code, back, 0, header);

    let mut module = module_from("throwing_loop", 4, code);
    module.constants = vec![Constant::String {
        utf16: "loop finished".encode_utf16().collect(),
    }];
    module
}

/// One artifact's observable outcome, reduced to something comparable.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Returned(String),
    Threw(String),
}

fn observe(module: BytecodeModule, jit: bool) -> Outcome {
    let mut interp = Interpreter::new();
    if jit {
        // Threshold 1 makes the first back edge tier up, so the loop body runs
        // compiled rather than interpreted for all but its first trip.
        interp.set_jit_osr_threshold(1);
        Arc::new(OtterJitCompiler::default()).install(&mut interp);
    }
    let context = interp
        .link_module(module)
        .expect("fixture artifact is admitted");
    match interp.run(&context) {
        Ok(value) => Outcome::Returned(describe(&interp, value)),
        Err(error) => Outcome::Threw(error.message()),
    }
}

fn describe(interp: &Interpreter, value: Value) -> String {
    value.as_string(interp.gc_heap()).map_or_else(
        || format!("{value:?}"),
        |s| s.with_utf16(interp.gc_heap(), String::from_utf16_lossy),
    )
}

/// Run one artifact on the interpreter and on the JIT and require agreement.
fn run_on_each_tier(name: &str, build: impl Fn() -> BytecodeModule) {
    let oracle = observe(build(), false);
    let tiered = observe(build(), true);
    assert_eq!(
        oracle, tiered,
        "{name}: interpreter and JIT disagree on one verified artifact"
    );
}

#[test]
fn hot_loop_agrees_across_tiers() {
    run_on_each_tier("hot loop", || hot_loop_module(4_000));
}

#[test]
fn integer_overflow_in_a_hot_loop_agrees_across_tiers() {
    run_on_each_tier("overflowing accumulator", || {
        overflowing_accumulator_module(4_000)
    });
}

#[test]
fn a_throw_after_a_hot_loop_agrees_across_tiers() {
    // A throwing exit disables compare/branch fusion, so this case covers the
    // unfused lowering of a loop that the returning case never reaches.
    run_on_each_tier("throwing loop", || throwing_loop_module(4_000));
}

#[test]
fn tiering_actually_happens_for_these_artifacts() {
    // Without this the agreement cases could pass by never leaving the
    // interpreter. A compiled run of a 4000-trip loop must be observably
    // different work than an interpreted one, which the compiler probe
    // reports as at least one compiled entry.
    let probe = otter_jit::JitCompilerProbe::new(Arc::new(OtterJitCompiler::default()));
    let mut interp = Interpreter::new();
    interp.set_jit_osr_threshold(1);
    probe.install(&mut interp);
    let context = interp
        .link_module(hot_loop_module(4_000))
        .expect("fixture artifact is admitted");
    interp.run(&context).expect("hot loop returns");
    assert!(
        probe.measurement().invocations > 0,
        "no compilation was attempted, so the agreement cases prove nothing"
    );
}
