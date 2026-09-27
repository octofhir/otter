//! Snapshot relocation of closure-owned capture storage.
//!
//! # Contents
//! - Shared mutable captures called after restore, donor destruction and GC.
//!
//! # Invariants
//! - Restored native/VM capture windows must address the restored closure.
//! - Sibling closures preserve their shared binding identity.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn restored_closures_share_captures_after_donor_is_dropped() {
    let mut donor = Runtime::builder().build().expect("donor");
    donor
        .eval(SourceInput::from_javascript(
            "globalThis.pair = (function () { let n = 10; return [() => ++n, () => n]; })();",
        ))
        .expect("capture owners");
    donor.force_gc().expect("empty nursery before snapshot");
    let snapshot = donor.capture_isolate_snapshot().expect("capture");
    let mut restored = Runtime::from_isolate_snapshot(&snapshot).expect("restore");
    drop(donor);
    restored.force_gc().expect("restored collection");
    let result = restored.eval(SourceInput::from_javascript(
        "let sum = 0; for (let i = 0; i < 2000; ++i) { sum += pair[0](); if (pair[1]() !== i + 11) throw Error('alias'); } sum;",
    )).expect("restored captures");
    assert_eq!(result.completion_string(), "2021000");
    restored.force_gc().expect("collection after calls");
    let result = restored
        .eval(SourceInput::from_javascript("pair[1]()"))
        .expect("capture survives");
    assert_eq!(result.completion_string(), "2010");
}
