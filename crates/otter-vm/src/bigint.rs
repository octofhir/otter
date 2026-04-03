//! BigInt-constant side tables for the new VM.
//!
//! Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>

/// Stable BigInt-constant identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BigIntId(pub u16);

/// Immutable BigInt-constant table for a function.
///
/// Each entry is a decimal string representation of an arbitrary-precision integer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BigIntTable {
    values: Vec<Box<str>>,
}

impl BigIntTable {
    /// Creates a BigInt-constant table from values.
    #[must_use]
    pub fn new(values: Vec<Box<str>>) -> Self {
        Self { values }
    }

    /// Creates an empty BigInt-constant table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Adds a BigInt constant and returns its identifier.
    pub fn add(&mut self, value: impl Into<Box<str>>) -> BigIntId {
        let id = BigIntId(self.values.len() as u16);
        self.values.push(value.into());
        id
    }

    /// Returns the BigInt constant for the given identifier.
    #[must_use]
    pub fn get(&self, id: BigIntId) -> Option<&str> {
        self.values.get(usize::from(id.0)).map(|s| s.as_ref())
    }

    /// Returns the number of BigInt constants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}
