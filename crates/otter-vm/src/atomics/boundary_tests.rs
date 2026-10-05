//! Actual waitAsync diagnostic allocation failure keeps its native cause.
//!
//! # Contents
//! - Oversized wait result label through the production scoped builder.
//! - Real full collection rewriting unrelated child and alias roots.
//!
//! # Invariants
//! - Physical allocation exceeds the actual configured heap cap.
//! - No allocator mock, manual root slice or synthetic OOM flag is used.
//!
//! # See also
//! - `super::wait_async_result` owns result publication.
//! - Runtime async_native_methods tests execute actual notify completion jobs.

use super::*;
use crate::{Interpreter, NativeCallInfo};

#[test]
fn wait_async_result_oom_preserves_fields_and_moved_child_aliases() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("capped wait fixture");
    vm.gc_heap_mut().set_gc_stress(0, true);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let child = scope.object().expect("young child");
            let marker = scope.number(719.0);
            scope.set(child, "marker", marker).unwrap();
            let alias = scope.value(scope.raw(child));
            let old_offset = scope.raw(child).as_object().unwrap().offset();
            let before = scope.context().heap().gc_cycle_counts();
            let label = "x".repeat(cap as usize);
            let error = super::wait_async_result(scope.context(), false, &label)
                .expect_err("actual result-label allocation cannot fit cap");
            assert!(
                matches!(error, NativeError::ExecutionFailure(crate::RunError {
                error: crate::VmError::OutOfMemory { requested_bytes, heap_limit_bytes }, .. })
                if requested_bytes > cap && heap_limit_bytes == cap),
                "{error:?}"
            );
            let after = scope.context().heap().gc_cycle_counts();
            assert!(after.1 > before.1, "real cap-triggered full collection");
            assert_ne!(scope.raw(child).as_object().unwrap().offset(), old_offset);
            assert_eq!(scope.raw(child), scope.raw(alias));
            let marker = scope.get(child, "marker").unwrap();
            assert_eq!(scope.raw(marker).as_number().unwrap().as_f64(), 719.0);
        })
    });
}
