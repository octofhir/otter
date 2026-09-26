//! Source-owned global reads inside actual SSA-inlined bodies.
//!
//! # Contents
//! - Generated hits, getter reentry and unresolved-binding throws.
//! - Global-object proofs that survive unrelated global additions and miss
//!   into exact semantics after delete / redefinition.
//!
//! # Invariants
//! - An inlined body's global reads carry the body's own baked proofs.
//! - A dictionary global object is proven by its slot layout: appending a key
//!   keeps every existing key at its slot, while delete / redefine / re-entry
//!   into dictionary mode retire the proof.
//! - Cold reads commit once in the callee's source activation and return to SSA.
//! - Code-owned safepoints preserve exact roots and source stacks across GC.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn inline_global_reads_reenter_with_the_callee_source() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
Object.defineProperty(globalThis,'inlineGlobal',{value:3.25,writable:true,configurable:true});
var holder={value:7,seen:0};
function baselineRead(o){try{o.seen=1;return o.value;}catch(e){return -1;}}
for(var j=0;j<20000;j++)baselineRead(holder);
function leaf(x){return inlineGlobal+x;}
function caller(x,mark){mark.before++;var value=leaf(x);mark.after++;return value;}
var mark={before:0,after:0};
for(var i=0;i<70000;i++)caller(1.25,mark);
"#,
            ),
            "inline-global-warm.js",
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
        ir.contains("GuardCallTarget { guard: Plain") && ir.contains("inline-frames="),
        "{ir}"
    );
    assert!(!ir.contains("Direct {"), "leaf call must disappear: {ir}");
    // The spliced body bakes its own binding proofs: its global read is a
    // guarded generated load, not an unconditional runtime binding call.
    assert!(
        ir.lines().any(|line| line.contains("BindingGuard")
            && line.contains("Read(Global")
            && line.contains("target: Global(")),
        "inlined global read must carry a generated proof: {ir}"
    );
    assert!(
        warm.jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .any(|bundle| bundle.manifest().function_name() == "baselineRead"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_none())
    );
    assert!(
        !warm
            .jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .any(|bundle| bundle.manifest().function_name() == "baselineRead"
                && bundle.file(JitArtifactFileName::OptimizedIr).is_some())
    );
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
    let safepoints: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Safepoints)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let mut inline_records = 0;
    for point in safepoints["safepoints"].as_array().unwrap() {
        for frame in point["inlineFrames"].as_array().unwrap() {
            inline_records += 1;
            let entry = &frame["entry"];
            assert!(entry["closure"].is_u64());
            for slot in frame["slots"]
                .as_array()
                .unwrap()
                .iter()
                .chain([&entry["this"], &entry["closure"]])
            {
                if let Some(index) = slot.as_u64() {
                    assert!(
                        point["taggedLocations"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|root| root["kind"] == "spillSlot"
                                && root["index"].as_u64() == Some(index))
                    );
                } else {
                    assert!(slot.is_null());
                }
            }
        }
    }
    assert!(
        inline_records > 0,
        "the safepoint artifact must expose the actual inline recipe"
    );
    runtime.force_gc().unwrap();
    let before = runtime.execution_stats();
    let result=runtime.run_script(SourceInput::from_javascript(r#"
var reads=0,stackText='',beforeCount=mark.before,afterCount=mark.after;
Object.defineProperty(globalThis,'inlineGlobal',{get(){reads++;stackText=(new Error('probe')).stack;return baselineRead(holder);},configurable:true});
var result=caller(1.25,mark);
JSON.stringify([result,reads,mark.before-beforeCount,mark.after-afterCount,
    stackText.indexOf('leaf')>=0,stackText.indexOf('caller')>stackText.indexOf('leaf'),
    stackText.indexOf('inline-global-warm.js:6:')>=0,stackText.indexOf('inline-global-warm.js:7:')>=0]);
"#),"inline-global-getter.js").unwrap();
    assert_eq!(
        result.completion_string(),
        "[8.25,1,1,1,true,true,true,true]"
    );
    let after = runtime.execution_stats();
    assert!(
        after.jit_optimized_entries + after.jit_generated_optimizing_entries
            > before.jit_optimized_entries + before.jit_generated_optimizing_entries
    );
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
delete globalThis.inlineGlobal;
var beforeCount=mark.before,afterCount=mark.after,caught;
try{caller(1.25,mark);}catch(error){caught=error;}
JSON.stringify([caught.name,mark.before-beforeCount,mark.after-afterCount,
    caught.stack.indexOf('leaf')>=0,caught.stack.indexOf('caller')>caught.stack.indexOf('leaf')]);
"#,
            ),
            "inline-global-missing.js",
        )
        .unwrap();
    assert_eq!(
        result.completion_string(),
        "[\"ReferenceError\",1,0,true,true]"
    );
    assert_eq!(
        runtime.execution_stats().jit_optimized_deopts,
        after.jit_optimized_deopts
    );
    let result=runtime.run_script(SourceInput::from_javascript(r#"
var reads=0,coercions=0,beforeCount=mark.before,afterCount=mark.after;
Object.defineProperty(globalThis,'inlineGlobal',{get(){reads++;return {valueOf(){coercions++;return 7;}};},configurable:true});
var result=caller(1.25,mark);
JSON.stringify([result,reads,coercions,mark.before-beforeCount,mark.after-afterCount]);
"#),"inline-global-deopt.js").unwrap();
    assert_eq!(result.completion_string(), "[8.25,1,1,1,1]");
    assert!(
        result
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                otter_runtime::JitDebugEvent::InlineDeoptFrame {
                    index: 1,
                    total: 2,
                    ..
                }
            ))
    );
}

fn tiered() -> Runtime {
    Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .build()
        .unwrap()
}

#[test]
fn appended_globals_keep_generated_global_reads() {
    let mut runtime = tiered();
    runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var provenGlobal = 5;
function readGlobal(n) { var s = 0; for (var i = 0; i < n; i++) s += provenGlobal; return s; }
for (var k = 0; k < 400; k++) readGlobal(200);
"#,
            ),
            "layout-warm.js",
        )
        .unwrap();
    // Unrelated globals appear after the reader was compiled.
    runtime
        .run_script(
            SourceInput::from_javascript(
                "globalThis.lateGlobalA = 1; var lateGlobalB = 2; globalThis.lateGlobalC = 3;",
            ),
            "layout-append.js",
        )
        .unwrap();
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript("readGlobal(20000)"),
            "layout-read.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "100000");
    let after = runtime.execution_stats();
    let stubs = after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions;
    assert!(
        stubs < 100,
        "appending globals must not retire the read's proof: {stubs} reentrant transitions"
    );
}

#[test]
fn deleted_or_redefined_globals_miss_to_exact_semantics() {
    let mut runtime = tiered();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
globalThis.movingGlobal = 3;
function readMoving() { return movingGlobal; }
var warm = 0;
for (var k = 0; k < 20000; k++) warm += readMoving();
var observed = [warm];
delete globalThis.movingGlobal;
try { readMoving(); observed.push("no throw"); } catch (e) { observed.push(e.name); }
var getterCalls = 0;
Object.defineProperty(globalThis, "movingGlobal", { get() { getterCalls++; return 11; }, configurable: true });
var afterGetter = 0;
for (var k = 0; k < 20000; k++) afterGetter += readMoving();
observed.push(afterGetter, getterCalls);
Object.defineProperty(globalThis, "movingGlobal", { value: 7, writable: true, configurable: true });
var afterData = 0;
for (var k = 0; k < 20000; k++) afterData += readMoving();
observed.push(afterData);
JSON.stringify(observed);
"#,
            ),
            "layout-delete.js",
        )
        .unwrap();
    assert_eq!(
        result.completion_string(),
        "[60000,\"ReferenceError\",220000,20000,140000]"
    );
}
