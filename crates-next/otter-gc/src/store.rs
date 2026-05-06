//! Safe write-barrier child enumeration for contributor-facing
//! mutation APIs.
//!
//! VM and builtin authors should not call raw barrier entry points or
//! manufacture slot pointers. Instead, values that may contain GC
//! edges implement [`GcStore`], and mutation helpers call
//! [`crate::GcHeap::record_write`] after a store. The heap owns the
//! low-level card marking and insertion barrier details.
//!
//! # Contents
//!
//! - [`GcEdge`] — opaque outgoing GC edge reported by a stored value.
//! - [`GcStore`] — safe trait for enumerating edges in a stored value.
//!
//! # Invariants
//!
//! - [`GcEdge`] does not expose the raw compressed pointer to
//!   downstream crates.
//! - Implementing [`GcStore`] is safe Rust: implementations may only
//!   report already-owned [`crate::Gc`] handles as outgoing edges.
//! - The caller never supplies raw slot addresses; the heap computes
//!   the parent card from the owner object.
//!
//! # See also
//!
//! - [Task 94](../../../docs/new-engine/tasks/94-gc-contributor-api-surface.md)
//! - [GC architecture plan §6.1](../../../docs/new-engine/gc-architecture.md)

use crate::compressed::{Gc, RawGc};

/// Opaque outgoing edge reported by a value being stored in a
/// GC-managed parent object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GcEdge {
    raw: RawGc,
}

impl GcEdge {
    /// Build an edge from a typed GC handle.
    #[must_use]
    pub fn from_gc<T: ?Sized>(gc: Gc<T>) -> Option<Self> {
        if gc.is_null() {
            None
        } else {
            Some(Self { raw: gc.raw() })
        }
    }

    /// Build an edge from raw VM metadata.
    ///
    /// This is for audited VM adapter layers whose value model is
    /// already type-erased. Normal contributor code should report
    /// typed [`Gc`] handles through [`Self::from_gc`].
    #[doc(hidden)]
    #[must_use]
    pub fn from_raw(raw: RawGc) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }

    pub(crate) fn raw(self) -> RawGc {
        self.raw
    }
}

/// Safe trait for values that can be stored into a GC-managed parent.
///
/// Implementations report the outgoing GC edges owned by `self`. They
/// do not receive raw slot pointers and do not call barriers
/// themselves.
pub trait GcStore {
    /// Visit every outgoing edge owned by this stored value.
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(GcEdge));
}

impl<T: ?Sized> GcStore for Gc<T> {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(GcEdge)) {
        if let Some(edge) = GcEdge::from_gc(*self) {
            visitor(edge);
        }
    }
}

impl<T: GcStore> GcStore for Option<T> {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(GcEdge)) {
        if let Some(value) = self {
            value.visit_gc_edges(visitor);
        }
    }
}

impl<T: GcStore> GcStore for [T] {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(GcEdge)) {
        for value in self {
            value.visit_gc_edges(visitor);
        }
    }
}
