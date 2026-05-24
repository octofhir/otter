//! Sweep-time finalize dispatch: register a [`SafeFinalize`] body,
//! drop the only strong root, force a full GC, and assert the
//! finalize wrapper fires before the body's storage is reclaimed.

use std::sync::atomic::{AtomicU32, Ordering};

use otter_gc::raw::SlotVisitor;
use otter_gc::{GcHeap, HandleScope, SafeFinalize, SafeTraceable};

static FINALIZE_COUNT_REGISTERED: AtomicU32 = AtomicU32::new(0);
static FINALIZE_COUNT_UNREGISTERED: AtomicU32 = AtomicU32::new(0);

const FINALIZER_REGISTERED_TYPE_TAG: u8 = 0xE2;
const FINALIZER_UNREGISTERED_TYPE_TAG: u8 = 0xE3;

struct RegisteredFinalizerBody;

impl SafeTraceable for RegisteredFinalizerBody {
    const TYPE_TAG: u8 = FINALIZER_REGISTERED_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut SlotVisitor<'_>) {}
}

impl SafeFinalize for RegisteredFinalizerBody {
    fn finalize_safe(&mut self) {
        FINALIZE_COUNT_REGISTERED.fetch_add(1, Ordering::SeqCst);
    }
}

struct UnregisteredFinalizerBody;

impl SafeTraceable for UnregisteredFinalizerBody {
    const TYPE_TAG: u8 = FINALIZER_UNREGISTERED_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut SlotVisitor<'_>) {}
}

impl SafeFinalize for UnregisteredFinalizerBody {
    fn finalize_safe(&mut self) {
        FINALIZE_COUNT_UNREGISTERED.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn sweep_invokes_safe_finalize_for_dead_bodies() {
    FINALIZE_COUNT_REGISTERED.store(0, Ordering::SeqCst);

    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<RegisteredFinalizerBody>();
    heap.register_finalize::<RegisteredFinalizerBody>();

    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let live = heap.alloc_old(RegisteredFinalizerBody).unwrap();
    let _root = scope.local(live);
    // Allocate a second body directly in old space and keep no
    // root → dead at the next full GC sweep.
    let _dead = heap.alloc_old(RegisteredFinalizerBody).unwrap();

    heap.collect_full(&mut |_| {});

    let observed = FINALIZE_COUNT_REGISTERED.load(Ordering::SeqCst);
    assert_eq!(
        observed, 1,
        "finalize should fire exactly once for the unrooted body (saw {observed})",
    );
}

#[test]
fn unregistered_body_skips_finalize_dispatch() {
    // Same body type as above, but allocated through a fresh heap
    // that never calls `register_finalize`. The sweep walks the
    // dead body and reclaims storage without invoking `finalize`.
    FINALIZE_COUNT_UNREGISTERED.store(0, Ordering::SeqCst);

    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<UnregisteredFinalizerBody>();
    // intentionally no
    // `register_finalize::<UnregisteredFinalizerBody>()`

    let _dead = heap.alloc_old(UnregisteredFinalizerBody).unwrap();
    heap.collect_full(&mut |_| {});

    assert_eq!(
        FINALIZE_COUNT_UNREGISTERED.load(Ordering::SeqCst),
        0,
        "unregistered finalize must not fire",
    );
}
