//! Lifetimes of heap assumptions retained by compiled code.
//!
//! # Contents
//! - [`CompilationRoot`] retains a pinned shape, a prototype validity cell or
//!   a callee identity cell.
//! - [`CalleeIdentityCell`] caches the callee a generated call site proved.
//!
//! # Invariants
//! - Compile sessions retain every embedded reference until installation.
//! - Installed generations retain roots through invalidation until physical
//!   retirement, including while an active frame can return into their code.
//! - Shapes are pinned GC cells; validity cells are address-stable Rust
//!   allocations containing no GC references.
//! - A callee identity cell is an address-stable strong root the collector
//!   rewrites in place. Generated code writes it without a barrier: roots are
//!   rescanned when a marking cycle finishes.
//!
//! # See also
//! - [`crate::jit_registry`] for installed generations.
//! - [`crate::object::prototype_validity`] for mutation dependencies.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{ShapeHandle, prototype_validity::PrototypeValidity};

/// The last callee value one generated call site proved to be its baked
/// target. A later call with the identical value skips the full proof; any
/// other value takes the proof and, on success, replaces the cached one.
#[derive(Debug)]
pub(crate) struct CalleeIdentityCell(AtomicU64);

impl CalleeIdentityCell {
    /// Bits no `Value` encoding produces: an empty cell matches nothing.
    pub(crate) const EMPTY: u64 = u64::MAX;

    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(Self::EMPTY))
    }

    /// Address generated code reads and writes.
    pub(crate) fn address(&self) -> u64 {
        self.0.as_ptr() as u64
    }

    fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if crate::value::tag::is_cell_bits(self.0.load(Ordering::Relaxed)) {
            visitor(self.0.as_ptr().cast());
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CompilationRoot {
    Shape(ShapeHandle),
    Prototype(Arc<PrototypeValidity>),
    CalleeIdentity(Arc<CalleeIdentityCell>),
}

impl CompilationRoot {
    pub(crate) fn key(&self) -> (u8, usize) {
        match self {
            Self::Shape(shape) => (0, shape.offset() as usize),
            Self::Prototype(cell) => (1, cell.address()),
            Self::CalleeIdentity(cell) => (2, cell.address() as usize),
        }
    }

    pub(crate) fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        match self {
            // Shapes never move, so the tracer never rewrites this slot.
            Self::Shape(shape) => visitor(std::ptr::from_ref(shape).cast_mut().cast()),
            Self::Prototype(_) => {}
            Self::CalleeIdentity(cell) => cell.trace(visitor),
        }
    }
}
