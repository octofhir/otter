//! Object.setPrototypeOf return identity through actual Array sidecar collection.
//!
//! # Contents
//! - Exact cap admission forces collection while the native entry installs a
//!   fresh Array prototype override.
//! - Object and explicit-null prototypes retain the original receiver identity.
//!
//! # Invariants
//! - Fresh receiver handles are not aged by fixture preparation.
//! - The production sidecar allocator owns the measured collection.
//! - Numeric offsets observe motion and never act as collector roots.
//! - Return values are compared with current handles before inspecting payloads.
//!
//! # See also
//! - `super::native_set_prototype_of` owns native return publication.
//! - `crate::object_internal_ops::prototype_set` owns value-level semantics.
//! - `crate::array::set_prototype_override` reserves the indexed sidecar.

use crate::ValueWindow;
use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, array};
use otter_gc::SafeTraceable;

struct Reclaimable([u64; 16384]);

impl SafeTraceable for Reclaimable {
    const TYPE_TAG: u8 = 0xe8;

    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

#[test]
fn set_prototype_returns_current_array_after_collecting_sidecar_birth() {
    for null_prototype in [false, true] {
        let cap = 4 * 1024 * 1024;
        let mut vm = Interpreter::with_string_heap_cap(cap).expect("prototype fixture bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("array-set-prototype.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("real prototype source context");
        vm.force_gc().expect("settle source before fresh inputs");
        NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<(), NativeError> {
                    let prototype = if null_prototype {
                        scope.null()
                    } else {
                        scope.object()?
                    };
                    let receiver = scope.array(1)?;
                    let alias = scope.value(scope.raw(receiver));
                    let marker = scope.value(Value::number_i32(719));
                    scope.set_index(receiver, 0, marker)?;
                    let old_receiver = scope.raw(receiver).as_array().unwrap().offset();
                    let old_prototype = scope
                        .raw(prototype)
                        .as_object()
                        .map(|object| object.offset());
                    assert!(unsafe {
                        (*scope.raw(receiver).as_array().unwrap().as_header_ptr()).is_young()
                    });
                    let current = scope.raw(receiver).as_array().unwrap();
                    assert!(scope.context().heap().read_payload(current, |body| {
                        body.exotic.is_null() && body.expando.is_null()
                    }));
                    let cycles = scope.context().heap().gc_cycle_counts();
                    // An old-space sidecar does not use nursery stress. Its
                    // exact cap admission must collect and reclaim this dead
                    // LOS cell before publishing the requested prototype.
                    scope
                        .context()
                        .heap_mut()
                        .alloc_old(Reclaimable([0; 16384]))?;
                    assert_eq!(scope.context().heap().gc_cycle_counts(), cycles);
                    let heap = scope.context().heap_mut();
                    let reserve = cap
                        .checked_sub(heap.tracked_bytes())
                        .and_then(|remaining| remaining.checked_sub(1))
                        .expect("fresh inputs leave cap pressure headroom");
                    assert!(reserve > 0);
                    heap.reserve_bytes_no_collect(reserve)?;
                    let pressure = heap.stats();
                    assert_eq!(pressure.tracked_bytes, cap - 1);
                    assert_eq!(
                        pressure.tracked_bytes,
                        pressure.allocated_bytes as u64 + pressure.reserved_bytes
                    );
                    assert_eq!(heap.gc_cycle_counts(), cycles);
                    let sidecar_bytes = (std::mem::size_of::<otter_gc::GcHeader>()
                        + std::mem::size_of::<array::ArrayExoticSlots>())
                    .next_multiple_of(otter_gc::OBJECT_ALIGNMENT)
                        as u64;
                    assert!(cap - pressure.tracked_bytes < sidecar_bytes);
                    let before = heap.gc_stats().clone();
                    let args = [scope.raw(receiver), scope.raw(prototype)];
                    let result =
                        super::native_set_prototype_of(scope.context(), ValueWindow::from(&args))?;
                    let result = scope.value(result);
                    let after = scope.context().heap_mut().gc_stats().clone();
                    assert!(after.minor_gc_cycles > before.minor_gc_cycles);
                    assert!(after.gc_cycles > before.gc_cycles);
                    assert!(after.minor_slot_updates > before.minor_slot_updates);
                    assert_eq!(
                        after.by_type[array::ARRAY_EXOTIC_SLOTS_TYPE_TAG as usize]
                            .alloc_count_total,
                        before.by_type[array::ARRAY_EXOTIC_SLOTS_TYPE_TAG as usize]
                            .alloc_count_total
                            + 1,
                        "the measured native operation births exactly one sidecar"
                    );
                    assert_ne!(
                        scope.raw(receiver).as_array().unwrap().offset(),
                        old_receiver
                    );
                    assert_eq!(scope.raw(receiver), scope.raw(alias));
                    assert_eq!(
                        scope.raw(result),
                        scope.raw(receiver),
                        "native return must reread the moved receiver handle"
                    );
                    let current = scope.raw(result).as_array().unwrap();
                    let current_prototype = scope.raw(prototype);
                    if let Some(old) = old_prototype {
                        assert_ne!(current_prototype.as_object().unwrap().offset(), old);
                    }
                    let heap = scope.context().heap();
                    assert_eq!(
                        array::prototype_override(current, heap),
                        Some(current_prototype)
                    );
                    assert_eq!(array::len(current, heap), 1);
                    assert_eq!(array::get(current, heap, 0), Value::number_i32(719));
                    assert!(heap.read_payload(current, |body| !body.exotic.is_null()
                        && body.expando.is_null()));
                    scope.context().heap_mut().release_bytes(reserve);
                    Ok(())
                })
            },
        )
        .expect("collecting native prototype completion");
    }
}
