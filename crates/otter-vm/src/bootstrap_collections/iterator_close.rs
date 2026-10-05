//! IteratorClose after an abrupt Map/Set constructor step.
//!
//! # Contents
//! - The single scoped preservation extent for the incoming completion.
//! - Exact completed terminal failure from cleanup.
//!
//! # Invariants
//! - The iterator and incoming thrown value remain collector-traced handles.
//! - A catchable cleanup throw is suppressed only to preserve the incoming
//!   completion, including its owned detail and source-frame provenance.
//! - A completed terminal failure during cleanup leaves with its actual
//!   cause; no original error is restored over that new failure.
//!
//! # See also
//! - `super::populate_from_iterable` and `super::build_adder_args`.
//! - `crate::native_function::vm_to_native_error` owns projection.

use crate::{ExecutionContext, Local, NativeError, NativeScope};

pub(super) fn preserving_completion<'s>(
    scope: &mut NativeScope<'s, '_>,
    context: Option<&ExecutionContext>,
    iterator: Local<'s>,
    name: &'static str,
) -> Result<(), NativeError> {
    let iterator = scope.raw(iterator);
    scope.with_turn_parts(|interp, stack| {
        interp
            .iterator_close_discarding_completion(stack, context, &iterator)
            .map_err(|error| error.into_native(interp, name))
    })
}

#[cfg(test)]
mod tests;
