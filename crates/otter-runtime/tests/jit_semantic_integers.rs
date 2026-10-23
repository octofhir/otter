//! Exact integer speculation over both legal Number encodings.
//!
//! # Contents
//! - Integral Double arguments retain native Int32 arithmetic.
//! - Negative zero exits before the integer projection loses its sign.
//!
//! # Invariants
//! - The producer really returns Double words under the normal tier policy.
//! - Integer feedback describes values, while physical field proofs remain
//!   independent of the arithmetic projection.
//!
//! # See also
//! - `jit_boxed_arithmetic` covers general Float64 boxing and widening.
//! - `jit_numeric_fields` covers physical field summaries and recovery.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn integral_double_arguments_keep_native_integer_arithmetic() {
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
function semanticHalf(n) { return n / 2; }
function semanticPlus(n) {
  for (var probe = 0; probe < 3; probe++) n = n;
  return n + 1;
}
for (var i = 0; i < 12000; i++) semanticHalf(0.5 + (i & 1));
for (var i = 0; i < 70000; i++) semanticPlus(semanticHalf((i & 15) * 2));
"#,
            ),
            "semantic-integer-warm.js",
        )
        .unwrap();
    let integer_ir: Vec<_> = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .filter(|bundle| bundle.manifest().function_name() == "semanticPlus")
        .filter_map(|bundle| bundle.file(JitArtifactFileName::OptimizedIr))
        .collect();
    assert!(
        integer_ir.iter().any(|file| {
            let ir = std::str::from_utf8(file.contents()).unwrap();
            ir.contains("CheckedNumberToInt32")
                && ir.contains("Int32Add")
                && !ir.contains("Float64Add")
        }),
        "integral Double feedback must produce an actual native integer body"
    );
    let before = runtime.execution_stats();
    for integer in [i32::MIN, -731, 0, 719, i32::MAX - 1] {
        let source = format!("semanticHalf({})", i64::from(integer) * 2);
        let bits = runtime
            .eval_value(
                SourceInput::from_javascript(source),
                "semantic-integer-producer.js",
                |_, value| value.to_bits(),
            )
            .unwrap();
        assert_eq!(bits, otter_vm::Value::double(f64::from(integer)).to_bits());
        let source = format!("semanticPlus(semanticHalf({}))", i64::from(integer) * 2);
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "semantic-integer-probe.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), (integer + 1).to_string());
    }
    let after = runtime.execution_stats();
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_template_entries,
        before.jit_generated_template_entries
    );
    let zero = runtime
        .run_script(
            SourceInput::from_javascript("semanticPlus(semanticHalf(-0))"),
            "semantic-negative-zero.js",
        )
        .unwrap();
    assert_eq!(zero.completion_string(), "1");
    assert_eq!(
        runtime.execution_stats().jit_optimized_deopts,
        after.jit_optimized_deopts + 1
    );
}
