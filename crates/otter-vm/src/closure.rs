//! Closure creation metadata for the new VM.

use crate::bytecode::ProgramCounter;
use crate::frame::RegisterIndex;
use crate::module::FunctionIndex;
use crate::object::ClosureFlags;

/// Stable upvalue identifier inside a closure object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UpvalueId(pub u16);

/// Closure-creation metadata attached to a bytecode PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClosureTemplate {
    callee: FunctionIndex,
    capture_count: RegisterIndex,
    flags: ClosureFlags,
}

impl ClosureTemplate {
    /// Creates metadata for one closure-creation site.
    #[must_use]
    pub const fn new(callee: FunctionIndex, capture_count: RegisterIndex) -> Self {
        Self {
            callee,
            capture_count,
            flags: ClosureFlags::normal(),
        }
    }

    /// Creates metadata for one closure-creation site with explicit flags.
    #[must_use]
    pub const fn with_flags(
        callee: FunctionIndex,
        capture_count: RegisterIndex,
        flags: ClosureFlags,
    ) -> Self {
        Self {
            callee,
            capture_count,
            flags,
        }
    }

    /// Returns the closure callee function index.
    #[must_use]
    pub const fn callee(self) -> FunctionIndex {
        self.callee
    }

    /// Returns the number of captured slots to copy from the capture window.
    #[must_use]
    pub const fn capture_count(self) -> RegisterIndex {
        self.capture_count
    }

    /// Returns the closure function kind flags.
    #[must_use]
    pub const fn flags(self) -> ClosureFlags {
        self.flags
    }
}

/// Immutable closure-creation table for one function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureTable {
    templates: Box<[Option<ClosureTemplate>]>,
}

impl ClosureTable {
    /// Creates a closure-creation table indexed by bytecode PC.
    #[must_use]
    pub fn new(templates: Vec<Option<ClosureTemplate>>) -> Self {
        Self {
            templates: templates.into_boxed_slice(),
        }
    }

    /// Creates an empty closure-creation table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of stored closure templates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    /// Returns `true` when the table has no templates.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Returns the closure template for the given bytecode PC.
    #[must_use]
    pub fn get(&self, pc: ProgramCounter) -> Option<ClosureTemplate> {
        self.templates.get(pc as usize).copied().flatten()
    }
}

impl Default for ClosureTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClosureTable, ClosureTemplate};

    #[test]
    fn closure_table_resolves_templates_by_pc() {
        let template = ClosureTemplate::new(crate::module::FunctionIndex(2), 1);
        let table = ClosureTable::new(vec![None, Some(template)]);

        assert_eq!(table.get(0), None);
        assert_eq!(table.get(1), Some(template));
        assert_eq!(table.get(2), None);
    }
}
