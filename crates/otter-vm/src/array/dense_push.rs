//! Dense append publication and allocation-free ordinary-array push.
//!
//! # Contents
//! - One slab append owner for reserved dense writes, including hole filling.
//! - A guarded spare-capacity completion used by generated push and tail stores.
//!
//! # Invariants
//! - The initialized slab prefix is published before the body's cached length.
//! - An append cannot move the slab or change its capacity or receiver state;
//!   only its live length and element kind need republishing.
//! - A leaf miss has no effects. A hit records both array and slab write barriers.
//! - Callers prove the source-realm indexed-prototype protector before the leaf.
//!
//! # See also
//! - [`super::elements`] owns representation widening and dirty slot ranges.
//! - [`crate::runtime_stubs::array_push_leaf`] owns the compiled result boundary.

use super::{ArrayBody, JsArray, record_array_write};
use crate::Value;
use otter_gc::GcHeap;

impl ArrayBody {
    /// Append to already reserved storage without allocating or collecting.
    #[inline]
    pub(super) fn push_dense(&mut self, value: Value) {
        let len = self.dense_len();
        assert!(
            len < self.dense_capacity(),
            "dense push without a reservation"
        );
        let slab = self.slab_body_mut().expect("dense push without a slab");
        slab.set(len, value);
        slab.set_len(len + 1);
        let kind = slab.kind();
        self.dense_len.set((len + 1) as u32);
        self.dense_kind.set(kind as u8);
    }
}

/// Complete a plain tail append, or decline before any mutation.
///
/// No sidecar means extensible, writable length, default prototype ownership,
/// and no indexed descriptors, accessors, sparse elements, or captured JSON.
/// The named expando cannot affect indexed writes after callable resolution.
pub(crate) fn try_push_spare(arr: JsArray, heap: &mut GcHeap, value: Value) -> Option<usize> {
    let length = heap.with_payload(arr, |body| {
        let len = body.length();
        if !body.exotic.is_null()
            || len != body.dense_len()
            || len >= body.dense_capacity()
            || len >= 1 << 20
        {
            return None;
        }
        body.push_dense(value);
        body.set_length(len + 1);
        Some(body.length())
    })?;
    record_array_write(heap, arr, &value);
    Some(length)
}

#[cfg(test)]
mod tests;
