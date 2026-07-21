//! Public traced host-data integration tests.
//!
//! # Contents
//! - Moving-GC retention through an opaque host slot.
//! - Collection after the host reference is removed.
//!
//! # Invariants
//! - The payload never observes a raw VM value or collector visitor.
//! - Slot reads and writes happen through a native handle scope.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use otter_runtime::{
    Runtime, RuntimeHostDataTracer, RuntimeHostValueSlot, RuntimeTracedHostObjectData, SourceInput,
};

struct ListenerPayload {
    callback: RuntimeHostValueSlot,
    drops: Arc<AtomicUsize>,
}

impl RuntimeTracedHostObjectData for ListenerPayload {
    fn trace_gc_slots(&mut self, tracer: &mut RuntimeHostDataTracer<'_>) {
        tracer.trace(&mut self.callback);
    }
}

impl Drop for ListenerPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn traced_host_slot_survives_moving_gc_and_releases_after_unlink() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::builder().build().expect("runtime");

    runtime
        .eval_value(
            SourceInput::from_javascript(
                "const target = { marker: 42 };\n\
                 globalThis.weakTarget = new WeakRef(target);\n\
                 globalThis.hostBox = { target };\n\
                 hostBox;",
            ),
            "<traced-host-setup>",
            |ctx, box_value| {
                ctx.scope(|mut scope| {
                    let box_value = scope.value(box_value);
                    let target = scope.get(box_value, "target").expect("target");
                    let host = scope
                        .traced_host_object(ListenerPayload {
                            callback: RuntimeHostValueSlot::empty(),
                            drops: drops.clone(),
                        })
                        .expect("traced host object");
                    scope
                        .set_host_data_value::<ListenerPayload>(host, target, |payload| {
                            &mut payload.callback
                        })
                        .expect("store traced target");
                    let reread = scope
                        .host_data_value::<ListenerPayload>(host, |payload| &payload.callback)
                        .expect("read traced target");
                    let marker = scope.get(reread, "marker").unwrap();
                    assert_eq!(scope.number_value(marker).unwrap(), 42.0);
                    scope.set(box_value, "host", host).expect("publish host");
                    let undefined = scope.undefined();
                    scope
                        .set(box_value, "target", undefined)
                        .expect("drop ordinary target edge");
                });
            },
        )
        .expect("setup script");

    for _ in 0..8 {
        runtime.force_gc().expect("moving/full GC");
    }
    let retained = runtime
        .eval(SourceInput::from_javascript("weakTarget.deref().marker"))
        .expect("retained target");
    assert_eq!(retained.completion_string(), "42");
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    runtime
        .eval(SourceInput::from_javascript("hostBox.host = undefined"))
        .expect("unlink host");
    for _ in 0..2 {
        runtime.force_gc().expect("collect host and target");
    }
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn traced_host_payload_drops_with_runtime_dispose() {
    let drops = Arc::new(AtomicUsize::new(0));
    {
        let mut runtime = Runtime::builder().build().expect("runtime");
        runtime
            .eval_value(
                SourceInput::from_javascript("globalThis.keep = {}; keep"),
                "<traced-host-dispose>",
                |ctx, keep| {
                    ctx.scope(|mut scope| {
                        let keep = scope.value(keep);
                        let host = scope
                            .traced_host_object(ListenerPayload {
                                callback: RuntimeHostValueSlot::empty(),
                                drops: drops.clone(),
                            })
                            .expect("traced host object");
                        scope.set(keep, "host", host).expect("publish host");
                    });
                },
            )
            .expect("setup script");
    }
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
