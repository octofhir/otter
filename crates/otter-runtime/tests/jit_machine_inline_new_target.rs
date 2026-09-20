//! Lexical new.target and ordinary return semantics after nested inline deopt.
//!
//! # Contents
//! - A constructor-created arrow that inlines a numeric helper.
//!
//! # Invariants
//! - Deopt retains lexical new.target without treating an arrow as a constructor.
use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};
#[test]
fn inline_exit_in_arrow_keeps_lexical_new_target_and_plain_return() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm=runtime.run_script(SourceInput::from_javascript(r#"
function leaf(x){return x+1;}
function Maker(){this.probe=1.25;this.run=()=>{var ignored=leaf(this.probe);this.seen=new.target;return 7;};}
var owner=new Maker();for(var i=0;i<70000;i++)owner.run();
"#),"inline-new-target-warm.js").unwrap();
    let irs = warm
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .filter_map(|bundle| bundle.file(JitArtifactFileName::OptimizedIr))
        .map(|ir| String::from_utf8_lossy(ir.contents()).into_owned())
        .collect::<Vec<_>>();
    assert!(
        irs.iter()
            .any(|ir| ir.contains("GuardCallTarget { guard: Plain")),
        "must splice leaf into arrow: {irs:#?}; {:?}",
        warm.jit_debug_report()
    );
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var coercions=0;owner.probe={valueOf(){coercions++;return 3;}};
var result=owner.run();JSON.stringify([result===7,owner.seen===Maker,coercions]);
"#,
            ),
            "inline-new-target-exit.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "[true,true,1]");
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
        "{:?}",
        result.jit_debug_report()
    );
}
