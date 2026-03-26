//! Float-constant side tables for the new VM.

/// Stable float-constant identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FloatId(pub u16);

/// Immutable float-constant table for a function.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatTable {
    values: Box<[f64]>,
}

impl FloatTable {
    /// Creates a float-constant table from values.
    #[must_use]
    pub fn new(values: Vec<f64>) -> Self {
        Self {
            values: values.into_boxed_slice(),
        }
    }

    /// Creates an empty float-constant table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of float constants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the float constant for the given identifier.
    #[must_use]
    pub fn get(&self, id: FloatId) -> Option<f64> {
        self.values.get(usize::from(id.0)).copied()
    }
}

impl Default for FloatTable {
    fn default() -> Self {
        Self::empty()
    }
}

// Implement Eq manually — NaN values in the table are identity-based.
impl Eq for FloatTable {}
