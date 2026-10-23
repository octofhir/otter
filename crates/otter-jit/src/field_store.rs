//! One selected field-observation record per native data store.
//!
//! # Contents
//! - [`FieldStoreOwner`] — the defining owner a native own, named or literal
//!   store observes, with the captured membership its probe tries first.
//! - [`StoreValueBound`] — the categories a stored value can have after the
//!   store's own effects.
//!
//! # Invariants
//! - Captured categories `C` of a defining owner monotonically belong to its
//!   live categories `L`. A post-effect bound `B ⊆ C` therefore cannot widen
//!   `L` for the whole generation: such a store, and one whose captured word
//!   is the absorbing Top, carries owner zero and emits no observation.
//! - A nonzero owner keeps both captured and live membership: the live word
//!   may have grown after the generation captured it.
//! - An exact Int32 owner admits only its captured value. After constness
//!   collapses, every Int32 value belongs to its permanent category union.
//! - Object membership compares rooted retained target offsets
//!   (`stored_shapes`), never loaded-value shapes or packed identities.
//! - Selection is pure: it neither demands guards nor records path facts.
//!   Whoever forms a bound owns the proof that the bound holds at the store.
//!
//! # See also
//! - `otter_vm::object::field_type` owns classification and monotone widening.
//! - `otter_vm::jit::JitFieldType` carries each source body's captured records.

use otter_vm::jit::JitFieldType;
use otter_vm::object::field_type;
use smallvec::SmallVec;

/// The defining field cell a native store observes; zero observes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FieldStoreOwner {
    pub(crate) owner_shape: u32,
    /// Rooted historical targets whose current membership needs no widening.
    pub(crate) stored_shapes: SmallVec<[u32; 2]>,
    pub(crate) nullish_mask: u8,
    pub(crate) int32: bool,
    /// Encoded Double membership, including integral Double words.
    pub(crate) double: bool,
    pub(crate) int32_constant: Option<i32>,
}

/// Every category a stored value can have once its store completes: at most
/// two exact ordinary shapes, Int32/Double words and nullish alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreValueBound {
    pub(crate) shapes: SmallVec<[u32; 2]>,
    pub(crate) nullish: u8,
    pub(crate) int32: bool,
    /// Encoded Double membership, including integral Double words.
    pub(crate) double: bool,
    pub(crate) int32_constant: Option<i32>,
}

impl StoreValueBound {
    /// The bound of an exact ordinary object of one shape.
    pub(crate) fn exact(shape: u32) -> Self {
        Self {
            shapes: smallvec::smallvec![shape],
            nullish: 0,
            int32: false,
            double: false,
            int32_constant: None,
        }
    }

    /// `self ∪ {shape}`, or `None` past two ordinary shapes.
    pub(crate) fn with_shape(&self, shape: u32) -> Option<Self> {
        let mut shapes = self.shapes.clone();
        if !shapes.contains(&shape) {
            if shapes.len() == 2 {
                return None;
            }
            shapes.push(shape);
        }
        Some(Self {
            shapes,
            nullish: self.nullish,
            int32: self.int32,
            double: self.double,
            int32_constant: self.int32_constant,
        })
    }
}

impl FieldStoreOwner {
    /// The record of a store through `record`'s defining owner whose value
    /// satisfies `bound` after the store's effects, when one is proved.
    pub(crate) fn select(record: &JitFieldType, bound: Option<&StoreValueBound>) -> Self {
        let observed = record.guard.expected & field_type::TOP == 0
            && !bound.is_some_and(|bound| Self::captures(record, bound));
        Self {
            owner_shape: if observed {
                record.guard.owner_shape
            } else {
                0
            },
            stored_shapes: record.stored_shapes.clone(),
            nullish_mask: record.nullish_mask,
            int32: record.guard.expected & field_type::INT32 != 0,
            double: record.guard.expected & field_type::DOUBLE != 0,
            int32_constant: field_type::int32_constant(record.guard.expected),
        }
    }

    /// Whether the captured categories already contain every category of `bound`.
    pub(crate) fn captures(record: &JitFieldType, bound: &StoreValueBound) -> bool {
        u64::from(bound.nullish) & !record.guard.expected == 0
            && (!bound.int32
                || (record.guard.expected & field_type::INT32 != 0
                    && field_type::int32_constant(record.guard.expected)
                        .is_none_or(|value| bound.int32_constant == Some(value))))
            && (!bound.double || record.guard.expected & field_type::DOUBLE != 0)
            && bound
                .shapes
                .iter()
                .all(|shape| record.stored_shapes.contains(shape))
    }

    /// Whether the store must run the observation probe.
    pub(crate) fn observes(&self) -> bool {
        self.owner_shape != 0
    }
}

#[cfg(test)]
#[path = "field_store_probe_tests.rs"]
mod probe_tests;
