//! Owned native-callable layout consumed by both code generators.
//!
//! # Contents
//! - [`JitNativeCallLayout`] describes the native body's C dispatch prefix.
//!
//! # Invariants
//! Offsets start at the decompressed GC cell, including its collector header.
//! The layout contains no live heap pointers and is stable across collection.
//! Entry kind selects payload interpretation; captures remain traced by the body.
//!
//! # See also
//! - [`crate::native_function`] owns and mutates the one callable header.
//! - [`super::JitCompileSnapshot`] owns this metadata during compilation.

/// Complete C dispatch prefix of a native-function cell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitNativeCallLayout {
    /// External-reference identity used by exact builtin guards.
    pub identity_byte: u32,
    /// Immutable [`crate::native_function::NativeEntryKind`] byte.
    pub kind_byte: u32,
    /// Mutable callable/property-policy flags.
    pub flags_byte: u32,
    /// Typed function address, intrinsic discriminant or host-reference index.
    pub payload_byte: u32,
    /// Compressed ValueSlab handle containing all traced captures.
    pub captures_byte: u32,
    /// Compressed handle of the own-property bag, which carries the
    /// callable's own `[[Prototype]]` once a lookup-start load prepared it.
    pub own_props_byte: u32,
    /// Flag granting this callable [[Construct]].
    pub constructable_flag: u8,
    /// Flag granting ordinary extension of the native callable's own properties.
    pub extensible_flag: u8,
    /// Flag marking an installed `[[Prototype]]` override.
    pub prototype_override_flag: u8,
}

impl JitNativeCallLayout {
    /// Describe the VM-owned production layout.
    #[must_use]
    pub fn current() -> Self {
        crate::native_function::jit_call_layout()
    }
}
