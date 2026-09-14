//! Base-constructor bodies spliced into the caller's allocated SSA graph.
//!
//! # Contents
//! - Shared nursery allocation with explicit full-call misses.
//! - Object/primitive return selection and exact post-allocation exits.
//!
//! # Invariants
//! - Artifacts prove actual receiver probes and inlined field operations.
//! - Numeric exits retain the allocated receiver and new.target without replay.

#![cfg(target_arch = "aarch64")]
use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn inline_base_constructor_allocates_and_resumes_once() {
    for declaration in [
        "var C=(function(){return function C(x,replacement){x.count++;x.saved=this;this.x=x.value+1;this.target=new.target;return replacement;};})();C.prototype={};",
        "class C {constructor(x,replacement){x.count++;x.saved=this;this.x=x.value+1;this.target=new.target;return replacement;}}",
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(JitSelection::ProductionTiered)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .unwrap();
        let warm = runtime
            .run_script(
                SourceInput::from_javascript(format!(
                    "{declaration}\n{}",
                    r#"
function make(x,replacement){return new C(x,replacement);}
var payload={count:0,saved:null,value:1.25};for(var i=0;i<70000;i++)make(payload,5);
"#
                )),
                "inline-construct-warm.js",
            )
            .unwrap();
        let irs = warm
            .jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .filter(|b| b.manifest().function_name() == "make")
            .filter_map(|b| b.file(JitArtifactFileName::OptimizedIr))
            .map(|f| String::from_utf8_lossy(f.contents()).into_owned())
            .collect::<Vec<_>>();
        assert!(
            irs.iter()
                .any(|ir| ir.contains("GuardCallTarget { guard: Construct")
                    && ir.contains("AllocateObject {")
                    && ir.contains("AllocationHit")
                    && ir.contains("PublishObject {")
                    && ir.contains("BaseConstructResult")
                    && ir.contains("CacheIrStoreField {")),
            "factory must splice constructor allocation and fields: {irs:#?}; {:?}",
            warm.jit_debug_report()
        );
        let before = runtime.execution_stats();
        let steady = runtime
            .run_script(
                SourceInput::from_javascript(
                    "for(var i=0;i<1024;i++)make(payload,5);payload.saved.x;",
                ),
                "inline-construct-steady.js",
            )
            .unwrap();
        assert_eq!(steady.completion_string(), "2.25");
        let after = runtime.execution_stats();
        assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
        if std::env::var("OTTER_GC_STRESS")
            .ok()
            .is_none_or(|s| s == "0")
        {
            assert!(
                after.jit_receiver_alloc_generated - before.jit_receiver_alloc_generated > 512,
                "nursery hit must actually execute: {before:?} -> {after:?}"
            );
            assert!(
                after.jit_generated_calls - before.jit_generated_calls < 1536,
                "construct linkage must remain cold: {before:?} -> {after:?}"
            );
        }
        let result = runtime.run_script(SourceInput::from_javascript(r#"
payload.value=2.25;var replacement={answer:37};var objectResult=make(payload,replacement);
var fn=function answer(){};var fnResult=make(payload,fn);
var a=make(payload,undefined),b=make(payload,'primitive'),c=make(payload,Symbol('primitive')),d=make(payload,13n);
JSON.stringify([objectResult===replacement,fnResult===fn,a.x,b.x,c.x,d.x,a.target===C,a instanceof C,a!==b]);
"#),"inline-construct-results.js").unwrap();
        assert_eq!(
            result.completion_string(),
            "[true,true,3.25,3.25,3.25,3.25,true,true,true]"
        );
        let result = runtime.run_script(SourceInput::from_javascript(r#"
var coercions=0,badValue={valueOf(){coercions++;return 7;}};
for(var i=0;i<16;i++)make(payload,5);
payload.count=0;payload.saved=null;payload.value=badValue;
var instance=make(payload,5);
JSON.stringify([instance===payload.saved,instance.x,instance.target===C,instance instanceof C,payload.count,coercions]);
"#),"inline-construct-exit.js").unwrap();
        assert_eq!(result.completion_string(), "[true,8,true,true,1,1]");
        if std::env::var("OTTER_GC_STRESS")
            .ok()
            .is_none_or(|s| s == "0")
        {
            assert!(
                result
                    .jit_debug_report()
                    .unwrap()
                    .events()
                    .iter()
                    .any(|e| matches!(
                        e,
                        otter_vm::JitDebugEvent::InlineDeoptFrame {
                            index: 1,
                            total: 2,
                            ..
                        }
                    )),
                "must exit an already allocated inline constructor: {:?}",
                result.jit_debug_report()
            );
        }
    }
}

#[test]
fn constructor_parameter_guard_retains_allocated_entry() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
class NumberBox {constructor(x){this.x=x+1;this.target=new.target;}}
function box(x){return new NumberBox(x);}
for(var i=0;i<70000;i++)box(1.25);
"#,
            ),
            "inline-construct-parameter-warm.js",
        )
        .unwrap();
    assert!(
        warm.jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .any(|bundle| bundle.manifest().function_name() == "box"
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| {
                        let ir = String::from_utf8_lossy(file.contents());
                        ir.contains("GuardCallTarget { guard: Construct")
                            && ir.contains("AllocateObject {")
                            && ir.contains("PublishObject {")
                    })),
        "numeric constructor must be spliced"
    );
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var coercions=0,input={valueOf(){coercions++;return 8;}};
for(var i=0;i<16;i++)box(1.25);
var result=box(input);
JSON.stringify([result.x,result.target===NumberBox,result instanceof NumberBox,coercions]);
"#,
            ),
            "inline-construct-parameter-exit.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "[9,true,true,1]");
    if std::env::var("OTTER_GC_STRESS")
        .ok()
        .is_none_or(|s| s == "0")
    {
        assert!(
            result
                .jit_debug_report()
                .unwrap()
                .events()
                .iter()
                .any(|event| matches!(
                    event,
                    otter_vm::JitDebugEvent::InlineDeoptFrame {
                        index: 1,
                        total: 2,
                        ..
                    }
                )),
            "parameter guard must resume the allocated inline entry: {:?}",
            result.jit_debug_report()
        );
    }
}

#[test]
fn inline_constructor_roots_receiver_across_getter_collection() {
    let mut runtime = Runtime::builder()
        .max_heap_bytes(8 * 1024 * 1024)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
class Cell {constructor(p){p.saved=this;this.value=p.value;this.target=new.target;}}
function cell(p){return new Cell(p);}
var p={saved:null,value:null};for(var i=0;i<70000;i++)cell(p);
"#,
            ),
            "inline-construct-gc-warm.js",
        )
        .unwrap();
    assert!(
        warm.jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .any(|bundle| bundle.manifest().function_name() == "cell"
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| {
                        let ir = String::from_utf8_lossy(file.contents());
                        ir.contains("GuardCallTarget { guard: Construct")
                            && ir.contains("AllocateObject {")
                            && ir.contains("PublishObject {")
                    }))
    );
    let before = runtime.execution_stats();
    let result=runtime.run_script(SourceInput::from_javascript(r#"
var allocate=false,reads=0,last=null;
Object.defineProperty(p,'value',{get(){
    if(allocate){reads++;for(var k=0;k<60000;k++)last={marker:37,next:null};}
    return last;
},configurable:true});
for(var i=0;i<16;i++)cell(p);
allocate=true;
var result=cell(p);
JSON.stringify([result===p.saved,result.value===last,result.value.marker,result.target===Cell,reads]);
"#),"inline-construct-gc-getter.js").unwrap();
    assert_eq!(result.completion_string(), "[true,true,37,true,1]");
    let after = runtime.execution_stats();
    assert!(
        after.gc_minor_cycles + after.gc_cycles > before.gc_minor_cycles + before.gc_cycles,
        "getter must trigger collection"
    );
    assert_eq!(
        after.jit_optimized_deopts, before.jit_optimized_deopts,
        "property cold completion must remain inside the generated constructor"
    );
}
