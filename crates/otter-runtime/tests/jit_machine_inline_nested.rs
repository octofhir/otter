//! Nested plain/method SSA inlining with effect-once cold reentry.
//!
//! # Contents
//! - Real dot-product method bodies inside the caller's Machine graph.
//! - Getter return/throw, source stacks, and caller continuation under moving GC.
//! - Four-frame cold publication and deopt through a mixed helper chain.
//!
//! # Invariants
//! - An inlining claim requires emitted body IR and real optimizing entries.
//! - A cold property operation commits once and returns to the same Machine body.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn nested_inline_chain_commits_cold_effects_and_restores_frames() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function dot(b) { return this.x * b.x + this.y * b.y + this.z * b.z; }
function forward(b) { return this.dot(b)+1; }
function wrapper(a,b) { return a.forward(b)+2; }
var proto = {dot:dot,forward:forward};
function caller(a, b, mark) {
    mark.before++;
    var value = wrapper(a,b);
    mark.after++;
    return value;
}
var a = {x:1.25,y:2,z:3}, b = {x:4,y:5,z:6}, mark = {before:0,after:0};
Object.setPrototypeOf(a,proto);
for (var i=0;i<70000;i++) caller(a,b,mark);
"#,
            ),
            "inline-property-warm.js",
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
        ir.matches("GuardCallTarget { guard: Method").count() >= 2
            && ir.contains("GuardCallTarget { guard: Plain")
            && ir.contains("inline-frames="),
        "property body must be spliced: {ir}"
    );
    assert!(
        !ir.contains("Direct {"),
        "dot must have no native call descriptor: {ir}"
    );
    assert!(
        ir.matches("CacheIrLoadField {").count() >= 6,
        "callee field reads must exist: {ir}"
    );
    let before = runtime.execution_stats();
    let fast = runtime
        .run_script(
            SourceInput::from_javascript(
                "var valid=0; for(var i=0;i<512;i++) if(caller(a,b,mark)===36) valid++; valid;",
            ),
            "inline-property-fast.js",
        )
        .unwrap();
    assert_eq!(fast.completion_string(), "512");
    let after = runtime.execution_stats();
    assert!(
        after.jit_optimized_entries + after.jit_generated_optimizing_entries
            - before.jit_optimized_entries
            - before.jit_generated_optimizing_entries
            >= 512
    );
    let before = runtime.execution_stats();
    let cold = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var reads=0, stackText='', beforeCount=mark.before, afterCount=mark.after;
var changed = {get x(){ reads++; stackText=(new Error('probe')).stack; return 7; }, y:2,z:3};
var value = caller(a,changed,mark);
JSON.stringify([value,reads,mark.before-beforeCount,mark.after-afterCount,
    stackText.indexOf('dot')>=0,stackText.indexOf('caller')>stackText.indexOf('dot'),
    stackText.indexOf('inline-property-warm.js:2:')>=0,
    stackText.indexOf('inline-property-warm.js:8:')>=0,
    stackText.indexOf('forward')>stackText.indexOf('dot'),
    stackText.indexOf('wrapper')>stackText.indexOf('forward')]);
"#,
            ),
            "inline-property-cold.js",
        )
        .unwrap();
    assert_eq!(
        cold.completion_string(),
        "[24.75,1,1,1,true,true,true,true,true,true]"
    );
    let after = runtime.execution_stats();
    assert_eq!(
        after.jit_optimized_deopts - before.jit_optimized_deopts,
        0,
        "getter success must return to the Machine body"
    );
    let thrown = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var reads=0, beforeCount=mark.before, afterCount=mark.after, caught=null;
var changed = {get x(){reads++; throw new Error('inline-getter');}, y:2,z:3};
try { caller(a,changed,mark); } catch(error) { caught=error; }
JSON.stringify([caught.message,reads,mark.before-beforeCount,mark.after-afterCount,
    caught.stack.indexOf('dot')>=0,caught.stack.indexOf('caller')>caught.stack.indexOf('dot')]);
"#,
            ),
            "inline-property-throw.js",
        )
        .unwrap();
    assert_eq!(
        thrown.completion_string(),
        "[\"inline-getter\",1,1,0,true,true]"
    );
    let after_throw = runtime.execution_stats();
    assert_eq!(
        after_throw.jit_optimized_deopts - after.jit_optimized_deopts,
        0
    );
    let resumed = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var reads=0, firstReads=0, coercions=0, beforeCount=mark.before, afterCount=mark.after;
var changed = {get x(){firstReads++; return 1;}, y:2,
    get z(){reads++; return {valueOf(){coercions++; return 7;}};}};
var value = caller(a,changed,mark);
JSON.stringify([value,reads,coercions,mark.before-beforeCount,mark.after-afterCount,firstReads]);
"#,
            ),
            "inline-property-deopt.js",
        )
        .unwrap();
    assert_eq!(resumed.completion_string(), "[29.25,1,1,1,1,1]");
    assert!(
        resumed
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                otter_runtime::JitDebugEvent::InlineDeoptFrame {
                    index: 3,
                    total: 4,
                    ..
                }
            )),
        "post-getter numeric exit must reconstruct the callee after its read"
    );
}

#[test]
fn nested_guard_miss_preserves_completed_parent_reads() {
    // An own forward slot keeps the outer guard valid when the inherited dot
    // slot becomes an accessor. The exit must occur inside forward, after bias.
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function dot(b){return this.x*b.x+this.y*b.y+this.z*b.z;}
function forward(b){var before=b.bias;return this.dot(b)+before;}
function wrapper(a,b){return a.forward(b)+2;}
var proto={dot:dot},a={x:1.25,y:2,z:3,forward:forward},b={x:4,y:5,z:6,bias:1};
Object.setPrototypeOf(a,proto);
var mark={before:0,after:0};
function caller(a,b,mark){mark.before++;var result=wrapper(a,b);mark.after++;return result;}
for(var i=0;i<70000;i++)caller(a,b,mark);
"#,
            ),
            "nested-guard-warm.js",
        )
        .unwrap();
    let ir = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "caller"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some()
        })
        .and_then(|bundle| bundle.file(JitArtifactFileName::OptimizedIr))
        .unwrap();
    let ir = String::from_utf8_lossy(ir.contents());
    assert!(
        ir.matches("GuardCallTarget { guard: Method").count() >= 2 && !ir.contains("Direct {"),
        "{ir}"
    );
    runtime.force_gc().unwrap();
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var reads=0,lookups=0,calls=0,stackText='';
Object.defineProperty(b,'bias',{get(){reads++;return 1;}});
Object.defineProperty(proto,'dot',{get(){lookups++;return function replacement(b){
    calls++;stackText=(new Error('nested')).stack;return this.x+b.x+100;
};}});
var beforeCount=mark.before,afterCount=mark.after;
var result=caller(a,b,mark);
JSON.stringify([result,reads,lookups,calls,mark.before-beforeCount,mark.after-afterCount,
    stackText.indexOf('forward')>=0,stackText.indexOf('wrapper')>stackText.indexOf('forward'),
    stackText.indexOf('caller')>stackText.indexOf('wrapper')]);
"#,
            ),
            "nested-guard-miss.js",
        )
        .unwrap();
    assert_eq!(
        result.completion_string(),
        "[108.25,1,1,1,1,1,true,true,true]"
    );
    assert!(runtime.execution_stats().jit_optimized_deopts > before.jit_optimized_deopts);
    assert!(
        result
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                otter_runtime::JitDebugEvent::InlineDeoptFrame {
                    index: 2,
                    total: 3,
                    ..
                }
            )),
        "guard exit must reconstruct caller, wrapper and forward"
    );
}
