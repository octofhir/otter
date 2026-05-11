//! Regression coverage for task 81 WeakRef / FinalizationRegistry GC semantics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use otter_bytecode::{BytecodeModule, SourceKind};
use otter_gc::raw::RawGc;
use otter_vm::native_value;
use otter_vm::object::{OBJECT_BODY_TYPE_TAG, alloc_object};
use otter_vm::weak_refs::{
    FINALIZATION_REGISTRY_BODY_TYPE_TAG, WEAK_REF_BODY_TYPE_TAG, alloc_finalization_registry,
    alloc_finalization_registry_with_context, alloc_weak_ref, finalization_registry_cell_count,
    finalization_registry_register, finalization_registry_unregister,
    process_weak_refs_and_finalizers, weak_ref_deref,
};
use otter_vm::{ExecutionContext, Interpreter, Value};

fn full_gc_with_roots(
    heap: &mut otter_gc::GcHeap,
    roots: &mut [&mut RawGc],
) -> Vec<otter_vm::weak_refs::FinalizationJob> {
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for slot in roots.iter_mut() {
            visitor(*slot as *mut RawGc);
        }
    };
    heap.mark_phase(&mut visit);
    otter_vm::collections::run_ephemeron_fixpoint(heap);
    let jobs = process_weak_refs_and_finalizers(heap);
    heap.sweep_phase();
    jobs
}

fn empty_module() -> BytecodeModule {
    BytecodeModule {
        module: "weakref-finalization-test".to_string(),
        source_kind: SourceKind::JavaScript,
        functions: Vec::new(),
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

fn empty_context() -> ExecutionContext {
    ExecutionContext::from_module(empty_module())
}

#[test]
fn weak_ref_does_not_keep_target_alive() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let target = alloc_object(&mut heap).expect("target");
    let weak_ref = alloc_weak_ref(&mut heap, &Value::Object(target)).expect("weak ref");
    assert_eq!(weak_ref_deref(weak_ref, &heap), Value::Object(target));

    let mut weak_root = weak_ref.raw();
    let jobs = full_gc_with_roots(&mut heap, &mut [&mut weak_root]);
    assert!(jobs.is_empty());
    assert_eq!(weak_ref_deref(weak_ref, &heap), Value::Undefined);
    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        0
    );
}

#[test]
fn weak_ref_returns_target_while_strongly_rooted() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let target = alloc_object(&mut heap).expect("target");
    let weak_ref = alloc_weak_ref(&mut heap, &Value::Object(target)).expect("weak ref");

    let mut weak_root = weak_ref.raw();
    let mut target_root = target.raw();
    let jobs = full_gc_with_roots(&mut heap, &mut [&mut weak_root, &mut target_root]);
    assert!(jobs.is_empty());
    assert_eq!(weak_ref_deref(weak_ref, &heap), Value::Object(target));
    assert!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes > 0,
        "strong root should keep target object live"
    );
}

#[test]
fn weak_ref_target_becomes_unavailable_after_force_gc() {
    let mut interp = Interpreter::new();
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    let weak_ref = alloc_weak_ref(interp.gc_heap_mut(), &Value::Object(target)).expect("weak ref");
    let global = *interp.global_this();
    otter_vm::object::set(global, interp.gc_heap_mut(), "wr", Value::WeakRef(weak_ref));

    interp.force_gc();
    assert_eq!(weak_ref_deref(weak_ref, interp.gc_heap()), Value::Undefined);
}

#[test]
fn finalization_registry_registers_without_strong_target_retention() {
    let mut interp = Interpreter::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback = native_value(interp.gc_heap_mut(), "cleanup", {
        let calls = Arc::clone(&calls);
        move |_, _, _| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(Value::Undefined)
        }
    })
    .expect("native cleanup");
    let registry = alloc_finalization_registry(interp.gc_heap_mut(), callback).expect("registry");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "registry",
        Value::FinalizationRegistry(registry),
    );
    // Stabilise the baseline before measuring: bootstrap creates
    // transient `ObjectBody` allocations (intermediate descriptor
    // values, intern-time scratch shapes, etc.) that are unreachable
    // but not yet collected. Force a GC so the baseline reflects the
    // actual reachable set, and the post-`register` GC can be
    // compared apples-to-apples.
    interp.force_gc();
    let baseline_object_live_bytes =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    finalization_registry_register(
        registry,
        interp.gc_heap_mut(),
        &Value::Object(target),
        Value::Boolean(true),
        None,
    )
    .expect("register");

    interp.force_gc();
    assert!(interp.microtasks().has_pending_sync());
    assert_eq!(
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        baseline_object_live_bytes,
        "registry cell must not strongly retain target"
    );

    interp
        .drain_microtasks(&empty_context())
        .expect("drain cleanup");
    interp.force_gc();
    interp
        .drain_microtasks(&empty_context())
        .expect("second drain");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "finalizer should enqueue at most once"
    );
}

#[test]
fn dropped_finalization_registry_self_cycle_is_reaped() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let target = alloc_object(&mut heap).expect("target");
    let callback =
        native_value(&mut heap, "cleanup", |_, _, _| Ok(Value::Undefined)).expect("native cleanup");
    let registry = alloc_finalization_registry(&mut heap, callback).expect("registry");
    finalization_registry_register(
        registry,
        &mut heap,
        &Value::Object(target),
        Value::FinalizationRegistry(registry),
        None,
    )
    .expect("register");
    assert!(heap.gc_stats().by_type[FINALIZATION_REGISTRY_BODY_TYPE_TAG as usize].live_bytes > 0);

    let jobs = full_gc_with_roots(&mut heap, &mut []);
    assert!(
        jobs.is_empty(),
        "dead registries must not enqueue callbacks while swept"
    );
    assert_eq!(
        heap.gc_stats().by_type[FINALIZATION_REGISTRY_BODY_TYPE_TAG as usize].live_bytes,
        0,
        "registry self-cycle through held value should be reclaimed"
    );
}

#[test]
fn finalization_registry_schedules_cleanup_microtask() {
    let mut interp = Interpreter::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_held = Arc::new(AtomicBool::new(false));
    let callback = {
        let calls = Arc::clone(&calls);
        let seen_held = Arc::clone(&seen_held);
        native_value(interp.gc_heap_mut(), "cleanup", move |_, args, _| {
            calls.fetch_add(1, Ordering::Relaxed);
            seen_held.store(
                matches!(args.first(), Some(Value::Boolean(true))),
                Ordering::Relaxed,
            );
            Ok(Value::Undefined)
        })
        .expect("native cleanup")
    };
    let registry = alloc_finalization_registry(interp.gc_heap_mut(), callback).expect("registry");
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    finalization_registry_register(
        registry,
        interp.gc_heap_mut(),
        &Value::Object(target),
        Value::Boolean(true),
        None,
    )
    .expect("register");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "registry",
        Value::FinalizationRegistry(registry),
    );

    interp.force_gc();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "cleanup must not run during raw GC"
    );
    assert!(interp.microtasks().has_pending_sync());
    interp
        .drain_microtasks(&empty_context())
        .expect("drain finalization callback");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(seen_held.load(Ordering::Relaxed));

    interp.force_gc();
    interp
        .drain_microtasks(&empty_context())
        .expect("second drain");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "cleanup should run exactly once"
    );
}

#[test]
fn finalization_callback_cannot_observe_collected_target_through_weak_ref() {
    let mut interp = Interpreter::new();
    let observed_undefined = Arc::new(AtomicBool::new(false));
    let callback = {
        let observed_undefined = Arc::clone(&observed_undefined);
        native_value(interp.gc_heap_mut(), "cleanup", move |ctx, args, _| {
            let interp = ctx.interp_mut();
            let Some(Value::WeakRef(weak_ref)) = args.first() else {
                return Ok(Value::Undefined);
            };
            observed_undefined.store(
                weak_ref_deref(*weak_ref, interp.gc_heap()) == Value::Undefined,
                Ordering::Relaxed,
            );
            Ok(Value::Undefined)
        })
        .expect("native cleanup")
    };
    let registry = alloc_finalization_registry(interp.gc_heap_mut(), callback).expect("registry");
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    let weak_ref = alloc_weak_ref(interp.gc_heap_mut(), &Value::Object(target)).expect("weak ref");
    finalization_registry_register(
        registry,
        interp.gc_heap_mut(),
        &Value::Object(target),
        Value::WeakRef(weak_ref),
        None,
    )
    .expect("register");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "registry",
        Value::FinalizationRegistry(registry),
    );

    interp.force_gc();
    interp
        .drain_microtasks(&empty_context())
        .expect("drain finalization callback");
    assert!(observed_undefined.load(Ordering::Relaxed));
}

#[test]
fn finalization_cleanup_job_carries_registry_context() {
    let mut interp = Interpreter::new();
    let called = Arc::new(AtomicBool::new(false));
    let callback = native_value(interp.gc_heap_mut(), "cleanup", {
        let called = Arc::clone(&called);
        move |_, _, _| {
            called.store(true, Ordering::Relaxed);
            Ok(Value::Undefined)
        }
    })
    .expect("native cleanup");
    let registry = alloc_finalization_registry_with_context(
        interp.gc_heap_mut(),
        callback,
        Some(ExecutionContext::from_module(empty_module())),
    )
    .expect("registry");
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    finalization_registry_register(
        registry,
        interp.gc_heap_mut(),
        &Value::Object(target),
        Value::Undefined,
        None,
    )
    .expect("register");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "registry",
        Value::FinalizationRegistry(registry),
    );

    interp.force_gc();
    interp
        .drain_microtasks_with_default(None)
        .expect("cleanup job should carry its own context");
    assert!(called.load(Ordering::Relaxed));
}

#[test]
fn pending_finalization_microtask_roots_held_value_across_next_gc() {
    let mut interp = Interpreter::new();
    let observed_undefined = Arc::new(AtomicBool::new(false));
    let callback = {
        let observed_undefined = Arc::clone(&observed_undefined);
        native_value(interp.gc_heap_mut(), "cleanup", move |ctx, args, _| {
            let interp = ctx.interp_mut();
            let Some(Value::WeakRef(weak_ref)) = args.first() else {
                return Ok(Value::Undefined);
            };
            observed_undefined.store(
                weak_ref_deref(*weak_ref, interp.gc_heap()) == Value::Undefined,
                Ordering::Relaxed,
            );
            Ok(Value::Undefined)
        })
        .expect("native cleanup")
    };
    let registry = alloc_finalization_registry(interp.gc_heap_mut(), callback).expect("registry");
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    let weak_ref = alloc_weak_ref(interp.gc_heap_mut(), &Value::Object(target)).expect("weak ref");
    finalization_registry_register(
        registry,
        interp.gc_heap_mut(),
        &Value::Object(target),
        Value::WeakRef(weak_ref),
        None,
    )
    .expect("register");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "registry",
        Value::FinalizationRegistry(registry),
    );

    interp.force_gc();
    assert!(interp.microtasks().has_pending_sync());
    interp.force_gc();
    interp
        .drain_microtasks(&empty_context())
        .expect("drain finalization callback");
    assert!(observed_undefined.load(Ordering::Relaxed));
}

#[test]
fn cleanup_callback_allocates_only_after_raw_gc_sweep_boundary() {
    let mut interp = Interpreter::new();
    let callback = native_value(interp.gc_heap_mut(), "cleanup", |ctx, _, _| {
        let interp = ctx.interp_mut();
        let allocated = alloc_object(interp.gc_heap_mut())?;
        let global = *interp.global_this();
        otter_vm::object::set(
            global,
            interp.gc_heap_mut(),
            "allocated_after_gc",
            Value::Object(allocated),
        );
        Ok(Value::Undefined)
    })
    .expect("native cleanup");
    let registry = alloc_finalization_registry(interp.gc_heap_mut(), callback).expect("registry");
    let target = alloc_object(interp.gc_heap_mut()).expect("target");
    finalization_registry_register(
        registry,
        interp.gc_heap_mut(),
        &Value::Object(target),
        Value::Boolean(true),
        None,
    )
    .expect("register");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "registry",
        Value::FinalizationRegistry(registry),
    );

    interp.force_gc();
    assert!(
        otter_vm::object::get(global, interp.gc_heap(), "allocated_after_gc").is_none(),
        "raw GC must only enqueue cleanup and must not run user callback"
    );
    interp
        .drain_microtasks(&empty_context())
        .expect("drain finalization callback");
    assert!(matches!(
        otter_vm::object::get(global, interp.gc_heap(), "allocated_after_gc"),
        Some(Value::Object(_))
    ));
    interp.force_gc();
    assert!(matches!(
        otter_vm::object::get(global, interp.gc_heap(), "allocated_after_gc"),
        Some(Value::Object(_))
    ));
}

#[test]
fn finalization_registry_unregister_token_removes_cells() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let callback =
        native_value(&mut heap, "cleanup", |_, _, _| Ok(Value::Undefined)).expect("native cleanup");
    let registry = alloc_finalization_registry(&mut heap, callback).expect("registry");
    let target = alloc_object(&mut heap).expect("target");
    let token = alloc_object(&mut heap).expect("token");
    finalization_registry_register(
        registry,
        &mut heap,
        &Value::Object(target),
        Value::Boolean(false),
        Some(&Value::Object(token)),
    )
    .expect("register");
    assert_eq!(finalization_registry_cell_count(registry, &heap), 1);
    assert!(
        finalization_registry_unregister(registry, &mut heap, &Value::Object(token))
            .expect("unregister")
    );
    assert_eq!(finalization_registry_cell_count(registry, &heap), 0);
}

#[test]
fn weak_finalization_registry_prunes_dead_handles_and_keeps_lazy_flag() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    assert!(!heap.has_finalization_registries());
    let target = alloc_object(&mut heap).expect("target");
    let _weak_ref = alloc_weak_ref(&mut heap, &Value::Object(target)).expect("weak ref");
    assert_eq!(heap.weak_ref_count(), 1);
    assert_eq!(
        heap.gc_stats().by_type[WEAK_REF_BODY_TYPE_TAG as usize].alloc_count_total,
        1
    );

    full_gc_with_roots(&mut heap, &mut []);
    assert_eq!(heap.weak_ref_count(), 0);
    assert!(!heap.has_finalization_registries());

    let callback =
        native_value(&mut heap, "cleanup", |_, _, _| Ok(Value::Undefined)).expect("native cleanup");
    let registry = alloc_finalization_registry(&mut heap, callback).expect("registry");
    assert_eq!(heap.finalization_registry_count(), 1);
    assert!(heap.has_finalization_registries());
    let mut registry_root = registry.raw();
    full_gc_with_roots(&mut heap, &mut [&mut registry_root]);
    assert_eq!(
        heap.gc_stats().by_type[FINALIZATION_REGISTRY_BODY_TYPE_TAG as usize].alloc_count_total,
        1
    );
}
