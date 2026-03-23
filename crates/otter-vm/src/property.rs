//! Property-name side tables for the new VM.

/// Stable property-name identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PropertyNameId(pub u16);

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
    use super::{PropertyNameId, PropertyNameTable};

    #[test]
    fn property_name_table_resolves_names() {
        let table = PropertyNameTable::new(vec!["count", "total"]);

        assert_eq!(table.len(), 2);
        assert_eq!(table.get(PropertyNameId(0)), Some("count"));
        assert_eq!(table.get(PropertyNameId(1)), Some("total"));
        assert_eq!(table.get(PropertyNameId(2)), None);
    }
}
