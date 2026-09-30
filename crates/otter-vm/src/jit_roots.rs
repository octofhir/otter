//! Lifetimes of heap assumptions retained by compiled code.
//!
//! # Contents
//! - [`CompilationRoot`] retains a pinned shape or a prototype validity cell.
//!
//! # Invariants
//! - Compile sessions retain every embedded reference until installation.
//! - Installed generations retain roots through invalidation until physical
//!   retirement, including while an active frame can return into their code.
//! - Shapes are pinned GC cells; validity cells are address-stable Rust
//!   allocations containing no GC references.
//!
//! # See also
//! - [`crate::jit_registry`] for installed generations.
//! - [`crate::object::prototype_validity`] for mutation dependencies.

use std::sync::Arc;

use crate::object::{ShapeHandle, prototype_validity::PrototypeValidity};

#[derive(Debug, Clone)]
pub(crate) enum CompilationRoot {
    Shape(ShapeHandle),
    Prototype(Arc<PrototypeValidity>),
}

impl CompilationRoot {
    pub(crate) fn key(&self) -> (u8, usize) {
        match self {
            Self::Shape(shape) => (0, shape.offset() as usize),
            Self::Prototype(cell) => (1, cell.address()),
        }
    }

    pub(crate) fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if let Self::Shape(shape) = self {
            // Shapes never move, so the tracer never rewrites this slot.
            visitor(std::ptr::from_ref(shape).cast_mut().cast());
        }
    }
}
