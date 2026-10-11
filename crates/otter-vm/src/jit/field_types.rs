//! Owned field-value bounds prepared for native compilation.
//!
//! # Contents
//! - [`JitFieldTypeGuard`] identifies one defining shape's monotone word.
//! - [`JitFieldType`] separates historical store membership from the stable
//!   ordinary targets and nullish alternatives an own-data load can prove.
//!
//! # Invariants
//! - Shape offsets name non-moving cells rooted by the compile session and
//!   retained by the installed generation; no raw moving value crosses here.
//! - The expected word contains VM-local shape identities, not cage offsets.
//!   Target offsets are resolved separately and never truncated from identities.
//! - Exact-identity counts and nullish bits occupy the low byte. Each widening
//!   changes it permanently in one owner's history, so comparing the captured
//!   byte proves the complete word unchanged without a separate generation tag.
//! - Nonempty load targets describe the entire bound: stable ordinary targets
//!   and the declared nullish alternatives, with no other value category.
//! - Store targets independently resolve historical identities, including
//!   unstable shapes. They authorize membership only for a current RHS shape
//!   proof and never describe the current value of a loaded field.
//! - An empty load target list grants no result shape proof but retains the
//!   store's defining-cell authority and each resolvable historical target.
//!
//! # See also
//! - [`crate::object::field_type`] owns observation and the scalar lattice.
//! - [`super::JitCompileSnapshot`] carries these records per source body.

use smallvec::SmallVec;

/// Defining-cell snapshot checked before a native own-data load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JitFieldTypeGuard {
    /// Compressed defining-shape offset, retained by the generation.
    pub owner_shape: u32,
    /// Entire canonical monotone word captured while preparing this source body.
    /// Native guards compare its low byte; exact targets use the full snapshot.
    pub expected: u64,
}

/// Result bounds and store authority for a shape-owned data location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitFieldType {
    /// Defining cell and captured word, even when no load proof is available.
    pub guard: JitFieldTypeGuard,
    /// Stable ordinary target shapes, resolved to rooted cage offsets.
    pub shapes: SmallVec<[u32; 2]>,
    /// Independently resolved historical shapes, retained even when unstable.
    /// A missing historical identity excludes only that identity from this list.
    pub stored_shapes: SmallVec<[u32; 2]>,
    /// Permitted nullish alternatives from the VM-owned scalar encoding.
    pub nullish_mask: u8,
}
