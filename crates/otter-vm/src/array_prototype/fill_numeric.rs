//! Non-reentrant numeric typed-array stores for borrowed Array fill.
//!
//! # Contents
//! - Admission after the Array driver's length and bound coercions.
//! - Scalar encoding shared with ordinary integer-indexed stores.
//! - Bounded byte batches with native work and cancellation checkpoints.
//!
//! # Invariants
//! - Only Number values and live, fixed, local numeric views are admitted.
//! - Every requested index is within the live view. Declines have no effects.
//! - Observing checkpoints, encoding and byte writes cannot collect or enter
//!   JavaScript; no buffer borrow crosses a checkpoint.
//! - Enforcing policies use the driver's per-Set loop, preserving partial
//!   completion, resource checks and repeated cooperative yields.
//!
//! # See also
//! - `super::mutate` owns Array fill's observable protocol and scoped operands.
//! - `crate::binary::typed_array::TypedArrayKind::write` owns scalar encoding.

use crate::{Interpreter, Value, VmError};

/// Maximum bytes committed between cancellation checks.
const FILL_BATCH_BYTES: usize = 64 * 1024;

impl Interpreter {
    pub(super) fn try_fill_numeric_typed_array(
        &mut self,
        receiver: Value,
        value: Value,
        start: usize,
        end: usize,
    ) -> Result<bool, VmError> {
        if self.work_budget().enforces_on_exceedance() || value.as_number().is_none() {
            return Ok(false);
        }
        let Some(view) = receiver.as_typed_array(self.gc_heap()) else {
            return Ok(false);
        };
        let kind = view.kind();
        let heap = self.gc_heap();
        let buffer = view.buffer(heap);
        if kind.is_bigint()
            || buffer.is_shared()
            || buffer.is_resizable(heap)
            || buffer.is_detached(heap)
            || end > view.length(heap)
        {
            return Ok(false);
        }
        let width = kind.bytes_per_element();
        let offset = view.raw_byte_offset(heap);
        let mut scalar = [0u8; 8];
        kind.write(heap, &mut scalar[..width], 0, &value);
        let mut next = start;
        while next < end {
            let stop = end.min(next + FILL_BATCH_BYTES / width);
            self.charge_native_loop_work((stop - next) as u64)?;
            buffer.with_bytes_mut(self.gc_heap_mut(), |bytes| {
                let range = &mut bytes[offset + next * width..offset + stop * width];
                if width == 1 {
                    range.fill(scalar[0]);
                } else {
                    for element in range.chunks_exact_mut(width) {
                        element.copy_from_slice(&scalar[..width]);
                    }
                }
            });
            next = stop;
        }
        Ok(true)
    }
}
