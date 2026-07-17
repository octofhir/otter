//! Same-stack synchronous native re-entry invariants.
//!
//! # Contents
//! - A public high-level native binding that invokes one rooted JS callback.
//! - Interpreter/template parity for success, throw cleanup, stack diagnostics,
//!   and a compiled callback's uncommon bailout path.
//! - A second script on the same runtime proving the activation state remains
//!   reusable after every nested completion mode.
//!
//! # Invariants
//! - `NativeScope::call` re-enters through the caller's activation stack.
//! - A nested callback's `Error.stack` retains JavaScript frames below the
//!   native boundary.
//! - Return, throw, and JIT bailout all clean back to the native call floor.
//! - No callback value or result crosses an allocation without a scoped root.

use otter_runtime::{
    JitSelection, NativeCtx, NativeError, Runtime, RuntimeExecutionStats, SourceInput, Value,
};

fn invoke_rooted_callback(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let callback = scope.argument(args, 0);
        let argument = scope.argument(args, 1);
        let receiver = scope.undefined();
        let result = scope.call(callback, receiver, &[argument])?;
        Ok(scope.finish(result))
    })
}

struct RunResult {
    completion: String,
    reuse_completion: String,
    stats: RuntimeExecutionStats,
}

fn run(selection: JitSelection) -> RunResult {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    runtime
        .install_native_global("__nativeInvoke", 2, invoke_rooted_callback)
        .expect("install rooted callback native");

    let completion = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                function successLeaf(value) {
                    return value + 1;
                }
                function nativeBoundary(callback, value) {
                    return __nativeInvoke(callback, value);
                }

                // Compile both the JS-to-native caller and the callback entered
                // back from NativeScope::call without loop OSR.
                let warmChecksum = 0;
                for (let i = 0; i < 512; i++) {
                    warmChecksum += nativeBoundary(successLeaf, i);
                }

                function stackLeaf() {
                    const stack = new Error("same-stack-reentry").stack;
                    return [
                        stack.includes("stackLeaf"),
                        stack.includes("stackOuter"),
                        stack.includes("nativeBoundary")
                    ].join(":");
                }
                function stackOuter() {
                    return nativeBoundary(stackLeaf, 0);
                }

                function throwingLeaf() {
                    throw new Error("nested native callback throw");
                }
                function throwingOuter() {
                    return nativeBoundary(throwingLeaf, 0);
                }

                let caughtMessage = false;
                try {
                    throwingOuter();
                } catch (error) {
                    caughtMessage = String(error).includes("nested native callback throw");
                }
                const afterThrow = nativeBoundary(successLeaf, 41);

                function bailoutLeaf(value) {
                    if (typeof value === "number") {
                        return value + 2;
                    }
                    const payload = { value, nested: [1, 2, 3] };
                    return payload.value.length + payload.nested.length;
                }
                for (let i = 0; i < 512; i++) {
                    bailoutLeaf(i);
                }
                // The numeric hot path has compiled; this uncommon object path
                // resumes through the template tier's re-entrant bailout.
                const bailoutResult = nativeBoundary(bailoutLeaf, "abcd");
                const afterBailout = nativeBoundary(successLeaf, 99);

                function jsonAbruptCleanup() {
                    let caught = false;
                    try {
                        JSON.stringify({
                            get value() {
                                throw new Error("json abrupt completion");
                            }
                        });
                    } catch (error) {
                        caught = String(error).includes("json abrupt completion");
                    }
                    return caught && JSON.stringify({ reusable: 42 }) === '{"reusable":42}';
                }

                JSON.stringify([
                    warmChecksum,
                    stackOuter(),
                    caughtMessage,
                    afterThrow,
                    bailoutResult,
                    afterBailout,
                    jsonAbruptCleanup()
                ]);
                "#,
            ),
            "runtime-reentry-invariants.js",
        )
        .expect("same-stack re-entry fixture")
        .completion_string()
        .to_owned();

    let reuse_completion = runtime
        .run_script(
            SourceInput::from_javascript("__nativeInvoke(value => value * 2, 21);"),
            "runtime-reentry-reuse.js",
        )
        .expect("runtime remains reusable")
        .completion_string()
        .to_owned();

    RunResult {
        completion,
        reuse_completion,
        stats: runtime.execution_stats(),
    }
}

#[test]
fn native_scope_call_reuses_the_current_activation_stack() {
    let oracle = run(JitSelection::InterpreterOnly);
    let compiled = run(JitSelection::Template);

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        "[131328,\"true:true:true\",true,42,7,100,true]"
    );
    assert_eq!(compiled.reuse_completion, oracle.reuse_completion);
    assert_eq!(compiled.reuse_completion, "42");
    assert!(
        compiled.stats.jit_compile_attempts > 0,
        "fixture must compile the native caller and nested callback"
    );
    assert_eq!(
        compiled.stats.jit_osr_attempts, 0,
        "fixture must exercise whole-function entries, not loop OSR"
    );
    assert!(
        compiled.stats.jit_reentrant_stub_transitions > 0,
        "uncommon nested callback path must cross a JIT bailout transition"
    );
}
