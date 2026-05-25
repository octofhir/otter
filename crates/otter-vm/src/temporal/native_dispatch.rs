//! Shared anchor for the per-class `couch!` blocks.

#![allow(missing_docs)]

use crate::object::{self, JsObject};

pub fn temporal_host(global: JsObject, heap: &mut otter_gc::GcHeap) -> JsObject {
    object::get(global, heap, "Temporal")
        .and_then(|v| v.as_object())
        .expect("Temporal namespace must be installed before class constructors")
}
