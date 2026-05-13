//! Atomized property-name constants for executable VM code.
//!
//! The bytecode constant pool remains the public/debug source of truth.
//! This module adds a tiny VM-owned key identity layer over string
//! constants so named property bytecodes can carry stable ids toward
//! inline caches without changing JavaScript semantics or bytecode JSON.
//!
//! # Contents
//! - [`AtomId`] — deterministic VM-local string-key id.
//! - [`PropertyAtom`] — compact identity for one string constant.
//! - [`AtomizedPropertyKey`] — borrowed runtime view: atom id plus text.
//!
//! # Invariants
//! - Atom ids are local to one [`crate::ExecutionContext`].
//! - The atom text is borrowed from the context's decoded string table.
//! - Computed property keys and symbols do not pass through this layer.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`crate::property_dispatch`]

/// Deterministic VM-local atom id for a string property key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AtomId(u32);

impl AtomId {
    /// Build an atom id from a constant-pool index.
    #[must_use]
    pub(crate) const fn from_constant_index(index: u32) -> Self {
        Self(index)
    }

    /// Raw numeric id used by future executable inline caches.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

/// Compact identity for one string constant used as a property key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PropertyAtom {
    id: AtomId,
}

impl PropertyAtom {
    /// Create an atom identity.
    #[must_use]
    pub(crate) const fn new(id: AtomId) -> Self {
        Self { id }
    }

    /// Stable VM-local id.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn id(self) -> AtomId {
        self.id
    }
}

/// Borrowed view of an atomized property key for one dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AtomizedPropertyKey<'a> {
    atom: PropertyAtom,
    text: &'a str,
}

impl<'a> AtomizedPropertyKey<'a> {
    /// Create a borrowed atomized key view.
    #[must_use]
    pub(crate) const fn new(atom: PropertyAtom, text: &'a str) -> Self {
        Self { atom, text }
    }

    /// Stable VM-local atom identity.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn atom(self) -> PropertyAtom {
        self.atom
    }

    /// String spelling used for existing object-model lookups.
    #[must_use]
    pub(crate) const fn name(self) -> &'a str {
        self.text
    }
}
