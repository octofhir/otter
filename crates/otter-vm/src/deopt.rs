//! Deoptimization metadata for the new VM.

use crate::bytecode::ProgramCounter;

/// Stable identifier of a deoptimization site in compiled code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptId(pub u32);

/// Static metadata for a single deoptimization site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptSite {
    id: DeoptId,
    pc: ProgramCounter,
}

impl DeoptSite {
    /// Creates a deoptimization site.
    #[must_use]
    pub const fn new(id: DeoptId, pc: ProgramCounter) -> Self {
        Self { id, pc }
    }

    /// Returns the site identifier.
    #[must_use]
    pub const fn id(self) -> DeoptId {
        self.id
    }

    /// Returns the program counter that owns the deopt site.
    #[must_use]
    pub const fn pc(self) -> ProgramCounter {
        self.pc
    }
}

/// Immutable table of deoptimization sites for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeoptTable {
    sites: Box<[DeoptSite]>,
}

impl DeoptTable {
    /// Creates a deoptimization table from owned site metadata.
    #[must_use]
    pub fn new(sites: Vec<DeoptSite>) -> Self {
        Self {
            sites: sites.into_boxed_slice(),
        }
    }

    /// Creates an empty deoptimization table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of sites in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// Returns the immutable site slice.
    #[must_use]
    pub fn sites(&self) -> &[DeoptSite] {
        &self.sites
    }
}

impl Default for DeoptTable {
    fn default() -> Self {
        Self::empty()
    }
}
