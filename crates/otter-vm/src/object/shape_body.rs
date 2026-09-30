//! GC-managed hidden-class layout nodes.
//!
//! Shape nodes are immutable after allocation and record the transition from
//! their parent — parent shape, added property key, property count, and the
//! slot offset assigned to that key — together with what every object of the
//! lineage shares: its `[[Prototype]]` (V8 `Map::prototype`, JSC
//! `Structure::m_prototype`, SpiderMonkey `BaseShape::proto`) and its
//! dictionary shape. Transition tables and flattened lookup caches live
//! outside this GC body so mutation never requires `Cell`/`RefCell` inside
//! traced payloads.
//!
//! # Contents
//! - [`ShapeBody`] — immutable hidden-class layout node.
//! - [`ShapePrototype`] — the lineage's `[[Prototype]]` as a shape stores it.
//! - [`alloc_root_shape_body_with_roots`] — allocate a prototype's empty root
//!   shape together with its dictionary shape.
//! - [`alloc_child_shape_body_with_roots`] — allocate one append transition.
//! - [`null_root`] / [`set_null_root`] — the `null`-prototype root, kept in
//!   the heap's embedder root slot so heap-only code reaches it.
//! - [`shape_offset_of_atom`] / [`shape_offset_of_str`] / [`shape_keys_ordered`]
//!   — parent-chain readers.
//!
//! # Invariants
//! - `parent == Gc::null()` and `transition_key == Gc::null()` only for a
//!   root or a dictionary shape, the only nodes whose `transition_atom` is
//!   [`AtomId::NONE`].
//! - Every node of a lineage has its root's prototype and dictionary shape: a
//!   shape fixes its objects' `[[Prototype]]`, so a prototype change is a
//!   change of shape. A dictionary shape describes no keys (a dictionary
//!   object keeps them in its sidecar) and is its own dictionary shape.
//! - A node's `transition_atom` is the isolate-global atom of its
//!   `transition_key`; the two never disagree.
//! - Non-root `own_offset` is the parent's `property_count`.
//! - `property_count` is the number of string-keyed own slots represented by
//!   the full parent chain.
//! - Shape bodies have no interior mutability; all transition/cache mutation
//!   belongs to interpreter-owned side tables.
//! - The C layout exposes only the immutable identity word to generated
//!   shared-cache probes; shapes live in non-moving old space, so a handle
//!   never relocates. Shapes are collectable (see
//!   [`super::shape_runtime`]); an id is never reused, a handle may be once
//!   its shape is collected.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-ownpropertykeys>
//! - Architecture plan §4.1 (hidden classes).

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::property_atom::AtomId;
use crate::string::{JsStringHandle, eq_str};

use super::descriptor::PropertyFlags;
use super::{ShapeId, next_shape_id};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ShapeBody`].
///
/// `0x12` is already used by `ArrayBody` in this branch, so shapes use a fresh
/// tag in the active VM payload range.
pub const SHAPE_BODY_TYPE_TAG: u8 = 0x22;

/// GC handle to a hidden-class layout node.
pub type ShapeHandle = otter_gc::Gc<ShapeBody>;

/// [`ShapeBody::kind`] of a dictionary shape.
pub(crate) const SHAPE_KIND_DICTIONARY: u8 = 1 << 0;

/// A lineage's `[[Prototype]]`: `null`, an ordinary object (the compressed
/// handle generated code reads), or a non-ordinary value — a function, an
/// array, a Proxy — that ordinary chain walks cannot follow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ShapePrototype {
    /// `null`.
    Null,
    /// An ordinary object.
    Object(super::JsObject),
    /// A non-ordinary object value.
    Value(crate::Value),
}

/// Immutable hidden-class layout node.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct ShapeBody {
    /// VM-local identity used by property inline-cache guards.
    id: ShapeId,
    /// Parent shape, or `Gc::null()` for root.
    parent: ShapeHandle,
    /// Key added by this transition, or `Gc::null()` for root.
    transition_key: JsStringHandle,
    /// Isolate-global atom of [`Self::transition_key`], or [`AtomId::NONE`]
    /// for the root. This is the identity every chain walk compares: one
    /// `u32` per link instead of a string content compare per link.
    transition_atom: AtomId,
    /// Number of string-keyed slots represented by this shape.
    property_count: u32,
    /// Slot assigned to [`Self::transition_key`]. Zero for root.
    own_offset: u32,
    /// Attribute bits (`writable`/`enumerable`/`configurable`) of the slot
    /// added by this transition. Default-data for ordinary appends; carries
    /// non-default attributes when the transition was created by a
    /// descriptor/define path. The source of truth for a shaped object's
    /// per-slot attributes unless the object has overridden them in place.
    /// Meaningless for the root.
    own_flags: PropertyFlags,
    /// `true` when the slot added by this transition is an accessor rather
    /// than a data property. Meaningless for the root.
    own_is_accessor: bool,
    /// [`SHAPE_KIND_DICTIONARY`] for a dictionary shape; `0` otherwise.
    kind: u8,
    /// The lineage's ordinary-object `[[Prototype]]`; null for a `null` or a
    /// non-ordinary prototype.
    prototype: super::JsObject,
    /// The lineage's non-ordinary `[[Prototype]]`, or `undefined`.
    prototype_value: crate::Value,
    /// The lineage's dictionary shape; in the dictionary shape itself, which
    /// is its own, the lineage's root.
    dictionary: ShapeHandle,
}

/// Identity word read by generated probes of the shared property lookup table.
pub(crate) const SHAPE_BODY_ID_OFFSET: usize = std::mem::offset_of!(ShapeBody, id);

/// `u32` property count, read by generated code that needs a shaped object's
/// slot count without a compile-time shape.
pub(crate) const SHAPE_BODY_PROPERTY_COUNT_OFFSET: usize =
    std::mem::offset_of!(ShapeBody, property_count);

/// Kind byte generated code tests for a dictionary shape.
pub(crate) const SHAPE_BODY_KIND_OFFSET: usize = std::mem::offset_of!(ShapeBody, kind);

/// Compressed ordinary-object prototype generated chain walks read.
pub(crate) const SHAPE_BODY_PROTOTYPE_OFFSET: usize = std::mem::offset_of!(ShapeBody, prototype);

impl ShapeBody {
    /// An empty node of `prototype`'s lineage: a root when `dictionary` names
    /// the lineage's dictionary shape, or that dictionary shape itself.
    #[must_use]
    fn empty(id: ShapeId, prototype: ShapePrototype, kind: u8, dictionary: ShapeHandle) -> Self {
        let (prototype, prototype_value) = match prototype {
            ShapePrototype::Null => (super::JsObject::null(), crate::Value::undefined()),
            ShapePrototype::Object(object) => (object, crate::Value::undefined()),
            ShapePrototype::Value(value) => (super::JsObject::null(), value),
        };
        Self {
            id,
            parent: ShapeHandle::null(),
            transition_key: JsStringHandle::null(),
            transition_atom: AtomId::NONE,
            property_count: 0,
            own_offset: 0,
            own_flags: PropertyFlags::data_default(),
            own_is_accessor: false,
            kind,
            prototype,
            prototype_value,
            dictionary,
        }
    }

    #[must_use]
    fn child(
        parent_handle: ShapeHandle,
        parent: &ShapeBody,
        key: JsStringHandle,
        atom: AtomId,
        own_flags: PropertyFlags,
        own_is_accessor: bool,
    ) -> Self {
        debug_assert!(
            parent_handle.offset().is_multiple_of(8),
            "misaligned shape parent at child creation: parent={:?}",
            parent_handle
        );
        debug_assert!(
            key.is_null() || key.offset().is_multiple_of(8),
            "misaligned shape key at child creation: key={:?}",
            key
        );
        debug_assert_ne!(
            atom,
            AtomId::NONE,
            "a non-root shape node names the key it adds",
        );
        debug_assert_eq!(parent.kind, 0, "a dictionary shape has no children");
        Self {
            id: next_shape_id(),
            parent: parent_handle,
            transition_key: key,
            transition_atom: atom,
            property_count: parent.property_count + 1,
            own_offset: parent.property_count,
            own_flags,
            own_is_accessor,
            kind: 0,
            prototype: parent.prototype,
            prototype_value: parent.prototype_value,
            dictionary: parent.dictionary,
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

    /// Isolate-global atom added by this transition, [`AtomId::NONE`] for root.
    #[must_use]
    pub(crate) const fn transition_atom(&self) -> AtomId {
        self.transition_atom
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

    /// Attribute bits of the slot added by this transition.
    #[must_use]
    pub(crate) const fn own_flags(&self) -> PropertyFlags {
        self.own_flags
    }

    /// `true` when the slot added by this transition is an accessor.
    #[must_use]
    pub(crate) const fn own_is_accessor(&self) -> bool {
        self.own_is_accessor
    }

    /// `true` for a root or a dictionary shape: a node that adds no key.
    #[must_use]
    pub(crate) const fn is_root(&self) -> bool {
        self.parent.is_null()
    }

    /// `true` for a dictionary shape.
    #[must_use]
    pub(crate) const fn is_dictionary(&self) -> bool {
        self.kind & SHAPE_KIND_DICTIONARY != 0
    }

    /// The `[[Prototype]]` of every object with this shape.
    #[must_use]
    pub(crate) fn prototype(&self) -> ShapePrototype {
        if !self.prototype.is_null() {
            ShapePrototype::Object(self.prototype)
        } else if self.prototype_value.is_undefined() {
            ShapePrototype::Null
        } else {
            ShapePrototype::Value(self.prototype_value)
        }
    }

    /// Report the function id a non-ordinary prototype value carries.
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.prototype_value, visitor);
    }

    /// The lineage's dictionary shape, given this node's own handle.
    #[must_use]
    pub(crate) fn dictionary(&self, own: ShapeHandle) -> ShapeHandle {
        if self.is_dictionary() {
            own
        } else {
            self.dictionary
        }
    }
}

impl otter_gc::SafeTraceable for ShapeBody {
    const TYPE_TAG: u8 = SHAPE_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        if !self.parent.is_null() {
            let p = &mut self.parent as *mut ShapeHandle as *mut RawGc;
            visitor(p);
        }
        if !self.transition_key.is_null() {
            let p = &mut self.transition_key as *mut JsStringHandle as *mut RawGc;
            visitor(p);
        }
        if !self.prototype.is_null() {
            let p = &mut self.prototype as *mut super::JsObject as *mut RawGc;
            visitor(p);
        }
        self.prototype_value.pelt_trace(visitor);
        if !self.dictionary.is_null() {
            let p = &mut self.dictionary as *mut ShapeHandle as *mut RawGc;
            visitor(p);
        }
    }
}

/// Allocate `prototype`'s empty root shape and its dictionary shape, and
/// return the root.
///
/// Shapes are allocated directly in non-moving old space: the JIT bakes a
/// shape's handle offset into emitted guards and publications, so the offset
/// must stay stable while anything names the shape (compiled code keeps the
/// shapes it bakes alive). Old-space placement guarantees that without a
/// separate stability mechanism. A young prototype is remembered from both.
pub(crate) fn alloc_root_shape_body_with_roots(
    heap: &mut GcHeap,
    prototype: ShapePrototype,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    // A pending body is traced across its own allocation.
    let root = heap.alloc_old_with_roots(
        ShapeBody::empty(next_shape_id(), prototype, 0, ShapeHandle::null()),
        external_visit,
    )?;
    // The root holds the collector-rewritten prototype; shapes never move.
    let prototype = heap.read_payload(root, ShapeBody::prototype);
    remember_prototype(heap, root, prototype);
    let dictionary = heap.alloc_old_with_roots(
        ShapeBody::empty(next_shape_id(), prototype, SHAPE_KIND_DICTIONARY, root),
        external_visit,
    )?;
    let prototype = heap.read_payload(dictionary, ShapeBody::prototype);
    remember_prototype(heap, dictionary, prototype);
    // Completes the pair before either shape is published.
    heap.with_payload(root, |body| body.dictionary = dictionary);
    heap.record_write(root, &dictionary);
    Ok(root)
}

/// Heap embedder root slot holding the `null`-prototype root shape.
const NULL_ROOT_SLOT: usize = 0;

/// The root shape of `null`-prototype objects.
#[must_use]
pub(crate) fn null_root(heap: &GcHeap) -> ShapeHandle {
    // SAFETY: only `set_null_root` fills the slot, and with a shape.
    unsafe { heap.embedder_root(NULL_ROOT_SLOT).cast() }
}

/// Make `root` the isolate's `null`-prototype root; the heap keeps it alive.
pub(crate) fn set_null_root(heap: &GcHeap, root: ShapeHandle) {
    heap.set_embedder_root(NULL_ROOT_SLOT, root.raw());
}

/// The root of a live, non-null `shape`'s lineage.
#[must_use]
pub(crate) fn lineage_root_of(heap: &GcHeap, shape: ShapeHandle) -> ShapeHandle {
    let body = body_of(shape);
    if body.is_dictionary() {
        return body.dictionary;
    }
    let mut current = shape;
    loop {
        let parent = heap.read_payload(current, ShapeBody::parent);
        if parent.is_null() {
            return current;
        }
        current = parent;
    }
}

/// Record the old-to-young edge from a new old-space shape to its prototype.
fn remember_prototype(heap: &mut GcHeap, shape: ShapeHandle, prototype: ShapePrototype) {
    use crate::pelt::PeltField as _;
    let mut value = match prototype {
        ShapePrototype::Null => return,
        ShapePrototype::Object(object) => crate::Value::object(object),
        ShapePrototype::Value(value) => value,
    };
    let mut record = |slot: *mut RawGc| {
        // SAFETY: the slot points into the local copy of the value; it is read
        // to record the edge only.
        let raw = unsafe { *slot };
        heap.record_write_edge(shape, raw);
    };
    value.pelt_trace(&mut record);
}

/// Allocate a child shape for adding `key` to `parent`.
///
/// Old-space pinned for the same reason as [`alloc_root_shape_body_with_roots`];
/// it shares its parent's prototype, whose edge the parent already records.
pub(crate) fn alloc_child_shape_body_with_roots(
    heap: &mut GcHeap,
    parent: ShapeHandle,
    key: JsStringHandle,
    atom: AtomId,
    own_flags: PropertyFlags,
    own_is_accessor: bool,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    let body = heap.read_payload(parent, |parent_body| {
        ShapeBody::child(parent, parent_body, key, atom, own_flags, own_is_accessor)
    });
    let prototype = body.prototype();
    let child = heap.alloc_old_with_roots(body, external_visit)?;
    remember_prototype(heap, child, prototype);
    Ok(child)
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
        debug_assert_eq!(
            unsafe { (*shape.as_header_ptr()).type_tag() },
            SHAPE_BODY_TYPE_TAG,
            "shape handle does not point at ShapeBody: shape={:?} swept={}",
            shape,
            unsafe { (*shape.as_header_ptr()).is_swept() }
        );
        debug_assert_eq!(
            unsafe { (*shape.as_header_ptr()).size_bytes() },
            (std::mem::size_of::<otter_gc::GcHeader>() + std::mem::size_of::<ShapeBody>()) as u32,
            "shape handle points at wrong-sized cell: shape={:?} swept={}",
            shape,
            unsafe { (*shape.as_header_ptr()).is_swept() }
        );
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
/// Walk `shape`'s parent chain and return the slot for an interned atom.
///
/// This is the lookup the whole named-property path runs: one `u32` compare
/// per link, no heap string touched, no per-link `eq_str`. The root's
/// [`AtomId::NONE`] matches no interned name, so the walk needs no root test.
#[must_use]
pub(crate) fn shape_offset_of_atom(
    heap: &GcHeap,
    mut shape: ShapeHandle,
    atom: AtomId,
) -> Option<u32> {
    debug_assert_ne!(atom, AtomId::NONE, "the root atom names no property");
    while !shape.is_null() {
        let (parent, transition_atom, own_offset) = heap.read_payload(shape, |body| {
            (body.parent(), body.transition_atom(), body.own_offset())
        });
        if transition_atom == atom {
            return Some(own_offset);
        }
        shape = parent;
    }
    None
}

pub(crate) fn shape_offset_of_str(heap: &GcHeap, mut shape: ShapeHandle, key: &str) -> Option<u32> {
    while !shape.is_null() {
        let (parent, transition_key, own_offset) = heap.read_payload(shape, |body| {
            (body.parent(), body.transition_key(), body.own_offset())
        });
        debug_assert!(
            transition_key.is_null() || transition_key.offset().is_multiple_of(8),
            "misaligned shape transition key: shape={:?} swept={} parent={:?} key={:?}",
            shape,
            unsafe { (*shape.as_header_ptr()).is_swept() },
            parent,
            transition_key
        );
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

/// The id of a live, non-null `shape`, read without the heap.
#[inline]
#[must_use]
pub(crate) fn id_of(shape: ShapeHandle) -> ShapeId {
    debug_assert!(!shape.is_null());
    // SAFETY: a non-null shape handle decompresses to a live ShapeBody cell;
    // its payload follows the header.
    unsafe {
        (*(shape
            .as_header_ptr()
            .cast::<u8>()
            .add(otter_gc::header::HEADER_SIZE)
            .cast::<ShapeBody>()))
        .id
    }
}

/// The live, non-null `shape`'s body, read without the heap.
fn body_of<'a>(shape: ShapeHandle) -> &'a ShapeBody {
    debug_assert!(!shape.is_null());
    // SAFETY: a non-null shape handle decompresses to a live ShapeBody cell
    // in non-moving old space; its immutable payload follows the header.
    unsafe {
        &*(shape
            .as_header_ptr()
            .cast::<u8>()
            .add(otter_gc::header::HEADER_SIZE)
            .cast::<ShapeBody>())
    }
}

/// The `[[Prototype]]` a live, non-null `shape` fixes, read without the heap.
#[must_use]
pub(crate) fn prototype_of(shape: ShapeHandle) -> ShapePrototype {
    body_of(shape).prototype()
}

/// `true` when a live, non-null `shape` is a dictionary shape.
#[must_use]
pub(crate) fn is_dictionary_of(shape: ShapeHandle) -> bool {
    body_of(shape).is_dictionary()
}

/// The dictionary shape of a live, non-null `shape`'s lineage.
#[must_use]
pub(crate) fn dictionary_of(shape: ShapeHandle) -> ShapeHandle {
    body_of(shape).dictionary(shape)
}

/// The property count of a live, non-null `shape`, read without the heap.
///
/// An object's slot count is its shape's, and the collector's trace of the
/// object reads it here. Shapes live in non-moving old space and are
/// immutable, so the read is valid at any point where the object's shape
/// handle is.
#[inline]
#[must_use]
pub(crate) fn property_count_of(shape: ShapeHandle) -> u32 {
    debug_assert!(!shape.is_null());
    // SAFETY: a non-null shape handle decompresses to a live ShapeBody cell;
    // its payload follows the header.
    unsafe {
        (*(shape
            .as_header_ptr()
            .cast::<u8>()
            .add(otter_gc::header::HEADER_SIZE)
            .cast::<ShapeBody>()))
        .property_count
    }
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

/// Walk `shape`'s parent chain and return the attributes of the slot at
/// `offset`: its `(flags, is_accessor)` pair. `None` when no transition in
/// the chain owns that offset (e.g. the root, or an out-of-range offset).
///
/// Mirror of [`shape_key_at_offset`] for the per-slot attribute payload. Reads
/// are O(chain depth); hot paths should prefer a flattened cache once the shape
/// becomes the authoritative attribute source.
#[must_use]
pub(crate) fn shape_slot_attrs(
    heap: &GcHeap,
    mut shape: ShapeHandle,
    offset: u32,
) -> Option<(PropertyFlags, bool)> {
    while !shape.is_null() {
        let (parent, own_offset, own_flags, own_is_accessor, is_root) =
            heap.read_payload(shape, |body| {
                (
                    body.parent(),
                    body.own_offset(),
                    body.own_flags(),
                    body.own_is_accessor(),
                    body.is_root(),
                )
            });
        if !is_root && own_offset == offset {
            return Some((own_flags, own_is_accessor));
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

/// Return transition atoms with their slot offsets, root-first.
#[must_use]
pub(crate) fn shape_atoms_ordered(heap: &GcHeap, mut shape: ShapeHandle) -> Vec<(AtomId, u32)> {
    let mut atoms = Vec::new();
    while !shape.is_null() {
        let (parent, transition_atom, own_offset) = heap.read_payload(shape, |body| {
            (body.parent(), body.transition_atom(), body.own_offset())
        });
        if transition_atom != AtomId::NONE {
            atoms.push((transition_atom, own_offset));
        }
        shape = parent;
    }
    atoms
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
        let root = alloc_root_shape_body_with_roots(&mut heap, ShapePrototype::Null, &mut roots)
            .expect("root");

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
        let mut root = ShapeHandle::null();
        let mut x = JsStringHandle::null();
        let mut y = JsStringHandle::null();
        let mut sx = ShapeHandle::null();
        let mut sxy = ShapeHandle::null();
        let mut scope = otter_gc::RootScope::new(&mut heap);
        // SAFETY: every handle slot precedes the scope and remains stationary
        // through all allocations and the final parent-chain walk.
        unsafe {
            scope.add_raw_slot((&mut root as *mut ShapeHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut x as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut y as *mut JsStringHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut sx as *mut ShapeHandle).cast::<RawGc>());
            scope.add_raw_slot((&mut sxy as *mut ShapeHandle).cast::<RawGc>());
        }
        root = alloc_root_shape_body_with_roots(&mut heap, ShapePrototype::Null, &mut roots)
            .expect("root");
        x = alloc_key(&mut heap, 1, "x");
        y = alloc_key(&mut heap, 2, "y");

        let flags = PropertyFlags::data_default();
        let atom_x = AtomId::from_global(1);
        let atom_y = AtomId::from_global(2);
        sx =
            alloc_child_shape_body_with_roots(&mut heap, root, x, atom_x, flags, false, &mut roots)
                .expect("sx");
        sxy = alloc_child_shape_body_with_roots(&mut heap, sx, y, atom_y, flags, false, &mut roots)
            .expect("sxy");

        assert_eq!(shape_offset_of_key(&heap, sxy, x), Some(0));
        assert_eq!(shape_offset_of_key(&heap, sxy, y), Some(1));
        assert_eq!(shape_offset_of_atom(&heap, sxy, atom_x), Some(0));
        assert_eq!(shape_offset_of_atom(&heap, sxy, atom_y), Some(1));
        assert_eq!(
            shape_offset_of_atom(&heap, sxy, AtomId::from_global(3)),
            None
        );

        let keys = shape_keys_ordered(&heap, sxy);
        assert_eq!(keys.len(), 2);
        assert_eq!(to_utf16_vec(&heap, keys[0].0), vec![b'x' as u16]);
        assert_eq!(to_utf16_vec(&heap, keys[1].0), vec![b'y' as u16]);
        assert_eq!(keys[0].1, 0);
        assert_eq!(keys[1].1, 1);
    }
}
