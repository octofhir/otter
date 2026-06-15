//! GC-managed hidden-class layout nodes.
//!
//! Shape nodes are immutable after allocation and record only the transition
//! from their parent: parent shape, added property key, property count, and the
//! slot offset assigned to that key. Transition tables and flattened lookup
//! caches live outside this GC body so mutation never requires `Cell`/`RefCell`
//! inside traced payloads.
//!
//! # Contents
//! - [`ShapeBody`] — immutable hidden-class layout node.
//! - [`alloc_root_shape_body_with_roots`] — allocate the empty root shape.
//! - [`alloc_child_shape_body_with_roots`] — allocate one append transition.
//! - [`shape_offset_of_str`] / [`shape_keys_ordered`] — parent-chain readers.
//!
//! # Invariants
//! - `parent == Gc::null()` and `transition_key == Gc::null()` only for root.
//! - Non-root `own_offset` is the parent's `property_count`.
//! - `property_count` is the number of string-keyed own slots represented by
//!   the full parent chain.
//! - Shape bodies have no interior mutability; all transition/cache mutation
//!   belongs to interpreter-owned side tables.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-ownpropertykeys>
//! - Architecture plan §4.1 (hidden classes).

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::string::{JsStringHandle, eq_str};

use super::{ShapeId, next_shape_id};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ShapeBody`].
///
/// `0x12` is already used by `ArrayBody` in this branch, so shapes use a fresh
/// tag in the active VM payload range.
pub const SHAPE_BODY_TYPE_TAG: u8 = 0x22;

/// GC handle to a hidden-class layout node.
pub type ShapeHandle = otter_gc::Gc<ShapeBody>;

/// Immutable hidden-class layout node.
#[derive(Debug, Clone)]
pub struct ShapeBody {
    /// VM-local identity used by property inline-cache guards.
    id: ShapeId,
    /// Parent shape, or `Gc::null()` for root.
    parent: ShapeHandle,
    /// Key added by this transition, or `Gc::null()` for root.
    transition_key: JsStringHandle,
    /// Number of string-keyed slots represented by this shape.
    property_count: u32,
    /// Slot assigned to [`Self::transition_key`]. Zero for root.
    own_offset: u32,
}

impl ShapeBody {
    #[must_use]
    fn root(id: ShapeId) -> Self {
        Self {
            id,
            parent: ShapeHandle::null(),
            transition_key: JsStringHandle::null(),
            property_count: 0,
            own_offset: 0,
        }
    }

    #[must_use]
    fn child(parent: ShapeHandle, key: JsStringHandle, parent_property_count: u32) -> Self {
        Self {
            id: next_shape_id(),
            parent,
            transition_key: key,
            property_count: parent_property_count + 1,
            own_offset: parent_property_count,
        }
    }

    /// VM-local identity used by IC guards.
    #[must_use]
    pub(crate) const fn id(&self) -> ShapeId {
        self.id
    }

    /// Parent shape, or `Gc::null()` for root.
    #[must_use]
    pub(crate) const fn parent(&self) -> ShapeHandle {
        self.parent
    }

    /// Property key added by this transition, or `Gc::null()` for root.
    #[must_use]
    pub(crate) const fn transition_key(&self) -> JsStringHandle {
        self.transition_key
    }

    /// Number of string-keyed own slots represented by this shape.
    #[must_use]
    pub(crate) const fn property_count(&self) -> u32 {
        self.property_count
    }

    /// Slot offset assigned by this transition. Meaningful only for non-root.
    #[must_use]
    pub(crate) const fn own_offset(&self) -> u32 {
        self.own_offset
    }

    /// `true` for the root shape.
    #[must_use]
    pub(crate) const fn is_root(&self) -> bool {
        self.parent.is_null()
    }
}

impl otter_gc::SafeTraceable for ShapeBody {
    const TYPE_TAG: u8 = SHAPE_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.parent.is_null() {
            let p = &self.parent as *const ShapeHandle as *mut RawGc;
            visitor(p);
        }
        if !self.transition_key.is_null() {
            let p = &self.transition_key as *const JsStringHandle as *mut RawGc;
            visitor(p);
        }
    }
}

/// Allocate the root hidden-class node.
///
/// Shapes are allocated directly in non-moving old space: they are immortal
/// (rooted forever by the shape-transition tables) and the JIT bakes a
/// shape's handle offset into emitted monomorphic property guards, so the
/// offset must stay stable for the life of the isolate. Old-space pinning
/// guarantees that without a separate stability mechanism.
pub(crate) fn alloc_root_shape_body_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    heap.alloc_old_with_roots(ShapeBody::root(next_shape_id()), external_visit)
}

/// Allocate a child shape for adding `key` to `parent`.
///
/// Old-space pinned for the same reason as [`alloc_root_shape_body_with_roots`].
pub(crate) fn alloc_child_shape_body_with_roots(
    heap: &mut GcHeap,
    parent: ShapeHandle,
    key: JsStringHandle,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    let parent_property_count = heap.read_payload(parent, ShapeBody::property_count);
    heap.alloc_old_with_roots(
        ShapeBody::child(parent, key, parent_property_count),
        external_visit,
    )
}

/// Walk `shape`'s parent chain and return the slot for `key`.
#[must_use]
#[cfg(test)]
pub(crate) fn shape_offset_of_key(
    heap: &GcHeap,
    mut shape: ShapeHandle,
    key: JsStringHandle,
) -> Option<u32> {
    while !shape.is_null() {
        let (parent, transition_key, own_offset) = heap.read_payload(shape, |body| {
            (body.parent(), body.transition_key(), body.own_offset())
        });
        if transition_key == key {
            return Some(own_offset);
        }
        shape = parent;
    }
    None
}

/// Walk `shape`'s parent chain and return the slot for a UTF-8 property key.
///
/// This is the mutation-free bridge used by object helpers that do not have a
/// mutable [`super::shape_runtime::ShapeRuntime`] borrow. Hot paths should keep
/// using the runtime cache; this helper lets legacy object code read ShapeBody
/// state without interning or mutating side tables.
#[must_use]
pub(crate) fn shape_offset_of_str(heap: &GcHeap, mut shape: ShapeHandle, key: &str) -> Option<u32> {
    while !shape.is_null() {
        let (parent, transition_key, own_offset) = heap.read_payload(shape, |body| {
            (body.parent(), body.transition_key(), body.own_offset())
        });
        if !transition_key.is_null() && eq_str(heap, transition_key, key) {
            return Some(own_offset);
        }
        shape = parent;
    }
    None
}

/// Return the number of string-keyed slots represented by `shape`.
#[must_use]
pub(crate) fn shape_property_count(heap: &GcHeap, shape: ShapeHandle) -> u32 {
    heap.read_payload(shape, ShapeBody::property_count)
}

/// Return the transition key installed at `offset`, if the shape contains one.
#[must_use]
pub(crate) fn shape_key_at_offset(
    heap: &GcHeap,
    mut shape: ShapeHandle,
    offset: u32,
) -> Option<JsStringHandle> {
    while !shape.is_null() {
        let (parent, transition_key, own_offset, is_root) = heap.read_payload(shape, |body| {
            (
                body.parent(),
                body.transition_key(),
                body.own_offset(),
                body.is_root(),
            )
        });
        if !is_root && own_offset == offset {
            return Some(transition_key);
        }
        shape = parent;
    }
    None
}

/// Validate a cached slot offset against a UTF-8 property key.
#[must_use]
pub(crate) fn shape_key_matches_str(
    heap: &GcHeap,
    shape: ShapeHandle,
    offset: u32,
    key: &str,
) -> bool {
    let Some(actual) = shape_key_at_offset(heap, shape, offset) else {
        return false;
    };
    !actual.is_null() && eq_str(heap, actual, key)
}

/// Return transition keys in ordinary insertion order with their slot offsets.
#[must_use]
pub(crate) fn shape_keys_ordered(
    heap: &GcHeap,
    mut shape: ShapeHandle,
) -> Vec<(JsStringHandle, u32)> {
    let mut reversed = Vec::new();
    while !shape.is_null() {
        let (parent, transition_key, own_offset, is_root) = heap.read_payload(shape, |body| {
            (
                body.parent(),
                body.transition_key(),
                body.own_offset(),
                body.is_root(),
            )
        });
        if !is_root {
            reversed.push((transition_key, own_offset));
        }
        shape = parent;
    }
    reversed.reverse();
    reversed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::{JsStringId, alloc_flat_string_body_with_roots, to_utf16_vec};

    fn alloc_key(heap: &mut GcHeap, id: u32, key: &str) -> JsStringHandle {
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let units: Vec<u16> = key.encode_utf16().collect();
        alloc_flat_string_body_with_roots(heap, JsStringId::new(id), &units, &mut roots)
            .expect("key")
    }

    #[test]
    fn root_shape_has_no_parent_or_key() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let root = alloc_root_shape_body_with_roots(&mut heap, &mut roots).expect("root");

        heap.read_payload(root, |body| {
            assert!(body.is_root());
            assert!(body.parent().is_null());
            assert!(body.transition_key().is_null());
            assert_eq!(body.property_count(), 0);
        });
    }

    #[test]
    fn child_shapes_keep_gc_string_keys_in_order() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let root = alloc_root_shape_body_with_roots(&mut heap, &mut roots).expect("root");
        let x = alloc_key(&mut heap, 1, "x");
        let y = alloc_key(&mut heap, 2, "y");

        let sx = alloc_child_shape_body_with_roots(&mut heap, root, x, &mut roots).expect("sx");
        let sxy = alloc_child_shape_body_with_roots(&mut heap, sx, y, &mut roots).expect("sxy");

        assert_eq!(shape_offset_of_key(&heap, sxy, x), Some(0));
        assert_eq!(shape_offset_of_key(&heap, sxy, y), Some(1));

        let keys = shape_keys_ordered(&heap, sxy);
        assert_eq!(keys.len(), 2);
        assert_eq!(to_utf16_vec(&heap, keys[0].0), vec![b'x' as u16]);
        assert_eq!(to_utf16_vec(&heap, keys[1].0), vec![b'y' as u16]);
        assert_eq!(keys[0].1, 0);
        assert_eq!(keys[1].1, 1);
    }
}
