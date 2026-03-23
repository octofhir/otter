//! String-literal side tables for the new VM.

/// Stable string-literal identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StringId(pub u16);

/// Immutable string-literal table for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringTable {
    values: Box<[Box<str>]>,
}

impl StringTable {
    /// Creates a string-literal table from owned values.
    #[must_use]
    pub fn new(values: Vec<impl Into<Box<str>>>) -> Self {
        let values = values
            .into_iter()
            .map(Into::into)
            .collect::<Vec<Box<str>>>()
            .into_boxed_slice();

        Self { values }
    }

    /// Creates an empty string-literal table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::<Box<str>>::new())
    }

    /// Returns the number of string literals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the string literal for the given identifier.
    #[must_use]
    pub fn get(&self, id: StringId) -> Option<&str> {
        self.values.get(usize::from(id.0)).map(Box::as_ref)
    }
}

impl Default for StringTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{StringId, StringTable};

    #[test]
    fn string_table_resolves_literals() {
        let table = StringTable::new(vec!["otter", "vm"]);

        assert_eq!(table.len(), 2);
        assert_eq!(table.get(StringId(0)), Some("otter"));
        assert_eq!(table.get(StringId(1)), Some("vm"));
        assert_eq!(table.get(StringId(2)), None);
    }
}
