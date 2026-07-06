//! VM-typed helpers over [`otter_gc::RootScope`].
//!
//! [`otter_gc::RootScope`] speaks raw `RawGc` slots; this module supplies
//! the erased tracers for VM value shapes (`Value`, value vectors) so call
//! sites can root locals without hand-rolling a [`otter_gc::FrameRoots`]
//! provider per function. Preferred over the legacy patterns (per-alloc
//! `*_with_roots` closures, module-root re-fetch dances) for new code.
//!
//! # Contents
//! - [`RootScopeExt`] — typed `add_*` methods on `RootScope`.
//!
//! # Invariants
//! - Every rooted local must outlive the scope; the `unsafe` on each
//!   method is exactly that obligation.
//!
//! # See also
//! - `otter_gc::root_scope` — the underlying RAII provider.

use otter_gc::RootScope;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::Value;
use crate::object::JsObject;

unsafe fn trace_value_slot(slot: *mut (), visitor: &mut dyn FnMut(*mut RawGc)) {
    // SAFETY: registered for a `*mut Value` slot by `RootScopeExt::add_value`.
    unsafe { (*slot.cast::<Value>()).trace_value_slot_mut(visitor) }
}

unsafe fn trace_value_smallvec4(slot: *mut (), visitor: &mut dyn FnMut(*mut RawGc)) {
    // SAFETY: registered for a `*mut SmallVec<[Value; 4]>` slot.
    let vec = unsafe { &mut *slot.cast::<SmallVec<[Value; 4]>>() };
    for value in vec.iter_mut() {
        value.trace_value_slot_mut(visitor);
    }
}

/// Typed rooting entry points for VM locals.
pub(crate) trait RootScopeExt {
    /// Root a `Value` local.
    ///
    /// # Safety
    /// `slot` must outlive the scope.
    unsafe fn add_value(&mut self, slot: &mut Value);

    /// Root a `JsObject` handle local.
    ///
    /// # Safety
    /// `slot` must outlive the scope.
    unsafe fn add_object(&mut self, slot: &mut JsObject);

    /// Root every element of an argv-shaped vector in place.
    ///
    /// # Safety
    /// `slot` must outlive the scope and must not be moved-from while
    /// the scope is open (replacing its contents is fine).
    unsafe fn add_value_smallvec(&mut self, slot: &mut SmallVec<[Value; 4]>);
}

impl RootScopeExt for RootScope {
    unsafe fn add_value(&mut self, slot: &mut Value) {
        // SAFETY: contract forwarded to the caller.
        unsafe { self.add_erased((slot as *mut Value).cast::<()>(), trace_value_slot) }
    }

    unsafe fn add_object(&mut self, slot: &mut JsObject) {
        // SAFETY: `JsObject` is a bare GC handle; contract forwarded.
        unsafe { self.add_raw_slot((slot as *mut JsObject).cast::<RawGc>()) }
    }

    unsafe fn add_value_smallvec(&mut self, slot: &mut SmallVec<[Value; 4]>) {
        // SAFETY: contract forwarded to the caller.
        unsafe {
            self.add_erased(
                (slot as *mut SmallVec<[Value; 4]>).cast::<()>(),
                trace_value_smallvec4,
            )
        }
    }
}
