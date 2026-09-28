//! Non-moving single-value cells: global declarative-record bindings and
//! engine-internal shared flags.
//!
//! An [`UpvalueCell`] is one GC-allocated `Value` slot in old space whose
//! address never changes. Script-level `let` / `const` / `class` bindings of
//! the global Environment Record live in these cells, so a load IC and
//! generated code can embed the cell offset permanently; the Promise
//! combinators share one "already called" flag cell between resolving
//! functions. Function and block scopes do not use cells: their captured
//! bindings live in context slots ([`crate::context`]).
//!
//! # Contents
//! - [`UpvalueCellBody`] — GC-allocated payload (one [`crate::Value`]).
//! - [`UpvalueCell`] — `Copy` 4-byte handle (`Gc<UpvalueCellBody>`).
//! - [`alloc_upvalue`] / [`read_upvalue`] / [`store_upvalue`] —
//!   write-barrier-aware mutation helpers.
//!
//! # Invariants
//! - Cells are allocated in old space and never move.
//! - Writes flow through [`store_upvalue`] so the generational write
//!   barrier records any new old-to-young reference.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-global-environment-records>
//! - [`crate::context`] — per-scope binding storage.

use otter_macros::Pelt;

use crate::Value;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`UpvalueCellBody`].
pub const UPVALUE_CELL_TYPE_TAG: u8 = 0x10;

/// GC-allocated payload backing every [`UpvalueCell`] handle.
///
/// Holds a single `Value`. Mutation flows through [`store_upvalue`]; reads
/// through [`read_upvalue`]; allocation through [`alloc_upvalue`].
///
/// # Layout
///
/// One `Value` field. The global declarative record and its load caches
/// store four-byte compressed handles to these cells; the cell is the one
/// authoritative location of the binding.
#[derive(Clone, Copy, Pelt)]
#[pelt(tag = UPVALUE_CELL_TYPE_TAG)]
pub struct UpvalueCellBody {
    /// The bound `Value`. Stores fire the generational write barrier
    /// through [`store_upvalue`] for every RHS that carries a GC
    /// handle.
    pub value: Value,
}

impl UpvalueCellBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.value, visitor);
    }
}

/// Compressed handle to an [`UpvalueCellBody`]. `Copy + Eq + Hash`
/// (inherited from [`otter_gc::Gc`]); identity comparison via
/// `cell == other`.
pub type UpvalueCell = otter_gc::Gc<UpvalueCellBody>;

/// Allocate a fresh [`UpvalueCell`] pre-populated with `value` on
/// the GC heap.
///
/// Cells are allocated directly in old space: permanent global lexical
/// proofs rely on each cell retaining a stable value address.
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

/// [`alloc_upvalue`] with caller-owned roots exposed to any collection the
/// allocation triggers (heap-cap emergency full GC). Use when the caller
/// holds young handles in plain Rust locals across this call.
pub fn alloc_upvalue_with_roots(
    heap: &mut otter_gc::GcHeap,
    value: Value,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<UpvalueCell, otter_gc::OutOfMemory> {
    heap.alloc_old_with_roots(UpvalueCellBody { value }, external_visit)
}

/// Read the value of `cell`.
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
