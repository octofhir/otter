//! Public TypedArray reverse work-control and raw-byte regressions.
//!
//! # Contents
//! - Fatal rejection preserves exactly the completed outer element pairs.
//! - Observing and yielding policies account for every logical pair.
//!
//! # Invariants
//! - Setup and inspection do not run under the rejecting policy.
//! - A byte oracle checks the entire view, without decoding floating or BigInt values.
//! - Fatal host control bypasses JavaScript catches.
//!
//! # See also
//! - `otter-vm/src/binary/typed_array_prototype/reverse.rs`.

use otter_runtime::{
    JitSelection, OtterError, Runtime, SourceInput, WorkBudget, WorkBudgetExceededAction,
};

const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

#[test]
fn typed_reverse_preserves_every_element_bit_and_ignores_arguments() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../../otter-difftest/corpus/typed_array_reverse_control.js"
                )),
                "reverse-control.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "ok:typed-reverse-control");
    }
}

#[test]
fn typed_reverse_yields_without_draining_microtasks() {
    for selection in SELECTIONS {
        for input in [
            "new Uint8Array(6000)",
            "new BigUint64Array(6000)",
            "new Uint8Array(new SharedArrayBuffer(6000))",
        ] {
            let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
            runtime.run_script(SourceInput::from_javascript(format!("globalThis.reverseInput={input};globalThis.reverseRaw=new Uint8Array(reverseInput.buffer);for(let i=0;i<reverseRaw.length;++i)reverseRaw[i]=(i*37+11)%251;globalThis.reverseLog=[];")),"reverse-yield-setup.js").unwrap();
            runtime.set_work_budget(WorkBudget {
                on_exceeded: WorkBudgetExceededAction::Yield,
                max_work_units_per_turn: Some(128),
                ..WorkBudget::default()
            });
            let before = runtime.work_budget_stats().forced_yields;
            let result=runtime.run_script(SourceInput::from_javascript("queueMicrotask(()=>reverseLog.push('microtask'));const result=reverseInput.reverse();reverseLog.push('after');result===reverseInput;"),"reverse-yield.js").unwrap();
            assert_eq!(result.completion_string(), "true");
            assert!(runtime.work_budget_stats().forced_yields >= before + 20);
            assert_eq!(runtime.work_budget_stats().budget_rejections, 0);
            runtime.set_work_budget(WorkBudget::default());
            runtime.force_gc().unwrap();
            assert_eq!(runtime.eval(SourceInput::from_javascript("{const width=reverseInput.BYTES_PER_ELEMENT;let valid=true;for(let i=0;i<6000;++i)for(let b=0;b<width;++b)if(reverseRaw[i*width+b]!==(((5999-i)*width+b)*37+11)%251)valid=false;JSON.stringify([valid,reverseLog]);}")).unwrap().completion_string(),"[true,[\"after\",\"microtask\"]]");
        }
    }
}

#[test]
fn typed_reverse_observing_policy_charges_pairs_and_skips_short_views() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        runtime
            .run_script(
                SourceInput::from_javascript("globalThis.reverseInput=new Float64Array(6001);"),
                "reverse-observe-setup.js",
            )
            .unwrap();
        let before = runtime.work_budget_stats().work_units_executed;
        runtime
            .run_script(
                SourceInput::from_javascript("reverseInput.reverse();"),
                "reverse-observe.js",
            )
            .unwrap();
        assert!(runtime.work_budget_stats().work_units_executed - before >= 3000);
        let result=runtime.run_script(SourceInput::from_javascript("{const empty=new Uint8Array(0),one=new Uint8Array([19]);JSON.stringify([empty.reverse()===empty,one.reverse()===one,one[0]]);}"),"reverse-short.js").unwrap();
        assert_eq!(result.completion_string(), "[true,true,19]");
    }
}

#[test]
fn typed_reverse_rejects_work_and_preserves_only_complete_outer_pairs() {
    for selection in SELECTIONS {
        for input in [
            "new Uint8Array(6000)",
            "new Float64Array(new ArrayBuffer(48016),8,6000)",
            "new BigUint64Array(new ArrayBuffer(48016),8,6000)",
            "new Uint8Array(new SharedArrayBuffer(6000))",
            "new Uint8Array(new ArrayBuffer(6000,{maxByteLength:12000}))",
        ] {
            let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
            runtime.run_script(SourceInput::from_javascript(format!(
            "globalThis.reverseInput = {input}; globalThis.reverseBytes=new Uint8Array(reverseInput.buffer,reverseInput.byteOffset,reverseInput.byteLength); globalThis.reverseWidth=reverseInput.BYTES_PER_ELEMENT; for(let i=0;i<reverseBytes.length;++i) reverseBytes[i]=(i*37+11)%251; globalThis.reverseCaught=false; globalThis.reverseAfter=false;"
        )), "reverse-input.js").unwrap();
            runtime.set_work_budget(WorkBudget {
                on_exceeded: WorkBudgetExceededAction::Reject,
                max_work_units_per_turn: Some(1000),
                ..WorkBudget::default()
            });
            let error = runtime.run_script(SourceInput::from_javascript(
            "try { reverseInput.reverse(); } catch(error) { reverseCaught=true; } reverseAfter=true;"
        ), "reverse-reject.js").expect_err("reverse must charge each pair before swapping it");
            assert!(
                matches!(error, OtterError::Runtime { diagnostic } if diagnostic.code == "BUDGET_EXCEEDED")
            );
            runtime.set_work_budget(WorkBudget::default());
            runtime.force_gc().unwrap();
            let result = runtime.eval(SourceInput::from_javascript(
            "{ let pairs=0; while(pairs<3000&&reverseBytes[pairs*reverseWidth]===(((5999-pairs)*reverseWidth)*37+11)%251) ++pairs; let valid=pairs>0&&pairs<=1000; for(let i=0;i<6000;++i){const source=i<pairs||i>=6000-pairs?5999-i:i;for(let b=0;b<reverseWidth;++b)if(reverseBytes[i*reverseWidth+b]!==(((source*reverseWidth+b)*37+11)%251)) valid=false;} JSON.stringify([valid,reverseCaught,reverseAfter]); }"
        )).unwrap();
            assert_eq!(
                result.completion_string(),
                "[true,false,false]",
                "{selection:?}"
            );
        }
    }
}
