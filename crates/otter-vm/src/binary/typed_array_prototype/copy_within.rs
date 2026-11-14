//! Ordered TypedArray copyWithin with cancellable, bit-preserving byte moves.
//!
//! # Contents
//! - Initial length capture and target/start/end coercions in source order.
//! - Live bounds validation only for an initially positive copy count.
//! - Directional memmove batches or enforcing per-byte checkpoints.
//!
//! # Invariants
//! - Both fixed and tracking views are validated after coercion when required.
//! - Empty initial ranges return without another buffer validation.
//! - Copies preserve raw bits, including NaN payloads and BigInt representations.
//! - Overlapping destinations above the source are copied from the end.
//! - Observing batches are at most 64 KiB; enforcing policies check each byte.
//! - No buffer borrow crosses a checkpoint; the current receiver is re-read after it.
//! - Yields retain the current JS turn; rejection preserves the directional byte prefix.
//!
//! # See also
//! - `super::validate_typed_array` owns the common live-view check.
//! - <https://tc39.es/ecma262/#sec-%typedarray%.prototype.copywithin>

use super::{clamp_relative_index, receiver, to_integer_or_infinity_arg, validate_typed_array};
use crate::{NativeCtx, NativeError, Value, ValueWindow};

#[cfg(test)]
mod tests;

const COPY_BATCH_BYTES: usize = 64 * 1024;
const NAME: &str = "TypedArray.prototype.copyWithin";

pub(super) fn run(ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>) -> Result<Value, NativeError> {
    let view = receiver(ctx)?;
    validate_typed_array(&view, ctx.heap())?;
    let len = view.length(ctx.heap()) as i64;
    let target = to_integer_or_infinity_arg(ctx, args.first(), 0, NAME)?;
    let start = to_integer_or_infinity_arg(ctx, args.get(1), 0, NAME)?;
    let end = to_integer_or_infinity_arg(ctx, args.get(2), len, NAME)?;
    let to = clamp_relative_index(target, len);
    let from = clamp_relative_index(start, len);
    let end = clamp_relative_index(end, len);
    let count = (end - from).min(len - to).max(0);
    if count == 0 {
        return Ok(*ctx.this_value());
    }
    let view = receiver(ctx)?;
    validate_typed_array(&view, ctx.heap())?;
    let live_len = view.length(ctx.heap()) as i64;
    let count = count.min(live_len - from).min(live_len - to).max(0) as usize;
    let width = view.kind().bytes_per_element();
    let offset = view.raw_byte_offset(ctx.heap());
    let source = offset + from as usize * width;
    let target = offset + to as usize * width;
    let count = count * width;
    let backward = source < target && target < source + count;
    let batch_limit = if ctx.cx.interp.work_budget().enforces_on_exceedance() {
        1
    } else {
        COPY_BATCH_BYTES
    };
    let mut copied = 0;
    while copied < count {
        let batch = (count - copied).min(batch_limit);
        let index = if backward {
            count - copied - batch
        } else {
            copied
        };
        ctx.cx
            .interp
            .charge_native_loop_work(batch as u64)
            .map_err(|error| {
                crate::native_function::vm_to_native_error(ctx.cx.interp, error, NAME)
            })?;
        let buffer = receiver(ctx)?.buffer(ctx.heap());
        buffer.with_bytes_mut(ctx.heap_mut(), |bytes| {
            bytes.copy_within(source + index..source + index + batch, target + index);
        });
        copied += batch;
    }
    Ok(*ctx.this_value())
}
