//! Native access widths for shape-owned field words.
//!
//! # Contents
//! - [`FieldAccess`] selects a tagged load, an int32 payload load or a store.
//!
//! # Invariants
//! - A tagged word occupies eight bytes on both native targets.
//! - An int32 load requires a live field-summary proof and reads its low word.
//! - Stores always publish the complete tagged value and own their barriers.
//!
//! # See also
//! - `graph::builder_field_type` owns the summary proofs.
//! - `otter_vm::object::FieldLocation` owns bank and index geometry.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldAccess {
    LoadTagged,
    LoadInt32,
    Store,
}
