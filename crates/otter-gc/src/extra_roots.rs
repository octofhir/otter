//! Type-erased runtime root source registered with a heap.
//!
//! # Contents
//!
//! - [`ExtraRootSource`] — safe trait implemented by runtime owners of roots.
//! - [`ExtraRoots`] — raw-pointer trampoline stored by [`crate::heap::GcHeap`].
//!
//! # Invariants
//!
//! - The source passed to [`ExtraRoots::new`] must outlive its heap registration.
//!   Callers enforce this by installing/restoring the registration around the VM
//!   turn or explicit GC scope.
//! - The VM crate implements only the safe trait; the raw pointer dereference is
//!   kept inside this crate's audited unsafe boundary.
//!
//! # See also
//!
//! - [`crate::heap::GcHeap::install_extra_roots`]

use crate::compressed::RawGc;

/// Safe callback surface for owner-managed root slots not stored in the heap's
/// handle stack or global handle table.
pub trait ExtraRootSource {
    /// Visit every mutable raw root slot owned by this source.
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc));
}

/// Type-erased root source registration held by [`crate::heap::GcHeap`].
#[derive(Clone, Copy)]
pub struct ExtraRoots {
    data: *const (),
    thunk: unsafe fn(*const (), &mut dyn FnMut(*mut RawGc)),
}

impl ExtraRoots {
    /// Create a registration for `source`.
    #[must_use]
    pub fn new<S: ExtraRootSource>(source: &S) -> Self {
        unsafe fn thunk<S: ExtraRootSource>(data: *const (), visitor: &mut dyn FnMut(*mut RawGc)) {
            // SAFETY: `ExtraRoots::new` records the concrete `S` pointer, and
            // the heap registration contract requires the source to outlive the
            // installed `ExtraRoots` value.
            unsafe { (&*(data as *const S)).visit_extra_roots(visitor) };
        }

        Self {
            data: source as *const S as *const (),
            thunk: thunk::<S>,
        }
    }

    /// Visit the source's roots. Public so a composite
    /// [`ExtraRootSource`] (e.g. a native-call scope that adds its own
    /// argument roots on top of the interpreter's runtime roots) can
    /// re-dispatch into an inner registration without the VM crate
    /// needing raw-pointer dereference of its own.
    pub fn visit(self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: callers install `ExtraRoots` only for scopes where `data`
        // still points at the original `ExtraRootSource`.
        unsafe { (self.thunk)(self.data, visitor) };
    }
}
