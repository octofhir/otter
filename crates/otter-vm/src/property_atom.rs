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
//! - [`AtomTableBuilder`] and [`AtomTable`] — transient build step and frozen
//!   execution table for decoded strings and named-property atoms.
//!
//! # Invariants
//! - Atom ids are local to one [`crate::ExecutionContext`].
//! - The atom text is borrowed from a frozen [`AtomTable`].
//! - Computed property keys and symbols do not pass through this layer.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`crate::property_dispatch`]

use otter_bytecode::Constant;

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

/// Transient builder for [`AtomTable`].
///
/// The builder may allocate and decode freely while an
/// [`crate::ExecutionContext`] is being constructed. Runtime dispatch receives
/// only the frozen table.
#[derive(Debug, Default)]
pub(crate) struct AtomTableBuilder {
    decoded_strings: Vec<Option<String>>,
    property_atoms: Vec<Option<PropertyAtom>>,
}

impl AtomTableBuilder {
    /// Build atom metadata from a bytecode constant pool.
    #[must_use]
    pub(crate) fn from_constants(constants: &[Constant]) -> Self {
        let mut builder = Self {
            decoded_strings: Vec::with_capacity(constants.len()),
            property_atoms: Vec::with_capacity(constants.len()),
        };
        for (idx, constant) in constants.iter().enumerate() {
            builder.push(idx, constant);
        }
        builder
    }

    fn push(&mut self, idx: usize, constant: &Constant) {
        match constant {
            Constant::String { utf16 } => {
                self.decoded_strings
                    .push(Some(String::from_utf16_lossy(utf16)));
                let idx = u32::try_from(idx).expect("constant pool index exceeds u32");
                self.property_atoms
                    .push(Some(PropertyAtom::new(AtomId::from_constant_index(idx))));
            }
            _ => {
                self.decoded_strings.push(None);
                self.property_atoms.push(None);
            }
        }
    }

    /// Seal the transient buffers into an immutable execution table.
    #[must_use]
    pub(crate) fn freeze(self) -> AtomTable {
        AtomTable {
            decoded_strings: self.decoded_strings.into_boxed_slice(),
            property_atoms: self.property_atoms.into_boxed_slice(),
        }
    }
}

/// Frozen atom table published with an [`crate::ExecutionContext`].
#[derive(Debug)]
pub(crate) struct AtomTable {
    decoded_strings: Box<[Option<String>]>,
    property_atoms: Box<[Option<PropertyAtom>]>,
}

impl AtomTable {
    /// Build and freeze an atom table from a bytecode constant pool.
    #[must_use]
    pub(crate) fn from_constants(constants: &[Constant]) -> Self {
        AtomTableBuilder::from_constants(constants).freeze()
    }

    /// Resolve a string constant as a borrowed UTF-8 string.
    #[must_use]
    pub(crate) fn string_constant_str(&self, idx: u32) -> Option<&str> {
        self.decoded_strings
            .get(idx as usize)
            .and_then(Option::as_deref)
    }

    /// Resolve a string constant as an atomized property key.
    #[must_use]
    pub(crate) fn property_atom(&self, idx: u32) -> Option<AtomizedPropertyKey<'_>> {
        let atom = self
            .property_atoms
            .get(idx as usize)
            .and_then(|atom| *atom)?;
        let text = self.string_constant_str(idx)?;
        Some(AtomizedPropertyKey::new(atom, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn builder_freezes_decoded_strings_and_atoms() {
        let constants = vec![
            Constant::Number {
                bits: 1.0f64.to_bits(),
            },
            Constant::String {
                utf16: utf16("name"),
            },
            Constant::String {
                utf16: utf16("other"),
            },
        ];

        let table = AtomTableBuilder::from_constants(&constants).freeze();

        assert_eq!(table.string_constant_str(0), None);
        assert_eq!(table.string_constant_str(1), Some("name"));
        assert_eq!(table.string_constant_str(2), Some("other"));

        let key = table.property_atom(1).expect("string atom");
        assert_eq!(key.name(), "name");
        assert_eq!(key.atom().id().get(), 1);
        assert!(table.property_atom(0).is_none());
    }

    #[test]
    fn atom_ids_are_deterministic_constant_indexes() {
        let constants = vec![
            Constant::String { utf16: utf16("x") },
            Constant::String { utf16: utf16("x") },
        ];
        let table = AtomTable::from_constants(&constants);

        let first = table.property_atom(0).unwrap().atom().id().get();
        let second = table.property_atom(1).unwrap().atom().id().get();

        assert_eq!(first, 0);
        assert_eq!(second, 1);
    }
}
