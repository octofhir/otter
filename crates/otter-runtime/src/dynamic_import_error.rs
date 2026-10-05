//! Rooted error materialization for dynamic module loading.
//!
//! # Contents
//! - One realm-prototype and message builder for direct and extra-realm imports.
//! - Moving-collection and actual heap-cap regression proofs.
//!
//! # Invariants
//! - The native scope retains the prototype, receiver and message across every
//!   allocation and descriptor publication.
//! - Allocation refusal preserves the allocator request and limit through the
//!   canonical runtime error projection.
//! - The completed value is returned directly to the synchronous settlement
//!   owner; this module retains no raw VM value or asynchronous callback.
//!
//! # See also
//! - `crate::realm` owns extra-realm entry.
//! - `crate::map_native_error` owns native-to-runtime error projection.

use crate::OtterError;
use otter_vm::{ErrorKind, Interpreter, NativeCallInfo, NativeCtx, Value};

pub(super) fn allocate(
    interp: &mut Interpreter,
    kind: ErrorKind,
    message: &str,
) -> Result<Value, OtterError> {
    let prototype = Value::object(interp.error_classes_for_trace().prototype(kind));
    NativeCtx::with_host_context(interp, NativeCallInfo::default_call(), None, |native| {
        native.scope(|mut scope| {
            let prototype = scope.value(prototype);
            let object = scope.object_with_prototype(prototype)?;
            let message = scope.string(message)?;
            scope.define(
                object,
                "message",
                message,
                otter_vm::object::PropertyFlags::new(true, false, true),
            )?;
            Ok(scope.finish(object))
        })
    })
    .map_err(crate::map_native_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_error_message_and_prototype_survive_actual_moving_collection() {
        let mut interp = Interpreter::new().expect("error fixture bootstrap");
        interp.gc_heap_mut().set_gc_stress(1, false);
        let before = interp.gc_heap().gc_cycle_counts();
        let message = "missing module 🙂".repeat(16);
        let value = allocate(&mut interp, ErrorKind::TypeError, &message).unwrap();
        let root = NativeCtx::with_host_context(
            &mut interp,
            NativeCallInfo::default_call(),
            None,
            |native| native.persistent_root_insert(value),
        );
        let after = interp.gc_heap().gc_cycle_counts();
        assert!(
            after.0 > before.0 || after.1 > before.1,
            "real builder collection"
        );
        interp
            .force_gc()
            .expect("actual post-builder full collection");
        let (stored_message, name) = NativeCtx::with_host_context(
            &mut interp,
            NativeCallInfo::default_call(),
            None,
            |native| {
                let value = native.persistent_root_remove(root).expect("rooted error");
                native
                    .scope(|mut scope| {
                        let object = scope.value(value);
                        let message = scope.get(object, "message")?;
                        let name = scope.get(object, "name")?;
                        Ok::<_, otter_vm::NativeError>((
                            scope.string_value(message)?,
                            scope.string_value(name)?,
                        ))
                    })
                    .unwrap()
            },
        );
        assert_eq!(stored_message, message);
        assert_eq!(name, "TypeError");
    }

    #[test]
    fn error_message_cap_refusal_keeps_actual_allocation_facts() {
        let cap = 2 * 1024 * 1024;
        let mut interp = Interpreter::with_string_heap_cap(cap).expect("capped fixture bootstrap");
        interp.gc_heap_mut().set_gc_stress(0, false);
        let error =
            allocate(&mut interp, ErrorKind::TypeError, &"m".repeat(cap as usize)).unwrap_err();
        assert!(
            matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
            if requested_bytes > cap && heap_limit_bytes == cap),
            "{error:?}"
        );
        assert_eq!(error.exit_code(), 5);
    }
}
