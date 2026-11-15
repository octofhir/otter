//! Rooted append of a plain dense array into an existing array.
//!
//! # Contents
//! - Structural source admission without numeric element boxing.
//! - A bulk copy into a freshly reserved empty slab of compatible storage.
//! - Semantic chunk copying for reused, mixed and nonempty destinations.
//!
//! # Invariants
//! - Refusal precedes effects; reservation roots both owners and may move them.
//! - Raw copying uses only the reservation's new private slab, never reused
//!   published storage that a concurrent marker could already be reading.
//! - Source and target payload pointers are reread after reservation.
//! - Tagged copies record slab edges before returning; dense caches and length
//!   publish the initialized prefix without another collection.
//!
//! # See also
//! - `array::reserve_dense_capacity` owns allocation and COW detachment.
//! - `ElementSlabBody::copy_hole_free_range` owns the fresh-slab copy contract.

use super::{
    DenseElementKind, JsArray, Value, elements, len, record_array_write, record_slab_contents,
    reserve_dense_capacity,
};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::RawGc;

#[cfg(test)]
mod tests;

/// Append `src`'s elements to `dst` when `src` is a plain dense array (no
/// sidecar, no holes, nothing past its dense prefix): one reservation and no
/// intermediate list. `Ok(false)` leaves both untouched.
pub(crate) fn append_plain_dense(
    mut dst: JsArray,
    mut src: JsArray,
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<bool, otter_gc::OutOfMemory> {
    let Some((count, kind)) = heap.read_payload(src, |body| {
        let count = body.dense_len();
        if !body.exotic.is_null() || body.length() != count {
            return None;
        }
        if count != 0 {
            let slab = elements::body_of(body.slab)?;
            // SAFETY: the borrowed array keeps its slab live; this check
            // neither allocates nor mutates the prefix.
            if unsafe { (*slab).has_hole_in(0, count) } {
                return None;
            }
        }
        Some((count, body.dense_kind()))
    }) else {
        return Ok(false);
    };
    let start = len(dst, heap);
    let target_len = start.saturating_add(count);
    // This is precisely the reservation owner's growth condition. Keep only
    // the scalar proof across collection, never an outgoing slab pointer.
    let fresh_empty = heap.read_payload(dst, |body| {
        body.length() == 0 && body.dense_len() == 0 && target_len > body.dense_capacity()
    });
    {
        let src_slot = std::ptr::addr_of_mut!(src);
        let mut reserve_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            visitor(src_slot.cast::<RawGc>());
        };
        reserve_dense_capacity(&mut dst, heap, target_len, kind, &mut reserve_roots)?;
    }
    const CHUNK: usize = 32;
    let mut copied = 0;
    if fresh_empty {
        // Reservation may move either owner and the source slab. The new
        // target is private; an already published reusable slab never enters
        // this path, even if its live length happens to be zero.
        let source_slab = heap.read_payload(src, |body| body.slab);
        let target_slab = heap.read_payload(dst, |body| body.slab);
        let source_kind = elements::kind_of(source_slab);
        let target_kind = elements::kind_of(target_slab);
        let compatible = (source_kind == DenseElementKind::Tagged
            && target_kind == DenseElementKind::Tagged)
            || (source_kind.is_numeric() && target_kind.is_numeric());
        if source_slab != target_slab && compatible {
            let source = elements::body_of(source_slab).expect("plain dense source slab");
            let target = elements::body_of(target_slab).expect("fresh reserved slab");
            // SAFETY: reservation created a distinct private empty slab. No
            // allocation intervenes, and the whole source prefix is hole-free.
            // A fresh young slab has no concurrent reader; a fresh old slab
            // during marking is born black, outside the marker's worklist.
            unsafe { (*target).copy_hole_free_range(&*source, 0, count) };
            record_slab_contents(heap, target_slab, count);
            heap.with_payload(dst, |body| {
                body.set_dense_len(count);
                body.refresh_element_cache();
            });
            copied = count;
        }
        // A post-reservation mismatch must finish through the ordinary copy;
        // returning false here would hide the reservation's existing effects.
    }
    while copied < count {
        let n = (count - copied).min(CHUNK);
        let mut chunk = [Value::undefined(); CHUNK];
        heap.read_payload(src, |body| {
            for (offset, slot) in chunk[..n].iter_mut().enumerate() {
                *slot = body.dense_value(copied + offset).expect("plain dense slot");
            }
        });
        heap.with_payload(dst, |body| {
            body.refresh_element_cache();
            while body.dense_len() < start {
                body.push_dense(Value::hole());
            }
            for value in &chunk[..n] {
                body.push_dense(*value);
            }
        });
        for value in &chunk[..n] {
            record_array_write(heap, dst, value);
        }
        copied += n;
    }
    heap.with_payload(dst, |body| {
        body.set_length(target_len);
        body.mark_dirty();
    });
    Ok(true)
}
