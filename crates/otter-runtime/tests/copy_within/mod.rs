//! Public TypedArray copyWithin bounds and work-control regressions.
//!
//! # Contents
//! - Tracking-view revalidation after bound coercion.
//! - Fatal rejection preserves the byte algorithm's directional prefix.
//! - Cooperative yields retain microtask order; observing calls account for bytes.
//!
//! # Invariants
//! - Setup and inspection run outside the rejecting work policy.
//! - Every changed byte is checked against the original deterministic pattern.
//! - Source catches cannot intercept a fatal budget rejection.
//!
//! # See also
//! - `otter-vm/src/binary/typed_array_prototype/copy_within.rs`.

use otter_runtime::{
    JitSelection, OtterError, Runtime, SourceInput, WorkBudget, WorkBudgetExceededAction,
};

const SELECTIONS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

#[test]
fn copy_within_preserves_raw_bits_order_and_live_bounds() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../../otter-difftest/corpus/typed_array_copy_within_control.js"
                )),
                "copy-within-control.js",
            )
            .unwrap();
        assert_eq!(
            result.completion_string(),
            "ok:copy-within-control",
            "{selection:?}"
        );
    }
}

#[test]
fn copy_within_revalidates_tracking_views_after_nonempty_bound_coercion() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime.run_script(SourceInput::from_javascript(
            "{ const buffer = new ArrayBuffer(4, {maxByteLength: 8}); const view = new Uint8Array(buffer, 2); let result; try { view.copyWithin({valueOf(){buffer.resize(1); return 0;}}, 0, 1); result = 'returned'; } catch(error) { result = error.name; } JSON.stringify([result, view.length]); }"
        ), "copy-within-bounds.js").unwrap();
        assert_eq!(
            result.completion_string(),
            "[\"TypeError\",0]",
            "{selection:?}"
        );
    }
}

#[test]
fn copy_within_rejection_preserves_the_exact_directional_byte_prefix() {
    for selection in SELECTIONS {
        for input in [
            "new Uint8Array(20000)",
            "new Float64Array(new ArrayBuffer(20016), 8, 2500)",
            "new BigUint64Array(new ArrayBuffer(20016), 8, 2500)",
            "new Uint8Array(new SharedArrayBuffer(20000))",
            "new Uint8Array(new ArrayBuffer(20000, {maxByteLength: 40000}))",
        ] {
            for (target, source, count) in [(0, 4000, 16000), (4000, 0, 16000)] {
                let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
                runtime.run_script(SourceInput::from_javascript(format!(
                "globalThis.copyWithinInput = {input}; globalThis.copyWithinBytes = new Uint8Array(copyWithinInput.buffer, copyWithinInput.byteOffset, copyWithinInput.byteLength); globalThis.copyWithinWidth = copyWithinInput.BYTES_PER_ELEMENT; for(let i = 0; i < copyWithinBytes.length; ++i) copyWithinBytes[i] = (i * 37 + 11) % 251; globalThis.copyWithinCaught = false; globalThis.copyWithinAfter = false;"
            )), "copy-within-input.js").unwrap();
                runtime.set_work_budget(WorkBudget {
                    on_exceeded: WorkBudgetExceededAction::Reject,
                    max_work_units_per_turn: Some(1000),
                    ..WorkBudget::default()
                });
                let error = runtime.run_script(SourceInput::from_javascript(format!(
                "try {{ copyWithinInput.copyWithin({target} / copyWithinWidth, {source} / copyWithinWidth, {} / copyWithinWidth); }} catch(error) {{ copyWithinCaught = true; }} copyWithinAfter = true;", source + count
            )), "copy-within-reject.js").expect_err("every copied byte must consume work before its write");
                assert!(
                    matches!(error, OtterError::Runtime { diagnostic } if diagnostic.code == "BUDGET_EXCEEDED")
                );
                assert_eq!(runtime.work_budget_stats().budget_rejections, 1);
                runtime.set_work_budget(WorkBudget::default());
                runtime.force_gc().unwrap();
                let backward = source < target && target < source + count;
                let result = runtime.eval(SourceInput::from_javascript(format!(
                "{{ let changed = 0; for(let i = 0; i < 20000; ++i) if(copyWithinBytes[i] !== (i * 37 + 11) % 251) ++changed; let valid = changed > 0 && changed <= 1000; const first = {backward} ? {} - changed : {target}; const last = first + changed; for(let i = 0; i < 20000; ++i) {{ const index = i >= first && i < last ? i + {source} - {target} : i; if(copyWithinBytes[i] !== (index * 37 + 11) % 251) valid = false; }} JSON.stringify([valid, copyWithinCaught, copyWithinAfter]); }}", target + count
            ))).unwrap();
                assert_eq!(
                    result.completion_string(),
                    "[true,false,false]",
                    "{selection:?}: {target}/{source}/{count}"
                );
            }
        }
    }
}

#[test]
fn copy_within_yields_without_draining_microtasks_inside_the_copy() {
    for selection in SELECTIONS {
        for input in [
            "new Uint8Array(8000)",
            "new BigUint64Array(1000)",
            "new Uint8Array(new SharedArrayBuffer(8000))",
        ] {
            let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
            runtime.run_script(SourceInput::from_javascript(format!(
                "globalThis.yieldCopy = {input}; globalThis.yieldBytes = new Uint8Array(yieldCopy.buffer); for(let i=0;i<8000;++i) yieldBytes[i]=(i*37+11)%251; globalThis.copyYieldLog=[];"
            )), "copy-yield-setup.js").unwrap();
            runtime.set_work_budget(WorkBudget {
                on_exceeded: WorkBudgetExceededAction::Yield,
                max_work_units_per_turn: Some(128),
                ..WorkBudget::default()
            });
            let before = runtime.work_budget_stats().forced_yields;
            let result = runtime.run_script(SourceInput::from_javascript(
                "queueMicrotask(()=>copyYieldLog.push('microtask')); const result=yieldCopy.copyWithin(1,0); copyYieldLog.push('after'); result===yieldCopy;"
            ), "copy-yield.js").unwrap();
            assert_eq!(result.completion_string(), "true");
            assert!(runtime.work_budget_stats().forced_yields >= before + 60);
            assert_eq!(runtime.work_budget_stats().budget_rejections, 0);
            runtime.set_work_budget(WorkBudget::default());
            runtime.force_gc().unwrap();
            assert_eq!(runtime.eval(SourceInput::from_javascript(
                "{ const width=yieldCopy.BYTES_PER_ELEMENT; let valid=true; for(let i=width;i<8000;++i) if(yieldBytes[i]!==((i-width)*37+11)%251) valid=false; JSON.stringify([valid,copyYieldLog]); }"
            )).unwrap().completion_string(), "[true,[\"after\",\"microtask\"]]");
        }
    }
}

#[test]
fn copy_within_accounts_for_observing_bytes_and_controls_self_copies() {
    for selection in SELECTIONS {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        runtime
            .run_script(
                SourceInput::from_javascript("globalThis.countedCopy=new Float64Array(2500);"),
                "copy-observe-setup.js",
            )
            .unwrap();
        let before = runtime.work_budget_stats().work_units_executed;
        runtime
            .run_script(
                SourceInput::from_javascript("countedCopy.copyWithin(1,0);"),
                "copy-observe.js",
            )
            .unwrap();
        assert!(runtime.work_budget_stats().work_units_executed - before >= 2499 * 8);
        runtime.set_work_budget(WorkBudget {
            on_exceeded: WorkBudgetExceededAction::Reject,
            max_work_units_per_turn: Some(1000),
            ..WorkBudget::default()
        });
        let error = runtime
            .run_script(
                SourceInput::from_javascript("countedCopy.copyWithin(0,0);"),
                "copy-self-reject.js",
            )
            .expect_err("a nonempty self-copy still owns logical byte work");
        assert!(
            matches!(error, OtterError::Runtime { diagnostic } if diagnostic.code == "BUDGET_EXCEEDED")
        );
        runtime.set_work_budget(WorkBudget::default());
        runtime.force_gc().unwrap();
        assert_eq!(
            runtime
                .eval(SourceInput::from_javascript(
                    "countedCopy[0]===0&&countedCopy[2499]===0"
                ))
                .unwrap()
                .completion_string(),
            "true"
        );
    }
}
