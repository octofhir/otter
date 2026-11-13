//! Array join with primitive snapshot admission and live reentrant reads.
//!
//! # Contents
//! - Join driver, ordered length/separator/element coercion and UTF-16 output.
//! - Existing Latin-1 fast path for contiguous primitive strings.
//!
//! # Invariants
//! - A dense snapshot admits only own primitive data elements: no conversion
//!   can run JavaScript and change a later indexed Get.
//! - Object/function coercions, holes, getters and proxies use live reads.
//! - Receiver and separator are scoped before boxing or coercion. The scope
//!   closes on normal and abrupt completion; it
//!   never leaves a manual JSON handle in an enclosing activation.
//! - Existing primitive snapshot roots follow moving collection during number
//!   formatting; reentrant receivers use the handle arena.
//! - Output keeps UTF-16 units, including unpaired surrogates, end to end.
//! - Materialization retains the existing Array-driver length ceiling.
//!
//! # See also
//! - `super::Interpreter::array_to_string` calls the receiver's currently resolved join.
//! - `super::join_rooting_tests` checks collecting abrupt cleanup.

use crate::native_abi::CommittedValueError;
use crate::rooting::RootScopeExt;
use crate::string::JsString;
use crate::{ActivationStack, ExecutionContext, Interpreter, Value};

use super::{MAX_ARRAY_LIKE_PROBE_LEN, length_of_array_like};

impl Interpreter {
    /// §23.1.3.18 live `Array.prototype.join` over a generic array-like receiver.
    pub(crate) fn array_join(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        separator_arg: Option<Value>,
        roots: &mut [&mut [Value]],
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            // Root both inputs before ToObject: boxing a primitive can collect
            // while an object separator is still waiting for its ToString.
            let receiver_root = interp.scoped_value(scope, receiver);
            let separator_root =
                interp.scoped_value(scope, separator_arg.unwrap_or(Value::undefined()));
            let receiver = interp.escape_scoped(receiver_root);
            let o = if receiver.is_object_type() {
                receiver
            } else {
                interp
                    .box_sloppy_this_primitive_runtime_rooted(receiver, roots)
                    .map_err(CommittedValueError::JavaScript)?
            };
            let o_root = interp.scoped_value(scope, o);
            // §23.1.3.16 step 2 — len = ? LengthOfArrayLike(O). Reads
            // `O.length` through `[[Get]]`, so a `get length()` accessor
            // fires here exactly once.
            let len = length_of_array_like(interp, stack, context, &o)?;
            // §23.1.3.16 step 3 — sep = (separator is undefined) ? ","
            // : ? ToString(separator). Ordered AFTER the length read.
            // Kept as UTF-16 units end to end: a lossy Rust-string detour would
            // replace lone surrogates with U+FFFD, which join must preserve.
            let separator_value = interp.escape_scoped(separator_root);
            let separator: Vec<u16> = if separator_value.is_undefined() {
                vec![b',' as u16]
            } else {
                let js =
                    crate::coerce::to_js_string_or_throw(interp, stack, context, &separator_value)?;
                js.to_utf16_vec(&interp.gc_heap)
            };
            // The existing materialization ceiling bounds the retained parts
            // buffer for a pathological LengthOfArrayLike result.
            let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
            if cap == 0 {
                return Ok(Value::string(
                    JsString::from_str("", interp.gc_heap_mut())
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?,
                ));
            }
            // §23.1.3.16 steps 4-8 visit EVERY index 0..len through
            // `[[Get]]` — a pre-collected present-index walk is unsound
            // because an element's ToString can install prototype indices
            // mid-join (`Object.prototype[3] = ...` inside a `toString`)
            // that later Gets must observe. Only primitive own data can bypass
            // those reads without a reentrant conversion changing their results.
            let indices: Vec<usize> = (0..cap).collect();
            let mut parts: Vec<Vec<u16>> = vec![Vec::new(); cap];
            // Snapshot only primitive dense elements: their ToString cannot run
            // JavaScript or mutate a later index. Object/function elements keep
            // indexed reads live across reentrant coercion. Read the snapshot in
            // ONE payload read before touching the allocator.
            // `Get(O, ToString(k))` interns each index-key string, and
            // that allocation can trigger a young-gen scavenge that relocates the
            // (young) receiver array mid-loop — reading `o` again afterwards then
            // dereferences a moved-from cell. A plain dense primitive array has no
            // observable getters or conversions; anything exotic
            // (accessors / sparse / prototype indices) keeps the full `[[Get]]`
            // ladder, rooting the receiver across each allocating read.
            let o = interp.escape_scoped(o_root);
            let dense_snapshot: Option<Vec<Value>> = o.as_array().and_then(|arr| {
                interp.gc_heap.read_payload(arr, |body| {
                    // Plain dense array only (no accessors / sparse / prototype
                    // override / named props), then check every value's conversion.
                    if !body.exotic.is_null() {
                        return None;
                    }
                    let mut elems = Vec::with_capacity(cap);
                    for k in 0..cap {
                        let v = body.dense_value(k).unwrap_or(Value::hole());
                        // A hole is only join-equivalent to `undefined` when
                        // no prototype supplies the index — `Get(O, k)` walks
                        // the chain, so any gap sends us to the generic
                        // per-index `[[Get]]` ladder below.
                        if v.is_hole() || v.is_object_type() {
                            return None;
                        }
                        elems.push(v);
                    }
                    Some(elems)
                })
            });
            // Latin-1 strings (and holes, `undefined` and `null`, which join as
            // nothing) with a Latin-1 separator: no ToString runs, so the result
            // is their bytes, written once.
            if let Some(elems) = dense_snapshot.as_ref()
                && let Some(joined) = join_latin1_strings(elems, &separator, &interp.gc_heap)
            {
                return JsString::from_latin1(&joined, interp.gc_heap_mut())
                    .map(Value::string)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()));
            }
            if let Some(mut elems) = dense_snapshot {
                // An element's ToString can scavenge; the not-yet-visited snapshot
                // values must follow their cells.
                let mut element_roots = otter_gc::RootScope::new(&mut interp.gc_heap);
                // SAFETY: `elems` precedes the scope and never reallocates under it.
                unsafe { element_roots.add_value_vec(&mut elems) };
                for k in 0..elems.len() {
                    let v = elems[k];
                    if v.is_hole() || v.is_undefined() || v.is_null() {
                        continue;
                    }
                    let js = crate::coerce::to_js_string_or_throw(interp, stack, context, &v)?;
                    parts[k] = js.to_utf16_vec(&interp.gc_heap);
                }
            } else {
                for k in indices {
                    if k >= cap {
                        continue;
                    }
                    let o_cur = interp.escape_scoped(o_root);
                    let v = interp.get_property_value_for_call(
                        stack,
                        context,
                        o_cur,
                        &k.to_string(),
                    )?;
                    if v.is_undefined() || v.is_null() {
                        parts[k] = Vec::new();
                    } else {
                        let js = crate::coerce::to_js_string_or_throw(interp, stack, context, &v)?;
                        parts[k] = js.to_utf16_vec(interp.gc_heap());
                    }
                }
            }
            let total: usize = parts.iter().map(Vec::len).sum::<usize>()
                + separator.len() * (parts.len().saturating_sub(1));
            let mut joined: Vec<u16> = Vec::with_capacity(total);
            for (k, part) in parts.iter().enumerate() {
                if k > 0 {
                    joined.extend_from_slice(&separator);
                }
                joined.extend_from_slice(part);
            }
            Ok(Value::string(
                JsString::from_utf16_units(&joined, interp.gc_heap_mut())
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?,
            ))
        })
    }
}

/// The bytes `join` of `elements` with `separator` produces when every
/// element is a contiguous Latin-1 string, a hole, `undefined` or `null` and
/// the separator is Latin-1; `None` for anything else, which takes the
/// ToString of each element.
fn join_latin1_strings(
    elements: &[Value],
    separator: &[u16],
    heap: &otter_gc::GcHeap,
) -> Option<Vec<u8>> {
    let separator: Vec<u8> = separator
        .iter()
        .map(|&unit| u8::try_from(unit).ok())
        .collect::<Option<_>>()?;
    let mut joined = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        if index > 0 {
            joined.extend_from_slice(&separator);
        }
        if element.is_hole() || element.is_undefined() || element.is_null() {
            continue;
        }
        element
            .as_string(heap)?
            .with_latin1(heap, |bytes| joined.extend_from_slice(bytes))?;
    }
    Some(joined)
}
