//! Focused test hooks for internal VM unit tests.
//!
//! # Contents
//!
//! - Old-space object, array, WeakRef, and FinalizationRegistry constructors
//!   for raw mark/sweep regression fixtures that operate directly on a
//!   `GcHeap`.
//! - Read-only inspectors for invariant tests that need to assert internal
//!   rooted payloads without exposing those payloads through production APIs.
//!
//! # Invariants
//!
//! - Helpers in this module exist only for unit tests that exercise raw
//!   mark/sweep phases without an interpreter frame/root stack.
//! - Production paths must use stack, runtime, or native root contracts instead
//!   of these old-space fixture helpers.
//! - This module is hidden from public API docs and is not a contributor-facing
//!   extension surface.
//!
//! # See also
//!
//! - [`crate::weak_refs`]

use crate::execution_context::ExecutionContext;
use crate::native_function::{NativeCallTarget, NativeFunction};
use crate::object::JsObject;
use crate::promise::JsPromiseHandle;
use crate::weak_refs::{self, JsFinalizationRegistry, JsWeakRef};
use crate::{JsArray, Value, VmError};

/// Allocate an old-space object for raw mark/sweep regression fixtures.
pub fn alloc_old_object(heap: &mut otter_gc::GcHeap) -> Result<JsObject, otter_gc::OutOfMemory> {
    crate::object::alloc_object_old_for_fixture(heap)
}

/// Allocate an old-space array for raw mark/sweep regression fixtures.
pub fn alloc_old_array(heap: &mut otter_gc::GcHeap) -> Result<JsArray, otter_gc::OutOfMemory> {
    crate::array::alloc_array_old_for_fixture(heap)
}

/// Allocate an old-space array from elements for raw mark/sweep fixtures.
pub fn array_from_elements_old(
    heap: &mut otter_gc::GcHeap,
    values: impl IntoIterator<Item = Value>,
) -> Result<JsArray, otter_gc::OutOfMemory> {
    crate::array::from_elements_old_for_fixture(heap, values)
}

/// Allocate an old-space WeakRef for raw mark/sweep regression fixtures.
pub fn alloc_weak_ref(heap: &mut otter_gc::GcHeap, target: &Value) -> Result<JsWeakRef, VmError> {
    weak_refs::alloc_weak_ref_for_mark_sweep_fixture(heap, target)
}

/// Allocate an old-space FinalizationRegistry for raw mark/sweep regression
/// fixtures.
pub fn alloc_finalization_registry(
    heap: &mut otter_gc::GcHeap,
    cleanup_callback: Value,
) -> Result<JsFinalizationRegistry, VmError> {
    alloc_finalization_registry_with_context(heap, cleanup_callback, None)
}

/// Allocate an old-space FinalizationRegistry with cleanup context for raw
/// mark/sweep regression fixtures.
pub fn alloc_finalization_registry_with_context(
    heap: &mut otter_gc::GcHeap,
    cleanup_callback: Value,
    cleanup_context: Option<ExecutionContext>,
) -> Result<JsFinalizationRegistry, VmError> {
    weak_refs::alloc_finalization_registry_for_mark_sweep_fixture(
        heap,
        cleanup_callback,
        cleanup_context,
    )
}

pub fn promise_has_object_fulfill_capability(
    promise: JsPromiseHandle,
    heap: &otter_gc::GcHeap,
) -> bool {
    promise
        .debug_fulfill_reactions(heap)
        .iter()
        .any(|reaction| matches!(reaction.capability.promise, Value::Object(_)))
}

pub fn promise_fulfill_reaction_count(promise: JsPromiseHandle, heap: &otter_gc::GcHeap) -> usize {
    promise.debug_fulfill_reactions(heap).len()
}

pub fn promise_fulfill_reaction_debug(
    promise: JsPromiseHandle,
    heap: &otter_gc::GcHeap,
) -> Vec<String> {
    promise
        .debug_fulfill_reactions(heap)
        .iter()
        .map(|reaction| format!("{:?}", reaction.handler))
        .collect()
}

pub fn native_function_captures(native: NativeFunction, heap: &otter_gc::GcHeap) -> Vec<Value> {
    match native.call_target(heap) {
        NativeCallTarget::Dynamic { captures, .. }
        | NativeCallTarget::LocalDynamic { captures, .. } => captures.into_iter().collect(),
        NativeCallTarget::Static(_) | NativeCallTarget::VmIntrinsic(_) => Vec::new(),
    }
}
