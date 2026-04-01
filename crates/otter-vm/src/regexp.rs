//! RegExp-constant side tables for the new VM.
//!
//! Spec: <https://tc39.es/ecma262/#sec-regexp-regular-expression-objects>

/// Stable RegExp-constant identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegExpId(pub u16);

/// One entry in a RegExp-constant table (pattern + flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegExpEntry {
    /// The raw pattern string (without surrounding `/`).
    pub pattern: Box<str>,
    /// The canonical flags string (alphabetically sorted subset of `dgimsuyv`).
    pub flags: Box<str>,
}

/// Immutable RegExp-constant table for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegExpTable {
    entries: Box<[RegExpEntry]>,
}

impl RegExpTable {
    /// Creates a RegExp-constant table from `(pattern, flags)` pairs.
    #[must_use]
    pub fn new(entries: Vec<(Box<str>, Box<str>)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(pattern, flags)| RegExpEntry { pattern, flags })
                .collect(),
        }
    }

    /// Creates an empty RegExp-constant table.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            entries: Box::new([]),
        }
    }

    /// Returns the number of RegExp constants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the RegExp entry for the given identifier.
    #[must_use]
    pub fn get(&self, id: RegExpId) -> Option<&RegExpEntry> {
        self.entries.get(usize::from(id.0))
    }
}

impl Default for RegExpTable {
    fn default() -> Self {
        Self::empty()
    }
}
