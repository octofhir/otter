//! Native access widths for shape-owned field words.
//!
//! # Contents
//! - [`FieldAccess`] selects a complete physical word, Int32 payload or store.
//!
//! # Invariants
//! - A physical word occupies eight bytes on both native targets; the caller
//!   returns the Value word unchanged, including integral Double words.
//! - An int32 load requires a live field-summary proof and reads its low word.
//! - Stores publish an admitted complete physical word and own their barriers.
//!
//! # See also
//! - `graph::builder_field_type` owns the summary proofs.
//! - `otter_vm::object::FieldLocation` owns bank and index geometry.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldAccess {
    /// Load the physical Value word without changing its numeric encoding.
    LoadTagged,
    LoadInt32,
    Store,
}
