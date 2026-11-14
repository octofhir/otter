//! Ordered TypedArray fill preparation and controlled stores.
//!
//! # Contents
//! - Initial receiver/length validation, value conversion, then start/end conversion.
//! - Unconditional live buffer validation, including empty ranges.
//! - Shared non-reentrant bulk admission or per-element work checkpoints.
//!
//! # Invariants
//! - Value conversion precedes bound conversion and runs exactly once.
//! - The converted BigInt stays anchored through bound coercions and checkpoints.
//! - Length is captured before coercion; writes clamp to the live validated view.
//! - Enforcing budgets checkpoint before each store and preserve its committed prefix.
//! - Receiver and payload are re-read after checkpoints; fatal errors remain fatal.
//!
//! # See also
//! - `super::ta_coerce_value` owns element conversion.
//! - `crate::binary::typed_array_fill` owns shared scalar encoding and byte batches.

use super::{
    clamp_relative_index, receiver, ta_coerce_value, to_integer_or_infinity_arg,
    validate_typed_array, with_anchors,
};
use crate::{NativeCtx, NativeError, Value, ValueWindow};

#[cfg(test)]
mod tests;

pub(super) fn run(ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>) -> Result<Value, NativeError> {
    let name = "TypedArray.prototype.fill";
    let view = receiver(ctx)?;
    validate_typed_array(&view, ctx.heap())?;
    let len = view.length(ctx.heap()) as i64;
    let raw_value = args.first().copied().unwrap_or(Value::undefined());
    let value = ta_coerce_value(ctx, view.kind(), &raw_value)?;
    with_anchors(ctx, |ctx, base| {
        ctx.cx.interp.push_iteration_anchor(value);
        let start = to_integer_or_infinity_arg(ctx, args.get(1), 0, name)?;
        let end = to_integer_or_infinity_arg(ctx, args.get(2), len, name)?;
        let start = clamp_relative_index(start, len) as usize;
        let end = clamp_relative_index(end, len) as usize;
        // Re-take and validate the buffer witness after all coercions, even
        // when the range is empty. Either view kind can become out of bounds.
        let view = receiver(ctx)?;
        validate_typed_array(&view, ctx.heap())?;
        let end = end.min(view.length(ctx.heap()));
        if start >= end {
            return Ok(*ctx.this_value());
        }
        let value = ctx.cx.interp.iteration_anchor(base);
        let receiver_value = *ctx.this_value();
        let filled = ctx
            .cx
            .interp
            .try_fill_typed_array(receiver_value, value, start, end)
            .map_err(|error| {
                crate::native_function::vm_to_native_error(ctx.cx.interp, error, name)
            })?;
        if !filled {
            for index in start..end {
                ctx.cx
                    .interp
                    .charge_native_loop_iteration()
                    .map_err(|error| {
                        crate::native_function::vm_to_native_error(ctx.cx.interp, error, name)
                    })?;
                let view = receiver(ctx)?;
                let value = ctx.cx.interp.iteration_anchor(base);
                view.set(ctx.heap_mut(), index, &value);
            }
        }
        Ok(*ctx.this_value())
    })
}
