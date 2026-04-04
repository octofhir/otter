//! Integration tests for MIR construction from bytecode.

use otter_jit::mir::builder::build_mir;
use otter_jit::mir::verify::verify;
use otter_vm::source;

fn compile_and_build_mir(source: &str) -> otter_jit::mir::graph::MirGraph {
    let module = source::compile_script(source, "test.js").expect("compilation failed");
    // The module body is at index 0 (named after the source URL).
    // Pick the first named user function (skip the module body).
    let func = module
        .functions()
        .iter()
        .skip(1)
        .next()
        .unwrap_or_else(|| &module.functions()[0]);
    build_mir(func, None).expect("MIR should build")
}

#[test]
fn test_simple_return() {
    let graph = compile_and_build_mir("function f() { return 42; }");
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
    assert!(graph.block_count() >= 1);
}

#[test]
fn test_arithmetic_loop() {
    let graph = compile_and_build_mir(
        "function f(n) { let sum = 0; for (let i = 0; i < n; i++) { sum += i; } return sum; }",
    );
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
    // Should have multiple blocks (loop header, body, exit).
    assert!(
        graph.block_count() >= 3,
        "expected >=3 blocks, got {}",
        graph.block_count()
    );
}

#[test]
fn test_property_access() {
    let graph = compile_and_build_mir("function f(obj) { return obj.x + obj.y; }");
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
}

#[test]
fn test_conditional() {
    let graph =
        compile_and_build_mir("function f(x) { if (x > 0) { return x; } else { return -x; } }");
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
    // Should have at least 3 blocks: entry, then, else.
    assert!(
        graph.block_count() >= 3,
        "expected >=3 blocks, got {}",
        graph.block_count()
    );
}

#[test]
fn test_function_call() {
    let graph = compile_and_build_mir("function f(g, x) { return g(x + 1); }");
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
}

#[test]
fn test_array_operations() {
    let graph = compile_and_build_mir(
        "function f(arr) { let sum = 0; for (let i = 0; i < arr.length; i++) { sum += arr[i]; } return sum; }",
    );
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
}

#[test]
fn test_closure() {
    let graph =
        compile_and_build_mir("function f() { let x = 10; return function() { return x; }; }");
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
}

#[test]
fn test_try_catch() {
    let graph = compile_and_build_mir("function f() { try { return 1; } catch(e) { return 0; } }");
    println!("{}", graph);
    let result = verify(&graph);
    assert!(result.is_ok(), "verify errors: {:?}", result.err());
}

#[test]
fn test_mir_display_roundtrip() {
    let graph = compile_and_build_mir("function f(a, b) { return a + b; }");
    let display = format!("{}", graph);
    // Verify the display contains expected patterns.
    assert!(display.contains("function f"), "missing function name");
    assert!(
        display.contains("return") || display.contains("return_undefined"),
        "missing return"
    );
}
