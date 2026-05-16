//! Focused test hooks for VM integration tests.
//!
//! # Contents
//!
//! - Old-space WeakRef and FinalizationRegistry constructors for raw
//!   mark/sweep regression fixtures that operate directly on a `GcHeap`.
//! - Read-only inspectors for integration tests that need to assert internal
//!   rooted payloads without exposing those payloads through production APIs.
//!
//! # Invariants
//!
//! - Helpers in this module exist only for integration tests that exercise raw
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
use crate::promise::{JsPromiseHandle, PromiseReactionHandler};
use crate::weak_refs::{self, JsFinalizationRegistry, JsWeakRef};
use crate::{Value, VmError};

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

pub fn promise_has_parked_object_register(
    promise: JsPromiseHandle,
    heap: &otter_gc::GcHeap,
    register: usize,
) -> bool {
    promise
        .debug_fulfill_reactions(heap)
        .iter()
        .any(|reaction| match &reaction.handler {
            PromiseReactionHandler::AsyncResume { parked, .. }
            | PromiseReactionHandler::AsyncGenResume { parked, .. } => {
                crate::generator::parked_frame_register_is_object(*parked, heap, register)
            }
            PromiseReactionHandler::Call(_) => false,
        })
}

pub fn promise_has_parked_fulfill_reaction(
    promise: JsPromiseHandle,
    heap: &otter_gc::GcHeap,
) -> bool {
    promise
        .debug_fulfill_reactions(heap)
        .iter()
        .any(|reaction| {
            matches!(
                &reaction.handler,
                PromiseReactionHandler::AsyncResume { .. }
                    | PromiseReactionHandler::AsyncGenResume { .. }
            )
        })
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
