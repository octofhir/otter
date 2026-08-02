//! Isolate-local table of host payloads GC bodies name by index.
//!
//! A heap body must own nothing outside the cage, but some callables
//! genuinely are host state — a dynamic native's `Arc`'d closure has no
//! page-image representation at all. The body therefore stores a `u32`
//! index into this table and the table owns the payload, the same split
//! [`crate::external_refs::ExternalRefTable`] makes for static entry
//! addresses. A dump carries the index; a restore re-creates the
//! payload (for the bootstrap graph, by re-installing the closure under
//! its name) and the index resolves again.
//!
//! Unlike the external-ref table, entries here are *owned*, not
//! interned addresses, and bodies die: a released slot goes on a free
//! list and is handed out again. Release happens from the sweep, via
//! [`crate::trace::ReleaseHostRefs`] — a finalizer cannot do it, because
//! finalizers run against the body alone with no heap in reach.
//!
//! # Invariants
//!
//! - Index `0` is [`NO_HOST_REF`] and never names a payload, so a
//!   zeroed body field reads as "none".
//! - Entries are isolate-local. The table imposes no `Send` bound on
//!   payloads; the isolate that made an entry is the only one that can
//!   reach it.
//! - A slot is released at most once per occupancy: the sweep runs the
//!   release hook exactly once per dead body (`set_swept` guards the
//!   old space, from-space reset guards the nursery).
//!
//! # See also
//!
//! - [`crate::external_refs`] — the address-interning sibling.
//! - [`crate::trace::ReleaseHostRefs`] — the sweep-time hook.

use std::any::Any;

/// The reserved "this body holds none" index.
pub const NO_HOST_REF: u32 = 0;

/// Owned host payloads, indexed by the bodies that name them.
#[derive(Default)]
pub struct HostRefTable {
    /// Slot 0 is permanently vacant so [`NO_HOST_REF`] never resolves.
    entries: Vec<Option<Box<dyn Any>>>,
    free: Vec<u32>,
}

impl HostRefTable {
    /// Store `payload` and return its index.
    pub fn insert(&mut self, payload: Box<dyn Any>) -> u32 {
        if self.entries.is_empty() {
            self.entries.push(None);
        }
        if let Some(index) = self.free.pop() {
            self.entries[index as usize] = Some(payload);
            return index;
        }
        let index = u32::try_from(self.entries.len()).expect("host ref table exceeds u32");
        self.entries.push(Some(payload));
        index
    }

    /// The payload behind `index`, or `None` for [`NO_HOST_REF`] and
    /// released slots.
    #[must_use]
    pub fn get(&self, index: u32) -> Option<&dyn Any> {
        if index == NO_HOST_REF {
            return None;
        }
        self.entries
            .get(index as usize)
            .and_then(|slot| slot.as_deref())
    }

    /// Drop the payload behind `index` and recycle the slot.
    pub fn release(&mut self, index: u32) {
        if index == NO_HOST_REF {
            return;
        }
        let Some(slot) = self.entries.get_mut(index as usize) else {
            return;
        };
        if slot.take().is_some() {
            self.free.push(index);
        }
    }

    /// Occupied entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|slot| slot.is_some()).count()
    }

    /// `true` when no entry is occupied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
