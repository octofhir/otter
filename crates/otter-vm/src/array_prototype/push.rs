//! Handle-scoped `Array.prototype.push`.
//!
//! # Contents
//! - Native entry and internal dispatcher entry into one scoped spec driver.
//! - Dense append guards and generic indexed/length `Set` operations.
//!
//! # Invariants
//! - The receiver and all pending arguments are handle indices across every
//!   allocating or reentrant step. Raw operands are read only for that step.
//! - Dense growth owns its pending-value root; the surrounding handle arena
//!   keeps later arguments and the caller's receiver current.
//! - Generic setters run once, in index order, followed by the throwing length
//!   write, including when a setter has moved the receiver.
//!
//! # See also
//! - [`crate::handles`] for collector-rewritten local handles.
//! - [`crate::array`] for the dense slab reservation and write barriers.

use crate::native_abi::CommittedValueError;
use smallvec::SmallVec;

use crate::handles::Local;
use crate::{
    ActivationStack, ExecutionContext, Interpreter, NativeCtx, NativeError, Value, VmError,
};

use super::{format_index_key, length_of_array_like};

pub(super) fn native_push(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if ctx.this_value().is_null() || ctx.this_value().is_undefined() {
        return Err(NativeError::TypeError {
            name: "push",
            reason: "Array.prototype method called on null or undefined".to_string(),
        });
    }
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "push",
            reason: "Array.prototype method requires an execution context".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        let arguments: SmallVec<[Local<'_>; 8]> =
            args.iter().map(|value| scope.value(*value)).collect();
        scope.with_turn_parts(|interp, stack| {
            interp
                .array_push_scoped(stack, &context, receiver, &arguments)
                .map_err(|error| error.into_native(interp, "push"))
        })
    })
}

impl Interpreter {
    /// Enter the shared push driver from a resolved internal builtin call.
    pub(crate) fn array_push(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        _roots: &[&[Value]],
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let receiver = interp.scoped_value(scope, receiver);
            let arguments: SmallVec<[Local<'_>; 8]> = args
                .iter()
                .map(|value| interp.scoped_value(scope, *value))
                .collect();
            interp.array_push_scoped(stack, context, receiver, &arguments)
        })
    }

    fn array_push_scoped(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Local<'_>,
        arguments: &[Local<'_>],
    ) -> Result<Value, CommittedValueError> {
        let value = self.escape_scoped(receiver);
        if !value.is_object_type() {
            let boxed = self
                .box_sloppy_this_primitive_runtime_rooted(value, &[])
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            // This driver owns the receiver slot; replace its primitive with
            // ToObject's result immediately, before another allocation.
            self.set_scoped(receiver, boxed);
        }
        if let Some(array) = self.escape_scoped(receiver).as_array() {
            let start = crate::array::len(array, self.gc_heap());
            let end = start.saturating_add(arguments.len());
            if !self.array_index_accessor_protector
                && crate::array::length_writable(array, self.gc_heap())
                && crate::array::can_fast_fill_dense_range(array, self.gc_heap(), start, end)
            {
                for &argument in arguments {
                    let array = self
                        .escape_scoped(receiver)
                        .as_array()
                        .expect("push receiver is an array");
                    let value = self.escape_scoped(argument);
                    crate::array::push_with_roots(array, self.gc_heap_mut(), value, &mut |_| {})
                        .map_err(VmError::from)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                }
                return Ok(Value::number_f64(end as f64));
            }
        }
        let value = self.escape_scoped(receiver);
        let length = length_of_array_like(self, stack, context, &value)? as f64;
        if length + arguments.len() as f64 > 9_007_199_254_740_991.0 {
            return Err(CommittedValueError::JavaScript(
                self.err_type("Pushing too many elements onto an array-like".into()),
            ));
        }
        let mut next = length;
        for &argument in arguments {
            let key = format_index_key(next);
            let value = self.escape_scoped(argument);
            let object = self.escape_scoped(receiver);
            if let Some(array) = object.as_array() {
                if !self.array_ordinary_set_own(stack, context, array, &key, value)? {
                    let array = self
                        .escape_scoped(receiver)
                        .as_array()
                        .expect("push receiver is an array");
                    let message = if crate::array::is_extensible(array, self.gc_heap()) {
                        format!("Cannot assign to read only property '{key}'")
                    } else {
                        format!("Cannot add property {key}, object is not extensible")
                    };
                    return Err(CommittedValueError::JavaScript(
                        self.err_type(message.into()),
                    ));
                }
            } else {
                self.array_set_property_throwing(stack, context, object, &key, value)?;
            }
            next += 1.0;
        }
        let object = self.escape_scoped(receiver);
        self.array_set_length_throwing(stack, context, object, next)?;
        Ok(Value::number_f64(next))
    }
}

#[cfg(test)]
mod tests;
