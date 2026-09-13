//! Source-owned field initialization inside actual SSA-inlined bodies.
//!
//! # Contents
//! - Generated field writes and setter/throw cold completion.
//! - Moving values and exact exits after a committed field initialization.
//!
//! # Invariants
//! - Initializer calls disappear from caller Machine IR.
//! - Setter effects commit once with callee source frames still published.

#![cfg(target_arch = "aarch64")]
use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn inline_initializer_stores_commit_once() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function initialize(o,x){"use strict";o.x=x;return o.bias+1;}
function caller(o,x,mark){mark.before++;var value=initialize(o,x);mark.after++;return value;}
var payload={value:37},o={x:null,bias:1.25},mark={before:0,after:0};
for(var i=0;i<70000;i++)caller(o,payload,mark);
"#,
            ),
            "inline-store-warm.js",
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
        ir.contains("InlineCallGuard")
            && ir.contains("inline-frames=")
            && ir.contains("PropertyStore {"),
        "{ir}"
    );
    assert!(
        !ir.contains("Direct {"),
        "initializer call must disappear: {ir}"
    );
    let before = runtime.execution_stats();
    let cold=runtime.run_script(SourceInput::from_javascript(r#"
var writes=0,saved,sourceStack;
Object.defineProperty(o,'x',{set(v){writes++;saved=v;sourceStack=new Error().stack;},configurable:true});
var beforeCount=mark.before,afterCount=mark.after;
var answer=caller(o,payload,mark);
JSON.stringify([answer,writes,saved===payload,saved.value,mark.before-beforeCount,mark.after-afterCount,
    sourceStack.indexOf('initialize')>=0,sourceStack.indexOf('caller')>sourceStack.indexOf('initialize')]);
"#),"inline-store-cold.js").unwrap();
    assert_eq!(cold.completion_string(), "[2.25,1,true,37,1,1,true,true]");
    assert_eq!(
        runtime.execution_stats().jit_optimized_deopts,
        before.jit_optimized_deopts
    );
    let thrown=runtime.run_script(SourceInput::from_javascript(r#"
var sentinel={value:91},caught;
Object.defineProperty(o,'x',{set(v){writes++;saved=v;throw sentinel;},configurable:true});
var beforeCount=mark.before,afterCount=mark.after,writesBefore=writes;
try{caller(o,payload,mark);}catch(error){caught=error;}
JSON.stringify([caught===sentinel,writes-writesBefore,saved===payload,mark.before-beforeCount,mark.after-afterCount]);
"#),"inline-store-throw.js").unwrap();
    assert_eq!(thrown.completion_string(), "[true,1,true,1,0]");
    assert_eq!(
        runtime.execution_stats().jit_optimized_deopts,
        before.jit_optimized_deopts
    );
    let readonly=runtime.run_script(SourceInput::from_javascript(r#"
Object.defineProperty(o,'x',{value:null,writable:false,configurable:true});
var caught,beforeCount=mark.before,afterCount=mark.after;
try{caller(o,payload,mark);}catch(error){caught=error;}
JSON.stringify([caught.name,mark.before-beforeCount,mark.after-afterCount,
    caught.stack.indexOf('initialize')>=0,caught.stack.indexOf('caller')>caught.stack.indexOf('initialize')]);
"#),"inline-store-readonly.js").unwrap();
    assert_eq!(
        readonly.completion_string(),
        "[\"TypeError\",1,0,true,true]"
    );
    assert_eq!(
        runtime.execution_stats().jit_optimized_deopts,
        before.jit_optimized_deopts
    );
    let exit=runtime.run_script(SourceInput::from_javascript(r#"
var coercions=0;
Object.defineProperty(o,'x',{set(v){writes++;saved=v;},configurable:true});
o.bias={valueOf(){coercions++;return 7;}};
var beforeCount=mark.before,afterCount=mark.after,writesBefore=writes;
var answer=caller(o,payload,mark);
JSON.stringify([answer,writes-writesBefore,coercions,saved===payload,mark.before-beforeCount,mark.after-afterCount]);
"#),"inline-store-exit.js").unwrap();
    assert_eq!(exit.completion_string(), "[8,1,1,true,1,1]");
    assert!(
        exit.jit_debug_report()
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
        "must exit inside initialized callee: {:?}",
        exit.jit_debug_report()
    );
}
