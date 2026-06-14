//! ECMAScript `WeakRef` and `FinalizationRegistry` value bodies.
//!
//! Both types are backed by explicit `GcHeap` APIs. `WeakRef`
//! stores a weak raw handle and never traces it. A
//! `FinalizationRegistry` traces its cleanup callback and held
//! values strongly, while targets and unregister tokens remain
//! weak. Post-mark processing clears dead weak refs and returns
//! cleanup work for the interpreter to enqueue on the isolate-local
//! microtask queue.
//!
//! # Contents
//!
//! - [`JsWeakRef`] / [`WeakRefBody`] — weak target holder.
//! - [`JsFinalizationRegistry`] / [`FinalizationRegistryBody`] —
//!   cleanup callback plus registered cells.
//! - [`process_weak_refs_and_finalizers`] — VM-side post-mark hook.
//!
//! # Invariants
//!
//! - Weak targets and unregister tokens are never traced as strong
//!   edges.
//! - Cleanup callbacks are not invoked during GC; the interpreter
//!   enqueues [`crate::microtask::MicrotaskKind::FinalizationCallback`]
//!   jobs after the raw weak-processing pass.
//! - A finalized cell is removed before the callback is enqueued, so
//!   cleanup fires at most once per cell.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-weak-ref-objects>
//! - <https://tc39.es/ecma262/#sec-finalization-registry-objects>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)

use crate::Value;
use crate::abstract_ops::is_callable;
use crate::array::{ARRAY_BODY_TYPE_TAG, ArrayBody};
use crate::collections::{
    MAP_BODY_TYPE_TAG, MapBody, SET_BODY_TYPE_TAG, SetBody, WEAK_MAP_BODY_TYPE_TAG,
    WEAK_SET_BODY_TYPE_TAG, WeakMapBody, WeakSetBody,
};
use crate::execution_context::ExecutionContext;
use crate::object::{OBJECT_BODY_TYPE_TAG, ObjectBody};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`WeakRefBody`].
pub const WEAK_REF_BODY_TYPE_TAG: u8 = 0x17;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`FinalizationRegistryBody`].
pub const FINALIZATION_REGISTRY_BODY_TYPE_TAG: u8 = 0x18;

/// Heap-shared `WeakRef` handle.
pub type JsWeakRef = otter_gc::Gc<WeakRefBody>;

/// Heap-shared `FinalizationRegistry` handle.
pub type JsFinalizationRegistry = otter_gc::Gc<FinalizationRegistryBody>;

/// GC-allocated payload backing every [`JsWeakRef`].
///
/// The target is weak by spec and is cleared by
/// [`process_weak_refs_and_finalizers`] after marking, so the derive
/// skips it deliberately.
#[derive(Debug, Clone, otter_macros::Pelt)]
#[pelt(tag = WEAK_REF_BODY_TYPE_TAG)]
pub struct WeakRefBody {
    #[pelt(skip)]
    target: RawGc,
    prototype_override: Option<Value>,
}

/// One registered finalization cell.
///
/// `target` and `unregister_token` are weak — the GC never marks
/// through them; `held_value` is the strongly-traced cleanup argument
/// (§27.7.4 step 7). `FinalizerCell` is never registered as its own
/// GC body — it lives inline inside `FinalizationRegistryBody.cells`
/// — so it stays a plain Rust struct with a hand-written
/// `PeltField` impl that the surrounding `Vec<FinalizerCell>`
/// dispatches through.
#[derive(Debug, Clone)]
pub struct FinalizerCell {
    target: RawGc,
    held_value: Value,
    unregister_token: Option<RawGc>,
}

impl crate::pelt::PeltField for FinalizerCell {
    #[inline]
    fn pelt_trace(&self, visitor: &mut SlotVisitor<'_>) {
        // `target` + `unregister_token` are weak per §27.7.4.
        let _ = self.target;
        let _ = self.unregister_token;
        self.held_value.trace_value_slots(visitor);
    }
}

/// GC-allocated payload backing every [`JsFinalizationRegistry`].
#[derive(Debug, Clone, otter_macros::Pelt)]
#[pelt(tag = FINALIZATION_REGISTRY_BODY_TYPE_TAG)]
pub struct FinalizationRegistryBody {
    cleanup_callback: Value,
    #[pelt(skip)]
    cleanup_context: Option<ExecutionContext>,
    cells: Vec<FinalizerCell>,
    prototype_override: Option<Value>,
}

/// Cleanup work prepared during post-mark weak processing.
#[derive(Debug, Clone)]
pub struct FinalizationJob {
    /// Cleanup callback supplied to the registry constructor.
    pub cleanup_callback: Value,
    /// VM context that owns the cleanup callback.
    pub context: Option<ExecutionContext>,
    /// Held value passed as the sole cleanup callback argument.
    pub held_value: Value,
}

/// Allocate a fresh `WeakRef` while exposing caller-owned roots.
///
/// # Errors
///
/// Returns [`crate::VmError::TypeMismatch`] when `target` cannot be
/// held weakly by the currently migrated GC value model, or
/// [`crate::VmError::OutOfMemory`] if the heap allocation fails.
pub(crate) fn alloc_weak_ref_with_roots(
    heap: &mut otter_gc::GcHeap,
    target: &Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsWeakRef, crate::VmError> {
    weak_target_raw(target)?;
    let mut allocation_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        target.trace_value_slots(visitor);
    };
    let weak_ref = heap.alloc_with_roots(
        WeakRefBody {
            target: RawGc::NULL,
            prototype_override: None,
        },
        &mut allocation_roots,
    )?;
    let target = weak_target_raw(target)?;
    heap.with_payload(weak_ref, |body| {
        body.target = target;
    });
    heap.register_weak_ref(weak_ref);
    Ok(weak_ref)
}

#[cfg(test)]
pub(crate) fn alloc_weak_ref_for_mark_sweep_fixture(
    heap: &mut otter_gc::GcHeap,
    target: &Value,
) -> Result<JsWeakRef, crate::VmError> {
    let target = weak_target_raw(target)?;
    let weak_ref = heap.alloc_old(WeakRefBody {
        target,
        prototype_override: None,
    })?;
    heap.register_weak_ref(weak_ref);
    Ok(weak_ref)
}

pub(crate) fn weak_ref_prototype_override(
    weak_ref: JsWeakRef,
    heap: &otter_gc::GcHeap,
) -> Option<Value> {
    heap.read_payload(weak_ref, |body| body.prototype_override)
}

pub(crate) fn set_weak_ref_prototype_override(
    weak_ref: JsWeakRef,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto;
    heap.with_payload(weak_ref, |body| {
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(weak_ref, value);
    }
}

/// Return the target while live, otherwise `undefined`.
#[must_use]
pub fn weak_ref_deref(weak_ref: JsWeakRef, heap: &otter_gc::GcHeap) -> Value {
    let target = heap.read_payload(weak_ref, |body| body.target);
    if target.is_null() {
        return Value::undefined();
    }
    raw_to_value(heap, target).unwrap_or(Value::undefined())
}

/// Allocate a fresh `FinalizationRegistry` while exposing caller-owned roots.
///
/// `cleanup_callback` is traced explicitly across allocation because it is not
/// reachable from the heap until the registry body has been installed.
pub(crate) fn alloc_finalization_registry_with_context_and_roots(
    heap: &mut otter_gc::GcHeap,
    cleanup_callback: Value,
    cleanup_context: Option<ExecutionContext>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsFinalizationRegistry, crate::VmError> {
    if !is_callable(&cleanup_callback) {
        return Err(crate::VmError::NotCallable);
    }
    let cleanup_callback_root = cleanup_callback;
    let mut allocation_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        cleanup_callback_root.trace_value_slots(visitor);
    };
    let registry = heap.alloc_with_roots(
        FinalizationRegistryBody {
            cleanup_callback,
            cleanup_context,
            cells: Vec::new(),
            prototype_override: None,
        },
        &mut allocation_roots,
    )?;
    heap.register_finalization_registry(registry);
    Ok(registry)
}

#[cfg(test)]
pub(crate) fn alloc_finalization_registry_for_mark_sweep_fixture(
    heap: &mut otter_gc::GcHeap,
    cleanup_callback: Value,
    cleanup_context: Option<ExecutionContext>,
) -> Result<JsFinalizationRegistry, crate::VmError> {
    if !is_callable(&cleanup_callback) {
        return Err(crate::VmError::NotCallable);
    }
    let barrier_cleanup_callback = cleanup_callback;
    let registry = heap.alloc_old(FinalizationRegistryBody {
        cleanup_callback,
        cleanup_context,
        cells: Vec::new(),
        prototype_override: None,
    })?;
    heap.register_finalization_registry(registry);
    heap.record_write(registry, &barrier_cleanup_callback);
    Ok(registry)
}

pub(crate) fn finalization_registry_prototype_override(
    registry: JsFinalizationRegistry,
    heap: &otter_gc::GcHeap,
) -> Option<Value> {
    heap.read_payload(registry, |body| body.prototype_override)
}

pub(crate) fn set_finalization_registry_prototype_override(
    registry: JsFinalizationRegistry,
    heap: &mut otter_gc::GcHeap,
    proto: Option<Value>,
) {
    let barrier_value = proto;
    heap.with_payload(registry, |body| {
        body.prototype_override = proto;
    });
    if let Some(value) = &barrier_value {
        heap.record_write(registry, value);
    }
}

/// Register a target/held-value pair.
pub fn finalization_registry_register(
    registry: JsFinalizationRegistry,
    heap: &mut otter_gc::GcHeap,
    target: &Value,
    held_value: Value,
    unregister_token: Option<&Value>,
) -> Result<(), crate::VmError> {
    let target_raw = weak_target_raw(target)?;
    if held_value.as_raw_gc() == Some(target_raw) {
        return Err(crate::VmError::TypeMismatch);
    }
    let barrier_held_value = held_value;
    let unregister_token = match unregister_token {
        None => None,
        Some(v) if v.is_undefined() => None,
        Some(value) => Some(weak_target_raw(value)?),
    };
    heap.with_payload(registry, |body| {
        body.cells.push(FinalizerCell {
            target: target_raw,
            held_value,
            unregister_token,
        });
    });
    // `held_value` is stored strongly in an old-space body. Record
    // the store after mutation when it carries a GC handle.
    heap.record_write(registry, &barrier_held_value);
    Ok(())
}

/// Remove cells matching `unregister_token`.
pub fn finalization_registry_unregister(
    registry: JsFinalizationRegistry,
    heap: &mut otter_gc::GcHeap,
    unregister_token: &Value,
) -> Result<bool, crate::VmError> {
    let token = weak_target_raw(unregister_token)?;
    let before = heap.read_payload(registry, |body| body.cells.len());
    heap.with_payload(registry, |body| {
        body.cells
            .retain(|cell| cell.unregister_token != Some(token));
    });
    let after = heap.read_payload(registry, |body| body.cells.len());
    Ok(after != before)
}

/// Number of registered cells, exposed for focused GC tests.
#[must_use]
pub fn finalization_registry_cell_count(
    registry: JsFinalizationRegistry,
    heap: &otter_gc::GcHeap,
) -> usize {
    heap.read_payload(registry, |body| body.cells.len())
}

/// Process weak references and finalizer cells after ordinary mark
/// and ephemeron fixpoint, before raw heap sweep.
#[must_use]
pub fn process_weak_refs_and_finalizers(heap: &mut otter_gc::GcHeap) -> Vec<FinalizationJob> {
    if heap.weak_finalization_registry_is_empty() {
        return Vec::new();
    }

    for raw in heap.weak_refs_snapshot() {
        if !heap.is_marked(raw) || heap.raw_type_tag(raw) != Some(WEAK_REF_BODY_TYPE_TAG) {
            continue;
        }
        let Some(weak_ref) = heap.cast_raw_if_type::<WeakRefBody>(raw) else {
            continue;
        };
        let target = heap.read_payload(weak_ref, |body| body.target);
        if !target.is_null() && !heap.is_marked(target) {
            heap.with_payload(weak_ref, |body| {
                body.target = RawGc::NULL;
            });
        }
    }

    if !heap.has_finalization_registries() {
        return Vec::new();
    }

    let mut jobs = Vec::new();
    for raw in heap.finalization_registries_snapshot() {
        if !heap.is_marked(raw)
            || heap.raw_type_tag(raw) != Some(FINALIZATION_REGISTRY_BODY_TYPE_TAG)
        {
            continue;
        }
        let Some(registry) = heap.cast_raw_if_type::<FinalizationRegistryBody>(raw) else {
            continue;
        };
        let dead_targets: std::collections::HashSet<_> = heap.read_payload(registry, |body| {
            body.cells
                .iter()
                .map(|cell| cell.target)
                .filter(|target| !target.is_null() && !heap.is_marked(*target))
                .collect()
        });
        if dead_targets.is_empty() {
            continue;
        }
        heap.with_payload(registry, |body| {
            let cleanup_callback = body.cleanup_callback;
            let cleanup_context = body.cleanup_context.clone();
            let mut retained = Vec::with_capacity(body.cells.len());
            for cell in body.cells.drain(..) {
                if dead_targets.contains(&cell.target) {
                    jobs.push(FinalizationJob {
                        cleanup_callback,
                        context: cleanup_context.clone(),
                        held_value: cell.held_value,
                    });
                } else {
                    retained.push(cell);
                }
            }
            body.cells = retained;
        });
    }
    jobs
}

fn weak_target_raw(value: &Value) -> Result<RawGc, crate::VmError> {
    value
        .as_raw_gc()
        .ok_or_else(|| crate::VmError::TypeMismatch)
}

fn raw_to_value(heap: &otter_gc::GcHeap, raw: RawGc) -> Option<Value> {
    match heap.raw_type_tag(raw)? {
        OBJECT_BODY_TYPE_TAG => heap.cast_raw_if_type::<ObjectBody>(raw).map(Value::object),
        ARRAY_BODY_TYPE_TAG => heap.cast_raw_if_type::<ArrayBody>(raw).map(Value::array),
        MAP_BODY_TYPE_TAG => heap.cast_raw_if_type::<MapBody>(raw).map(Value::map),
        SET_BODY_TYPE_TAG => heap.cast_raw_if_type::<SetBody>(raw).map(Value::set),
        WEAK_MAP_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<WeakMapBody>(raw)
            .map(Value::weak_map),
        WEAK_SET_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<WeakSetBody>(raw)
            .map(Value::weak_set),
        WEAK_REF_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<WeakRefBody>(raw)
            .map(Value::weak_ref),
        FINALIZATION_REGISTRY_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<FinalizationRegistryBody>(raw)
            .map(Value::finalization_registry),
        crate::symbol::SYMBOL_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<crate::symbol::SymbolBody>(raw)
            .map(|h| Value::symbol(crate::symbol::JsSymbol::from_handle(heap, h))),
        _ => None,
    }
}

/// Whether `name` is installed on `WeakRef.prototype`.
#[must_use]
pub fn is_weak_ref_builtin_method(name: &str) -> bool {
    matches!(name, "deref")
}

/// Whether `name` is installed on `FinalizationRegistry.prototype`.
#[must_use]
pub fn is_finalization_registry_builtin_method(name: &str) -> bool {
    matches!(name, "register" | "unregister")
}
