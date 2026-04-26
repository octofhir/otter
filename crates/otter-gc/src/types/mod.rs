//! Per-type GC-managed payload structs that the VM allocates via
//! [`GcHeap::alloc_typed`](crate::heap::GcHeap::alloc_typed).
//!
//! These types live in `otter-gc` (not `otter-vm`) because their
//! per-type [`crate::trace::TraceFn`] implementations need controlled
//! unsafe access to raw pointer fields — `otter-vm` forbids unsafe at
//! the lib level. The VM-side public API (`JsString::concat` and
//! friends) stays in `otter-vm`; it operates on
//! [`crate::gc_ref::GcRef<JsStringGc>`] via the safe
//! [`crate::gc_ref::GcRef::payload`] accessor.
//!
//! Each module here contributes one VM type plus its registration
//! helper. [`register_all`] walks every type and installs its trace
//! function on a fresh [`GcHeap`](crate::heap::GcHeap) — the VM
//! constructs a heap and immediately calls this once.

pub mod string;

use crate::heap::GcHeap;

/// Registers every per-type trace function on `heap`'s trace table.
///
/// Must be called exactly once per `GcHeap`, immediately after
/// construction. Calling twice double-registers and panics
/// (`TraceTable::register` enforces single-registration per tag).
pub fn register_all(heap: &mut GcHeap) {
    string::register(heap);
}
