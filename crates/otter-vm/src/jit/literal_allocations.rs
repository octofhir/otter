//! VM-owned literal geometry for native nursery allocation.
//!
//! # Contents
//! - Empty and static nonempty literal plans used by the graph tier.
//! - The ordinary-object header calculation shared with receiver allocation.
//!
//! # Invariants
//! Plans contain scalar geometry and an old shape offset rooted by the compile
//! session and installed generation. The source realm is the owning function's
//! realm. An array without a sidecar may be carved only in the default realm.
//! Object lookup/extensibility facts live only on the rooted immutable shape;
//! their former header bytes stay zero. Empty array payload fields all have
//! zero representations; Rust field order
//! and padding are never serialized across this boundary.
//!
//! # See also
//! `crate::literal_allocation` owns the matching source-realm slow semantics.

/// Prepared literal geometry for one exact source body.
#[derive(Debug, Clone, Default)]
pub struct JitLiteralAllocationPlans {
    /// Realm of this snapshot's source function, including an inlined body.
    pub realm_id: u32,
    /// Whether this exact source snapshot may form fixed allocation groups.
    /// The one heap policy is sampled before plan preparation. False retains
    /// standalone native allocations and their canonical collecting misses;
    /// generated group admission rechecks current policy after collection.
    pub group_allowed: bool,
    /// Rooted intrinsic ordinary-object shape; absent before VM preparation.
    pub object: Option<JitEmptyObjectAllocationPlan>,
    /// Layout of the default-realm bare empty array shell.
    pub array: JitEmptyArrayAllocationPlan,
    /// Final static object layouts by own logical instruction PC.
    pub objects: rustc_hash::FxHashMap<u32, JitObjectLiteralAllocationPlan>,
    /// Dense array layouts by own logical instruction PC.
    pub arrays: rustc_hash::FxHashMap<u32, JitArrayLiteralAllocationPlan>,
}

/// Rooted final shape and its inline initialization geometry at one site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitObjectLiteralAllocationPlan {
    /// Old compressed final shape offset retained by the generation.
    pub shape: u32,
    /// Header-relative compressed shape word.
    pub shape_byte: u32,
    /// Aligned size of the complete object cell.
    pub cell_bytes: u32,
    /// Complete young ordinary-object header.
    pub header_word: u64,
    /// Shape-owned number of inline fields.
    pub inline_capacity: u32,
    /// Number of initialized properties in source order.
    pub value_count: u32,
    /// VM-owned inline addressing geometry.
    pub fields: crate::object::FieldLayout,
}

impl JitObjectLiteralAllocationPlan {
    pub(crate) fn new(shape: crate::object::ShapeHandle, count: usize) -> Option<Self> {
        if shape.is_null() {
            return None;
        }
        let state = crate::object::shape_body::state_of(shape);
        if state.is_provisional() || state.is_dictionary() || state.is_opaque() {
            return None;
        }
        let capacity = crate::object::shape_body::inline_capacity_of(shape);
        if count > capacity {
            return None;
        }
        let fields = crate::object::FieldLayout::current();
        let cell_bytes = u32::try_from(fields.cell_bytes(capacity)).ok()?;
        Some(Self {
            shape: shape.offset(),
            shape_byte: u32::try_from(
                otter_gc::header::HEADER_SIZE + crate::object::OBJECT_BODY_SHAPE_OFFSET,
            )
            .ok()?,
            cell_bytes,
            header_word: ordinary_object_header_word(cell_bytes),
            inline_capacity: u32::try_from(capacity).ok()?,
            value_count: u32::try_from(count).ok()?,
            fields,
        })
    }
}

/// Two-cell array geometry; classification selects tagged or numeric storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitArrayLiteralAllocationPlan {
    /// Exact source element count, including holes.
    pub value_count: u32,
    /// Layout when any element is neither a number nor a hole.
    pub tagged: JitDenseArrayAllocationPlan,
    /// Layout for doubles with an initialized side hole bitmap.
    pub numeric: JitDenseArrayAllocationPlan,
}

impl JitArrayLiteralAllocationPlan {
    /// Prepare a bounded array literal's canonical dense representations.
    #[must_use]
    pub fn new(count: usize) -> Option<Self> {
        if count == 0 || count > 240 {
            return None;
        }
        Some(Self {
            value_count: u32::try_from(count).ok()?,
            tagged: JitDenseArrayAllocationPlan::new(count, false)?,
            numeric: JitDenseArrayAllocationPlan::new(count, true)?,
        })
    }
}

/// VM-derived header-inclusive offsets for one array shell and dense slab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDenseArrayAllocationPlan {
    /// Array shell geometry.
    pub shell: JitEmptyArrayAllocationPlan,
    /// Aligned slab bytes after the shell in the same reservation.
    pub slab_bytes: u32,
    /// Complete young slab header.
    pub slab_header_word: u64,
    /// Slab-relative start of element words.
    pub data_byte: u32,
    /// Slab-relative start of the numeric hole bitmap.
    pub bitmap_byte: u32,
    /// Number of numeric bitmap words; zero for tagged storage.
    pub bitmap_words: u32,
    /// Slab-relative capacity, live length, hole count, kind, and dirty interval.
    pub capacity_byte: u32,
    /// Slab-relative live length word.
    pub len_byte: u32,
    /// Slab-relative hole count word.
    pub hole_count_byte: u32,
    /// Slab-relative representation byte.
    pub kind_byte: u32,
    /// Slab-relative dirty interval start word.
    pub dirty_start_byte: u32,
    /// Header-relative shell slab handle.
    pub shell_slab_byte: u32,
    /// Header-relative shell logical length.
    pub shell_length_byte: u32,
    /// Header-relative shell cached element address.
    pub shell_data_byte: u32,
    /// Header-relative shell cached live length.
    pub shell_len_byte: u32,
    /// Header-relative shell cached capacity.
    pub shell_capacity_byte: u32,
    /// Header-relative shell cached kind.
    pub shell_kind_byte: u32,
    /// Initial dense kind: tagged or packed double.
    pub initial_kind: u8,
}

impl JitDenseArrayAllocationPlan {
    fn new(count: usize, numeric: bool) -> Option<Self> {
        use crate::array::elements as e;
        let header = otter_gc::header::HEADER_SIZE;
        let data_byte = u32::try_from(header + std::mem::size_of::<e::ElementSlabBody>()).ok()?;
        let bitmap_words = if numeric {
            count.div_ceil(64) as u32
        } else {
            0
        };
        let bitmap_byte = data_byte.checked_add(u32::try_from(count.checked_mul(8)?).ok()?)?;
        let bytes = bitmap_byte.checked_add(bitmap_words.checked_mul(8)?)?;
        let slab_bytes = u32::try_from(otter_gc::page::align_up(
            bytes as usize,
            otter_gc::page::CELL_SIZE,
        ))
        .ok()?;
        let offset = |byte: usize| u32::try_from(header + byte).ok();
        Some(Self {
            shell: JitEmptyArrayAllocationPlan::default(),
            slab_bytes,
            slab_header_word: young_header(e::ELEMENT_SLAB_BODY_TYPE_TAG, slab_bytes),
            data_byte,
            bitmap_byte,
            bitmap_words,
            capacity_byte: offset(e::SLAB_CAPACITY_BYTE)?,
            len_byte: offset(e::SLAB_LEN_BYTE)?,
            hole_count_byte: offset(e::SLAB_HOLE_COUNT_BYTE)?,
            kind_byte: offset(e::SLAB_KIND_BYTE)?,
            dirty_start_byte: offset(e::SLAB_DIRTY_START_BYTE)?,
            shell_slab_byte: offset(crate::array::ARRAY_BODY_SLAB_OFFSET)?,
            shell_length_byte: offset(crate::array::ARRAY_BODY_LENGTH_OFFSET)?,
            shell_data_byte: offset(crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET)?,
            shell_len_byte: offset(crate::array::ARRAY_BODY_DENSE_LEN_OFFSET)?,
            shell_capacity_byte: offset(crate::array::ARRAY_BODY_DENSE_CAP_OFFSET)?,
            shell_kind_byte: offset(crate::array::ARRAY_BODY_DENSE_KIND_OFFSET)?,
            initial_kind: if numeric {
                e::DenseElementKind::PackedDouble as u8
            } else {
                e::DenseElementKind::Tagged as u8
            },
        })
    }
}

fn young_header(tag: u8, bytes: u32) -> u64 {
    u64::from(tag)
        | (u64::from(super::JIT_GC_YOUNG_FLAG) << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET))
        | (u64::from(bytes) << (8 * otter_gc::header::HEADER_SIZE_BYTES_OFFSET))
}

/// Complete geometry of an ordinary empty object with spare inline slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitEmptyObjectAllocationPlan {
    /// Old compressed shape offset retained by a compilation root.
    pub shape: u32,
    /// Header-relative compressed shape-word offset.
    pub shape_byte: u32,
    /// Aligned header-inclusive allocation bytes.
    pub cell_bytes: u32,
    /// Complete young/extensible/size header; capacity belongs to the shape.
    pub header_word: u64,
    /// Header-relative offsets of the four initialized spare Value words.
    pub initial_value_bytes: [u32; 4],
}

impl JitEmptyObjectAllocationPlan {
    /// Compute the VM's empty-object geometry for a prepared rooted shape.
    #[must_use]
    pub fn new(shape: u32) -> Self {
        let capacity = crate::object::DEFAULT_INLINE_CAPACITY;
        const { assert!(crate::object::DEFAULT_INLINE_CAPACITY == 4) };
        const { assert!(crate::object::object_cell_bytes(4) <= u32::MAX as usize) };
        let layout = crate::object::FieldLayout::current();
        let cell_bytes = layout.cell_bytes(capacity) as u32;
        Self {
            shape,
            shape_byte: (otter_gc::header::HEADER_SIZE + crate::object::OBJECT_BODY_SHAPE_OFFSET)
                as u32,
            cell_bytes,
            header_word: ordinary_object_header_word(cell_bytes),
            initial_value_bytes: std::array::from_fn(|index| {
                layout.inline_byte(crate::object::FieldLocation::inline(index as u32))
            }),
        }
    }
}

/// Geometry of the null-slab, null-sidecar empty array shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitEmptyArrayAllocationPlan {
    /// Aligned header-inclusive allocation bytes; every payload word is zero.
    pub cell_bytes: u32,
    /// Complete young/type/size header, with zero body-owned header bytes.
    pub header_word: u64,
}

impl Default for JitEmptyArrayAllocationPlan {
    fn default() -> Self {
        const {
            assert!(crate::array::elements::DenseElementKind::Empty as u8 == 0);
            assert!(std::mem::align_of::<crate::array::ArrayBody>() <= otter_gc::OBJECT_ALIGNMENT);
            assert!(
                otter_gc::header::HEADER_SIZE
                    + std::mem::size_of::<crate::array::ArrayBody>()
                    + otter_gc::page::CELL_SIZE
                    <= u32::MAX as usize
            );
        };
        let bytes = otter_gc::header::HEADER_SIZE + std::mem::size_of::<crate::array::ArrayBody>();
        let cell_bytes = otter_gc::page::align_up(bytes, otter_gc::page::CELL_SIZE) as u32;
        Self {
            cell_bytes,
            header_word: u64::from(crate::array::ARRAY_BODY_TYPE_TAG)
                | (u64::from(super::JIT_GC_YOUNG_FLAG)
                    << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET))
                | (u64::from(cell_bytes) << (8 * otter_gc::header::HEADER_SIZE_BYTES_OFFSET)),
        }
    }
}

/// Complete young ordinary-object header for an authoritative aligned cell size.
/// Generated fixed and dynamic receiver writers share this physical owner.
#[must_use]
pub fn ordinary_object_header_word(cell_bytes: u32) -> u64 {
    u64::from(crate::object::OBJECT_BODY_TYPE_TAG)
        | (u64::from(super::JIT_GC_YOUNG_FLAG) << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET))
        | (u64::from(cell_bytes) << (8 * otter_gc::header::HEADER_SIZE_BYTES_OFFSET))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allocation_geometry_matches_vm_header_and_bodies() {
        let object = JitEmptyObjectAllocationPlan::new(8);
        assert_eq!(object.cell_bytes, 56);
        assert_eq!(object.initial_value_bytes, [24, 32, 40, 48]);
        assert_eq!(
            object.header_word as u8,
            crate::object::OBJECT_BODY_TYPE_TAG
        );
        assert_eq!(
            (object.header_word >> 16) as u16,
            0,
            "lookup/extensibility state is never duplicated in the object header"
        );
        assert_eq!(object.header_word >> 32, u64::from(object.cell_bytes));
        let array = JitEmptyArrayAllocationPlan::default();
        assert_eq!(
            array.cell_bytes as usize,
            otter_gc::page::align_up(
                otter_gc::header::HEADER_SIZE + std::mem::size_of::<crate::array::ArrayBody>(),
                otter_gc::page::CELL_SIZE,
            )
        );
        assert_eq!(array.header_word as u8, crate::array::ARRAY_BODY_TYPE_TAG);
        assert_eq!((array.header_word >> 16) as u16, 0);
        assert_eq!(array.header_word >> 32, u64::from(array.cell_bytes));
        let body = crate::array::ArrayBody::default();
        assert!(body.slab.is_null());
        assert!(body.exotic.is_null());
        assert_eq!(body.length, 0);
    }
}
