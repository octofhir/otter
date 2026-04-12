//! Phase 0.3: Deopt verification tests.
//!
//! Differential testing: run the same script through interpreter-only and
//! JIT-with-fallback, verify both produce the same result.

use otter_jit::deopt::execute_module_entry_with_runtime;
use otter_jit::pipeline::execute_function;
use otter_vm::interpreter::Interpreter;
use otter_vm::source::compile_script;
use otter_vm::{RegisterValue, RuntimeState};

/// Differential test helper: runs the same script through interpreter-only
/// and JIT-with-runtime-fallback, comparing results.
fn differential_test(script: &str, label: &str) {
    let module =
        compile_script(script, &format!("<deopt-diff-{label}>")).expect("script should compile");

    // Interpreter path (full runtime, this is the reference).
    let interp_result = Interpreter::new()
        .execute(&module)
        .unwrap_or_else(|e| panic!("[{label}] interpreter failed: {e}"));

    // JIT path with runtime fallback.
    let mut runtime = RuntimeState::new();
    let jit_result =
        execute_module_entry_with_runtime(&module, &mut runtime, std::ptr::null(), None)
            .unwrap_or_else(|e| panic!("[{label}] jit fallback failed: {e}"));

    assert_eq!(
        jit_result.return_value(),
        interp_result.return_value(),
        "[{label}] interpreter={:?} vs jit_fallback={:?}",
        interp_result.return_value(),
        jit_result.return_value(),
    );
}

// ============================================================
// Differential tests: arithmetic
// ============================================================

#[test]
fn diff_add_i32() {
    differential_test("1 + 2;", "add_i32");
}

#[test]
fn diff_sub_i32() {
    differential_test("10 - 3;", "sub_i32");
}

#[test]
fn diff_mul_i32() {
    differential_test("6 * 7;", "mul_i32");
}

#[test]
fn diff_div_i32() {
    differential_test("42 / 2;", "div_i32");
}

#[test]
fn diff_mod_i32() {
    differential_test("17 % 5;", "mod_i32");
}

#[test]
fn diff_neg() {
    differential_test("-(42);", "neg");
}

// ============================================================
// Differential tests: arithmetic loops
// ============================================================

#[test]
fn diff_loop_sum_small() {
    differential_test(
        "var sum = 0; var i = 0; while (i < 10) { sum += i; i++; } sum;",
        "loop_sum_small",
    );
}

#[test]
fn diff_loop_sum_large() {
    differential_test(
        "var sum = 0; var i = 0; while (i < 1000) { sum += i; i++; } sum;",
        "loop_sum_large",
    );
}

#[test]
fn diff_nested_loop() {
    differential_test(
        "var r = 0; var i = 0; while (i < 10) { var j = 0; while (j < 10) { r += 1; j++; } i++; } r;",
        "nested_loop",
    );
}

// ============================================================
// Differential tests: comparisons
// ============================================================

#[test]
fn diff_lt() {
    differential_test("3 < 5;", "lt");
}

#[test]
fn diff_gt() {
    differential_test("5 > 3;", "gt");
}

#[test]
fn diff_eq() {
    differential_test("42 === 42;", "eq");
}

#[test]
fn diff_ne() {
    differential_test("42 !== 43;", "ne");
}

// ============================================================
// Differential tests: conditionals
// ============================================================

#[test]
fn diff_if_true() {
    differential_test("var x = 0; if (true) { x = 42; } x;", "if_true");
}

#[test]
fn diff_if_false() {
    differential_test(
        "var x = 0; if (false) { x = 42; } else { x = 7; } x;",
        "if_false",
    );
}

// ============================================================
// Differential tests: mixed types (should deopt and resume)
// ============================================================

#[test]
fn diff_string_concat() {
    differential_test("'hello' + ' world';", "string_concat");
}

#[test]
fn diff_typeof() {
    differential_test("typeof 42;", "typeof");
}

#[test]
fn diff_undefined() {
    differential_test("undefined;", "undefined_val");
}

#[test]
fn diff_null() {
    differential_test("null;", "null_val");
}

#[test]
fn diff_bool_true() {
    differential_test("true;", "bool_true");
}

#[test]
fn diff_bool_false() {
    differential_test("false;", "bool_false");
}

// ============================================================
// Differential tests: object operations (likely deopt to interp)
// ============================================================

#[test]
fn diff_object_literal() {
    differential_test("var o = {}; o;", "object_literal");
}

#[test]
fn diff_object_property() {
    differential_test("var o = {}; o.x = 42; o.x;", "object_property");
}

// ============================================================
// Differential tests: function calls
// ============================================================

#[test]
fn diff_function_call() {
    differential_test(
        "function add(a, b) { return a + b; } add(20, 22);",
        "function_call",
    );
}

#[test]
fn diff_recursive_call() {
    differential_test(
        "function fib(n) { if (n <= 1) return n; return fib(n - 1) + fib(n - 2); } fib(10);",
        "recursive_call",
    );
}

// ============================================================
// Bailout verification
// ============================================================

/// Tests that JIT-with-fallback produces the same result as pure interpreter
/// for scripts containing unsupported ops (NewObject). The differential_test
/// helper already covers this, but this test explicitly verifies the
/// execute_module_entry_with_runtime path doesn't crash.
#[test]
fn bailout_on_unsupported_graceful() {
    // Use differential_test pattern which is proven to work.
    differential_test("var o = {}; 42;", "bailout_unsupported");
}

// ============================================================
// Telemetry verification
// ============================================================

#[test]
fn telemetry_records_compilation() {
    otter_jit::telemetry::reset();

    let script = "var x = 0; var i = 0; while (i < 5) { x += i; i++; } x;";
    let module = compile_script(script, "<telemetry-test>").expect("should compile");
    let function = module.entry_function();
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];

    let _ = execute_function(function, &mut registers);

    let snap = otter_jit::telemetry::snapshot();
    assert!(
        !snap.tier1_compile_times_ns.is_empty(),
        "should have recorded at least one Tier 1 compilation"
    );
    assert!(
        snap.tier1_compile_times_ns[0] > 0,
        "compile time should be non-zero"
    );
    assert!(
        !snap.functions.is_empty(),
        "should have per-function stats"
    );
}
