//! Bounded and cancellable polling through the public direct-runtime API.
//!
//! # Contents
//! - A pending waiter preserves its reaction across host interruption.
//! - Reactions that immediately notify a new waiter cannot consume another slice.
//!
//! # Invariants
//! - No elapsed-time threshold decides correctness.
//! - Zero-budget slices poll once and leave newly parked work for the next call.
//! - Full collections between slices preserve waiter promises and source contexts.

use otter_runtime::{JitSelection, OtterError, Runtime, SourceInput};
use std::time::Duration;

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

fn run(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), "atomic-driver.js")
        .unwrap()
        .completion_string()
        .to_owned()
}

#[test]
fn interruption_preserves_the_waiter_until_a_deliberate_resume() {
    for tier in TIERS {
        let mut runtime = Runtime::builder().jit_selection(tier).build().unwrap();
        run(
            &mut runtime,
            "globalThis.view=new Int32Array(new SharedArrayBuffer(4));globalThis.labels=[];Atomics.waitAsync(view,0,0).value.then(label=>labels.push(label));",
        );
        let interrupt = runtime.interrupt_handle();
        interrupt.interrupt();
        let result = runtime.drive_pending_atomic_waits(Duration::from_millis(20));
        assert!(
            matches!(result, Err(OtterError::Interrupted)),
            "{tier:?}: {result:?}"
        );
        assert!(
            interrupt.is_interrupted(),
            "the host owns clearing its interrupt"
        );
        interrupt.reset();
        runtime.force_gc().unwrap();
        assert_eq!(run(&mut runtime, "JSON.stringify(labels);"), "[]");
        assert_eq!(run(&mut runtime, "Atomics.notify(view,0,1);"), "1");
        assert!(runtime.drive_pending_atomic_waits(Duration::ZERO).unwrap());
        assert_eq!(run(&mut runtime, "JSON.stringify(labels);"), "[\"ok\"]");
    }
}

#[test]
fn ready_reaction_chains_respect_each_slice_and_survive_collection() {
    for tier in TIERS {
        let mut runtime = Runtime::builder().jit_selection(tier).build().unwrap();
        run(
            &mut runtime,
            r#"
            globalThis.view=new Int32Array(new SharedArrayBuffer(4));
            globalThis.steps=0;
            function next() {
                if (++steps < 32) {
                    Atomics.waitAsync(view,0,0).value.then(next);
                    if (Atomics.notify(view,0,1)!==1) throw new Error('notification');
                }
            }
            next();
        "#,
        );
        assert!(
            !runtime.drive_pending_atomic_waits(Duration::ZERO).unwrap(),
            "{tier:?}: a ready chain must not consume the entire queue"
        );
        assert_eq!(run(&mut runtime, "steps;"), "2");
        for expected in 3..=32 {
            runtime.force_gc().unwrap();
            assert_eq!(
                runtime.drive_pending_atomic_waits(Duration::ZERO).unwrap(),
                expected == 32
            );
            assert_eq!(run(&mut runtime, "steps;"), expected.to_string());
        }
        assert!(runtime.drive_pending_atomic_waits(Duration::ZERO).unwrap());
    }
}
