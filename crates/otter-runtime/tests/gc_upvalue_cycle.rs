//! Counter-closure leak regression — verifies that the
//! canonical `function counter() { let n = 0; return () => ++n; }`
//! shape returns to baseline `live_bytes` once the closure becomes
//! unreachable and a full GC fires.
//!
//! `globalThis` and the persistent script scope are both traced, so a
//! top-level `let` binding would keep its own upvalue cell live for the
//! lifetime of the runtime. The test therefore runs the whole idiom
//! inside an IIFE: once it returns, every local (`counter`, `c`, and the
//! captured `n` cell) is unreachable, so forcing GC must return the
//! per-type live byte count to baseline — a real reclamation check
//! rather than a snapshot of an unrooted heap.
//!
//! # See also
//!
//! - GC architecture plan §4.1, §6.3.

use otter_runtime::{Runtime, SourceInput};
use otter_vm::UPVALUE_CELL_TYPE_TAG;

#[test]
fn counter_closure_no_leak_after_force_gc() {
    let mut rt = Runtime::builder().build().expect("runtime");

    // Sample baseline before the script runs.
    let baseline = rt.heap_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;

    // Run the canonical counter-closure idiom inside an IIFE; the inner
    // function returns the arrow that captures `n`, and `MakeClosure`
    // allocates an `UpvalueCellBody` on the GC heap for `n`. Nothing
    // escapes the IIFE, so when it returns the closure and its captured
    // upvalue become unreachable.
    let result = rt
        .run_script(
            SourceInput::from_javascript(
                "(function () {\n\
                 \x20 function counter() { let n = 0; return () => ++n; }\n\
                 \x20 let c = counter(); c(); c(); c(); c();\n\
                 })();\n",
            ),
            "<counter>",
        )
        .expect("script ran");
    let _ = result;

    // The IIFE left no live reference, so every upvalue cell is
    // unreachable. Force a full GC and assert the per-type live byte
    // count returns to baseline.
    rt.force_gc().expect("force GC");
    let after = rt.heap_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after, baseline,
        "counter-closure upvalues must be reclaimed by force_gc"
    );
}
