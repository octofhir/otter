//! Code-object metadata, dependency, and lifetime contracts.
//!
//! # Contents
//! - [`CodeObjectMetadata`] identifies immutable code and all owned side tables.
//! - [`CodeDependency`] describes isolate state that can invalidate code.
//!
//! # Invariants
//! - Installed code always uses the current in-process VM layout.
//! - Entry is rejected before execution when a recorded dependency epoch is
//!   not exactly current for its `(kind, identity)` family.
//! - Metadata contains offsets/counts and stable ids, never Rust slices or
//!   container layouts.
//! - Epoch invalidation is monotonic: dependencies older than the current
//!   epoch are invalidated; future epochs are not invalidated but fail the
//!   exact-equality install/entry consistency check.
//! - Invalid code is unlinked before retirement and retained while any active
//!   frame can return into it.
//!
//! # See also
//! - [`super::safepoints`] for code-object-owned root tables.
//! - [`super::frame::NativeFrame`] for the active code-object id.

/// Stable identity of the array-index accessor protector epoch.
pub const ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY: u32 = 0;
/// Stable identity of the ordinary-object prototype shape epoch.
pub const ORDINARY_OBJECT_PROTOTYPE_SHAPE_IDENTITY: u32 = 0;

/// Machine-visible immutable code-object metadata header.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeObjectMetadata {
    /// Isolate-local installed code identity.
    pub id: u64,
    /// Source [`crate::code_block::CodeBlock`] identity.
    pub code_block_id: u32,
    /// Native entry offset from the code allocation base.
    pub entry_offset: u32,
    /// Native code size in bytes.
    pub code_size: u32,
    /// Number of safepoint entries owned by this object.
    pub safepoint_count: u32,
    /// Number of frame maps owned by this object.
    pub frame_map_count: u32,
    /// Number of spill maps owned by this object.
    pub spill_map_count: u32,
    /// Number of validity dependencies.
    pub dependency_count: u32,
}

/// Kind of assumption that can invalidate installed native code.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeDependencyKind {
    /// Realm identity.
    Realm = 0,
    /// Global/prototype/array protector epoch.
    Protector = 1,
    /// Builtin identity epoch.
    BuiltinIdentity = 2,
    /// Shape/prototype epoch.
    ShapeEpoch = 3,
}

/// Explicit validity dependency owned by a code object.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeDependency {
    /// Dependency family.
    pub kind: CodeDependencyKind,
    /// Isolate-local stable identity.
    pub identity: u32,
    /// Expected epoch/value at code entry.
    pub expected: u64,
}

impl CodeDependency {
    /// Construct one exact-match epoch dependency.
    #[must_use]
    pub const fn epoch(kind: CodeDependencyKind, identity: u32, expected: u64) -> Self {
        Self {
            kind,
            identity,
            expected,
        }
    }
}

/// Lifecycle state for installed code.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLifetimeState {
    /// Entry is valid and may be selected.
    Installed = 0,
    /// Entry is unlinked; active frames may still return through the code.
    Invalid = 1,
    /// No new or active entry; awaiting reclamation.
    Retired = 2,
}

const _: [(); 40] = [(); std::mem::size_of::<CodeObjectMetadata>()];
const _: [(); 8] = [(); std::mem::align_of::<CodeObjectMetadata>()];
const _: [(); 16] = [(); std::mem::size_of::<CodeDependency>()];
