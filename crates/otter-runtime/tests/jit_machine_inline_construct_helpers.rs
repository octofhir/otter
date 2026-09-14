//! Constructor SSA bodies nested through ordinary helper calls.
//!
//! # Contents
//! - Enclosing body splices with a generated construct cold sibling.
//! - Exact effects, receiver identity and source frames through nested exits.
//!
//! # Invariants
//! - Emitted outer IR must contain the helper and constructor bodies.
//! - Collection and throws preserve the same caller continuation and receiver.

#![cfg(target_arch = "aarch64")]
use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn enclosing_helper_splices_constructor_with_exact_cold_parent() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
class Cell {constructor(p) {p.saved=this;this.x=p.value+1;this.target=new.target;}}
function helper(p) {return new Cell(p);}
function outer(p) {p.before++;var result=helper(p);p.after++;return result;}
var p={before:0,after:0,saved:null,value:1.25};
for(var i=0;i<70000;i++)outer(p);
"#,
            ),
            "inline-construct-helper-warm.js",
        )
        .unwrap();
    let irs = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .filter(|b| b.manifest().function_name() == "outer")
        .filter_map(|b| b.file(JitArtifactFileName::OptimizedIr))
        .map(|f| String::from_utf8_lossy(f.contents()).into_owned())
        .collect::<Vec<_>>();
    assert!(
        irs.iter()
            .any(|ir| ir.contains("GuardCallTarget { guard: Plain")
                && ir.contains("GuardCallTarget { guard: Construct")
                && ir.contains("AllocateObject {")
                && ir.contains("AllocationHit")
                && ir.contains("PublishObject {")),
        "outer must contain both bodies: {irs:#?}; {:?}",
        warm.jit_debug_report()
    );
    let steady = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var before=p.before,after=p.after,valid=0;
for(var i=0;i<1024;i++){var v=outer(p);if(v===p.saved&&v.x===2.25&&v.target===Cell)valid++;}
JSON.stringify([valid,p.before-before,p.after-after]);
"#,
            ),
            "inline-construct-helper-steady.js",
        )
        .unwrap();
    assert_eq!(steady.completion_string(), "[1024,1024,1024]");
    let cold=runtime.run_script(SourceInput::from_javascript(r#"
var reads=0,text='';
Object.defineProperty(p,'value',{configurable:true,get(){reads++;text=new Error('probe').stack;return 7;}});
var before=p.before,after=p.after,v=outer(p);
JSON.stringify([v===p.saved,v.x,v.target===Cell,reads,p.before-before,p.after-after,
text.indexOf('at Cell (')>=0,text.indexOf('at helper (')>text.indexOf('at Cell ('),text.indexOf('at outer (')>text.indexOf('at helper (')]);
"#),"inline-construct-helper-cold.js").unwrap();
    assert_eq!(
        cold.completion_string(),
        "[true,8,true,1,1,1,true,true,true]"
    );
    let thrown=runtime.run_script(SourceInput::from_javascript(r#"
var reads=0;
Object.defineProperty(p,'value',{configurable:true,get(){reads++;throw new Error('helper-throw');}});
var before=p.before,after=p.after,message='',text='';
try{outer(p);}catch(e){message=e.message;text=e.stack;}
JSON.stringify([message,reads,p.before-before,p.after-after,
text.indexOf('at Cell (')>=0,text.indexOf('at helper (')>text.indexOf('at Cell ('),text.indexOf('at outer (')>text.indexOf('at helper (')]);
"#),"inline-construct-helper-throw.js").unwrap();
    assert_eq!(
        thrown.completion_string(),
        "[\"helper-throw\",1,1,0,true,true,true]"
    );
}

#[test]
fn nested_construct_deopt_keeps_receiver_and_source_generation() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
class Cell {constructor(p) {p.saved=this;this.x=p.value+1;this.target=new.target;}}
function helper(p){return new Cell(p);}
function bridge(p){return helper(p);}
function outer(p) {p.before++;var result=bridge(p);p.after++;return result;}
var p={before:0,after:0,saved:null,value:1.25};
for(var i=0;i<4096;i++)new Cell(p);
for(var i=0;i<4096;i++)helper(p);
for(var i=0;i<4096;i++)bridge(p);
for(var i=0;i<70000;i++)outer(p);
"#,
            ),
            "nested-construct-warm.js",
        )
        .unwrap();
    let bundles = warm.jit_artifacts().unwrap().bundles();
    let helper_id = bundles
        .iter()
        .find(|b| b.manifest().function_name() == "helper")
        .unwrap()
        .manifest()
        .function_id();
    let outer_ids = bundles
        .iter()
        .filter(|b| {
            b.manifest().function_name() == "outer"
                && b.file(JitArtifactFileName::OptimizedIr).is_some_and(|f| {
                    let ir = String::from_utf8_lossy(f.contents());
                    ir.contains("GuardCallTarget { guard: Plain")
                        && ir.contains("GuardCallTarget { guard: Construct")
                        && ir.contains("AllocateObject {")
                        && ir.contains("PublishObject {")
                })
        })
        .map(|b| b.manifest().code_object_id())
        .collect::<Vec<_>>();
    assert!(
        !outer_ids.is_empty(),
        "outer must splice the nested helpers: {:?}",
        warm.jit_debug_report()
    );
    let exit = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var coercions=0,bad={valueOf(){coercions++;return 7;}};
for(var i=0;i<16;i++)outer(p);
p.value=bad;var before=p.before,after=p.after,result=outer(p);
JSON.stringify([result===p.saved,result.x,result.target===Cell,result instanceof Cell,
p.before-before,p.after-after,coercions]);
"#,
            ),
            "nested-construct-exit.js",
        )
        .unwrap();
    assert_eq!(exit.completion_string(), "[true,8,true,true,1,1,1]");
    let events = exit.jit_debug_report().unwrap().events();
    let generated = events.iter().any(|e| {
        matches!(e,otter_vm::JitDebugEvent::GeneratedCallDeopt {
        caller_function_id,caller_code_object_id,..}
        if *caller_function_id==helper_id && outer_ids.contains(caller_code_object_id))
    });
    let inline = events.iter().any(|e| {
        matches!(
            e,
            otter_vm::JitDebugEvent::InlineDeoptFrame {
                index: 3,
                total: 4,
                ..
            }
        )
    });
    assert!(
        generated || inline,
        "must exit the nested constructor: {events:?}"
    );
    if std::env::var("OTTER_GC_STRESS").is_ok_and(|value| value == "1") {
        assert!(
            generated,
            "forced nursery miss must validate helper source in outer generation: {events:?}"
        );
    }
}
