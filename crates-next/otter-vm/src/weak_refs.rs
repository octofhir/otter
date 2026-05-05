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
//! - [GC architecture §6.2](../../../docs/new-engine/gc-architecture.md)

use crate::Value;
use crate::abstract_ops::is_callable;
use crate::array::{ARRAY_BODY_TYPE_TAG, ArrayBody};
use crate::collections::{
    MAP_BODY_TYPE_TAG, MapBody, SET_BODY_TYPE_TAG, SetBody, WEAK_MAP_BODY_TYPE_TAG,
    WEAK_SET_BODY_TYPE_TAG, WeakMapBody, WeakSetBody,
};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::object::{OBJECT_BODY_TYPE_TAG, ObjectBody};

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
#[derive(Debug, Clone, Copy)]
pub struct WeakRefBody {
    target: otter_gc::RawGc,
}

impl otter_gc::SafeTraceable for WeakRefBody {
    const TYPE_TAG: u8 = WEAK_REF_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut otter_gc::SlotVisitor<'_>) {
        // The target is weak by spec and is cleared by
        // `process_weak_refs_and_finalizers` after marking.
    }
}

/// One registered finalization cell.
#[derive(Debug, Clone)]
pub struct FinalizerCell {
    target: otter_gc::RawGc,
    held_value: Value,
    unregister_token: Option<otter_gc::RawGc>,
}

/// GC-allocated payload backing every [`JsFinalizationRegistry`].
#[derive(Debug, Clone)]
pub struct FinalizationRegistryBody {
    cleanup_callback: Value,
    cells: Vec<FinalizerCell>,
}

impl otter_gc::SafeTraceable for FinalizationRegistryBody {
    const TYPE_TAG: u8 = FINALIZATION_REGISTRY_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        self.cleanup_callback.trace_value_slots(visitor);
        for cell in &self.cells {
            cell.held_value.trace_value_slots(visitor);
        }
    }
}

/// Cleanup work prepared during post-mark weak processing.
#[derive(Debug, Clone)]
pub struct FinalizationJob {
    /// Cleanup callback supplied to the registry constructor.
    pub cleanup_callback: Value,
    /// Held value passed as the sole cleanup callback argument.
    pub held_value: Value,
}

/// Allocate a fresh `WeakRef`.
///
/// # Errors
///
/// Returns [`crate::VmError::TypeMismatch`] when `target` cannot be
/// held weakly by the currently migrated GC value model, or
/// [`crate::VmError::OutOfMemory`] if the heap allocation fails.
pub fn alloc_weak_ref(
    heap: &mut otter_gc::GcHeap,
    target: &Value,
) -> Result<JsWeakRef, crate::VmError> {
    let target = weak_target_raw(target)?;
    let weak_ref = heap.alloc_old(WeakRefBody { target })?;
    heap.register_weak_ref(weak_ref);
    Ok(weak_ref)
}

/// Return the target while live, otherwise `undefined`.
#[must_use]
pub fn weak_ref_deref(weak_ref: JsWeakRef, heap: &otter_gc::GcHeap) -> Value {
    let target = heap.read_payload(weak_ref, |body| body.target);
    if target.is_null() {
        return Value::Undefined;
    }
    raw_to_value(heap, target).unwrap_or(Value::Undefined)
}

/// Allocate a fresh `FinalizationRegistry`.
pub fn alloc_finalization_registry(
    heap: &mut otter_gc::GcHeap,
    cleanup_callback: Value,
) -> Result<JsFinalizationRegistry, crate::VmError> {
    if !is_callable(&cleanup_callback) {
        return Err(crate::VmError::NotCallable);
    }
    let registry = heap.alloc_old(FinalizationRegistryBody {
        cleanup_callback,
        cells: Vec::new(),
    })?;
    heap.register_finalization_registry(registry);
    Ok(registry)
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
    if held_value.as_gc_raw() == Some(target_raw) {
        return Err(crate::VmError::TypeMismatch);
    }
    let unregister_token = match unregister_token {
        Some(Value::Undefined) | None => None,
        Some(value) => Some(weak_target_raw(value)?),
    };
    heap.with_payload(registry, |body| {
        body.cells.push(FinalizerCell {
            target: target_raw,
            held_value,
            unregister_token,
        });
    });
    // `held_value` is stored strongly in an old-space body. Fire the
    // barrier after the store when it carries a GC handle.
    if let Some(child) = heap.read_payload(registry, |body| {
        body.cells
            .last()
            .and_then(|cell| cell.held_value.as_gc_raw())
    }) {
        let slot = finalization_registry_payload_slot(registry);
        heap.write_barrier_raw(registry, slot, child);
    }
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
                body.target = otter_gc::RawGc::NULL;
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
            let cleanup_callback = body.cleanup_callback.clone();
            let mut retained = Vec::with_capacity(body.cells.len());
            for cell in body.cells.drain(..) {
                if dead_targets.contains(&cell.target) {
                    jobs.push(FinalizationJob {
                        cleanup_callback: cleanup_callback.clone(),
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

fn weak_target_raw(value: &Value) -> Result<otter_gc::RawGc, crate::VmError> {
    value.as_gc_raw().ok_or(crate::VmError::TypeMismatch)
}

fn raw_to_value(heap: &otter_gc::GcHeap, raw: otter_gc::RawGc) -> Option<Value> {
    match heap.raw_type_tag(raw)? {
        OBJECT_BODY_TYPE_TAG => heap.cast_raw_if_type::<ObjectBody>(raw).map(Value::Object),
        ARRAY_BODY_TYPE_TAG => heap.cast_raw_if_type::<ArrayBody>(raw).map(Value::Array),
        MAP_BODY_TYPE_TAG => heap.cast_raw_if_type::<MapBody>(raw).map(Value::Map),
        SET_BODY_TYPE_TAG => heap.cast_raw_if_type::<SetBody>(raw).map(Value::Set),
        WEAK_MAP_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<WeakMapBody>(raw)
            .map(Value::WeakMap),
        WEAK_SET_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<WeakSetBody>(raw)
            .map(Value::WeakSet),
        WEAK_REF_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<WeakRefBody>(raw)
            .map(Value::WeakRef),
        FINALIZATION_REGISTRY_BODY_TYPE_TAG => heap
            .cast_raw_if_type::<FinalizationRegistryBody>(raw)
            .map(Value::FinalizationRegistry),
        _ => None,
    }
}

fn finalization_registry_payload_slot(registry: JsFinalizationRegistry) -> *mut otter_gc::RawGc {
    (registry.as_header_ptr() as *mut u8).wrapping_add(std::mem::size_of::<otter_gc::GcHeader>())
        as *mut otter_gc::RawGc
}

fn receiver_weak_ref(args: &IntrinsicArgs<'_>) -> Result<JsWeakRef, IntrinsicError> {
    match args.receiver {
        Value::WeakRef(w) => Ok(*w),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "WeakRef",
        }),
    }
}

fn receiver_finalization_registry(
    args: &IntrinsicArgs<'_>,
) -> Result<JsFinalizationRegistry, IntrinsicError> {
    match args.receiver {
        Value::FinalizationRegistry(r) => Ok(*r),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "FinalizationRegistry",
        }),
    }
}

fn impl_weak_ref_deref(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let weak_ref = receiver_weak_ref(args)?;
    let heap = args.gc_heap.borrow();
    Ok(weak_ref_deref(weak_ref, &heap))
}

fn impl_finalization_registry_register(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let registry = receiver_finalization_registry(args)?;
    let target = args.args.first().cloned().unwrap_or(Value::Undefined);
    let held_value = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    let unregister_token = args.args.get(2);
    let mut heap = args.gc_heap.borrow_mut();
    finalization_registry_register(registry, &mut heap, &target, held_value, unregister_token)
        .map_err(vm_to_intrinsic)?;
    Ok(Value::Undefined)
}

fn impl_finalization_registry_unregister(
    args: &IntrinsicArgs<'_>,
) -> Result<Value, IntrinsicError> {
    let registry = receiver_finalization_registry(args)?;
    let token = args.args.first().cloned().unwrap_or(Value::Undefined);
    let mut heap = args.gc_heap.borrow_mut();
    let removed =
        finalization_registry_unregister(registry, &mut heap, &token).map_err(vm_to_intrinsic)?;
    Ok(Value::Boolean(removed))
}

fn vm_to_intrinsic(err: crate::VmError) -> IntrinsicError {
    match err {
        crate::VmError::NotCallable => IntrinsicError::BadArgument {
            index: 0,
            reason: "must be callable",
        },
        _ => IntrinsicError::BadArgument {
            index: 0,
            reason: "must be an object",
        },
    }
}

/// `WeakRef.prototype` method table.
pub static WEAK_REF_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            WeakRef,
            "deref" / 0 => impl_weak_ref_deref,
        )
    });

/// `FinalizationRegistry.prototype` method table.
pub static FINALIZATION_REGISTRY_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            FinalizationRegistry,
            "register"   / 2 => impl_finalization_registry_register,
            "unregister" / 1 => impl_finalization_registry_unregister,
        )
    });

/// Lookup for `WeakRef.prototype.<name>`.
#[must_use]
pub fn lookup_weak_ref(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    WEAK_REF_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::WeakRef, name)
}

/// Lookup for `FinalizationRegistry.prototype.<name>`.
#[must_use]
pub fn lookup_finalization_registry(
    name: &str,
) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    FINALIZATION_REGISTRY_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::FinalizationRegistry, name)
}
