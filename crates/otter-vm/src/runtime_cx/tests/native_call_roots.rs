//! Native call metadata reloaded after one allocation-driven moving collection.
//!
//! # Contents
//! - Separate receiver and new-target root slots at the host native boundary.
//! - Young-state, exact collection-count and post-move payload observations.
//!
//! # Invariants
//! - Only NativeCallInfo owns the marker during the measured allocation.
//! - No shared Value reference or raw payload borrow crosses collection.
//! - Offset inequality precedes any post-collection type/header access.
//!
//! # See also
//! - NativeCallRoots registers the call metadata for with_host_context.
//! - The existing native_call_info_slots_follow_moving_gc covers forced minors.

use super::{NativeCallInfo, NativeCtx};
use crate::{Interpreter, Value, array};

fn metadata_value<const NEW_TARGET: bool>(ctx: &NativeCtx<'_>) -> Value {
    if NEW_TARGET {
        *ctx.new_target().expect("constructor metadata slot")
    } else {
        *ctx.this_value()
    }
}

fn exercise_metadata_slot<const NEW_TARGET: bool>() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    // Prime the ordinary object shape through the same public native owner.
    // This precedes marker construction and never changes the stress policy.
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.alloc_object()
            .expect("prime ordinary object allocation");
    });
    let marker = vm.with_handle_scope(|vm, scope| {
        let value = vm.scoped_value(scope, Value::number_i32(731));
        let done = vm.scoped_value(scope, Value::boolean(true));
        let array = vm
            .scoped_array_from(scope, &[value, done])
            .expect("young metadata marker");
        vm.escape_scoped(array)
    });
    // NativeCallInfo accepts metadata values directly; this test exercises its
    // storage contract, not JavaScript IsConstructor validation or dispatch.
    let info = if NEW_TARGET {
        NativeCallInfo::construct(Value::undefined(), Some(marker))
    } else {
        NativeCallInfo::call(marker)
    };
    NativeCtx::with_host_context(&mut vm, info, None, |ctx| {
        assert_eq!(ctx.is_construct_call(), NEW_TARGET);
        let before = metadata_value::<NEW_TARGET>(ctx)
            .as_raw_gc()
            .expect("metadata marker is a heap cell");
        // SAFETY: the freshly allocated marker is rooted by the active native
        // call metadata; no collection occurred after its final shell allocation.
        assert!(
            unsafe { (*before.as_header_ptr()).is_young() },
            "metadata marker must be young immediately before allocation"
        );
        let minor_before = ctx.heap_mut().gc_stats().minor_gc_cycles;
        let full_before = ctx.heap_mut().gc_stats().gc_cycles;
        ctx.heap_mut().set_gc_stress(1, false);
        ctx.alloc_object()
            .expect("one collecting native allocation");
        assert_eq!(ctx.heap_mut().gc_stats().minor_gc_cycles, minor_before + 1);
        assert_eq!(ctx.heap_mut().gc_stats().gc_cycles, full_before);
        let moved = metadata_value::<NEW_TARGET>(ctx);
        assert_ne!(
            moved.as_raw_gc().expect("rewritten metadata marker").0,
            before.0,
            "native call metadata must be reloaded after the allocating minor"
        );
        let moved = moved.as_array().expect("relocated metadata marker array");
        array::with_elements(moved, ctx.heap(), |values| {
            assert_eq!(values, &[Value::number_i32(731), Value::boolean(true)]);
        });
    });
}

#[test]
fn native_call_receiver_reloads_after_one_allocating_minor() {
    exercise_metadata_slot::<false>();
}

#[test]
fn native_call_new_target_reloads_after_one_allocating_minor() {
    exercise_metadata_slot::<true>();
}
