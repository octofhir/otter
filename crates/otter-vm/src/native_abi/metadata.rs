//! Code-object metadata, dependency, lifetime, and version contracts.
//!
//! # Contents
//! - [`LayoutVersionRecord`] and [`BuildVersionRecord`] gate native code entry.
//! - [`CodeObjectMetadata`] identifies immutable code and all owned side tables.
//! - [`CodeDependency`] describes isolate state that can invalidate code.
//!
//! # Invariants
//! - Entry is rejected before execution when any layout/build/stub version
//!   differs from the installed VM.
//! - Metadata contains offsets/counts and stable ids, never Rust slices or
//!   container layouts.
//! - Invalid code is unlinked before retirement and retained while any active
//!   frame can return into it.
//!
//! # See also
//! - [`super::safepoints`] for code-object-owned root tables.
//! - [`super::frame::NativeFrame`] for the active code-object id.

/// Native VM ABI layout version.
pub const VM_LAYOUT_VERSION: u32 = 3;
/// Runtime-stub table version.
pub const RUNTIME_STUB_TABLE_VERSION: u32 = 2;
/// Code-object metadata layout version.
pub const CODE_OBJECT_LAYOUT_VERSION: u32 = 1;
/// Reproducible build identity for transient native code.
pub const VM_BUILD_VERSION: u64 = 0x4f54_5445_525f_0002;

/// Complete native-layout compatibility record.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutVersionRecord {
    /// VM thread/frame/dispatch layout.
    pub vm_layout: u32,
    /// Runtime-stub ids and descriptor layout.
    pub runtime_stubs: u32,
    /// Code-object metadata layout.
    pub code_object: u32,
    /// Reserved; zero in version 2.
    pub reserved: u32,
}

impl LayoutVersionRecord {
    /// Versions used by this build.
    pub const CURRENT: Self = Self {
        vm_layout: VM_LAYOUT_VERSION,
        runtime_stubs: RUNTIME_STUB_TABLE_VERSION,
        code_object: CODE_OBJECT_LAYOUT_VERSION,
        reserved: 0,
    };
}

/// Build and target identity folded into installed-code validity.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildVersionRecord {
    /// Otter VM build identity.
    pub vm_build: u64,
    /// Target ABI hash selected by the JIT backend.
    pub target_abi: u64,
}

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
    /// Reserved; zero in layout version 1.
    pub reserved: u32,
    /// Required native-layout versions.
    pub layout: LayoutVersionRecord,
    /// Required build and target identity.
    pub build: BuildVersionRecord,
}

/// Kind of assumption that can invalidate installed native code.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeDependencyKind {
    /// VM field layout/build compatibility.
    VmLayout = 0,
    /// Runtime-stub table compatibility.
    RuntimeStubTable = 1,
    /// Realm identity.
    Realm = 2,
    /// Global/prototype/array protector epoch.
    Protector = 3,
    /// Builtin identity epoch.
    BuiltinIdentity = 4,
    /// Shape/prototype epoch.
    ShapeEpoch = 5,
}

/// Explicit validity dependency owned by a code object.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeDependency {
    /// Dependency family.
    pub kind: CodeDependencyKind,
    /// Reserved flags; zero in layout version 2.
    pub flags: u16,
    /// Isolate-local stable identity.
    pub identity: u32,
    /// Expected version/value at code entry.
    pub expected: u64,
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

const _: [(); 16] = [(); std::mem::size_of::<LayoutVersionRecord>()];
const _: [(); 16] = [(); std::mem::size_of::<BuildVersionRecord>()];
const _: [(); 72] = [(); std::mem::size_of::<CodeObjectMetadata>()];
const _: [(); 8] = [(); std::mem::align_of::<CodeObjectMetadata>()];
const _: [(); 16] = [(); std::mem::size_of::<CodeDependency>()];
const _: [(); 40] = [(); std::mem::offset_of!(CodeObjectMetadata, layout)];
const _: [(); 56] = [(); std::mem::offset_of!(CodeObjectMetadata, build)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_layout_record_is_complete() {
        assert_eq!(LayoutVersionRecord::CURRENT.vm_layout, VM_LAYOUT_VERSION);
        assert_eq!(
            LayoutVersionRecord::CURRENT.runtime_stubs,
            RUNTIME_STUB_TABLE_VERSION
        );
        assert_eq!(
            LayoutVersionRecord::CURRENT.code_object,
            CODE_OBJECT_LAYOUT_VERSION
        );
    }
}
