//! Performance measurement: JIT vs interpreter.
//!
//! Reports timing to stdout — run with `cargo test -p otter-jit --test perf_tests -- --nocapture`.

use std::time::Instant;

use otter_jit::pipeline::{JitExecResult, compile_function, execute_function};
use otter_vm::interpreter::Interpreter;
use otter_vm::source::compile_script;
use otter_vm::RegisterValue;

// ============================================================
// Core measurement helpers
// ============================================================

/// Measure JIT path: compile once, execute N times with full runtime.
/// Returns (compile_ns, per_exec_ns, result).
fn measure_jit(script: &str, exec_iterations: u32) -> Option<(u64, u64, RegisterValue)> {
    use otter_jit::deopt::execute_module_entry_with_runtime;
    use otter_vm::RuntimeState;

    let module = compile_script(script, "<bench>").expect("compile");
    let function = module.entry_function();

    // Compile latency.
    let compile_start = Instant::now();
    let _ = compile_function(function);
    let compile_ns = compile_start.elapsed().as_nanos() as u64;

    // Execute N times (each needs fresh RuntimeState for correctness since
    // scripts re-declare `var sum = 0` etc, but this measures the full path).
    let mut last_val = RegisterValue::undefined();
    let exec_start = Instant::now();
    for _ in 0..exec_iterations {
        let mut runtime = RuntimeState::new();
        match execute_module_entry_with_runtime(&module, &mut runtime, std::ptr::null(), None) {
            Ok(result) => { last_val = result.return_value(); }
            Err(_) => return None,
        }
    }
    let exec_total = exec_start.elapsed().as_nanos() as u64;
    let per_exec = exec_total / u64::from(exec_iterations);

    Some((compile_ns, per_exec, last_val))
}

/// Measure PURE interpreter (no JIT attempt) for fair comparison.
/// Uses same RuntimeState::new() overhead as measure_jit.
fn measure_interpreter_with_runtime(script: &str, iterations: u32) -> (u64, RegisterValue) {
    use otter_vm::RuntimeState;

    let module = compile_script(script, "<bench>").expect("compile");
    let mut last = RegisterValue::undefined();
    let start = Instant::now();
    for _ in 0..iterations {
        let mut runtime = RuntimeState::new();
        last = Interpreter::new()
            .execute_module(&module, &mut runtime)
            .expect("exec")
            .return_value();
    }
    let total = start.elapsed().as_nanos() as u64;
    (total / u64::from(iterations), last)
}

fn report(label: &str, interp_ns: u64, jit_compile_ns: u64, jit_exec_ns: u64) {
    let speedup = if jit_exec_ns > 0 {
        interp_ns as f64 / jit_exec_ns as f64
    } else {
        f64::INFINITY
    };
    let breakeven = if interp_ns > jit_exec_ns {
        let saved_per_call = interp_ns - jit_exec_ns;
        if saved_per_call > 0 { jit_compile_ns / saved_per_call } else { u64::MAX }
    } else {
        u64::MAX // JIT slower — never breaks even.
    };
    println!(
        "[PERF] {label:<30} interp={:>8}ns  jit_exec={:>8}ns  compile={:.1}ms  speedup={:.2}x  breakeven={} calls",
        interp_ns, jit_exec_ns,
        jit_compile_ns as f64 / 1_000_000.0,
        speedup, breakeven,
    );
}

// ============================================================
// Benchmarks
// ============================================================

#[test]
fn perf_sum_loop_1000() {
    let script = "var sum = 0; var i = 0; while (i < 1000) { sum += i; i++; } sum;";
    let iters = 10;

    let (interp_ns, interp_val) = measure_interpreter_with_runtime(script, iters);

    match measure_jit(script, iters) {
        Some((compile_ns, exec_ns, jit_val)) => {
            assert_eq!(interp_val, jit_val, "results must match");
            report("sum_loop(1000)", interp_ns, compile_ns, exec_ns);
        }
        None => {
            println!("[PERF] sum_loop(1000)            JIT path failed");
            println!("[PERF]   interpreter: {}ns/iter", interp_ns);
        }
    }
}

#[test]
fn perf_sum_loop_10000() {
    let script = "var sum = 0; var i = 0; while (i < 10000) { sum += i; i++; } sum;";
    let iters = 5;

    let (interp_ns, interp_val) = measure_interpreter_with_runtime(script, iters);

    match measure_jit(script, iters) {
        Some((compile_ns, exec_ns, jit_val)) => {
            assert_eq!(interp_val, jit_val, "results must match");
            report("sum_loop(10000)", interp_ns, compile_ns, exec_ns);
        }
        None => {
            println!("[PERF] sum_loop(10000)           JIT path failed");
            println!("[PERF]   interpreter: {}ns/iter", interp_ns);
        }
    }
}

#[test]
fn perf_nested_50x50() {
    let script = "var r = 0; var i = 0; while (i < 50) { var j = 0; while (j < 50) { r += 1; j++; } i++; } r;";
    let iters = 5;

    let (interp_ns, interp_val) = measure_interpreter_with_runtime(script, iters);

    match measure_jit(script, iters) {
        Some((compile_ns, exec_ns, jit_val)) => {
            assert_eq!(interp_val, jit_val, "results must match");
            report("nested(50x50)", interp_ns, compile_ns, exec_ns);
        }
        None => {
            println!("[PERF] nested(50x50)             JIT path failed");
            println!("[PERF]   interpreter: {}ns/iter", interp_ns);
        }
    }
}

#[test]
fn perf_sum_loop_100000() {
    let script = "var sum = 0; var i = 0; while (i < 100000) { sum += i; i++; } sum;";
    let iters = 3;

    let (interp_ns, interp_val) = measure_interpreter_with_runtime(script, iters);

    match measure_jit(script, iters) {
        Some((compile_ns, exec_ns, jit_val)) => {
            assert_eq!(interp_val, jit_val, "results must match");
            report("sum_loop(100000)", interp_ns, compile_ns, exec_ns);
        }
        None => {
            println!("[PERF] sum_loop(100000)          JIT path failed");
            println!("[PERF]   interpreter: {}ns/iter", interp_ns);
        }
    }
}

/// THE KEY BENCHMARK: pure JIT vs pure interpreter on a raw arithmetic loop.
/// Uses hand-crafted bytecode (no bootstrap/globals) to measure raw speedup.
#[test]
fn perf_pure_jit_vs_interpreter() {
    use otter_vm::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset};
    use otter_vm::frame::FrameLayout;
    use otter_vm::module::{Function, Module};
    use otter_vm::FunctionIndex;

    // Build raw bytecode for: sum=0; i=0; while(i<N) { sum+=i; i++; } return sum;
    // Registers: r0=sum, r1=i, r2=limit, r3=temp(cmp), r4=temp(1)
    // Expected result: sum of 0..99999 = 4999950000 — overflows i32!
    // JS semantics: += on large numbers → f64. But our bytecode uses i32 Add
    // which overflows to f64 in interpreter, while JIT AddI32 deopts on overflow.
    // Use a smaller limit that stays in i32 range.
    let limit = 10_000i32; // sum = 49995000, fits i32
    let function2 = Function::with_bytecode(
        Some("pure_loop"),
        FrameLayout::new(0, 0, 0, 5).expect("layout"),
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 0),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), limit),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::lt(BytecodeRegister::new(3), BytecodeRegister::new(1), BytecodeRegister::new(2)),
            Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
            Instruction::add(BytecodeRegister::new(0), BytecodeRegister::new(0), BytecodeRegister::new(1)),
            Instruction::add(BytecodeRegister::new(1), BytecodeRegister::new(1), BytecodeRegister::new(4)),
            Instruction::jump(JumpOffset::new(-5)),
            Instruction::ret(BytecodeRegister::new(0)),
        ]),
    );
    let module2 = Module::new(Some("pure-loop"), vec![function2], FunctionIndex(0)).expect("mod");
    let function = module2.entry_function();
    let reg_count = usize::from(function.frame_layout().register_count());

    // ---- Pure interpreter ----
    let iters = 10u32;
    let interp_start = Instant::now();
    let mut interp_val = RegisterValue::undefined();
    for _ in 0..iters {
        let regs = vec![RegisterValue::undefined(); reg_count];
        interp_val = Interpreter::new()
            .resume(&module2, FunctionIndex(0), 0, &regs)
            .expect("exec").return_value();
    }
    let interp_per = interp_start.elapsed().as_nanos() as u64 / u64::from(iters);

    // ---- Compile JIT ----
    let compile_start = Instant::now();
    let compiled = compile_function(function).expect("JIT must compile pure arithmetic");
    let compile_ns = compile_start.elapsed().as_nanos() as u64;

    // ---- Pure JIT execution ----
    let mut jit_val = RegisterValue::undefined();
    let jit_start = Instant::now();
    for _ in 0..iters {
        let mut regs = vec![RegisterValue::undefined(); reg_count];
        let result = execute_function(function, &mut regs).expect("jit exec");
        match result {
            JitExecResult::Ok(raw) => {
                jit_val = RegisterValue::from_raw_bits(raw).expect("valid");
            }
            JitExecResult::Bailout { bytecode_pc, reason } => {
                println!("[PERF] BAILOUT at pc={bytecode_pc} reason={reason:?}");
                println!("[PERF] interpreter: {interp_per}ns/iter");
                return;
            }
            JitExecResult::NotCompiled => {
                println!("[PERF] NOT COMPILED");
                return;
            }
        }
    }
    let jit_per = jit_start.elapsed().as_nanos() as u64 / u64::from(iters);

    assert_eq!(interp_val, jit_val, "results must match");

    let speedup = interp_per as f64 / jit_per as f64;
    println!(
        "[PERF] PURE_LOOP(10K)                 interp={:>10}ns  jit={:>10}ns  compile={:.1}ms  SPEEDUP={:.1}x  code={}B",
        interp_per, jit_per,
        compile_ns as f64 / 1_000_000.0,
        speedup,
        compiled.code_size,
    );
}

#[test]
fn perf_compile_latency() {
    let scripts = [
        ("tiny(1+2)", "1 + 2;"),
        ("loop(100)", "var s=0; var i=0; while(i<100){s+=i;i++;} s;"),
        ("loop(1000)", "var s=0; var i=0; while(i<1000){s+=i;i++;} s;"),
    ];

    println!("[PERF] === Compile Latency ===");
    for (label, script) in &scripts {
        let module = compile_script(script, "<compile-bench>").expect("compile");
        let function = module.entry_function();

        // 3 samples, take minimum.
        let mut min_ns = u64::MAX;
        for _ in 0..3 {
            let start = Instant::now();
            let _ = compile_function(function);
            let ns = start.elapsed().as_nanos() as u64;
            min_ns = min_ns.min(ns);
        }
        let code_size = compile_function(function).map(|c| c.code_size).unwrap_or(0);
        println!(
            "[PERF]   {label:<20} {:.2}ms  code={} bytes",
            min_ns as f64 / 1_000_000.0,
            code_size,
        );
    }
}
