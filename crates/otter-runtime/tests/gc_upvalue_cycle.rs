//! Counter-closure leak regression — verifies that the
//! canonical `function counter() { let n = 0; return () => ++n; }`
//! shape returns to baseline `live_bytes` after the outer
//! reference is dropped and a full GC fires.
//!
//! Phase 1 caveat: `JsObject::trace_gc_roots` is still a stub
//! (lands with task 77), so the script's `globalThis` does NOT
//! root the inner closure. The test allocates the upvalue chain
//! through host-side surfaces only, so the leak detection is
//! on the heap stats — not on JS-level reachability.
//!
//! # See also
//!
//! - GC architecture plan §4.1, §6.3.
//! - Task 76 — UpvalueCell migration.

use otter_runtime::{Runtime, SourceInput};
use otter_vm::UPVALUE_CELL_TYPE_TAG;

#[test]
fn counter_closure_no_leak_after_force_gc() {
    let mut rt = Runtime::builder().build().expect("runtime");

    // Sample baseline before the script runs.
    let baseline = rt.heap_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;

    // Run the canonical counter-closure idiom; the outer
    // function returns the inner arrow that captures `n`.
    // `MakeClosure` allocates an `UpvalueCellBody` on the GC
    // heap for `n`.
    let result = rt
        .run_script(
            SourceInput::from_javascript(
                "function counter() { let n = 0; return () => ++n; }\n\
                 let c = counter(); c(); c(); c(); c();\n",
            ),
            "<counter>",
        )
        .expect("script ran");
    let _ = result;

    // After the script returns, host-side roots are gone.
    // Phase-1 walker stops at `JsObject` (task 77) so the
    // closure isn't reachable through `globalThis` either —
    // every upvalue cell becomes unreachable. Force a full GC
    // and assert the per-type live byte count returns to
    // baseline.
    rt.force_gc();
    let after = rt.heap_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after, baseline,
        "counter-closure upvalues must be reclaimed by force_gc"
    );
}
