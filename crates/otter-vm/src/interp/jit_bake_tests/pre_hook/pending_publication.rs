//! Pending compiler roots published after an out-of-turn allocation starts.
//!
//! # Contents
//! - A real compile worker publishes one root while a heap pause waits for it.
//! - Public Promise allocation and stack-rooted allocation exercise both
//!   root-provider fallback boundaries without entering a runtime turn.
//! - An unrelated registered activation cannot hide staged argument roots.
//!
//! # Invariants
//! The marker is retained only by the pending compilation after publication.
//! The test observes relocation before reading its body, so an omitted root
//! fails without dereferencing a stale nursery offset. All rendezvous waits
//! are bounded and cancellation releases a reader during panic unwinding.
//!
//! # See also
//! - `allocation_ops` owns rooted allocation boundaries.
//! - `heap::observation` excludes readers before enumerating lazy providers.

use super::*;
use crate::jit_compile_lifetime::CompilationSources;
use std::sync::atomic::AtomicBool;

struct PublishingHook {
    right: Arc<otter_gc::read_right::HeapReadRight>,
    products: Arc<BakeProducts>,
    root: Arc<CalleeIdentityCell>,
    acquired: mpsc::Sender<()>,
    published: AtomicBool,
}

impl jit::JitCompilerHook for PublishingHook {
    fn compile_function(
        &self,
        request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        let guard = self.right.read();
        self.acquired
            .send(())
            .expect("mutator awaits worker reader");
        let deadline = Instant::now() + WAIT;
        while !guard.yield_requested() {
            if request.cancellation.is_cancelled() {
                return Ok(jit::JitCompileStatus::Cancelled);
            }
            assert!(
                Instant::now() < deadline,
                "allocation never requested a pause"
            );
            std::thread::yield_now();
        }
        // The collector is blocked in exclude(): no cell has moved. These
        // are the existing publication owner and mutex, not a parallel root.
        let initial = self.products.take();
        assert!(
            initial.roots.is_empty(),
            "the new root was absent at capture"
        );
        self.products.publish(
            vec![CompilationRoot::CalleeIdentity(self.root.clone())].into_boxed_slice(),
            initial.install,
            initial.events,
            initial.sources.expect("pending source lease"),
        );
        self.published.store(true, Ordering::Release);
        drop(guard);
        Ok(jit::JitCompileStatus::Unavailable)
    }

    fn compile_optimized_function(
        &self,
        request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        self.compile_function(request)
    }
}

#[derive(Clone, Copy)]
enum AllocationBoundary {
    PublicPendingPromise,
    StackRootedObject,
}

fn exercise_pending_publication(boundary: AllocationBoundary) {
    // The public Promise is deliberately old-space. Fill the cap without
    // collecting, then let its real allocation request an emergency full GC.
    // The stack-rooted object instead uses the ordinary young stress pause.
    let mut vm =
        Interpreter::with_string_heap_cap(32 * 1024 * 1024).expect("capped fixture interpreter");
    vm.set_jit_compiler_threads(1);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("pending-publication.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("linked source");
    let snapshot = context
        .jit_compile_snapshot(context.function_base())
        .expect("actual linked source snapshot");
    let sources = CompilationSources::retain(&context, &snapshot).expect("exact source lease");
    let products = Arc::new(BakeProducts::default());
    products.publish(Box::new([]), InstallTasks::default(), Vec::new(), sources);
    let cancellation = JitCompileCancellation::default();
    let pending = PendingCompile {
        function_id: snapshot.code_block.id,
        osr_pc: None,
        tier: JitDebugTier::Template,
        function: snapshot.code_block.clone(),
        products: products.clone(),
        cancellation: cancellation.clone(),
        retry_code_reclamation: false,
        feedback_epoch: None,
        queued_ns: 0,
        perf_name: None,
    };
    let request = jit::JitCompileRequest {
        cancellation,
        snapshot,
        debug: JitDebugRequest::disabled(),
        artifact_identity: None,
        osr_pc: None,
        code_object_id: 1,
    };
    // This final array-shell allocation leaves a young marker. The scope ends
    // before publication: Arc retention alone is not a GC root registration.
    let root = vm.with_handle_scope(|vm, scope| {
        let marker = vm.scoped_value(scope, Value::number_i32(731));
        let done = vm.scoped_value(scope, Value::boolean(true));
        let array = vm
            .scoped_array_from(scope, &[marker, done])
            .expect("young marker array");
        Arc::new(CalleeIdentityCell::holding(vm.escape_scoped(array)))
    });
    let before = root.held().as_array().expect("marker array").offset();
    let raw = root.held().as_raw_gc().expect("allocated marker");
    // SAFETY: the rooted constructor just returned and no allocation occurred
    // since. The header is inspected only here, never through an old offset.
    assert!(unsafe { (*raw.as_header_ptr()).is_young() });
    assert!(
        vm.object_layout_cache.shape_ids().next().is_none(),
        "this publication test does not exercise temporary layout-root slots"
    );
    assert!(!vm.gc_heap.has_frame_root_providers());
    assert!(
        !vm.gc_heap
            .has_extra_root_source(otter_gc::ExtraRoots::new(&vm))
    );
    let (acquired, reader) = mpsc::channel();
    let hook = Arc::new(PublishingHook {
        right: vm.gc_heap.read_right(),
        products: products.clone(),
        root: root.clone(),
        acquired,
        published: AtomicBool::new(false),
    });
    assert!(
        vm.jit_compile_queue
            .submit(
                hook.clone(),
                CompileInput::Request(Box::new(request)),
                1,
                pending
            )
            .is_none()
    );
    reader
        .recv_timeout(WAIT)
        .expect("worker owns heap read right");
    let minor_before = vm.gc_heap.gc_stats().minor_gc_cycles;
    let full_before = vm.gc_heap.gc_stats().gc_cycles;
    match boundary {
        AllocationBoundary::PublicPendingPromise => {
            let reserve = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserve)
                .expect("fill remaining cap");
            assert_eq!(vm.gc_heap.gc_stats().minor_gc_cycles, minor_before);
            assert_eq!(vm.gc_heap.gc_stats().gc_cycles, full_before);
            let outcome =
                crate::promise_dispatch::pending_runtime_rooted(&mut vm, &mut [], &mut []);
            vm.gc_heap.release_bytes(reserve);
            // Retaining the late compiler constant may legitimately change
            // whether emergency collection frees enough room for the promise.
            assert!(matches!(
                outcome,
                Ok(_) | Err(otter_gc::OutOfMemory::HeapCapExceeded { .. })
            ));
            assert_eq!(vm.gc_heap.gc_stats().gc_cycles, full_before + 1);
            assert!(vm.gc_heap.gc_stats().minor_gc_cycles > minor_before);
        }
        AllocationBoundary::StackRootedObject => {
            vm.gc_heap.set_gc_stress(1, false);
            let stack = crate::ActivationStack::new();
            vm.alloc_stack_rooted_object_with_pending_values(&stack, &mut [])
                .expect("actual young allocation");
            assert_eq!(vm.gc_heap.gc_stats().gc_cycles, full_before);
            assert_eq!(vm.gc_heap.gc_stats().minor_gc_cycles, minor_before + 1);
        }
    }
    assert!(hook.published.load(Ordering::Acquire));
    let moved_value = root.held();
    assert_ne!(
        moved_value.as_raw_gc().expect("published marker").0,
        before,
        "the newly published pending root must be rewritten before the pause finishes"
    );
    let moved = moved_value.as_array().unwrap();
    crate::array::with_elements(moved, &vm.gc_heap, |values| {
        assert_eq!(values, &[Value::number_i32(731), Value::boolean(true)]);
    });
    assert_eq!(
        vm.jit_compile_queue.pending_count(),
        1,
        "completed worker still awaits ACK"
    );
    assert!(!vm.gc_heap.has_frame_root_providers());
    assert!(
        !vm.gc_heap
            .has_extra_root_source(otter_gc::ExtraRoots::new(&vm))
    );
    let weak = Arc::downgrade(&root);
    drop(root);
    drop(hook);
    drop(products);
    let (pending, result) = finished(&mut vm.jit_compile_queue);
    assert_eq!(result.pre_hook_exit, None, "the actual hook entered");
    assert!(matches!(
        result.status,
        Ok(jit::JitCompileStatus::Unavailable)
    ));
    assert!(weak.upgrade().is_some(), "ACK retains the relocated root");
    drop(pending);
    assert!(
        weak.upgrade().is_none(),
        "ACK destruction releases the sole root owner"
    );
    assert_eq!(vm.jit_compile_queue.pending_count(), 0);
    assert!(vm.jit_compile_queue.take_finished().is_empty());
}

#[test]
fn public_pending_promise_traces_roots_published_while_its_pause_waits() {
    exercise_pending_publication(AllocationBoundary::PublicPendingPromise);
}

#[test]
fn stack_rooted_allocation_traces_roots_published_while_its_pause_waits() {
    exercise_pending_publication(AllocationBoundary::StackRootedObject);
}

#[test]
fn stack_rooted_allocation_traces_actuals_outside_an_unrelated_runtime_turn() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    let marker = vm.with_handle_scope(|vm, scope| {
        let value = vm.scoped_value(scope, Value::number_i32(513));
        let done = vm.scoped_value(scope, Value::boolean(true));
        let array = vm
            .scoped_array_from(scope, &[value, done])
            .expect("young marker array");
        vm.escape_scoped(array)
    });
    let before = marker.as_array().unwrap().offset();
    let mut actuals = crate::ActivationStack::new();
    let (argument, count) = actuals
        .stage_arguments([marker])
        .expect("one staged actual");
    assert_eq!(count, 1);
    let mut published = crate::ActivationStack::new();
    vm.with_runtime_turn(&mut published, |turn| {
        let (vm, published) = turn.into_parts();
        assert!(published.is_runtime_rooted_by(vm));
        assert!(!actuals.is_runtime_rooted_by(vm));
        // SAFETY: the marker just entered the staged span; no GC allocation
        // occurred since its final shell allocation. Inspect only this live
        // pre-collection header, before the original relocation assertion.
        let raw = unsafe { *argument }.as_raw_gc().expect("staged marker");
        assert!(
            unsafe { (*raw.as_header_ptr()).is_young() },
            "the staged marker must be young immediately before collection"
        );
        let minor_before = vm.gc_heap.gc_stats().minor_gc_cycles;
        let full_before = vm.gc_heap.gc_stats().gc_cycles;
        vm.gc_heap.set_gc_stress(1, false);
        vm.alloc_stack_rooted_object_with_pending_values(&actuals, &mut [])
            .expect("allocation with a distinct actual-argument owner");
        assert_eq!(vm.gc_heap.gc_stats().minor_gc_cycles, minor_before + 1);
        assert_eq!(vm.gc_heap.gc_stats().gc_cycles, full_before);
        // SAFETY: staging has not changed, and the distinct activation remains
        // stationary until this scope ends. Read the slot before its payload.
        let moved_value = unsafe { *argument };
        assert_ne!(
            moved_value.as_raw_gc().expect("staged marker").0,
            before,
            "an unrelated runtime turn must not hide staged actual roots"
        );
        let moved = moved_value.as_array().unwrap();
        crate::array::with_elements(moved, &vm.gc_heap, |values| {
            assert_eq!(values, &[Value::number_i32(513), Value::boolean(true)]);
        });
    });
}
