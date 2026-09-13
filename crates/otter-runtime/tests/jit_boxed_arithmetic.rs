//! Number arithmetic for mutable tagged inputs with boxed consumers.
//!
//! # Contents
//! - Fractional and overflowing values after Int32 warmup.
//! - Pre-operation coercion exits after committed property reads.
//!
//! # Invariants
//! - Numeric representation changes retain optimizing execution.
//! - A failed Number guard never repeats a getter or coercion effect.

#![cfg(target_arch = "aarch64")]
use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn boxed_immediates_accept_numbers_without_repeated_deopts() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function plus(o) { return o.x + 1; }
function minus(o) { return o.x - 1; }
function power(o) { return Math.pow(10, o.x + 1); }
function integer(o) { return (o.x + 1) | 0; }
var box = {x:2};
for (var i=0; i<70000; i++) { plus(box); minus(box); integer(box); power(box); }
"#,
            ),
            "boxed-warm.js",
        )
        .unwrap();
    for name in ["plus", "minus", "power"] {
        let bundle = warm
            .jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .find(|bundle| {
                bundle.manifest().function_name() == name
                    && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
            })
            .unwrap_or_else(|| panic!("{name} must optimize: {:?}", warm.jit_debug_report()));
        let ir = String::from_utf8_lossy(
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .unwrap()
                .contents(),
        );
        assert!(ir.contains("DecodeNumber"), "{name}: {ir}");
        assert!(
            !ir.contains("IntegerAddImmediate") && !ir.contains("IntegerSubImmediate"),
            "{name}: {ir}"
        );
    }
    let integer = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "integer"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .expect("integer consumer must optimize");
    assert!(
        String::from_utf8_lossy(
            integer
                .file(JitArtifactFileName::OptimizedIr)
                .unwrap()
                .contents()
        )
        .contains("IntegerAddImmediate")
    );
    let before = runtime.execution_stats();
    let numeric = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var count = 0;
for (var i=0; i<512; i++) {
 box.x = 1.5;
 if (plus(box) === 2.5 && minus(box) === 0.5) count++;
}
count;
"#,
            ),
            "boxed-fraction.js",
        )
        .unwrap();
    assert_eq!(numeric.completion_string(), "512");
    let after = runtime.execution_stats();
    assert!(
        after.jit_optimized_entries + after.jit_generated_optimizing_entries
            - before.jit_optimized_entries
            - before.jit_generated_optimizing_entries
            >= 512
    );
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    let matrix = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var values = [2147483647,-2147483648,0,-0,NaN,Infinity,-Infinity,9007199254740992];
var matches = 0;
for (var i=0; i<values.length; i++) {
 var n = values[i]; box.x = n;
 if (Object.is(plus(box), n+1) && Object.is(minus(box), n-1)) matches++;
}
var gets = 0, conversions = 0, token = {};
Object.defineProperty(box, 'x', {get:function() {
 gets++; return {[Symbol.toPrimitive]:function() { conversions++; return '3'; }};
}});
var concatenated = plus(box), subtracted = minus(box);
var plain = {x:2147483647};
JSON.stringify([matches, gets, conversions, concatenated, subtracted, integer(plain)]);
"#,
            ),
            "boxed-matrix.js",
        )
        .unwrap();
    assert_eq!(matrix.completion_string(), "[8,2,2,\"31\",2,-2147483648]");
    let exit = runtime.run_script(SourceInput::from_javascript(
        "var calls = 0; Math.pow = function(base, exponent) { calls++; return {base:base, exponent:exponent}; }; var result = power({x:1.5}); JSON.stringify([calls,result.base,result.exponent]);"
    ), "boxed-call-exit.js").unwrap();
    assert_eq!(exit.completion_string(), "[1,10,2.5]");
}
