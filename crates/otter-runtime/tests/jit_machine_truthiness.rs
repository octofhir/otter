//! Generated truthiness and explicit cold leaf completion across moving GC.
//!
//! # Contents
//! - Primitive/object truthiness, native HTMLDDA, and non-observable conversion.
//! - Multiple probes per block and live values spanning a cold call and join.
//!
//! # Invariants
//! - Optimizing IR must contain generated probes and actual native calls occur.
//! - Primitive-cell misses never deopt or invoke user coercion.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, RuntimeExtensionInstaller,
    SourceInput,
};
use otter_vm::{NativeCtx, NativeError, Value};

fn html_dda(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}

#[test]
fn machine_truthiness_preserves_all_value_classes_and_live_joins() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(RuntimeExtensionInstaller::new(|ctx| {
            ctx.install_native_global("__otter_is_htmldda", 0, html_dda)
        }))
        .build()
        .unwrap();
    let warm = runtime.run_script(SourceInput::from_javascript(r#"
function truthPair(a, b, live) { return [!a, !b, live]; }
function truthBranch(a, b) { if (a && b) return 1; return 0; }
function truthLoop(a, b) { var n = 0; for (var i = 0; i < 3; i++) n += truthBranch(a, b); return n; }
var warmTruth = 0;
for (var i = 0; i < 70000; i++) {
  warmTruth += truthBranch({}, {}) + truthLoop({}, {});
  truthPair({}, {}, 42);
}
warmTruth;
"#), "truthiness-warm.js").unwrap();
    assert_eq!(warm.completion_string(), "280000");
    for name in ["truthPair", "truthBranch"] {
        let bundle = warm
            .jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .find(|b| {
                b.manifest().function_name() == name
                    && b.file(JitArtifactFileName::OptimizedIr).is_some()
            })
            .unwrap_or_else(|| panic!("{name} must compile: {:?}", warm.jit_debug_report()));
        let ir = bundle.file(JitArtifactFileName::OptimizedIr).unwrap();
        assert!(String::from_utf8_lossy(ir.contents()).contains("TruthinessProbe"));
    }
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var conversions = 0;
var object = {[Symbol.toPrimitive]: function() { conversions++; throw 'coerced'; }};
var revoked = Proxy.revocable({}, {}); revoked.revoke();
var values = [undefined, null, false, 0, -0, NaN, '', 0n, __otter_is_htmldda,
  true, -1, 1.5, Infinity, 'x', 1n, Symbol('x'), object, [], Math.max, function(){}, revoked.proxy];
var expected = [false,false,false,false,false,false,false,false,false,
  true,true,true,true,true,true,true,true,true,true,true,true];
var matches = 0;
var live = {tag: 'live'};
for (var i = 0; i < 128; i++) {
  var j = i % values.length, k = (i + 7) % values.length;
  var pair = truthPair(values[j], values[k], live);
  if (pair[0] === !expected[j] && pair[1] === !expected[k] && pair[2] === live) matches++;
  if (truthLoop(values[j], values[k]) === (expected[j] && expected[k] ? 3 : 0)) matches++;
}
JSON.stringify([matches, conversions, live.tag]);
"#,
            ),
            "truthiness-probe.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "[256,0,\"live\"]");
    let after = runtime.execution_stats();
    assert!(after.jit_generated_calls - before.jit_generated_calls >= 128);
    assert_eq!(
        after.jit_generated_call_deopts, before.jit_generated_call_deopts,
        "cold truthiness completes through its leaf, never deopt"
    );
}
