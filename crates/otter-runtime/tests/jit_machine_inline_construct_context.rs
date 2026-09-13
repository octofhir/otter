//! Constructor entry state across an actual nested SSA deoptimization.
//!
//! # Contents
//! - Forwarded actual arguments and new.target after a numeric inline exit.
//!
//! # Invariants
//! - The callee is actually spliced and coercion commits exactly once.
//! - A hot factory and a fresh construct site retain moving argument values.

#![cfg(target_arch = "aarch64")]
use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn constructor_inline_exit_keeps_call_context() {
    for invocation in ["make(payload)", "new C(payload,19)"] {
        let mut runtime = Runtime::builder()
            .jit_selection(JitSelection::ProductionTiered)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .unwrap();
        let warm = runtime.run_script(SourceInput::from_javascript(r#"
function leaf(x){return x+1;}
function C(){var ignored=leaf(this.probe);this.target=new.target;this.initialize.apply(this,arguments);return 5;}
C.prototype={probe:1.25,initialize:function(x,y){this.x=x.value;this.y=y;this.count=arguments.length;}};
function make(x){return new C(x,19);}
var payload={value:37};
for(var i=0;i<70000;i++)make(payload);
"#),"construct-context-warm.js").unwrap();
        let bundles = warm.jit_artifacts().unwrap().bundles();
        assert!(
            bundles.iter().any(|bundle| {
                bundle.manifest().function_name() == "C"
                    && bundle
                        .file(JitArtifactFileName::OptimizedIr)
                        .is_some_and(|ir| {
                            String::from_utf8_lossy(ir.contents()).contains("InlineCallGuard")
                        })
            }),
            "constructor must contain an actual leaf splice"
        );
        assert!(
            bundles.iter().any(|bundle| {
                bundle.manifest().function_name() == "make"
                    && bundle
                        .file(JitArtifactFileName::OptimizedIr)
                        .is_some_and(|ir| {
                            String::from_utf8_lossy(ir.contents()).contains("Direct {")
                        })
            }),
            "factory must have a generated constructor call"
        );
        let source = format!(
            r#"
var coercions=0;C.prototype.probe={{valueOf(){{coercions++;return 7;}}}};
var result={invocation};JSON.stringify([result.x,result.y,result.count,result.target===C,result instanceof C,coercions]);
"#
        );
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "construct-context-deopt.js",
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "[37,19,2,true,true,1]",
            "{invocation}"
        );
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
            "must exercise nested inline reconstruction: {invocation}"
        );
    }
}
