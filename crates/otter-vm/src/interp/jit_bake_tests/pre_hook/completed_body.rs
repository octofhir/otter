//! Completed root bodies through a pending moving collection.
//!
//! # Contents
//! - Template rendezvous after its completed bake and actual prepared event.
//! - Graph rendezvous before the actual stable-shape census.
//! - Publication of a new Baker-owned cell, moving contents and exact source ACK.
//!
//! # Invariants
//! The extra identity cell is a publication-boundary probe created by the real
//! Baker. The Graph shape is a real pinned, stable hidden class inserted through
//! that same owner and checked in the hook's completed snapshot. These probes
//! do not claim emitted native-code use. Only pending products trace the new
//! cell; observer and transport Arcs are not registered GC roots.
//! Collection uses the registered Interpreter provider. Rendezvous waits are
//! bounded; no sleep, retry, forced tier admission or disabled GC proves success.
//!
//! # See also
//! - `super` covers earlier refusals and terminal disposition serialization.
//! - Runtime actual-only tests cover native admission and execution.

use super::*;
use std::sync::atomic::AtomicU32;

struct PublicationProbe {
    marker: Arc<CalleeIdentityCell>,
    function_id: u32,
    ready: mpsc::Sender<Arc<CalleeIdentityCell>>,
}

impl PublicationProbe {
    fn arrive(
        self,
        guard: &otter_gc::read_right::HeapReadGuard<'_>,
        baker: &mut Baker<'_>,
        snapshot: &jit::JitCompileSnapshot,
    ) {
        assert_eq!(snapshot.code_block.id, self.function_id);
        assert!(!snapshot.instructions.is_empty());
        assert!(baker.abandoned.is_none());
        assert!(!baker.env.contended());
        assert!(
            !guard.yield_requested(),
            "the mutator awaits this checkpoint"
        );
        // This cell did not exist when the job registered its captured roots.
        // Its distinct transport and observer Arcs do not register GC roots.
        let cell = baker.held_cell(self.marker.held());
        assert!(!Arc::ptr_eq(&cell, &self.marker));
        self.ready
            .send(cell)
            .expect("mutator awaits completed body");
        let deadline = Instant::now() + WAIT;
        while !guard.yield_requested() {
            assert!(
                Instant::now() < deadline,
                "mutator did not request the pause"
            );
            std::thread::yield_now();
        }
    }
}

pub(crate) struct PublishReady(PublicationProbe);

impl PublishReady {
    pub(crate) fn arrive(
        self,
        guard: &otter_gc::read_right::HeapReadGuard<'_>,
        baker: &mut Baker<'_>,
        snapshot: &jit::JitCompileSnapshot,
    ) {
        assert!(
            baker.events.iter().any(|event| matches!(
                event,
                JitDebugEvent::CompilePrepared {
                    function_id,
                    tier: JitDebugTier::Template,
                    ..
                } if *function_id == self.0.function_id
            )),
            "the real Template snapshot and prepared event already exist"
        );
        self.0.arrive(guard, baker, snapshot);
    }
}

pub(crate) struct BeforeShapeCensus {
    probe: PublicationProbe,
    hook: Arc<PublicationHook>,
}

impl BeforeShapeCensus {
    pub(crate) fn arrive(self, baker: &mut Baker<'_>, snapshot: &jit::JitCompileSnapshot) {
        assert!(
            !baker.events.iter().any(|event| matches!(
                event,
                JitDebugEvent::CompilePrepared { function_id, .. }
                    if *function_id == self.probe.function_id
            )),
            "the Graph census precedes the actual prepared event"
        );
        let guard = baker
            .right
            .expect("Graph worker holds its original read right");
        let marker = self.probe.marker.held().as_object().unwrap();
        let shape = crate::object::shape(marker, &baker.env.heap);
        assert!(!shape.is_null());
        // SAFETY: the same read guard excludes collection, and this is the
        // current shape of the live marker. No raw header escapes this check.
        assert!(
            !unsafe { (*shape.as_header_ptr()).is_young() },
            "shapes are pinned old cells"
        );
        assert!(crate::object::shape_body::is_stable(shape));
        let offset = baker
            .bake_shape(shape)
            .expect("ordinary stable shape is bakeable");
        assert!(!snapshot.stable_shapes.contains(&offset));
        assert!(!snapshot.left_shapes.contains(&offset));
        assert_eq!(self.hook.graph_shape.swap(offset, Ordering::Release), 0);
        self.probe.arrive(guard, baker, snapshot);
    }
}

#[derive(Debug, Default)]
struct PublicationHook {
    entries: AtomicUsize,
    graph_shape: AtomicU32,
}

impl jit::JitCompilerHook for PublicationHook {
    fn compile_function(
        &self,
        request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        assert!(!request.cancellation.is_cancelled());
        assert_eq!(self.graph_shape.load(Ordering::Acquire), 0);
        self.entries.fetch_add(1, Ordering::Relaxed);
        Ok(jit::JitCompileStatus::Unavailable)
    }

    fn compile_optimized_function(
        &self,
        request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        assert!(!request.cancellation.is_cancelled());
        let shape = self.graph_shape.load(Ordering::Acquire);
        assert_ne!(shape, 0, "the pre-census checkpoint supplied a real shape");
        assert!(
            request.snapshot.stable_shapes.contains(&shape),
            "the completed Graph census must retain its stable-shape fact"
        );
        assert!(!request.snapshot.left_shapes.contains(&shape));
        self.entries.fetch_add(1, Ordering::Relaxed);
        Ok(jit::JitCompileStatus::Unavailable)
    }
}

fn completed_body_publication(tier: JitDebugTier, cancel_before_hook: bool) {
    let mut vm = Interpreter::new().expect("publication fixture");
    vm.set_jit_compiler_threads(1);
    vm.set_jit_debug_request(JitDebugRequest::events());
    let context = vm
        .link_evictable_module(
            crate::test_support::minimal_bytecode_module("completed-body.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verified evictable source");
    let fid = context.function_base();
    let space = context.space().clone();
    let _runtime_roots = vm.scope_runtime_roots_guard();
    assert!(
        _runtime_roots.is_some(),
        "this fresh fixture owns its runtime root registration"
    );
    let marker = vm.with_handle_scope(|vm, scope| {
        let value = vm.scoped_value(scope, Value::number_i32(947));
        let done = vm.scoped_value(scope, Value::boolean(true));
        let marker = vm
            .scoped_iterator_result(scope, value, done)
            .expect("young marked object");
        vm.escape_scoped(marker)
    });
    // That handle scope has ended. No allocation occurs between this point
    // and the checkpoint; no initial pending root or persistent handle keeps
    // the marker alive during the tested collection.
    let before = marker.as_object().unwrap().offset();
    let raw = marker.as_raw_gc().unwrap();
    // SAFETY: the allocation is current and nothing has allocated since the
    // rooted constructor returned. The raw header is not retained across GC.
    assert!(unsafe { (*raw.as_header_ptr()).is_young() });
    let cycles = vm.gc_heap.gc_stats().minor_gc_cycles;
    let full_cycles = vm.gc_heap.gc_stats().gc_cycles;
    let hook = Arc::new(PublicationHook::default());
    let (ready, completed) = mpsc::channel();
    let (published, after_guard) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let (mut bake, pending) = job(&vm, &context, tier, 71, None, None);
    let probe = PublicationProbe {
        marker: Arc::new(CalleeIdentityCell::holding(marker)),
        function_id: fid,
        ready,
    };
    match tier {
        JitDebugTier::Template => bake.publish_ready = Some(PublishReady(probe)),
        JitDebugTier::Optimizing => {
            bake.before_shape_census = Some(BeforeShapeCensus {
                probe,
                hook: hook.clone(),
            });
        }
        _ => unreachable!("only Template and Graph have a bake body"),
    }
    bake.before_hook = Some(BeforeHook {
        ready: published,
        resume,
    });
    let token = pending.cancellation.clone();
    assert!(
        vm.jit_compile_queue
            .submit(
                hook.clone(),
                CompileInput::Bake(Box::new(bake)),
                71,
                pending
            )
            .is_none()
    );
    let cell = completed
        .recv_timeout(WAIT)
        .expect("completed bake owns its new root");
    assert_eq!(cell.held().as_object().unwrap().offset(), before);
    assert_eq!(vm.gc_heap.gc_stats().minor_gc_cycles, cycles);
    assert_eq!(hook.entries.load(Ordering::Relaxed), 0);

    vm.gc_heap.set_gc_stress(1, false);
    vm.with_handle_scope(|vm, scope| {
        vm.scoped_object_bare(scope)
            .expect("allocation requests a real minor pause");
    });
    assert_eq!(vm.gc_heap.gc_stats().minor_gc_cycles, cycles + 1);
    assert_eq!(vm.gc_heap.gc_stats().gc_cycles, full_cycles);
    let minor_after = vm.gc_heap.gc_stats().minor_gc_cycles;
    let full_after = vm.gc_heap.gc_stats().gc_cycles;
    eprintln!(
        "completed-body boundary: tier={tier:?} function_id={fid} cancel_before_hook={cancel_before_hook} minor_before={cycles} minor_after={minor_after} full_before={full_cycles} full_after={full_after} marker_before={before} marker_after={} hook_entries={}",
        cell.held().as_object().unwrap().offset(),
        hook.entries.load(Ordering::Relaxed),
    );

    if after_guard.recv_timeout(WAIT).is_err() {
        let (_pending, result) = finished(&mut vm.jit_compile_queue);
        assert_eq!(hook.entries.load(Ordering::Relaxed), 0);
        // The baseline refuses completed Template work at PublishReady and
        // Graph work at RootReady before its stable-shape census.
        assert_eq!(
            result.pre_hook_exit, None,
            "completed {tier:?} snapshot was discarded"
        );
        panic!("successful bake did not reach the post-publication rendezvous");
    }
    assert_ne!(
        cell.held().as_object().unwrap().offset(),
        before,
        "the newly published root was rewritten"
    );
    let moved = cell.held().as_object().unwrap();
    assert_eq!(
        crate::object::get(moved, &vm.gc_heap, "value"),
        Some(Value::number_i32(947))
    );
    assert_eq!(
        crate::object::get(moved, &vm.gc_heap, "done"),
        Some(Value::boolean(true))
    );
    assert_eq!(
        hook.entries.load(Ordering::Relaxed),
        0,
        "hook waits until after the real pause"
    );
    assert_eq!(vm.jit_compile_queue.pending_count(), 1);
    let address = cell.address();
    let weak = Arc::downgrade(&cell);
    drop(cell);
    if cancel_before_hook {
        token.cancel();
    }
    assert!(
        weak.upgrade().is_some(),
        "pending products own the new cell before hook entry"
    );
    release
        .send(())
        .expect("release the post-publication rendezvous");
    let deadline = Instant::now() + WAIT;
    while !vm.jit_compile_queue.has_finished() {
        assert!(
            Instant::now() < deadline,
            "worker did not acknowledge completed bake"
        );
        std::thread::yield_now();
    }
    drop(context);
    assert!(
        space.eviction_candidates().is_empty(),
        "published sources retain the exact payload until ACK"
    );
    assert!(
        weak.upgrade().is_some(),
        "finished worker does not release pending roots"
    );
    let (pending, result) = finished(&mut vm.jit_compile_queue);
    assert_eq!(result.code_object_id, 71);
    assert_eq!(pending.function_id, fid);
    assert_eq!(
        result.pre_hook_exit,
        cancel_before_hook.then_some(JitDebugPreHookExit::Cancelled)
    );
    assert_eq!(
        hook.entries.load(Ordering::Relaxed),
        usize::from(!cancel_before_hook)
    );
    if cancel_before_hook {
        assert!(matches!(
            result.status,
            Ok(jit::JitCompileStatus::Cancelled)
        ));
        assert_eq!(result.duration, Duration::ZERO);
    } else {
        assert!(matches!(
            result.status,
            Ok(jit::JitCompileStatus::Unavailable)
        ));
    }
    let products = pending.products.take();
    assert_eq!(
        products
            .events
            .iter()
            .filter(|event| matches!(event,
                JitDebugEvent::CompilePrepared { function_id, tier: prepared_tier, .. }
                    if *function_id == fid && *prepared_tier == tier
            ))
            .count(),
        1,
        "the completed root publishes its actual prepared event once"
    );
    assert!(
        products
            .sources
            .as_ref()
            .expect("successful publication retains sources")
            .is_current(&space, &pending.function, &[])
    );
    assert_eq!(
        products
            .roots
            .iter()
            .filter(|root| matches!(root,
                CompilationRoot::CalleeIdentity(cell) if cell.address() == address
            ))
            .count(),
        1,
        "the late root belongs to the acknowledged publication exactly once"
    );
    drop(pending);
    assert!(
        weak.upgrade().is_some(),
        "the ACK consumer still owns the products"
    );
    assert!(space.eviction_candidates().is_empty());
    drop(products);
    assert!(
        weak.upgrade().is_none(),
        "consuming ACK releases the late root"
    );
    let candidates = space.eviction_candidates();
    assert_eq!(
        candidates.len(),
        1,
        "consuming ACK releases the source lease"
    );
    assert_eq!(candidates[0].function_base, fid);
    assert!(matches!(
        space.evict_candidate(candidates[0]),
        crate::code_space::ChunkEvictionResult::Evicted { .. }
    ));
    assert_eq!(space.evictable_retained_bytes(), 0);
    assert_eq!(vm.jit_compile_queue.pending_count(), 0);
    assert!(
        vm.jit_compile_queue.take_finished().is_empty(),
        "one job has one terminal ACK"
    );
}

#[test]
fn completed_template_snapshot_publishes_before_the_waiting_minor_pause() {
    completed_body_publication(JitDebugTier::Template, false);
}

#[test]
fn completed_template_publication_preserves_late_cancellation_and_ack_ownership() {
    completed_body_publication(JitDebugTier::Template, true);
}

#[test]
fn completed_graph_census_publishes_before_the_waiting_minor_pause() {
    completed_body_publication(JitDebugTier::Optimizing, false);
}

#[test]
fn completed_graph_publication_preserves_late_cancellation_and_ack_ownership() {
    completed_body_publication(JitDebugTier::Optimizing, true);
}
