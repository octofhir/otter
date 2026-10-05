//! Scoped arithmetic's primitive concat formatting and cache owner.
//!
//! # Contents
//! - Traced small-integer string cache and canonical primitive conversion.
//! - One-result short Latin-1/int32 concatenation without a temporary string.
//!
//! # Invariants
//! The VM alone owns conversion and caching. The short path copies source bytes
//! into fixed stack storage before allocation; its integer input cannot move.
//! The general caller keeps every string operand in its handle arena.
//!
//! # See also
//! - `crate::runtime_stubs::string` owns compiled collecting root publication.

use crate::{Interpreter, JsString, Value, VmError, conversion, number};

impl Interpreter {
    /// Upper bound (exclusive) of the cached small-integer decimal strings.
    pub(crate) const SMALL_INT_STRING_CACHE: i32 = 1024;

    /// GC-traced cached small-integer decimal strings.
    pub(crate) fn small_int_strings_for_trace(&self) -> impl Iterator<Item = &Value> {
        self.small_int_string_cache.iter().flatten()
    }

    /// Decimal string for a small non-negative integer, served from the
    /// `SmallStrings`-style cache. Allocates and caches on first use; returns
    /// the shared immutable handle thereafter. `None` for inputs outside
    /// `0..SMALL_INT_STRING_CACHE` (the caller falls back to `number_to_string`).
    pub(crate) fn small_int_string(&mut self, i: i32) -> Result<Option<JsString>, VmError> {
        if !(0..Self::SMALL_INT_STRING_CACHE).contains(&i) {
            return Ok(None);
        }
        if let Some(cached) = self.small_int_string_cache[i as usize] {
            return Ok(cached.as_string(&self.gc_heap));
        }
        let s = number::ecma::number_to_string(f64::from(i), &mut self.gc_heap)
            .map_err(VmError::from)?;
        self.small_int_string_cache[i as usize] = Some(Value::string(s));
        Ok(Some(s))
    }

    /// `ToString` of a primitive operand for string concatenation, routing small
    /// non-negative integers through the [`Self::small_int_string`] cache to
    /// avoid re-allocating their decimal text on every concatenation.
    pub(crate) fn js_string_for_concat(&mut self, value: Value) -> Result<JsString, VmError> {
        if let Some(n) = value.as_number() {
            let f = n.as_f64();
            if f >= 0.0
                && f < Self::SMALL_INT_STRING_CACHE as f64
                && f.fract() == 0.0
                && let Some(s) = self.small_int_string(f as i32)?
            {
                return Ok(s);
            }
        }
        conversion::to_js_string_primitive(&value, self.gc_heap_mut())
    }

    /// One-allocation concat for `<short flat latin1 string> + <int32>` and its
    /// mirror — the common key-building shape (`"k" + n`). Formats the integer's
    /// ASCII digits straight into a single flat latin1 result, skipping the
    /// throwaway number string, the cons rope, and the flatten the general path
    /// would build. Returns `None` when the operands are not that shape (the
    /// caller takes the general concat path). Only exact int32-tagged operands
    /// qualify, so `ToString` semantics are unchanged. No rooting is needed: the
    /// string's bytes are copied before the result allocation and the integer is
    /// not a heap value.
    pub(crate) fn try_concat_string_int32(
        &mut self,
        lhs: Value,
        rhs: Value,
    ) -> Option<Result<Value, otter_gc::OutOfMemory>> {
        let (handle, n, number_first) = match (
            lhs.as_string(&self.gc_heap),
            rhs.as_i32(),
            lhs.as_i32(),
            rhs.as_string(&self.gc_heap),
        ) {
            (Some(string), Some(n), _, _) => (string.handle(), n, false),
            (_, _, Some(n), Some(string)) => (string.handle(), n, true),
            _ => return None,
        };
        let mut string_bytes = [0u8; 32];
        let string_len = crate::string::gc_body::read_short_flat_latin1(
            &self.gc_heap,
            handle,
            &mut string_bytes,
        )?;
        let mut digits = [0u8; crate::number::integer_fast::I32_BUF_LEN];
        let digit_len = crate::number::integer_fast::format_i32(n, &mut digits);
        let mut out = [0u8; 32 + crate::number::integer_fast::I32_BUF_LEN];
        let (first, second): (&[u8], &[u8]) = if number_first {
            (&digits[..digit_len], &string_bytes[..string_len])
        } else {
            (&string_bytes[..string_len], &digits[..digit_len])
        };
        out[..first.len()].copy_from_slice(first);
        out[first.len()..first.len() + second.len()].copy_from_slice(second);
        let total = first.len() + second.len();
        Some(
            crate::string::JsString::from_latin1(&out[..total], &mut self.gc_heap)
                .map(Value::string),
        )
    }
}
