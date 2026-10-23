//! Number arithmetic for mutable tagged inputs with boxed consumers.
//!
//! # Contents
//! - Fractional and overflowing values after Int32 warmup.
//! - Pre-operation coercion exits after committed property reads.
//! - Optimized Float64 results preserve Double words across integral values,
//!   -0, NaN, subnormals and infinities.
//!
//! # Invariants
//! - Widening a stored-value category exits dependent bodies at most twice;
//!   the bodies re-optimize over the generalized numeric domain.
//! - A failed Number guard never repeats a getter or coercion effect.
//! - Float64 boxing purifies NaN and preserves its Double encoding.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

#[test]
fn boxed_immediates_accept_numbers_without_repeated_deopts() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function plus(o) { for (var probe=0; probe<3; probe++) o=o; return o.x + 1; }
function minus(o) { for (var probe=0; probe<3; probe++) o=o; return o.x - 1; }
function power(o) { for (var probe=0; probe<3; probe++) o=o; return Math.pow(10, o.x + 1); }
function integer(o) { for (var probe=0; probe<3; probe++) o=o; return (o.x + 1) | 0; }
var box = {x:2};
for (var i=0; i<70000; i++) { plus(box); minus(box); integer(box); power(box); }
"#,
            ),
            "boxed-warm.js",
        )
        .unwrap();
    for name in ["plus", "minus", "power", "integer"] {
        assert!(
            warm.jit_artifacts()
                .unwrap()
                .bundles()
                .iter()
                .any(|bundle| {
                    bundle.manifest().function_name() == name
                        && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
                }),
            "{name} must optimize: {:?}",
            warm.jit_debug_report()
        );
    }
    let before = runtime.execution_stats();
    let numeric = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var count = 0;
var tenToTheFraction = Math.pow(10, 2.5);
for (var i=0; i<70000; i++) {
 box.x = 1.5;
 if (plus(box) === 2.5 && minus(box) === 0.5 && integer(box) === 2
   && power(box) === tenToTheFraction) count++;
}
count;
"#,
            ),
            "boxed-fraction.js",
        )
        .unwrap();
    assert_eq!(numeric.completion_string(), "70000");
    let after = runtime.execution_stats();
    // A fractional field generalizes the Int32 representation the bodies
    // depended on: each exits at most twice, at the generalization and at
    // one representation guard, and the loop runs optimized again with the
    // bodies standalone or inlined into its optimized loop body.
    let optimized = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| {
            generation.tier == NativeFrameKind::Optimizing
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.current_entry
        })
        .count();
    assert!(optimized >= 1, "{optimized} optimized entries");
    assert!(
        after.jit_optimized_deopts - before.jit_optimized_deopts <= 8,
        "{before:?} -> {after:?}"
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

#[test]
fn multiplication_preserves_signed_zero_in_every_compiled_tier() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(selection)
            .build()
            .unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
function mul(pair) { for (var probe=0; probe<3; probe++) pair=pair; return pair.left * pair.right; }
var pair = {left: 3, right: 4};
for (var i = 0; i < 70000; i++) mul(pair);
function isProduct(left, right, expected) {
  pair.left = left;
  pair.right = right;
  return Object.is(mul(pair), expected);
}
[
  isProduct(1, 0, 0),
  isProduct(-1, 0, -0),
  isProduct(1, -0, -0),
  isProduct(-1, -0, 0),
  isProduct(0, -1, -0),
  isProduct(-0, 1, -0),
  isProduct(-0, -1, 0)
].join(',');
"#,
                ),
                "jit-multiply-signed-zero.js",
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "true,true,true,true,true,true,true",
            "{selection:?}"
        );
        if matches!(selection, JitSelection::ProductionTiered) {
            let stats = runtime.execution_stats();
            assert!(
                stats.jit_optimized_entries + stats.jit_generated_optimizing_entries > 0,
                "the production assertion must execute optimizing code"
            );
        }
    }
}

#[test]
fn optimized_float64_boxing_preserves_double_words() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function half(x) { for (var probe=0; probe<3; probe++) x=x; return x / 2; }
function negate(x) { for (var probe=0; probe<3; probe++) x=x; return -x; }
var sink = 0;
for (var i=0; i<70000; i++) { sink += half((i & 31) + 0.5) + negate((i & 15) + 0.25); }
sink;
"#,
            ),
            "boxing-warm.js",
        )
        .unwrap();
    for name in ["half", "negate"] {
        let ir = warm
            .jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .filter(|bundle| bundle.manifest().function_name() == name)
            .filter_map(|bundle| bundle.file(JitArtifactFileName::OptimizedIr))
            .map(|file| String::from_utf8_lossy(file.contents()).into_owned())
            .collect::<String>();
        assert!(
            ir.contains("Float64ToTagged"),
            "{name} must box its double result: {ir}"
        );
    }
    let inputs = [
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        3.0,
        -3.0,
        6.0,
        -6.0,
        f64::from(i32::MAX),
        f64::from(i32::MIN),
        2.0 * f64::from(i32::MAX),
        2.0 * f64::from(i32::MIN),
        2.0 * f64::from(i32::MIN) - 2.0,
        4_294_967_296.0,
        4_294_967_298.0,
        8_589_934_592.0,
        -8_589_934_592.0,
        9_007_199_254_740_992.0,
        1e300,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        5e-324,
        -5e-324,
        1e-310,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ];
    let before = runtime.execution_stats();
    for input in inputs {
        let literal = if input.is_nan() {
            "NaN".to_owned()
        } else if input.is_infinite() {
            if input > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
        } else {
            format!("{input:e}")
        };
        for (name, expected) in [("half", input / 2.0), ("negate", -input)] {
            let bits = runtime
                .eval_value(
                    SourceInput::from_javascript(format!("{name}({literal})")),
                    "boxing-probe.js",
                    |_, value| value.to_bits(),
                )
                .unwrap();
            assert_eq!(
                bits,
                otter_vm::Value::double(expected).to_bits(),
                "{name}({literal}) must preserve its purified Double word"
            );
        }
    }
    let after = runtime.execution_stats();
    // Graph entries are uncounted: each probe script interprets only its own
    // top level, and no Template body serves the callee.
    assert_eq!(
        after.bytecode_calls - before.bytecode_calls,
        2 * inputs.len() as u64,
        "the probes must call the optimized bodies: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.jit_generated_template_entries,
        before.jit_generated_template_entries
    );
    assert_eq!(
        after.jit_optimized_deopts, before.jit_optimized_deopts,
        "numeric inputs keep the optimized bodies"
    );
    assert!(
        runtime
            .jit_code_generation_snapshot()
            .iter()
            .filter(|generation| generation.tier == NativeFrameKind::Optimizing
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.current_entry)
            .count()
            >= 2
    );
}
