//! Ordinary-object field locations and the one native storage geometry.
//!
//! # Contents
//! - [`FieldLocation`] identifies a shape-owned bank and word index.
//! - [`FieldLayout`] describes persistent inline and overflow storage.
//!
//! # Invariants
//! - A shape fixes its inline capacity and every field's storage bank.
//!   Overflow never moves the persistent inline prefix.
//! - Locations carry no address or GC handle and survive moving collection.
//! - Slot-to-byte conversion belongs here, including dynamically indexed
//!   generated probes. Shape identity proves the immutable capacity and bank.
//!
//! # See also
//! - `super::ObjectBody` owns storage publication and tracing.
//! - `super::slot_slab` owns the out-of-line word allocation.
//! - `crate::jit::JitCompileSnapshot` carries this geometry to both backends.

use super::{ObjectBody, slot_slab};

/// Shape-owned storage bank and index of one ordinary property word.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldLocation(u32);

impl FieldLocation {
    const INLINE: u32 = 1 << 31;
    /// Size in bytes of one tagged property word in either storage bank.
    pub const WORD_BYTES: u32 = std::mem::size_of::<crate::Value>() as u32;
    /// Left shift that converts a word index into a byte offset.
    pub const INDEX_SHIFT: u32 = Self::WORD_BYTES.trailing_zeros();

    /// Select an index in the persistent inline prefix.
    ///
    /// The owning shape must also prove that `index` is below its own capacity.
    ///
    /// # Panics
    /// Panics when `index` reaches the engine's maximum inline capacity.
    #[must_use]
    pub const fn inline(index: u32) -> Self {
        assert!(
            index < super::MAX_INLINE_CAPACITY as u32,
            "inline field exceeds capacity limit"
        );
        Self(Self::INLINE | index)
    }

    /// Select a suffix index relative to the first overflow-slab word.
    ///
    /// # Panics
    /// Panics when the corresponding byte offset cannot fit in a `u32`.
    #[must_use]
    pub const fn overflow(index: u32) -> Self {
        assert!(
            index <= u32::MAX / Self::WORD_BYTES,
            "field byte offset exceeds u32"
        );
        Self(index)
    }

    /// Resolve a logical property slot using its shape's immutable capacity.
    ///
    /// Slots below `inline_capacity` select the inline prefix; all remaining
    /// slots select the suffix after subtracting that capacity.
    ///
    /// # Panics
    /// Panics when the capacity exceeds the engine's maximum or the suffix
    /// index cannot be represented as a byte offset.
    #[must_use]
    pub const fn for_slot(slot: u32, inline_capacity: usize) -> Self {
        assert!(inline_capacity <= super::MAX_INLINE_CAPACITY);
        if slot < inline_capacity as u32 {
            Self::inline(slot)
        } else {
            Self::overflow(slot - inline_capacity as u32)
        }
    }

    /// Whether this location selects the persistent inline prefix.
    #[must_use]
    pub const fn is_inline(self) -> bool {
        self.0 & Self::INLINE != 0
    }

    /// Zero-based word index relative to the selected storage bank.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0 & !Self::INLINE
    }

    /// Recover a logical property slot using the owning shape's capacity.
    ///
    /// `inline_capacity` must be the same valid capacity used to resolve this
    /// location. Inline indices are unchanged; suffix indices add the prefix.
    #[must_use]
    pub const fn logical_slot(self, inline_capacity: usize) -> u32 {
        if self.is_inline() {
            self.index()
        } else {
            self.index() + inline_capacity as u32
        }
    }

    /// Convert a representable property-word count into its allocation bytes.
    ///
    /// The caller must ensure the multiplication fits in `usize`.
    #[must_use]
    pub const fn words_bytes(count: usize) -> usize {
        count * Self::WORD_BYTES as usize
    }

    /// Byte offset relative to the first word in the selected storage bank.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        self.index() * Self::WORD_BYTES
    }

    /// Stable scalar identity including the storage bank, for compiler caches.
    #[must_use]
    pub const fn cache_key(self) -> u32 {
        self.0
    }

    /// Rebuild a location from its [`Self::cache_key`] word.
    #[must_use]
    pub const fn from_cache_key(key: u32) -> Self {
        Self(key)
    }

    /// Locate a word after the caller selects this location's storage bank.
    ///
    /// # Safety
    /// `base` names this bank, the index is in bounds, and neither address is
    /// retained across collection or overflow replacement.
    #[inline]
    pub(crate) unsafe fn word_ptr(self, base: *mut crate::Value) -> *mut crate::Value {
        unsafe { base.cast::<u8>().add(self.byte_offset() as usize).cast() }
    }
}

impl serde::Serialize for FieldLocation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut field = serializer.serialize_struct("FieldLocation", 3)?;
        field.serialize_field(
            "bank",
            if self.is_inline() {
                "inline"
            } else {
                "overflow"
            },
        )?;
        field.serialize_field("index", &self.index())?;
        field.serialize_field("byteOffset", &self.byte_offset())?;
        field.end()
    }
}

/// Header-inclusive offsets used to select ordinary-object field storage.
///
/// This is scalar immutable data, not a pointer to an object or shape. All
/// offsets are relative to a decompressed GC header except `fixed_cell_bytes`,
/// which is a size. Overflow slabs hold only the suffix beyond shape capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldLayout {
    /// First inline word of an object cell.
    pub inline_values_byte: u32,
    /// Compressed overflow slab handle: zero means no suffix allocation.
    pub slab_handle_byte: u32,
    /// Slab-header-relative u32 capacity word.
    pub slab_capacity_byte: u32,
    /// First word of a slab cell.
    pub slab_words_byte: u32,
    /// Object cell size without its inline word tail.
    pub fixed_cell_bytes: u32,
}

impl FieldLayout {
    /// Derive the actual VM layout rather than a second backend definition.
    #[must_use]
    pub const fn current() -> Self {
        let header = otter_gc::header::HEADER_SIZE as u32;
        Self {
            inline_values_byte: header + std::mem::size_of::<ObjectBody>() as u32,
            slab_handle_byte: header + std::mem::offset_of!(ObjectBody, slab) as u32,
            slab_capacity_byte: header + slot_slab::SLOT_SLAB_CAPACITY_OFFSET as u32,
            slab_words_byte: header + slot_slab::SLOT_SLAB_WORDS_OFFSET as u32,
            fixed_cell_bytes: header + std::mem::size_of::<ObjectBody>() as u32,
        }
    }

    /// Header-inclusive offset for a known inline initialization location.
    #[must_use]
    pub const fn inline_byte(self, location: FieldLocation) -> u32 {
        assert!(
            location.is_inline(),
            "inline address requires an inline field"
        );
        match self.inline_values_byte.checked_add(location.byte_offset()) {
            Some(byte) => byte,
            None => panic!("inline field byte offset exceeds u32"),
        }
    }

    /// Allocation bytes of an object with an inline tail of this capacity.
    #[must_use]
    pub const fn cell_bytes(self, capacity: usize) -> usize {
        self.fixed_cell_bytes as usize + FieldLocation::words_bytes(capacity)
    }

    /// Verify a resident object's selected storage before a mutator access.
    /// This deliberately receives sizes, never pointers from snapshot restore
    /// or from a collector that is still rewriting the fixed handles.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_verify(
        self,
        inline_capacity: usize,
        object_cell_bytes: usize,
        live_slots: usize,
        slab: Option<(usize, usize)>,
    ) {
        assert!(
            inline_capacity <= super::MAX_INLINE_CAPACITY,
            "object inline capacity exceeds maximum"
        );
        assert_eq!(
            object_cell_bytes,
            self.cell_bytes(inline_capacity),
            "shape capacity disagrees with cell footprint"
        );
        assert_eq!(self.inline_values_byte % FieldLocation::WORD_BYTES, 0);
        assert_eq!(self.slab_words_byte % FieldLocation::WORD_BYTES, 0);
        let capacity = match slab {
            Some((capacity, cell_bytes)) => {
                assert_eq!(
                    cell_bytes,
                    self.slab_words_byte as usize + FieldLocation::words_bytes(capacity),
                    "slab capacity disagrees with cell footprint"
                );
                inline_capacity + capacity
            }
            None => inline_capacity,
        };
        assert!(
            live_slots <= capacity,
            "shape/dictionary slot count exceeds selected storage"
        );
    }
}

const _: () = {
    assert!(FieldLocation::WORD_BYTES == 8);
    assert!(FieldLayout::current().inline_values_byte == 24);
    assert!(FieldLayout::current().slab_words_byte == 16);
    assert!(std::mem::size_of::<FieldLocation>() == 4);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_geometry_is_storage_independent_and_send_sync() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<FieldLocation>();
        send_sync::<FieldLayout>();
        let layout = FieldLayout::current();
        for capacity in [0, 4, 64] {
            assert_eq!(layout.cell_bytes(capacity), 24 + capacity * 8);
            for slot in 0..capacity {
                let location = FieldLocation::inline(slot as u32);
                assert_eq!(location.index(), slot as u32);
                assert_eq!(layout.inline_byte(location) as usize, 24 + slot * 8);
                assert!(layout.inline_byte(location) as usize + 8 <= layout.cell_bytes(capacity));
            }
        }
    }

    #[test]
    fn field_word_address_uses_the_same_relative_offset_in_either_buffer() {
        let mut inline = [crate::Value::undefined(); 4];
        let mut slab = [crate::Value::undefined(); 8];
        let location = FieldLocation::inline(3);
        assert_ne!(location, FieldLocation::overflow(3));
        assert_ne!(location.cache_key(), FieldLocation::overflow(3).cache_key());
        assert_eq!(
            serde_json::to_value(FieldLocation::inline(3)).unwrap(),
            serde_json::json!({"bank":"inline", "index":3, "byteOffset":24})
        );
        assert_eq!(
            serde_json::to_value(FieldLocation::overflow(3)).unwrap(),
            serde_json::json!({"bank":"overflow", "index":3, "byteOffset":24})
        );
        // SAFETY: both buffers own the fourth word.
        unsafe {
            *location.word_ptr(inline.as_mut_ptr()) = crate::Value::number_i32(31);
            *location.word_ptr(slab.as_mut_ptr()) = inline[3];
        }
        assert_eq!(slab[3], crate::Value::number_i32(31));
        assert_eq!(inline[0], crate::Value::undefined());
    }

    #[test]
    fn shape_capacity_partitions_roots_and_keeps_inline_prefix_on_overflow() {
        let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let default_root = interpreter.null_prototype_root();
        // SAFETY: interpreter and its heap outlive this internal fixture scope.
        let scope = unsafe {
            otter_gc::HandleScope::from_ptr(interpreter.gc_heap_mut().handle_stack_ptr())
        };
        let mut objects = Vec::new();
        // Old-space fixture objects remain at stable addresses while the
        // ordinary setter owns every allocating property transition's roots.
        for capacity in [0, 4, 64] {
            let root = interpreter
                .object_root(None, capacity, crate::object::ShapeState::ORDINARY)
                .expect("capacity root");
            assert_eq!(
                root,
                interpreter
                    .object_root(None, capacity, crate::object::ShapeState::ORDINARY)
                    .expect("same root")
            );
            assert_eq!(super::super::shape_body::inline_capacity_of(root), capacity);
            assert_eq!(
                super::super::shape_body::inline_capacity_of(
                    super::super::shape_body::dictionary_of(root)
                ),
                capacity
            );
            let object = scope.local(
                super::super::alloc_object_body_old(
                    interpreter.gc_heap_mut(),
                    super::super::empty_object_body(root),
                )
                .expect("object"),
            );
            for index in 0..4 {
                interpreter
                    .create_data_property(
                        &mut object.get(),
                        &format!("p{index}"),
                        crate::Value::number_i32(index + 10),
                    )
                    .expect("field");
            }
            objects.push((object, capacity));
        }
        for pair in objects.windows(2) {
            assert_ne!(
                super::super::shape(pair[0].0.get(), interpreter.gc_heap()),
                super::super::shape(pair[1].0.get(), interpreter.gc_heap())
            );
        }
        for (object, capacity) in &objects {
            let capacity = *capacity;
            interpreter.gc_heap().read_payload(object.get(), |body| {
                body.debug_verify_field_layout();
                assert_eq!(body.inline_capacity(), capacity);
                assert_eq!(body.slab.is_null(), capacity >= 4);
                for index in 0..4 {
                    let field = FieldLocation::for_slot(index, capacity);
                    // SAFETY: the field is below the verified live slot count
                    // in the dynamically selected storage.
                    assert_eq!(
                        unsafe { *body.field_ptr(field) },
                        crate::Value::number_i32(index as i32 + 10)
                    );
                }
            });
        }
        for (object, capacity) in &objects {
            let capacity = *capacity;
            for index in 4..=capacity.max(4) {
                interpreter
                    .create_data_property(
                        &mut object.get(),
                        &format!("p{index}"),
                        crate::Value::number_i32(index as i32 + 10),
                    )
                    .expect("append");
            }
            interpreter.gc_heap().read_payload(object.get(), |body| {
                body.debug_verify_field_layout();
                assert!(!body.slab.is_null(), "overflow allocates a suffix");
                if capacity != 0 {
                    // SAFETY: p0 remains the first live inline word after growth.
                    assert_eq!(
                        unsafe { *body.inline_values_ptr() },
                        crate::Value::number_i32(10)
                    );
                    assert_eq!(body.location_for_slot(capacity), FieldLocation::overflow(0));
                }
                for index in 0..body.slot_count() {
                    assert_eq!(
                        body.slot_word(index),
                        crate::Value::number_i32(index as i32 + 10)
                    );
                }
            });
            let child = scope.local(
                super::super::alloc_object_with_shape_roots(
                    interpreter.gc_heap_mut(),
                    default_root,
                    &mut |_| {},
                )
                .expect("young child"),
            );
            interpreter
                .create_data_property(&mut child.get(), "marker", crate::Value::number_i32(79))
                .expect("child marker");
            interpreter
                .create_data_property(&mut object.get(), "p0", crate::Value::object(child.get()))
                .expect("reference field");
            interpreter
                .create_data_property(
                    &mut object.get(),
                    &format!("p{}", capacity.max(4)),
                    crate::Value::object(child.get()),
                )
                .expect("overflow reference");
            interpreter
                .force_gc()
                .expect("collection with live handles and shape-interner roots");
            let stored = super::super::get_own(object.get(), interpreter.gc_heap(), "p0")
                .and_then(|value| value.as_object())
                .expect("rewritten child field");
            assert_eq!(stored, child.get());
            assert_eq!(
                super::super::get_own(
                    object.get(),
                    interpreter.gc_heap(),
                    &format!("p{}", capacity.max(4))
                ),
                Some(crate::Value::object(child.get()))
            );
            assert_eq!(
                super::super::get_own(stored, interpreter.gc_heap(), "marker"),
                Some(crate::Value::number_i32(79))
            );
            // Deletion shifts a suffix field into the prefix when needed;
            // subsequent prototype and descriptor transitions preserve capacity.
            let mut current = object.get();
            assert!(
                super::super::delete(&mut current, interpreter.gc_heap_mut(), "p1")
                    .expect("field deletion allocation")
            );
            assert_eq!(current, object.get(), "rooted receiver after deletion");
            let prototype = crate::Value::object(child.get());
            assert!(
                super::super::set_prototype_value(
                    &mut current,
                    interpreter.gc_heap_mut(),
                    Some(prototype)
                )
                .expect("prototype transition allocation")
            );
            assert_eq!(
                current,
                object.get(),
                "rooted receiver after prototype preparation"
            );
            interpreter.migrate_slow_to_fast(&mut current);
            assert_eq!(current, object.get());
            interpreter.gc_heap().read_payload(current, |body| {
                body.debug_verify_field_layout();
                assert_eq!(body.inline_capacity(), capacity);
            });
            interpreter
                .force_gc()
                .expect("collection after layout transitions");
            assert_eq!(
                super::super::get_own(object.get(), interpreter.gc_heap(), "p0"),
                Some(crate::Value::object(child.get()))
            );
            assert_eq!(
                super::super::get_own(
                    object.get(),
                    interpreter.gc_heap(),
                    &format!("p{}", capacity.max(4))
                ),
                Some(crate::Value::object(child.get()))
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn verifier_rejects_capacity_count_and_slab_footprint_disagreement() {
        let layout = FieldLayout::current();
        layout.debug_verify(0, 24, 0, None);
        layout.debug_verify(4, 56, 4, None);
        layout.debug_verify(64, 536, 65, Some((128, 1040)));
        for (inline, bytes, count, slab) in [
            (65, 544, 0, None),
            (4, 48, 0, None),
            (4, 56, 5, None),
            (0, 24, 5, Some((4, 48))),
            (4, 56, 5, Some((8, 72))),
        ] {
            assert!(
                std::panic::catch_unwind(|| layout.debug_verify(inline, bytes, count, slab))
                    .is_err()
            );
        }
    }
}
