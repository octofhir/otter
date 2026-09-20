//! Actual numeric-body SSA inlining and nested deopt completion.
//!
//! # Contents
//! - Guarded callee bodies inside an optimizing caller.
//! - Caller effects, callee coercion and return continuation exactly once.
//!
//! # Invariants
//! - A source inlining claim requires emitted IR and real optimizing entries.
//! - A body exit reconstructs the callee and the caller after its call.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn scalar_inline_body_and_deopt_preserve_call_continuation() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function leaf(x) { return x + 3; }
function caller(x, mark) { mark.n++; var y = leaf(x); return y + 10; }
var mark = {n:0};
for(var i=0;i<70000;i++) caller(2,mark);
"#,
            ),
            "inline-warm.js",
        )
        .unwrap();
    let bundle = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "caller"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .unwrap_or_else(|| panic!("caller must optimize: {:?}", warm.jit_debug_report()));
    let ir = String::from_utf8_lossy(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    );
    assert!(
        ir.contains("GuardCallTarget { guard: Plain"),
        "caller must contain spliced body: {ir}"
    );
    assert!(
        !ir.contains("Direct {"),
        "the spliced leaf must have no native call descriptor: {ir}"
    );
    let before = runtime.execution_stats();
    let fast = runtime
        .run_script(
            SourceInput::from_javascript(
                "var valid=0; for(var i=0;i<512;i++) if(caller(2,mark)===15) valid++; valid;",
            ),
            "inline-fast.js",
        )
        .unwrap();
    assert_eq!(fast.completion_string(), "512");
    let after = runtime.execution_stats();
    assert!(
        after.jit_optimized_entries + after.jit_generated_optimizing_entries
            > before.jit_optimized_entries + before.jit_generated_optimizing_entries
    );
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var countBefore = mark.n, conversions = 0;
var value = caller({[Symbol.toPrimitive]:function(){conversions++; return '4';}}, mark);
JSON.stringify([value, mark.n-countBefore, conversions]);
"#,
            ),
            "inline-deopt.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "[\"4310\",1,1]");
    assert!(
        result
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                otter_runtime::JitDebugEvent::InlineDeoptFrame { function_id: 1, .. }
            )),
        "a real body exit must reconstruct the leaf activation"
    );
    assert!(
        warm.jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                otter_runtime::JitDebugEvent::InlineLowered {
                    parent_function_id: 2,
                    callee_function_id: 1,
                    outcome: otter_vm::JitInlineLoweringOutcome::Inlined,
                    ..
                }
            )),
        "successful Machine compilation must report the actual splice"
    );
}

#[test]
fn branching_inline_returns_and_identity_miss() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function choose(x) { if (x < 0) return x - 3; return x * 2; }
function caller(x) { return choose(x) + choose(-x); }
for(var i=0;i<70000;i++) caller(2);
"#,
            ),
            "inline-branch-warm.js",
        )
        .unwrap();
    let bundle = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "caller"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .unwrap();
    let ir = String::from_utf8_lossy(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    );
    assert!(
        ir.matches("GuardCallTarget { guard: Plain").count() >= 2,
        "both calls must splice: {ir}"
    );
    let fast = runtime
        .run_script(
            SourceInput::from_javascript("JSON.stringify([caller(2), caller(-4), caller(0)]);"),
            "inline-branch-fast.js",
        )
        .unwrap();
    assert_eq!(fast.completion_string(), "[-1,1,0]");
    let changed = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var calls=0;
choose=function(x){ calls++; return x+10; };
JSON.stringify([caller(3), calls]);
"#,
            ),
            "inline-identity.js",
        )
        .unwrap();
    assert_eq!(changed.completion_string(), "[20,2]");
}

#[test]
fn inline_body_throw_preserves_outer_handler_and_gc_roots() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function leaf(x) { return x - 3; }
function caller(x, keep) { var y=leaf(x); return [y,keep]; }
var keep={marker:127};
for(var i=0;i<70000;i++) caller(9,keep);
"#,
            ),
            "inline-root-warm.js",
        )
        .unwrap();
    let bundle = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "caller"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .unwrap();
    let ir = String::from_utf8_lossy(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    );
    assert!(
        ir.contains("GuardCallTarget { guard: Plain"),
        "caller must splice: {ir}"
    );
    let result = runtime.run_script(SourceInput::from_javascript(r#"
var conversions=0;
var pair=caller({valueOf:function(){conversions++;var garbage=[];for(var j=0;j<100;j++)garbage.push({j:j});return 11;}},keep);
var thrown={marker:256}, caught;
try { caller({valueOf:function(){throw thrown;}},keep); } catch(e) { caught=e; }
JSON.stringify([pair[0],pair[1]===keep,pair[1].marker,conversions,caught===thrown]);
"#), "inline-root-deopt.js").unwrap();
    assert_eq!(result.completion_string(), "[8,true,127,1,true]");
}

#[test]
fn inline_in_caller_loop_keeps_osr_and_backedge_state() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function leaf(x) { return x + 3; }
function loop(n,x) { var sum=0; for(var j=0;j<n;j++) sum+=leaf(x); return sum; }
for(var i=0;i<70000;i++) loop(2,4);
"#,
            ),
            "inline-loop-warm.js",
        )
        .unwrap();
    let bundle = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "loop"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .unwrap();
    let ir = String::from_utf8_lossy(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    );
    assert!(
        ir.contains("GuardCallTarget { guard: Plain"),
        "loop must splice: {ir}"
    );
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var calls=0;
JSON.stringify([loop(1000,5),loop(3,{valueOf:function(){calls++;return 4;}}),calls]);
"#,
            ),
            "inline-loop-deopt.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "[8000,21,3]");
}

#[test]
fn inline_arrow_restores_exact_closure_and_lexical_this() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var leaf=(function(){'use strict';return (x)=>this+x;}).call(7);
function caller(x){ return leaf(x); }
for(var i=0;i<70000;i++) caller(2);
"#,
            ),
            "inline-arrow-warm.js",
        )
        .unwrap();
    let bundle = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "caller"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .unwrap();
    let ir = String::from_utf8_lossy(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    );
    assert!(
        ir.contains("GuardCallTarget { guard: Plain"),
        "arrow must splice: {ir}"
    );
    let cycles = runtime.heap_stats().gc_cycles;
    runtime.force_gc().unwrap();
    assert!(runtime.heap_stats().gc_cycles > cycles);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var conversions=0;
var fast=caller(5);
var cold=caller({[Symbol.toPrimitive]:function(){conversions++;return '4';}});
JSON.stringify([fast,cold,conversions]);
"#,
            ),
            "inline-arrow-deopt.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "[12,\"74\",1]");
    assert!(
        result
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(event, otter_runtime::JitDebugEvent::InlineDeoptFrame { .. }))
    );
}
