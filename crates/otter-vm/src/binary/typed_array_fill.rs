//! Non-reentrant typed-array fill stores shared by the two fill protocols.
//!
//! # Contents
//! - Admission after each caller completes its own coercion/validation protocol.
//! - Scalar encoding shared with ordinary integer-indexed stores.
//! - Bounded byte batches with native work and cancellation checkpoints.
//!
//! # Invariants
//! - Only matching Number/BigInt primitives and live fixed local views are admitted.
//! - Every requested index is within the live view. Declines have no effects.
//! - Observing checkpoints, encoding and byte writes cannot collect or enter
//!   JavaScript; no buffer borrow crosses a checkpoint.
//! - Enforcing policies use the caller's per-store loop, preserving partial
//!   completion, resource checks and repeated cooperative yields.
//!
//! # See also
//! - `crate::array_prototype::mutate` owns the Array protocol and scoped operands.
//! - `super::typed_array_prototype::fill` owns TypedArray preparation and roots.
//! - `crate::binary::typed_array::TypedArrayKind::write` owns scalar encoding.

use crate::{Interpreter, Value, VmError};

/// Maximum bytes committed between cancellation checks.
const FILL_BATCH_BYTES: usize = 64 * 1024;

impl Interpreter {
    pub(crate) fn try_fill_typed_array(
        &mut self,
        receiver: Value,
        value: Value,
        start: usize,
        end: usize,
    ) -> Result<bool, VmError> {
        if self.work_budget().enforces_on_exceedance() {
            return Ok(false);
        }
        let Some(view) = receiver.as_typed_array(self.gc_heap()) else {
            return Ok(false);
        };
        let kind = view.kind();
        let heap = self.gc_heap();
        let buffer = view.buffer(heap);
        let matching_primitive = if kind.is_bigint() {
            value.as_big_int().is_some()
        } else {
            value.as_number().is_some()
        };
        if !matching_primitive
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
