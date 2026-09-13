//! Generated loose equality and committed coercion across exception/GC edges.
//!
//! # Contents
//! - Unseen comparisons, object identity and primitive comparison classes.
//! - Cold coercion exactly once, pure catch values and live moving roots.
//!
//! # Invariants
//! - Unseen object operands must not consume a numeric-deopt budget.
//! - Both comparison operators retain HTMLDDA and revoked Proxy semantics.
//! - A coercive miss completes once through the shared committed boundary.

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
fn loose_equality_probes_preserve_identity_coercion_and_catch_roots() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(RuntimeExtensionInstaller::new(|ctx| {
            ctx.install_native_global("__otter_is_htmldda", 0, html_dda)
        }))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function pair(a,b,live) { return [a == b, a != b, live]; }
function unseen(a,b,take) { if (take) return a != b; return false; }
function equals(a,b,live) { try { return a == b ? live : null; } catch(e) { return e; } }
function notEquals(a,b,live) { try { return a != b ? live : null; } catch(e) { return e; } }
var left = {}, right = {}, live = {tag:'live'}, token = {};
for (var i = 0; i < 70000; i++) {
  unseen(left,right,false); equals(left,right,live); notEquals(left,left,live); pair(left,right,live);
}
"#,
            ),
            "loose-warm.js",
        )
        .unwrap();
    for name in ["unseen", "equals", "notEquals", "pair"] {
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
        assert!(
            String::from_utf8_lossy(
                bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .unwrap()
                    .contents()
            )
            .contains("LooseEqualityProbe")
        );
    }
    let before = runtime.execution_stats();
    let fast = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var fastMatches = 0;
for (var i = 0; i < 512; i++) if (unseen(left,right,true) && !unseen(left,left,true)) fastMatches++;
fastMatches;
"#,
            ),
            "loose-unseen.js",
        )
        .unwrap();
    assert_eq!(fast.completion_string(), "512");
    let after = runtime.execution_stats();
    assert!(
        after.jit_optimized_entries + after.jit_generated_optimizing_entries
            - before.jit_optimized_entries
            - before.jit_generated_optimizing_entries
            >= 512,
        "unseen comparison must execute optimizing code: {before:?} -> {after:?}"
    );
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts, before.jit_generated_call_deopts,
        "the first unseen object comparison must not deopt"
    );
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var conversions = 0, throws = 0;
var coerces = {[Symbol.toPrimitive]: function() { conversions++; var value = {n:1}; return value.n; }};
var explodes = {[Symbol.toPrimitive]: function() { throws++; var value = {t:token}; throw value.t; }};
var revoked = Proxy.revocable({}, {}); revoked.revoke();
var symbol = Symbol('s');
var cases = [[left,left,true],[left,right,false],[coerces,explodes,false],
 [revoked.proxy,revoked.proxy,true],[revoked.proxy,left,false],
 [0,-0,true],[-0,0,true],[1/Infinity,-1/Infinity,true],[-1/Infinity,1/Infinity,true],[NaN,NaN,false],[1.5,1.5,true],[1.5,2.5,false],
 [2,2,true],[2,3,false],[1,'1',true],['x','x',true],['x','y',false],
 [1n,1,true],[1n,2n,false],[null,undefined,true],[null,left,false],
 [null,__otter_is_htmldda,true],[__otter_is_htmldda,undefined,true],
 [symbol,symbol,true],[symbol,Symbol('s'),false],[coerces,1,true]];
var matches = 0;
for (var i = 0; i < cases.length; i++) {
 var c = cases[i];
 if (equals(c[0],c[1],live) === (c[2] ? live : null)) matches++;
 if (notEquals(c[0],c[1],live) === (c[2] ? null : live)) matches++;
}
var one = equals(explodes,1,live), two = notEquals(explodes,1,live);
var caught = one === token && two === token;
var joined = pair(coerces, 1, live);
var escaped = false;
try { pair(explodes, 1, live); } catch(e) { escaped = e === token; }
JSON.stringify([matches, cases.length * 2, conversions, throws, caught, live.tag, joined[0], joined[1], joined[2] === live, escaped]);
"#,
            ),
            "loose-matrix.js",
        )
        .unwrap();
    assert_eq!(
        result.completion_string(),
        "[52,52,4,3,true,\"live\",true,false,true,true]"
    );
    runtime.force_gc().unwrap();
    let survived = runtime.run_script(SourceInput::from_javascript(
        "JSON.stringify([equals(left,left,live) === live, notEquals(left,right,live) === live, live.tag]);"
    ), "loose-after-gc.js").unwrap();
    assert_eq!(survived.completion_string(), "[true,true,\"live\"]");
}
