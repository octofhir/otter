//! Property-name side tables for the new VM.

use std::collections::BTreeMap;

/// Stable property-name identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PropertyNameId(pub u16);

/// Runtime-wide property-name registry.
///
/// Function-local property tables remain immutable compilation side tables, but
/// the object heap needs stable identifiers that survive across functions and
/// bootstrap-installed globals. This registry interns names into one runtime
/// key space.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyNameRegistry {
    names: Vec<Box<str>>,
    ids_by_name: BTreeMap<Box<str>, PropertyNameId>,
}

impl PropertyNameRegistry {
    /// Creates an empty runtime-wide property registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of interned property names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns `true` when no property names have been interned yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Interns one property name into the runtime-wide registry.
    pub fn intern(&mut self, name: &str) -> PropertyNameId {
        if let Some(id) = self.ids_by_name.get(name) {
            return *id;
        }

        let id = PropertyNameId(u16::try_from(self.names.len()).unwrap_or(u16::MAX));
        let owned_name: Box<str> = name.into();
        self.names.push(owned_name.clone());
        self.ids_by_name.insert(owned_name, id);
        id
    }

    /// Resolves a runtime-wide property id back to its name.
    #[must_use]
    pub fn get(&self, id: PropertyNameId) -> Option<&str> {
        self.names.get(usize::from(id.0)).map(Box::as_ref)
    }
}

/// Immutable property-name table for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyNameTable {
    names: Box<[Box<str>]>,
}

impl PropertyNameTable {
    /// Creates a property-name table from owned names.
    #[must_use]
    pub fn new(names: Vec<impl Into<Box<str>>>) -> Self {
        let names = names
            .into_iter()
            .map(Into::into)
            .collect::<Vec<Box<str>>>()
            .into_boxed_slice();

        Self { names }
    }

    /// Creates an empty property-name table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::<Box<str>>::new())
    }

    /// Returns the number of property names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Returns the property name for the given identifier.
    #[must_use]
    pub fn get(&self, id: PropertyNameId) -> Option<&str> {
        self.names.get(usize::from(id.0)).map(Box::as_ref)
    }

    /// Returns the immutable name slice.
    #[must_use]
    pub fn names(&self) -> &[Box<str>] {
        &self.names
    }
}

impl Default for PropertyNameTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{PropertyNameId, PropertyNameRegistry, PropertyNameTable};

    #[test]
    fn property_name_table_resolves_names() {
        let table = PropertyNameTable::new(vec!["count", "total"]);

        assert_eq!(table.len(), 2);
        assert_eq!(table.get(PropertyNameId(0)), Some("count"));
        assert_eq!(table.get(PropertyNameId(1)), Some("total"));
        assert_eq!(table.get(PropertyNameId(2)), None);
    }

    #[test]
    fn property_name_registry_interns_names_across_runtime() {
        let mut registry = PropertyNameRegistry::new();

        let count_a = registry.intern("count");
        let total = registry.intern("total");
        let count_b = registry.intern("count");

        assert_eq!(count_a, PropertyNameId(0));
        assert_eq!(total, PropertyNameId(1));
        assert_eq!(count_b, count_a);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get(count_a), Some("count"));
        assert_eq!(registry.get(total), Some("total"));
    }
}
