//! Regression coverage for guarded `Math.<method>(...)` intrinsic calls.
//!
//! # Contents
//! - Bootstrap calls and observable global, lexical, and method replacement.
//! - Object-to-primitive coercion on the generic boundary.
//! - Optimizing Int32 `abs` / `max` / `min`, including `INT32_MIN` and a
//!   post-tier-up method replacement.
//!
//! # Invariants
//! - Fast paths require the exact bootstrap method identity.
//! - User-visible coercion and replacement remain observable.
//! - Optimizing results match the interpreter oracle exactly.

use otter_runtime::{JitSelection, Otter, Runtime, SourceInput};

fn run(source: &str) {
    Otter::new()
        .blocking_run_script(source)
        .expect("script should run");
}

#[test]
fn math_call_uses_original_method_fast_path() {
    run(r#"
            if (Math.sqrt(9) !== 3) {
                throw new Error("bad sqrt");
            }
        "#);
}

#[test]
fn math_call_observes_method_overwrite() {
    run(r#"
            Math.sqrt = () => 7;
            if (Math.sqrt(9) !== 7) {
                throw new Error("overwritten Math.sqrt ignored");
            }
        "#);
}

#[test]
fn math_call_observes_global_replacement() {
    run(r#"
            globalThis.Math = { sqrt() { return 13; } };
            if (Math.sqrt(9) !== 13) {
                throw new Error("global Math replacement ignored");
            }
        "#);
}

#[test]
fn math_call_observes_lexical_shadow() {
    run(r#"
            let Math = { sqrt() { return 11; } };
            if (Math.sqrt(9) !== 11) {
                throw new Error("lexical Math shadow ignored");
            }
        "#);
}

#[test]
fn math_call_preserves_object_to_primitive() {
    run(r#"
            let hits = 0;
            const value = {
                valueOf() {
                    hits += 1;
                    return 9;
                }
            };
            if (Math.sqrt(value) !== 3 || hits !== 1) {
                throw new Error("object coercion skipped");
            }
        "#);
}

const OPTIMIZING_INT32_MATH: &str = r#"
function int32Math(limit) {
    let checksum = 0;
    for (let index = 0; index < limit; index++) {
        const value = (index & 15) - 8;
        checksum += Math.abs(value);
        checksum += Math.max(value, 2);
        checksum += Math.min(value, 2);
    }
    return checksum;
}

for (let warm = 0; warm < 4010; warm++) int32Math(16);
const beforeReplacement = [
    int32Math(128),
    Math.abs((-2147483647 - 1) | 0),
    Math.max(-7, 2),
    Math.min(-7, 2)
];
Math.abs = value => value + 1000;
const afterReplacement = [
    int32Math(4),
    Math.abs((-2147483647 - 1) | 0),
    Math.max(-7, 2),
    Math.min(-7, 2)
];
JSON.stringify([beforeReplacement, afterReplacement]);
"#;

fn run_int32_math(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(4)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(OPTIMIZING_INT32_MATH),
            "optimizing-int32-math.js",
        )
        .expect("Int32 Math matrix")
        .completion_string()
        .to_owned();
    let optimized_entries = runtime.execution_stats().jit_optimized_entries;
    (completion, optimized_entries)
}

#[test]
fn optimizing_int32_math_matches_interpreter_and_observes_replacement() {
    let (oracle, _) = run_int32_math(JitSelection::InterpreterOnly);
    let (compiled, optimized_entries) = run_int32_math(JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(oracle, r#"[[704,2147483648,2,-7],[3956,-2147482648,2,-7]]"#);
    #[cfg(target_arch = "aarch64")]
    assert!(
        optimized_entries > 0,
        "fixture must enter optimizing code before replacing Math.abs"
    );
}
