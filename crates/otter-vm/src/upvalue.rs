//! Closure upvalue cells — captured-binding storage for closures.
//!
//! Each `function* () { … }` / `() => …` body that reads or writes a
//! variable from an enclosing scope captures one [`UpvalueCell`] per
//! distinct binding. Cloning the wrapper shares the same heap slot so
//! every closure plus the outer scope observe each other's writes.
//!
//! # Contents
//! - [`UpvalueCellBody`] — GC-allocated payload (one [`crate::Value`]).
//! - [`UpvalueCell`] — `Copy` 4-byte handle (`Gc<UpvalueCellBody>`).
//! - [`alloc_upvalue`] / [`read_upvalue`] / [`store_upvalue`] —
//!   write-barrier-aware mutation helpers.
//!
//! # Invariants
//! - Writes flow through [`store_upvalue`] so the generational write
//!   barrier records any new old-to-young reference.
//! - The body holds exactly one `Value` field; iteration order matches
//!   spec [[Binding]] semantics by virtue of the binding map built at
//!   `MakeClosure` time (see [`crate::Op::MakeClosure`]).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-newdeclarativeenvironment>
//! - <https://tc39.es/ecma262/#sec-function-environment-records>

use otter_macros::Pelt;

use crate::Value;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`UpvalueCellBody`].
pub const UPVALUE_CELL_TYPE_TAG: u8 = 0x10;

/// GC-allocated payload backing every [`UpvalueCell`] handle.
///
/// Holds a single captured `Value`. Mutation flows through
/// [`store_upvalue`]; reads through [`read_upvalue`]; allocation
/// through [`alloc_upvalue`].
///
/// # Layout
///
/// One `Value` field. After task 76 the body is the only place the
/// captured value lives — every closure handle stores a
/// `Gc<UpvalueCellBody>` (4-byte compressed offset) instead of the
/// previous ref-counted mutable cell (8-byte pointer + allocation
/// overhead).
///
/// # Spec
///
/// Captured-binding semantics — ECMA-262 §9.1.1.1.4
/// (CreateMutableBinding) + §9.1.1.1.5 (InitializeBinding); the
/// closure spine that holds these cells is built by `Op::MakeClosure`
/// per §15.2.5 (FunctionDeclarationInstantiation).
#[derive(Pelt)]
#[pelt(tag = UPVALUE_CELL_TYPE_TAG)]
pub struct UpvalueCellBody {
    /// Captured `Value`. Stores fire the generational write barrier
    /// through [`store_upvalue`] for every RHS that carries a GC
    /// handle.
    pub value: Value,
}

/// Compressed handle to an [`UpvalueCellBody`]. `Copy + Eq + Hash`
/// (inherited from [`otter_gc::Gc`]); identity comparison via
/// `cell == other`.
pub type UpvalueCell = otter_gc::Gc<UpvalueCellBody>;

/// Allocate a fresh [`UpvalueCell`] pre-populated with `value` on
/// the GC heap.
///
/// Routes through [`otter_gc::GcHeap::alloc_old`] so the body is
/// allocated directly in old-space — Phase-1 closure spines
/// (`Rc<[UpvalueCell]>`) cannot yet be rewritten by the scavenger,
/// and old-space objects do not move.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim; runtime callers
/// translate it into [`crate::VmError::OutOfMemory`].
pub fn alloc_upvalue(
    heap: &mut otter_gc::GcHeap,
    value: Value,
) -> Result<UpvalueCell, otter_gc::OutOfMemory> {
    heap.alloc_old(UpvalueCellBody { value })
}

/// Read the captured value of `cell`.
#[must_use]
pub fn read_upvalue(heap: &otter_gc::GcHeap, cell: UpvalueCell) -> Value {
    heap.read_payload(cell, |body| body.value)
}

/// Write `value` into `cell`, firing the generational write barrier
/// so the scavenger sees any newly-established old → young pointer.
pub fn store_upvalue(heap: &mut otter_gc::GcHeap, cell: UpvalueCell, value: Value) {
    let barrier_value = value;
    heap.with_payload(cell, |body| {
        body.value = value;
    });
    heap.record_write(cell, &barrier_value);
}
