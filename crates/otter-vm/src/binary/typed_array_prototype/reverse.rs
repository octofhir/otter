//! Controlled TypedArray reverse using fixed-width raw element bands.
//!
//! # Contents
//! - Initial view validation and captured element count.
//! - Bounded outer-band exchanges with pair-based work accounting.
//! - Fixed-width element swaps preserving internal bytes and NaN payloads.
//!
//! # Invariants
//! - No element is decoded and no BigInt or scratch buffer is allocated.
//! - Observing batches touch at most 64 KiB; enforcing policies check each pair.
//! - Completed pairs advance from the outside inward; an odd middle stays untouched.
//! - No buffer borrow crosses a checkpoint; the receiver is reloaded after it.
//! - Checkpoints never drain JavaScript jobs, and reverse never coerces arguments.
//!
//! # See also
//! - `super::validate_typed_array` owns live receiver admission.

use super::{receiver, validate_typed_array};
use crate::{NativeCtx, NativeError, Value, ValueWindow};

#[cfg(test)]
mod tests;

const REVERSE_BATCH_BYTES: usize = 64 * 1024;
const NAME: &str = "TypedArray.prototype.reverse";

pub(super) fn run(ctx: &mut NativeCtx<'_>, _args: ValueWindow<'_>) -> Result<Value, NativeError> {
    let view = receiver(ctx)?;
    validate_typed_array(&view, ctx.heap())?;
    let len = view.length(ctx.heap());
    let pairs = len / 2;
    let width = view.kind().bytes_per_element();
    let offset = view.raw_byte_offset(ctx.heap());
    let batch_limit = if ctx.cx.interp.work_budget().enforces_on_exceedance() {
        1
    } else {
        REVERSE_BATCH_BYTES / (2 * width)
    };
    let mut done = 0;
    while done < pairs {
        let batch = (pairs - done).min(batch_limit);
        ctx.cx
            .interp
            .charge_native_loop_work(batch as u64)
            .map_err(|error| {
                crate::native_function::vm_to_native_error(ctx.cx.interp, error, NAME)
            })?;
        let left_start = offset + done * width;
        let right_start = offset + (len - done - batch) * width;
        let buffer = receiver(ctx)?.buffer(ctx.heap());
        buffer.with_bytes_mut(ctx.heap_mut(), |bytes| {
            let (lower, upper) = bytes.split_at_mut(right_start);
            let left = &mut lower[left_start..left_start + batch * width];
            let right = &mut upper[..batch * width];
            match width {
                1 => swap_bands::<1>(left, right),
                2 => swap_bands::<2>(left, right),
                4 => swap_bands::<4>(left, right),
                8 => swap_bands::<8>(left, right),
                _ => unreachable!("typed-array element width"),
            }
        });
        done += batch;
    }
    Ok(*ctx.this_value())
}

#[inline]
fn swap_bands<const WIDTH: usize>(left: &mut [u8], right: &mut [u8]) {
    let (left, left_remainder) = left.as_chunks_mut::<WIDTH>();
    let (right, right_remainder) = right.as_chunks_mut::<WIDTH>();
    debug_assert!(left_remainder.is_empty() && right_remainder.is_empty());
    left.swap_with_slice(right);
    left.reverse();
    right.reverse();
}
