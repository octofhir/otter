//! Lifetimes of heap assumptions retained by compiled code.
//!
//! # Contents
//! - [`CompilationRoot`] retains a pinned shape, a prototype validity cell, a
//!   callee identity cell or an `instanceof` cell.
//! - [`CalleeIdentityCell`] caches the callee a generated call site proved.
//! - [`InstanceofCell`] caches the target an `instanceof` site proved, with
//!   the prototype OrdinaryHasInstance searches for.
//!
//! # Invariants
//! - Compile sessions retain every embedded reference until installation.
//! - Installed generations retain roots through invalidation until physical
//!   retirement, including while an active frame can return into their code.
//! - Shapes are pinned GC cells; validity cells are address-stable Rust
//!   allocations containing no GC references.
//! - A callee identity cell is an address-stable strong root the collector
//!   rewrites in place. Generated code writes it without a barrier: roots are
//!   rescanned when a marking cycle finishes. An `instanceof` cell is the
//!   same kind of root; the VM empties every one whenever a closure that
//!   ever filled one changes its `prototype`, own properties or
//!   `[[Prototype]]` (see `Interpreter::retire_instanceof_proofs`).
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

/// The last `instanceof` target one generated site proved to answer through
/// the default `@@hasInstance`: an ordinary closure without own properties
/// or a `[[Prototype]]` override, whose `prototype` held an ordinary object,
/// with that object. A later test against the identical target reads the
/// cached prototype; the VM empties the cell before any change that could
/// alter that answer becomes observable.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct InstanceofCell {
    target: AtomicU64,
    prototype: AtomicU64,
}

const _: () = assert!(
    std::mem::offset_of!(InstanceofCell, target)
        == crate::jit::JIT_INSTANCEOF_CELL_TARGET_OFFSET as usize
);
const _: () = assert!(
    std::mem::offset_of!(InstanceofCell, prototype)
        == crate::jit::JIT_INSTANCEOF_CELL_PROTOTYPE_OFFSET as usize
);

impl InstanceofCell {
    pub(crate) fn new() -> Self {
        Self {
            target: AtomicU64::new(CalleeIdentityCell::EMPTY),
            prototype: AtomicU64::new(CalleeIdentityCell::EMPTY),
        }
    }

    /// Address generated code reads and writes.
    pub(crate) fn address(&self) -> u64 {
        std::ptr::from_ref(self) as u64
    }

    /// Forget the cached pair: the next test re-proves its target.
    pub(crate) fn clear(&self) {
        self.target
            .store(CalleeIdentityCell::EMPTY, Ordering::Relaxed);
        self.prototype
            .store(CalleeIdentityCell::EMPTY, Ordering::Relaxed);
    }

    fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for word in [&self.target, &self.prototype] {
            if crate::value::tag::is_cell_bits(word.load(Ordering::Relaxed)) {
                visitor(word.as_ptr().cast());
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CompilationRoot {
    Shape(ShapeHandle),
    Prototype(Arc<PrototypeValidity>),
    CalleeIdentity(Arc<CalleeIdentityCell>),
    Instanceof(Arc<InstanceofCell>),
}

impl CompilationRoot {
    pub(crate) fn key(&self) -> (u8, usize) {
        match self {
            Self::Shape(shape) => (0, shape.offset() as usize),
            Self::Prototype(cell) => (1, cell.address()),
            Self::CalleeIdentity(cell) => (2, cell.address() as usize),
            Self::Instanceof(cell) => (3, cell.address() as usize),
        }
    }

    pub(crate) fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        match self {
            // Shapes never move, so the tracer never rewrites this slot.
            Self::Shape(shape) => visitor(std::ptr::from_ref(shape).cast_mut().cast()),
            Self::Prototype(_) => {}
            Self::CalleeIdentity(cell) => cell.trace(visitor),
            Self::Instanceof(cell) => cell.trace(visitor),
        }
    }
}
