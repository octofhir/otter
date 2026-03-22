//! Interpreter entry points for the new VM.

use crate::module::Module;

/// Minimal interpreter shell for the new VM backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Interpreter;

impl Interpreter {
    /// Creates a new interpreter instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Executes a module.
    ///
    /// This is currently a scaffold entry point and does not yet interpret bytecode.
    pub fn execute(&self, _module: &Module) {}
}
