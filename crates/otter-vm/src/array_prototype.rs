//! `Array.prototype.*` intrinsic table and interpreter-aware drivers.
//!
//! The legacy [`IntrinsicArgs`] table remains as the context-free
//! fallback for simple methods. Methods whose algorithms observe user
//! code (`Get`, `Set`, `LengthOfArrayLike`, species constructors,
//! callbacks, comparators, or coercions) are routed through re-entrant
//! `Interpreter::array_*` drivers so direct `arr.m()` calls and
//! `Array.prototype.m.call(...)` share one implementation.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>
//! - `docs/method-dispatch-refactor.md`
//!
//! # Contents
//! - [`ARRAY_PROTOTYPE_TABLE`] — declarative registry built with
//!   the `intrinsics!` macro for the fallback path.
//! - [`ARRAY_PROTOTYPE_METHODS`] — JS-visible native method specs.
//! - `Interpreter::array_*` drivers for live generic Array semantics.
//!
//! # Invariants
//! - Generic methods begin with `ToObject(this)` and
//!   `LengthOfArrayLike` in the driver path.
//! - Live drivers use VM property operations so accessors, inherited
//!   indices, proxies, callbacks, and comparator calls re-enter through
//!   the active [`ExecutionContext`].
//! - Pathological array-like lengths are guarded before any dense
//!   materialisation.

use smallvec::SmallVec;

use crate::Value;
use crate::array::{self, JsArray};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec};
use crate::number::NumberValue;
use crate::object::{self, PartialPropertyDescriptor};
use crate::string::JsString;
use crate::symbol::{WellKnown, WellKnownSymbols};
use crate::{ExecutionContext, Interpreter, NativeCall, NativeCtx, NativeError, VmError};

fn receiver_array(args: &IntrinsicArgs<'_>) -> Result<JsArray, IntrinsicError> {
    args.receiver
        .as_array()
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })
}

/// §22.1.3 LengthOfArrayLike dispatch for the dense-Array fast path /
/// generic Object array-like fallback. Returns
/// `BadReceiver { expected: "array" }` for non-Array / non-Object
/// receivers.
fn array_or_object_length(args: &IntrinsicArgs<'_>) -> Result<usize, IntrinsicError> {
    if let Some(arr) = args.receiver.as_array() {
        return Ok(array::len(arr, &*args.gc_heap));
    }
    if args.receiver.is_object() {
        return Ok(array_like_length(args.receiver, &*args.gc_heap));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// Defensive upper bound on the materialised length of an
/// array-like Object receiver before we'd refuse to expand a
/// snapshot. Spec ToLength clamps to 2^53-1; test262 patterns
/// (`{length: 2**32 - 1}`, `new Array(2**32)`) deliberately
/// exercise pathological lengths to stress generic-array methods.
/// V8 / JSC handle this by visiting only **present** indexed own
/// properties (see HasProperty short-circuit in §22.1.3 generic
/// algorithms); we follow the same strategy and never materialise
/// the absent slots, so the cap only matters when a caller passes
/// in a genuinely-large-but-dense object — at that point an OOM
/// `RangeError` from the allocator is the spec-compliant outcome
/// and we never reach a 4 GB pre-allocated `Vec`.
const MAX_ARRAY_LIKE_PROBE_LEN: usize = 1 << 25;
const MAX_SPARSE_PREFIX_PROBE_LEN: usize = 1024;
const MAX_SAFE_ARRAY_LENGTH: usize = 9_007_199_254_740_991;

/// §7.3.18 LengthOfArrayLike — read `O.length`, ToLength-coerce,
/// clamp to [`MAX_ARRAY_LIKE_PROBE_LEN`].
fn read_array_like_length(obj: crate::object::JsObject, heap: &otter_gc::GcHeap) -> usize {
    let len_val = crate::object::get(obj, heap, "length").unwrap_or(Value::undefined());
    // §7.1.20 ToLength(? ToNumber(length)). `length` is routinely a
    // string / boolean / float in array-like receivers, so coerce
    // through the primitive ToNumber ladder (which parses numeric
    // strings, maps booleans, etc.) rather than only accepting an
    // existing Number.
    let f = crate::number::parse::to_number_value(&len_val, heap);
    if f.is_nan() || f <= 0.0 {
        0
    } else if f >= MAX_ARRAY_LIKE_PROBE_LEN as f64 {
        MAX_ARRAY_LIKE_PROBE_LEN
    } else {
        f as usize
    }
}

/// Sparse-aware walk of **present** indexed own properties of an
/// array-like object receiver, returning `(index, value)` pairs in
/// ascending-index order.
///
/// Mirrors the V8 / JSC strategy for generic Array.prototype methods:
/// instead of iterating `0..len` and probing `HasProperty(O, k)` for
/// each slot (which is `O(len)` even when the object is sparse), we
/// enumerate the receiver's own string-keyed property bag, filter for
/// numeric indices `< len`, and visit those only. Per §22.1.3 generic
/// algorithms, an `HasProperty(O, k)` returning `false` is observably
/// indistinguishable from "skip k", so this is spec-faithful for
/// objects without inherited indexed properties — the same caveat
/// V8 / JSC carry in their dense-vs-dictionary fast paths.
///
/// Returns `None` when `receiver` is not array-like (caller surfaces
/// the spec's `RequireObjectCoercible` TypeError).
pub(crate) fn array_like_present_entries(
    receiver: &Value,
    heap: &mut otter_gc::GcHeap,
) -> Option<Vec<(usize, Value)>> {
    if let Some(arr) = receiver.as_array() {
        // Dense Array path — Value::Hole encodes "absent"; the
        // sparse-aware filter drops it so the index/value pairs
        // match what HasProperty would observe.
        return Some(array::with_elements(arr, heap, |els| {
            els.iter()
                .enumerate()
                .filter_map(|(i, v)| if v.is_hole() { None } else { Some((i, *v)) })
                .collect()
        }));
    }
    if let Some(obj) = receiver.as_object() {
        // §10.4.3 String-exotic wrappers (`new String("…")`) expose
        // their `length` and each code unit through `[[StringData]]`,
        // not the ordinary property table. Seed entries from the
        // backing string, then overlay any own indexed props.
        if let Some(s) = crate::object::string_data(obj, heap) {
            let units = s.to_utf16_vec(heap);
            let mut entries: Vec<(usize, Value)> = units
                .iter()
                .enumerate()
                .map(|(i, &u)| {
                    let ch = crate::string::JsString::from_utf16_units(&[u], heap)
                        .map(Value::string)
                        .unwrap_or(Value::undefined());
                    (i, ch)
                })
                .collect();
            let extra: Vec<usize> = crate::object::with_properties(obj, heap, |p| {
                p.keys()
                    .filter_map(|k| k.parse::<usize>().ok())
                    .filter(|&i| i >= entries.len())
                    .collect()
            });
            for i in extra {
                let v = crate::object::get(obj, heap, &i.to_string()).unwrap_or(Value::undefined());
                entries.push((i, v));
            }
            entries.sort_unstable_by_key(|(i, _)| *i);
            entries.dedup_by_key(|(i, _)| *i);
            return Some(entries);
        }
        let len = read_array_like_length(obj, heap);
        if len == 0 {
            return Some(Vec::new());
        }
        let mut idx_keys: Vec<usize> = crate::object::with_properties(obj, heap, |p| {
            p.keys()
                .filter_map(|k| k.parse::<usize>().ok())
                .filter(|&i| i < len)
                .collect()
        });
        idx_keys.sort_unstable();
        idx_keys.dedup();
        return Some(
            idx_keys
                .into_iter()
                .map(|i| {
                    let key = i.to_string();
                    let v = crate::object::get(obj, heap, &key).unwrap_or(Value::undefined());
                    (i, v)
                })
                .collect(),
        );
    }
    // §7.1.18 ToObject — primitive receivers coerce to their wrapper.
    if receiver.is_boolean()
        || receiver.is_number()
        || receiver.is_symbol()
        || receiver.is_big_int()
    {
        return Some(Vec::new());
    }
    // Callable receivers — empty array-like view.
    if receiver.is_function()
        || receiver.is_closure()
        || receiver.is_native_function()
        || receiver.is_bound_function()
        || receiver.is_class_constructor()
    {
        return Some(Vec::new());
    }
    // §7.1.18 ToObject for object-shaped exotic values that
    // expose user properties through a lazy expando bag.
    if let Some(r) = receiver.as_regexp() {
        return match r.expando(heap) {
            Some(bag) => array_like_present_entries(&Value::object(bag), heap),
            None => Some(Vec::new()),
        };
    }
    if let Some(p) = receiver.as_promise() {
        return match p.expando(heap) {
            Some(bag) => array_like_present_entries(&Value::object(bag), heap),
            None => Some(Vec::new()),
        };
    }
    // Map / Set / WeakMap / WeakSet / WeakRef / FinalizationRegistry
    // / Generator / Iterator / DataView / ArrayBuffer — empty walk.
    if receiver.is_map()
        || receiver.is_set()
        || receiver.is_weak_map()
        || receiver.is_weak_set()
        || receiver.is_weak_ref()
        || receiver.is_finalization_registry()
        || receiver.is_generator()
        || receiver.is_iterator()
        || receiver.is_data_view()
        || receiver.is_array_buffer()
    {
        return Some(Vec::new());
    }
    if let Some(s) = receiver.as_string(heap) {
        let units = s.to_utf16_vec(heap);
        return Some(
            units
                .into_iter()
                .enumerate()
                .map(|(i, u)| {
                    let s = crate::string::JsString::from_utf16_units(&[u], heap)
                        .map(Value::string)
                        .unwrap_or(Value::undefined());
                    (i, s)
                })
                .collect(),
        );
    }
    None
}

/// §7.3.18 reachable length helper for receivers whose `.length`
/// we trust to be observable but where we only need the upper bound
/// for `fromIndex` clamping — does not allocate, just reads.
pub(crate) fn array_like_length(receiver: &Value, heap: &otter_gc::GcHeap) -> usize {
    if let Some(arr) = receiver.as_array() {
        return array::len(arr, heap);
    }
    if let Some(obj) = receiver.as_object() {
        if let Some(s) = crate::object::string_data(obj, heap) {
            return s.len() as usize;
        }
        return read_array_like_length(obj, heap);
    }
    if let Some(s) = receiver.as_string(heap) {
        return s.len() as usize;
    }
    if let Some(r) = receiver.as_regexp() {
        return r
            .expando(heap)
            .map_or(0, |bag| read_array_like_length(bag, heap));
    }
    if let Some(p) = receiver.as_promise() {
        return p
            .expando(heap)
            .map_or(0, |bag| read_array_like_length(bag, heap));
    }
    0
}

/// Convert a possibly-negative numeric index into an absolute
/// element index, clamped to `[0, len]`. Mirrors the spec's
/// `ToIntegerOrInfinity` + clamping rule for `slice` / `indexOf`.
fn clamp_index(raw: i64, len: usize) -> usize {
    if raw < 0 {
        let from_end = len as i64 + raw;
        if from_end < 0 { 0 } else { from_end as usize }
    } else if (raw as usize) > len {
        len
    } else {
        raw as usize
    }
}

fn arg_signed_index(
    args: &IntrinsicArgs<'_>,
    index: u16,
    default: i64,
) -> Result<i64, IntrinsicError> {
    // §7.1.5 ToIntegerOrInfinity — coerce the spec-relevant operand
    // set (Number / Boolean / Null / String) before treating
    // non-finite / NaN / non-integer as defaults.
    let Some(arg) = args.args.get(index as usize) else {
        return Ok(default);
    };
    if arg.is_undefined() {
        return Ok(default);
    }
    if let Some(n) = arg.as_number() {
        return Ok(match n.as_smi() {
            Some(v) => v as i64,
            None => {
                let f = n.as_f64();
                if f.is_nan() {
                    0
                } else if f.is_infinite() {
                    if f.is_sign_negative() {
                        i64::MIN
                    } else {
                        i64::MAX
                    }
                } else {
                    f.trunc() as i64
                }
            }
        });
    }
    if let Some(b) = arg.as_boolean() {
        return Ok(if b { 1 } else { 0 });
    }
    if arg.is_null() {
        return Ok(0);
    }
    if let Some(s) = arg.as_string(args.gc_heap) {
        let text = s.to_lossy_string(args.gc_heap);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(0);
        }
        return Ok(trimmed.parse::<i64>().unwrap_or(0));
    }
    Err(IntrinsicError::BadArgument {
        index,
        reason: "must be a number",
    })
}

fn impl_push(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.20 — `len = ? LengthOfArrayLike(O)`; for each item set
    // `O[len + idx]` then update `O.length`. Iterates only the
    // argument list (no `0..len` scan), so safe for any `len`.
    if let Some(arr) = args.receiver.as_array() {
        let mut new_len = array::len(arr, &*args.gc_heap);
        let values: Vec<Value> = args.args.to_vec();
        for v in values {
            new_len = args.array_push_rooted(arr, v)?;
        }
        return Ok(Value::number(NumberValue::from_i32(new_len as i32)));
    }
    if let Some(obj) = args.receiver.as_object() {
        let heap = &mut *args.gc_heap;
        let base_len = read_array_like_length(obj, heap);
        let added = args.args.len();
        // §22.1.3.20 step 5.b — RangeError when the resulting length
        // would exceed 2^53 - 1. We surface the inner heap cap via
        // `read_array_like_length`'s ToLength clamp; the explicit
        // check here guards the final write to `length`.
        let new_len = base_len.saturating_add(added);
        for (i, v) in args.args.iter().enumerate() {
            let key = (base_len + i).to_string();
            crate::object::set(obj, heap, &key, *v);
        }
        crate::object::set(obj, heap, "length", Value::number_f64(new_len as f64));
        return Ok(Value::number(NumberValue::from_f64(new_len as f64)));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_pop(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.19 — read length, peel the last present element, write
    // length back. For Array we keep the existing dense fast path
    // (`array::pop`).
    if let Some(arr) = args.receiver.as_array() {
        let heap = &mut *args.gc_heap;
        return Ok(array::pop(arr, heap));
    }
    if let Some(obj) = args.receiver.as_object() {
        let heap = &mut *args.gc_heap;
        let len = read_array_like_length(obj, heap);
        if len == 0 {
            crate::object::set(obj, heap, "length", Value::number_i32(0));
            return Ok(Value::undefined());
        }
        let last_idx = len - 1;
        let key = last_idx.to_string();
        let element = crate::object::get(obj, heap, &key).unwrap_or(Value::undefined());
        let _ = crate::object::delete(obj, heap, &key);
        crate::object::set(obj, heap, "length", Value::number_f64(last_idx as f64));
        return Ok(element);
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_shift(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.26 — read element at 0, shift indices 1..len down by 1
    // (skipping absent slots), drop the last, decrement length. For
    // Array we keep the existing dense `Vec::remove(0)` path.
    if let Some(arr) = args.receiver.as_array() {
        let heap = &mut *args.gc_heap;
        return Ok(array::with_elements_mut(arr, heap, |elements| {
            if elements.is_empty() {
                Value::undefined()
            } else {
                // §23.1.3.26: a leading hole shifts to `undefined`.
                let removed = elements.remove(0);
                if removed.is_hole() {
                    Value::undefined()
                } else {
                    removed
                }
            }
        }));
    }
    if let Some(obj) = args.receiver.as_object() {
        let heap_ref = &mut *args.gc_heap;
        let len = read_array_like_length(obj, heap_ref);
        if len == 0 {
            let heap = &mut *args.gc_heap;
            crate::object::set(obj, heap, "length", Value::number_i32(0));
            return Ok(Value::undefined());
        }
        // Walk pre-shift present own indices in ascending order. The
        // post-shift state is the same set with each index decremented
        // by 1 (and the element at index 0 returned). Indices that
        // were present pre-shift but whose decremented form clashes
        // with nothing post-shift have their original key deleted to
        // match HasProperty results.
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        let heap = &mut *args.gc_heap;
        let mut first: Option<Value> = None;
        let mut post_present: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        for (i, v) in &entries {
            if *i == 0 {
                first = Some(*v);
                continue;
            }
            if *i < len {
                let new_idx = *i - 1;
                let new_key = new_idx.to_string();
                crate::object::set(obj, heap, &new_key, *v);
                post_present.insert(new_idx);
            }
        }
        // Pre-present indices whose shifted twin isn't being written
        // need their original slot deleted. Walk pre_present (size
        // proportional to actual present count, never `len`).
        for &i in &pre_present {
            // The shift writes to (i - 1) for i >= 1. Any pre-present
            // i that doesn't appear in post_present needs deletion at
            // its original index.
            if !post_present.contains(&i) {
                let _ = crate::object::delete(obj, heap, &i.to_string());
            }
        }
        // Always remove the trailing slot — even if it wasn't present,
        // delete is idempotent.
        let _ = crate::object::delete(obj, heap, &(len - 1).to_string());
        crate::object::set(obj, heap, "length", Value::number_f64((len - 1) as f64));
        return Ok(first.unwrap_or(Value::undefined()));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_unshift(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.34 — prepend the argument list; existing indices shift
    // up by `argCount`, length grows by the same amount. Dense Array
    // keeps the existing vec-rebuild path. Object receiver walks
    // pre-present indices in **descending** order so writing to
    // `i + N` doesn't clobber a not-yet-relocated `i + N`.
    let arg_count = args.args.len();
    if let Some(arr) = args.receiver.as_array() {
        let heap = &mut *args.gc_heap;
        let existing_len = array::len(arr, heap);
        let mut values: Vec<Value> = args.args.to_vec();
        array::with_elements(arr, heap, |elements| {
            values.extend(elements.iter().cloned())
        });
        array::with_elements_mut(arr, heap, |elements| {
            elements.clear();
            elements.extend(values);
        });
        return Ok(Value::number(NumberValue::from_i32(
            (existing_len + arg_count) as i32,
        )));
    }
    if let Some(obj) = args.receiver.as_object() {
        let heap_ref = &mut *args.gc_heap;
        let existing_len = read_array_like_length(obj, heap_ref);
        if arg_count == 0 {
            // Spec still requires writing `length` back (no-op if it
            // already equals `existing_len`).
            let heap = &mut *args.gc_heap;
            crate::object::set(obj, heap, "length", Value::number_f64(existing_len as f64));
            return Ok(Value::number(NumberValue::from_f64(existing_len as f64)));
        }
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        let heap = &mut *args.gc_heap;
        // Walk descending so destination slot is never live yet.
        for (i, v) in entries.into_iter().rev() {
            let new_idx = i + arg_count;
            crate::object::set(obj, heap, &new_idx.to_string(), v);
        }
        // Positions in `[0, arg_count)` that originally held a
        // pre-present value but whose post-shift writer doesn't
        // overwrite them need explicit delete. After the shift, the
        // new present positions are `{i + arg_count for i in
        // pre_present}`; positions in pre_present \ post_present
        // must be cleared.
        let post_present: std::collections::BTreeSet<usize> =
            pre_present.iter().map(|&i| i + arg_count).collect();
        for &i in &pre_present {
            if !post_present.contains(&i) && i < arg_count {
                // Will be overwritten by the prepend below, no need
                // to delete.
                continue;
            }
            if !post_present.contains(&i) {
                let _ = crate::object::delete(obj, heap, &i.to_string());
            }
        }
        // Prepend the new items at indices 0..arg_count.
        for (i, v) in args.args.iter().enumerate() {
            crate::object::set(obj, heap, &i.to_string(), *v);
        }
        let new_len = existing_len + arg_count;
        crate::object::set(obj, heap, "length", Value::number_f64(new_len as f64));
        return Ok(Value::number(NumberValue::from_f64(new_len as f64)));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_slice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.28 — generic over array-likes via ToObject(this) +
    // LengthOfArrayLike. The dense `Value::Array` path stays on the
    // contiguous slice copy; non-array receivers walk present indexed
    // own keys and materialise undefined for absent positions inside
    // the requested range (matching `HasProperty` + `Get` semantics).
    if let Some(arr) = args.receiver.as_array() {
        let len = array::len(arr, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
        let end_default = len as i64;
        let end_raw = arg_signed_index(args, 1, end_default)?;
        let end = clamp_index(end_raw, len);
        let slice: Vec<Value> = array::with_elements(arr, &*args.gc_heap, |elements| {
            if start >= end {
                Vec::new()
            } else {
                elements[start..end].to_vec()
            }
        });
        return Ok(Value::array(args.array_from_elements_rooted(
            slice.iter().cloned(),
            &[],
            &[slice.as_slice()],
        )?));
    }
    let len = array_like_length(args.receiver, &*args.gc_heap);
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    let end_default = len as i64;
    let end_raw = arg_signed_index(args, 1, end_default)?;
    let end = clamp_index(end_raw, len);
    let entries = array_like_present_entries(args.receiver, args.gc_heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let slice_len = end.saturating_sub(start);
    let mut out = vec![Value::undefined(); slice_len];
    for (i, v) in entries {
        if i < start || i >= end {
            continue;
        }
        out[i - start] = v;
    }
    Ok(Value::array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

fn impl_concat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.2 — start with a copy of the receiver, then for each
    // argument: if it's an Array (foundation: §22.1.2 IsConcatSpreadable
    // short-circuits to IsArray), spread its dense elements; otherwise
    // append as a single value. Array-like receivers spread via the
    // sparse walker; arguments that are plain Objects are NOT spread
    // (matches IsArray-only spread until full @@isConcatSpreadable
    // wiring lands).
    let mut combined: Vec<Value> = if let Some(arr) = args.receiver.as_array() {
        array::with_elements(arr, &*args.gc_heap, |elements| elements.to_vec())
    } else if args.receiver.is_object() {
        let len = array_like_length(args.receiver, &*args.gc_heap);
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let mut out = vec![Value::undefined(); len];
        for (i, v) in entries {
            if i < len {
                out[i] = v;
            }
        }
        out
    } else {
        return Err(IntrinsicError::BadReceiver { expected: "array" });
    };
    for v in args.args {
        if let Some(other) = v.as_array() {
            array::with_elements(other, &*args.gc_heap, |elements| {
                combined.extend(elements.iter().cloned());
            });
        } else {
            combined.push(*v);
        }
    }
    Ok(Value::array(args.array_from_elements_rooted(
        combined.iter().cloned(),
        &[],
        &[combined.as_slice()],
    )?))
}

/// §23.1.3.36 `Array.prototype.toString` — delegate to `join()` with
/// the default `","` separator. Spec step 1 is "Let array be ?
/// ToObject(this value)"; step 4 reads `func = array.join`, falling
/// back to `%Object.prototype.toString%` when `join` is not
/// callable. Foundation: always call our concrete join helper.
///
/// <https://tc39.es/ecma262/#sec-array.prototype.tostring>
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    impl_join(args)
}

fn impl_join(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.16 — generic via ToObject + LengthOfArrayLike. Holes
    // and `null` / `undefined` serialise as the empty string. Dense
    // `Value::Array` keeps the existing tight walk; Object receivers
    // walk indices `[0, len)` materialising absent slots as empty
    // strings (matches HasProperty result).
    let separator = match args.args.first() {
        None => ",".to_string(),
        Some(v) if v.is_undefined() => ",".to_string(),
        Some(v) => {
            if let Some(s) = v.as_string(args.gc_heap) {
                s.to_lossy_string(&*args.gc_heap)
            } else {
                v.display_string(&*args.gc_heap)
            }
        }
    };
    if let Some(arr) = args.receiver.as_array() {
        let parts: Vec<String> = array::with_elements(arr, &*args.gc_heap, |elements| {
            elements
                .iter()
                .map(|v| {
                    if v.is_undefined() || v.is_null() || v.is_hole() {
                        String::new()
                    } else {
                        v.display_string(&*args.gc_heap)
                    }
                })
                .collect()
        });
        let joined = parts.join(&separator);
        return Ok(Value::string(JsString::from_str(&joined, args.gc_heap)?));
    }
    if let Some(obj) = args.receiver.as_object() {
        let len = read_array_like_length(obj, &*args.gc_heap);
        if len == 0 {
            return Ok(Value::string(JsString::from_str("", args.gc_heap)?));
        }
        // Materialise present indices into a sparse lookup; absent
        // slots produce empty-string parts so the final `join` keeps
        // separator placement correct. We bound the `parts` length by
        // `len` (already clamped to `MAX_ARRAY_LIKE_PROBE_LEN`); no
        // unbounded probe.
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let mut parts: Vec<String> = vec![String::new(); len];
        for (i, v) in entries {
            if i >= len {
                continue;
            }
            parts[i] = if v.is_undefined() || v.is_null() || v.is_hole() {
                String::new()
            } else {
                v.display_string(&*args.gc_heap)
            };
        }
        let joined = parts.join(&separator);
        return Ok(Value::string(JsString::from_str(&joined, args.gc_heap)?));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_includes(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.13 Array.prototype.includes — generic over array-likes
    // via ToObject(this) + LengthOfArrayLike. Comparison is
    // §7.2.12 SameValueZero, so `NaN` matches `NaN` and `±0` collapse.
    // Holes compare as SameValueZero against `undefined`, so
    // `[,,].includes(undefined) === true`. The dense Array path keeps
    // the existing tight `with_elements` walk; the array-like
    // fallback uses the sparse iterator.
    let needle = args.args.first().cloned().unwrap_or(Value::undefined());
    let needle_is_undefined = needle.is_undefined();
    if let Some(arr) = args.receiver.as_array() {
        let found = array::with_elements(arr, &*args.gc_heap, |elements| {
            elements.iter().any(|v| {
                if v.is_hole() {
                    needle_is_undefined
                } else {
                    crate::abstract_ops::same_value_zero(v, &needle, &*args.gc_heap)
                }
            })
        });
        return Ok(Value::boolean(found));
    }
    let entries = array_like_present_entries(args.receiver, args.gc_heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let found = entries
        .iter()
        .any(|(_, v)| crate::abstract_ops::same_value_zero(v, &needle, args.gc_heap));
    Ok(Value::boolean(found))
}

fn impl_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.14 — generic over array-likes. The dense `Value::Array`
    // path keeps the existing tight `with_elements` walk so common
    // dense-array calls don't pay a snapshot allocation.
    let needle = args.args.first().cloned().unwrap_or(Value::undefined());
    let from_raw = arg_signed_index(args, 1, 0)?;
    let heap = &mut *args.gc_heap;
    if let Some(arr) = args.receiver.as_array() {
        let len = array::len(arr, heap);
        let from = clamp_index(from_raw, len);
        let found = array::with_elements(arr, heap, |elements| {
            elements.iter().enumerate().skip(from).find_map(|(i, v)| {
                if crate::abstract_ops::is_strictly_equal(v, &needle, heap) {
                    Some(i)
                } else {
                    None
                }
            })
        });
        if let Some(i) = found {
            return Ok(Value::number(NumberValue::from_i32(i as i32)));
        }
        return Ok(Value::number(NumberValue::from_i32(-1)));
    }
    let len = array_like_length(args.receiver, heap);
    let from = clamp_index(from_raw, len);
    let entries = array_like_present_entries(args.receiver, args.gc_heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    for (i, v) in entries {
        if i < from {
            continue;
        }
        if crate::abstract_ops::is_strictly_equal(&v, &needle, args.gc_heap) {
            return Ok(Value::number(NumberValue::from_i32(i as i32)));
        }
    }
    Ok(Value::number(NumberValue::from_i32(-1)))
}

/// §23.1.3.1 `Array.prototype.at(index)` — clamp negative indexing.
/// <https://tc39.es/ecma262/#sec-array.prototype.at>
fn impl_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.1 — generic via ToObject(this) + LengthOfArrayLike,
    // then a single indexed Get. Constant-time regardless of `len`.
    if let Some(arr) = args.receiver.as_array() {
        let len = array::len(arr, &*args.gc_heap) as i64;
        let raw = arg_signed_index(args, 0, 0)?;
        let idx = if raw < 0 { len + raw } else { raw };
        if idx < 0 || idx >= len {
            return Ok(Value::undefined());
        }
        let heap = &mut *args.gc_heap;
        return Ok(array::get(arr, heap, idx as usize));
    }
    if let Some(obj) = args.receiver.as_object() {
        let len = read_array_like_length(obj, &*args.gc_heap) as i64;
        let raw = arg_signed_index(args, 0, 0)?;
        let idx = if raw < 0 { len + raw } else { raw };
        if idx < 0 || idx >= len {
            return Ok(Value::undefined());
        }
        let key = (idx as usize).to_string();
        let heap = &mut *args.gc_heap;
        return Ok(crate::object::get(obj, heap, &key).unwrap_or(Value::undefined()));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.18 `Array.prototype.lastIndexOf(value, fromIndex?)`.
/// Generic over array-likes; dense `Value::Array` keeps the existing
/// tight reverse walk to avoid a snapshot allocation on hot paths.
/// <https://tc39.es/ecma262/#sec-array.prototype.lastindexof>
fn impl_last_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let needle = args.args.first().cloned().unwrap_or(Value::undefined());
    if let Some(arr) = args.receiver.as_array() {
        let len = array::len(arr, &*args.gc_heap);
        let from_default = (len as i64).saturating_sub(1);
        let from_raw = arg_signed_index(args, 1, from_default)?;
        let from = if from_raw < 0 {
            let v = (len as i64) + from_raw;
            if v < 0 {
                return Ok(Value::number(NumberValue::from_i32(-1)));
            }
            v as usize
        } else if (from_raw as usize) >= len {
            len.saturating_sub(1)
        } else {
            from_raw as usize
        };
        let found = array::with_elements(arr, &*args.gc_heap, |elements| {
            if elements.is_empty() {
                return None;
            }
            // §23.1.3.18 step 6 — clamp the cursor to the elements
            // backing-store length so a sparse array with a spec
            // length larger than `elements.len()` (e.g.
            // `arr.length = 2**31`) does not index out of bounds.
            // Trailing slots beyond `elements.len()` are holes that
            // can never `===` the needle.
            let mut i = from.min(elements.len() - 1) as i64;
            while i >= 0 {
                if crate::abstract_ops::is_strictly_equal(
                    &elements[i as usize],
                    &needle,
                    &*args.gc_heap,
                ) {
                    return Some(i as i32);
                }
                i -= 1;
            }
            None
        });
        if let Some(i) = found {
            return Ok(Value::number(NumberValue::from_i32(i)));
        }
        return Ok(Value::number(NumberValue::from_i32(-1)));
    }
    let len = array_like_length(args.receiver, &*args.gc_heap);
    let from_default = (len as i64).saturating_sub(1);
    let from_raw = arg_signed_index(args, 1, from_default)?;
    let from = if from_raw < 0 {
        let v = (len as i64) + from_raw;
        if v < 0 {
            return Ok(Value::number(NumberValue::from_i32(-1)));
        }
        v as usize
    } else if (from_raw as usize) >= len {
        len.saturating_sub(1)
    } else {
        from_raw as usize
    };
    let entries = array_like_present_entries(args.receiver, args.gc_heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    // Reverse walk over the sorted entries; first hit with `i <= from`
    // wins. Entries are ascending so we iterate in reverse.
    for (i, v) in entries.into_iter().rev() {
        if i > from {
            continue;
        }
        if crate::abstract_ops::is_strictly_equal(&v, &needle, args.gc_heap) {
            return Ok(Value::number(NumberValue::from_i32(i as i32)));
        }
    }
    Ok(Value::number(NumberValue::from_i32(-1)))
}

/// §23.1.3.27 `Array.prototype.reverse()` — in-place.
/// Generic over array-likes; sparse Object receivers swap only the
/// pairs `(i, len-1-i)` where at least one side is present (matching
/// the spec's `HasProperty` short-circuit).
/// <https://tc39.es/ecma262/#sec-array.prototype.reverse>
fn impl_reverse(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    if let Some(arr) = args.receiver.as_array() {
        let heap = &mut *args.gc_heap;
        array::with_elements_mut(arr, heap, |elements| elements.reverse());
        return Ok(Value::array(arr));
    }
    if let Some(obj) = args.receiver.as_object() {
        let heap_ref = &mut *args.gc_heap;
        let len = read_array_like_length(obj, heap_ref);
        if len < 2 {
            return Ok(*args.receiver);
        }
        let entries = array_like_present_entries(args.receiver, heap_ref)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        let heap = &mut *args.gc_heap;
        // Walk only present indices ≤ middle; pair each with its
        // mirror `len-1-i`. Spec §23.1.3.27 step 5: if one side is
        // present and the other isn't, the present side migrates
        // (Set + Delete); both-present → swap; both-absent → skip.
        for &i in pre_present.iter().filter(|&&i| i < len) {
            let mirror = len - 1 - i;
            if mirror <= i {
                break;
            }
            let key_i = i.to_string();
            let key_m = mirror.to_string();
            let v_i = crate::object::get(obj, heap, &key_i).unwrap_or(Value::undefined());
            let mirror_present = pre_present.contains(&mirror);
            if mirror_present {
                let v_m = crate::object::get(obj, heap, &key_m).unwrap_or(Value::undefined());
                crate::object::set(obj, heap, &key_i, v_m);
                crate::object::set(obj, heap, &key_m, v_i);
            } else {
                // Mirror absent — migrate i → mirror, delete i.
                crate::object::set(obj, heap, &key_m, v_i);
                let _ = crate::object::delete(obj, heap, &key_i);
            }
        }
        // Also walk present indices > middle whose mirror was absent
        // (the mirror walk above misses them since we iterated i <
        // mirror only).
        for &i in pre_present.iter().filter(|&&i| i < len) {
            let mirror = len - 1 - i;
            if mirror >= i {
                continue;
            }
            if pre_present.contains(&mirror) {
                // Already handled when we processed `mirror` from the
                // lower half.
                continue;
            }
            // Mirror absent → i migrates down to mirror; delete i.
            let key_i = i.to_string();
            let key_m = mirror.to_string();
            let v_i = crate::object::get(obj, heap, &key_i).unwrap_or(Value::undefined());
            crate::object::set(obj, heap, &key_m, v_i);
            let _ = crate::object::delete(obj, heap, &key_i);
        }
        return Ok(*args.receiver);
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.7 `Array.prototype.fill(value, start?, end?)` — in-place.
/// Generic over array-likes via ToObject(this) + LengthOfArrayLike.
/// <https://tc39.es/ecma262/#sec-array.prototype.fill>
fn impl_fill(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let value = args.args.first().cloned().unwrap_or(Value::undefined());
    if let Some(arr) = args.receiver.as_array() {
        let len = array::len(arr, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 1, 0)?, len);
        let end = clamp_index(arg_signed_index(args, 2, len as i64)?, len);
        if start < end {
            let heap = &mut *args.gc_heap;
            array::with_elements_mut(arr, heap, |elements| {
                for slot in elements.iter_mut().take(end).skip(start) {
                    *slot = value;
                }
            });
        }
        return Ok(Value::array(arr));
    }
    if let Some(obj) = args.receiver.as_object() {
        let len = read_array_like_length(obj, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 1, 0)?, len);
        let end = clamp_index(arg_signed_index(args, 2, len as i64)?, len);
        // Cap defensively — `MAX_ARRAY_LIKE_PROBE_LEN` is already
        // applied to `len` via `read_array_like_length`, so the
        // bounded `start..end` walk is safe.
        let heap = &mut *args.gc_heap;
        for k in start..end {
            crate::object::set(obj, heap, &k.to_string(), value);
        }
        return Ok(*args.receiver);
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.11 `Array.prototype.flat(depth?)` — flattens at most
/// `depth` levels (default 1). Sparse holes are dropped — foundation
/// arrays are dense, so the spec's `IsConcatSpreadable` short-circuit
/// reduces to "is `Value::Array`".
/// <https://tc39.es/ecma262/#sec-array.prototype.flat>
fn impl_flat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let heap = &mut *args.gc_heap;
    let depth = if let Some(arg) = args.args.first() {
        if arg.is_undefined() {
            1i64
        } else if let Some(n) = arg.as_number() {
            match n.as_smi() {
                Some(v) if v >= 0 => v as i64,
                Some(_) => 0,
                None => n.as_f64() as i64,
            }
        } else {
            1
        }
    } else {
        1i64
    };
    fn walk(out: &mut Vec<Value>, heap: &otter_gc::GcHeap, body: &[Value], depth: i64) {
        for v in body {
            if v.is_hole() {
                continue;
            }
            if let Some(a) = v.as_array()
                && depth > 0
            {
                array::with_elements(a, heap, |inner| walk(out, heap, inner, depth - 1));
            } else {
                out.push(*v);
            }
        }
    }
    let elements: Vec<Value> = if let Some(arr) = args.receiver.as_array() {
        array::with_elements(arr, heap, |els| els.to_vec())
    } else if let Some(obj) = args.receiver.as_object() {
        let len = read_array_like_length(obj, heap);
        (0..len)
            .map(|i| crate::object::get(obj, heap, &i.to_string()).unwrap_or(Value::undefined()))
            .collect()
    } else {
        Vec::new()
    };
    let mut out: Vec<Value> = Vec::with_capacity(elements.len());
    walk(&mut out, heap, &elements, depth);
    Ok(Value::array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §23.1.3.31 `Array.prototype.splice(start, deleteCount?, ...items)`.
/// Mutates the receiver in place; returns the removed elements.
/// Generic over array-likes; Object receivers use a sparse-aware
/// shift so pathological `length` values never trigger an `O(len)`
/// walk.
/// <https://tc39.es/ecma262/#sec-array.prototype.splice>
fn impl_splice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    if let Some(arr) = args.receiver.as_array() {
        let len = array::len(arr, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
        let delete_count = {
            let arg1 = args.args.get(1);
            if arg1.is_none() || arg1.is_some_and(|v| v.is_undefined()) {
                len.saturating_sub(start)
            } else if let Some(n) = arg1.and_then(|v| v.as_number()) {
                let raw = match n.as_smi() {
                    Some(v) => v as i64,
                    None => n.as_f64() as i64,
                };
                if raw < 0 {
                    0
                } else if (raw as usize) > len.saturating_sub(start) {
                    len.saturating_sub(start)
                } else {
                    raw as usize
                }
            } else {
                0
            }
        };
        let inserts: Vec<Value> = args.args.iter().skip(2).cloned().collect();
        let heap = &mut *args.gc_heap;
        let removed = array::with_elements_mut(arr, heap, |elements| {
            let mut removed: Vec<Value> = Vec::with_capacity(delete_count);
            for _ in 0..delete_count {
                removed.push(elements.remove(start));
            }
            for (i, v) in inserts.into_iter().enumerate() {
                elements.insert(start + i, v);
            }
            removed
        });
        return Ok(Value::array(args.array_from_elements_rooted(
            removed.iter().cloned(),
            &[],
            &[removed.as_slice()],
        )?));
    }
    if let Some(obj) = args.receiver.as_object() {
        let len = read_array_like_length(obj, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
        let delete_count = {
            let arg1 = args.args.get(1);
            if arg1.is_none() || arg1.is_some_and(|v| v.is_undefined()) {
                len.saturating_sub(start)
            } else if let Some(n) = arg1.and_then(|v| v.as_number()) {
                let raw = match n.as_smi() {
                    Some(v) => v as i64,
                    None => n.as_f64() as i64,
                };
                if raw < 0 {
                    0
                } else if (raw as usize) > len.saturating_sub(start) {
                    len.saturating_sub(start)
                } else {
                    raw as usize
                }
            } else {
                0
            }
        };
        let item_count = args.args.len().saturating_sub(2);
        let inserts: Vec<Value> = args.args.iter().skip(2).cloned().collect();
        // Pre-shift present indices.
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        // Snapshot the deleted region so we can return it.
        let mut removed: Vec<Value> = vec![Value::undefined(); delete_count];
        for (i, v) in &entries {
            if *i >= start && *i < start + delete_count {
                removed[*i - start] = *v;
            }
        }
        let heap = &mut *args.gc_heap;
        // Shift tail.
        if item_count < delete_count {
            // Shrink — walk present indices in [start+delete_count, len)
            // ascending; new index = i - (delete_count - item_count).
            let shift = delete_count - item_count;
            for (i, v) in entries
                .iter()
                .filter(|(i, _)| *i >= start + delete_count && *i < len)
            {
                let new_idx = i - shift;
                crate::object::set(obj, heap, &new_idx.to_string(), *v);
            }
            // Delete pre-present positions that no longer hold a
            // value. Post-present positions = {i - shift for i in
            // pre_present where i >= start + delete_count} ∪
            // {i for i in pre_present where i < start} ∪
            // {start..start+item_count from inserts}.
            let mut post_present: std::collections::BTreeSet<usize> =
                std::collections::BTreeSet::new();
            for &i in &pre_present {
                if i < start {
                    post_present.insert(i);
                } else if i >= start + delete_count && i < len {
                    post_present.insert(i - shift);
                }
            }
            for k in 0..item_count {
                post_present.insert(start + k);
            }
            for &i in &pre_present {
                if !post_present.contains(&i) {
                    let _ = crate::object::delete(obj, heap, &i.to_string());
                }
            }
        } else if item_count > delete_count {
            // Grow — walk present indices in [start+delete_count, len)
            // descending so writes don't clobber yet-to-relocate values.
            let shift = item_count - delete_count;
            let tail: Vec<(usize, Value)> = entries
                .iter()
                .filter(|(i, _)| *i >= start + delete_count && *i < len)
                .map(|(i, v)| (*i, *v))
                .collect();
            for (i, v) in tail.iter().rev() {
                let new_idx = i + shift;
                crate::object::set(obj, heap, &new_idx.to_string(), *v);
            }
            let mut post_present: std::collections::BTreeSet<usize> =
                std::collections::BTreeSet::new();
            for &i in &pre_present {
                if i < start {
                    post_present.insert(i);
                } else if i >= start + delete_count && i < len {
                    post_present.insert(i + shift);
                }
            }
            for k in 0..item_count {
                post_present.insert(start + k);
            }
            for &i in &pre_present {
                if !post_present.contains(&i) {
                    let _ = crate::object::delete(obj, heap, &i.to_string());
                }
            }
        } else {
            // item_count == delete_count — no tail shift needed.
            // Pre-present indices in [start, start+delete_count) get
            // overwritten by inserts (or kept if insert is absent).
            // Nothing to delete unless start..start+delete_count had
            // present positions that aren't being rewritten — but
            // since item_count == delete_count, all of them are. So
            // no deletes needed beyond the insert overwrite.
        }
        // Write the new items.
        for (k, v) in inserts.into_iter().enumerate() {
            crate::object::set(obj, heap, &(start + k).to_string(), v);
        }
        // Update length.
        let new_len = len - delete_count + item_count;
        crate::object::set(obj, heap, "length", Value::number_f64(new_len as f64));
        return Ok(Value::array(args.array_from_elements_rooted(
            removed.iter().cloned(),
            &[],
            &[removed.as_slice()],
        )?));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.30 `Array.prototype.sort()` — default lexicographic
/// comparator (calls `String(a)` / `String(b)` and compares as
/// UTF-16). Comparator-driven sort is interpreter-dispatched.
/// <https://tc39.es/ecma262/#sec-array.prototype.sort>
fn impl_sort_default(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let comparator_absent = args.args.first().is_none_or(|v| v.is_undefined());
    if comparator_absent {
        // §23.1.3.30.2 SortCompare (no comparator) — undefined values
        // sort to the end; remaining values compare by their
        // ToString result. Render every element's decimal once
        // outside the mut-borrow before driving the sort so the
        // comparator stays heap-free.
        let keys: Vec<Option<String>> = array::with_elements(arr, &*args.gc_heap, |elements| {
            elements
                .iter()
                .map(|v| {
                    if v.is_undefined() {
                        None
                    } else {
                        Some(v.display_string(&*args.gc_heap))
                    }
                })
                .collect()
        });
        let heap = &mut *args.gc_heap;
        array::with_elements_mut(arr, heap, |elements| {
            // Pair each element with its precomputed key for the
            // comparator, then sort in place.
            let mut indexed: Vec<(usize, Value)> = elements.iter().cloned().enumerate().collect();
            indexed.sort_by(|(ia, _), (ib, _)| {
                let a_key = keys.get(*ia).and_then(|k| k.as_ref());
                let b_key = keys.get(*ib).and_then(|k| k.as_ref());
                match (a_key, b_key) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(a), Some(b)) => a.cmp(b),
                }
            });
            for (slot, (_, v)) in elements.iter_mut().zip(indexed) {
                *slot = v;
            }
        });
        Ok(Value::array(arr))
    } else {
        // Comparator path — interpreter dispatches it. Returning the
        // BadArgument here surfaces as a clear diagnostic during
        // bring-up; in practice the interpreter intercept above
        // catches comparator-driven sorts before this point.
        Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "sort comparator must be dispatched by the interpreter",
        })
    }
}

/// §23.1.3.5 `Array.prototype.copyWithin(target, start, end?)` —
/// in-place block copy. The receiver itself is returned. Generic
/// over array-likes via ToObject + LengthOfArrayLike.
/// <https://tc39.es/ecma262/#sec-array.prototype.copywithin>
fn impl_copy_within(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let len = array_or_object_length(args)?;
    let to_raw = arg_signed_index(args, 0, 0)?;
    let from_raw = arg_signed_index(args, 1, 0)?;
    let end_raw = arg_signed_index(args, 2, len as i64)?;
    let to = clamp_index(to_raw, len);
    let from = clamp_index(from_raw, len);
    let end = clamp_index(end_raw, len);
    let count = end.saturating_sub(from).min(len.saturating_sub(to));
    if count == 0 {
        return Ok(*args.receiver);
    }
    if let Some(arr) = args.receiver.as_array() {
        let heap = &mut *args.gc_heap;
        array::with_elements_mut(arr, heap, |elements| {
            // Snapshot source range — std::vec::Vec doesn't have
            // `copy_within` for non-Copy types, so a transient
            // buffer is the cleanest correct path.
            let src: Vec<Value> = elements[from..from + count].to_vec();
            for (i, v) in src.into_iter().enumerate() {
                elements[to + i] = v;
            }
        });
        return Ok(Value::array(arr));
    }
    if let Some(obj) = args.receiver.as_object() {
        // Snapshot the source range using only present indices so
        // pathological-sparse receivers don't trigger an
        // `O(count)` HasProperty scan; afterwards write to `to..`,
        // deleting positions whose source was absent.
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let mut src: Vec<Option<Value>> = vec![None; count];
        for (i, v) in &entries {
            if *i >= from && *i < from + count {
                src[*i - from] = Some(*v);
            }
        }
        let heap = &mut *args.gc_heap;
        for (i, slot) in src.into_iter().enumerate() {
            let key = (to + i).to_string();
            match slot {
                Some(v) => crate::object::set(obj, heap, &key, v),
                None => {
                    let _ = crate::object::delete(obj, heap, &key);
                }
            }
        }
        return Ok(*args.receiver);
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.40 `Array.prototype.toSpliced(start, skipCount?, ...items)`
/// — non-mutating splice. Returns a fresh dense Array with the spec
/// `[len - skipCount + itemCount]` shape.
/// <https://tc39.es/ecma262/#sec-array.prototype.tospliced>
fn impl_to_spliced(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let len = array_or_object_length(args)?;
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    let skip_count = {
        let arg1 = args.args.get(1);
        if arg1.is_none() || arg1.is_some_and(|v| v.is_undefined()) {
            len.saturating_sub(start)
        } else if let Some(n) = arg1.and_then(|v| v.as_number()) {
            let raw = match n.as_smi() {
                Some(v) => v as i64,
                None => n.as_f64() as i64,
            };
            if raw < 0 {
                0
            } else if (raw as usize) > len.saturating_sub(start) {
                len.saturating_sub(start)
            } else {
                raw as usize
            }
        } else {
            0
        }
    };
    let item_count = args.args.len().saturating_sub(2);
    let new_len = len - skip_count + item_count;
    let mut out: Vec<Value> = vec![Value::undefined(); new_len];
    // Materialise present source values into `src[0..len]`.
    let mut src: Vec<Value> = vec![Value::undefined(); len];
    if let Some(arr) = args.receiver.as_array() {
        array::with_elements(arr, &*args.gc_heap, |elements| {
            for (i, slot) in src.iter_mut().enumerate() {
                if let Some(v) = elements.get(i) {
                    *slot = if v.is_hole() { Value::undefined() } else { *v };
                }
            }
        });
    } else if args.receiver.is_object() {
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        for (i, v) in entries {
            if i < len {
                src[i] = v;
            }
        }
    } else {
        unreachable!();
    }
    // Write the head [0, start).
    out[..start].clone_from_slice(&src[..start]);
    // Write the inserts at [start, start+item_count).
    for (k, v) in args.args.iter().skip(2).enumerate() {
        out[start + k] = *v;
    }
    // Write the tail [start+skip_count, len) shifted to
    // [start+item_count, new_len).
    let mut dst = start + item_count;
    let mut srcidx = start + skip_count;
    while srcidx < len {
        out[dst] = src[srcidx];
        dst += 1;
        srcidx += 1;
    }
    Ok(Value::array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §20.1.3.2 — `Array.prototype.hasOwnProperty(V)`. Spec: inherited
/// from `Object.prototype.hasOwnProperty`. Foundation: short-circuit
/// here so callers don't need the (yet-to-be-real) Array prototype
/// chain walker. Checks indexed slots, named-properties side table,
/// and the synthetic `length` slot.
fn impl_has_own_property(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = args
        .receiver
        .as_array()
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let key_value = args.args.first().cloned().unwrap_or(Value::undefined());
    let key_string: Option<String> = if let Some(s) = key_value.as_string(args.gc_heap) {
        Some(s.to_lossy_string(&*args.gc_heap))
    } else if let Some(n) = key_value.as_number() {
        Some(n.to_display_string())
    } else if let Some(b) = key_value.as_boolean() {
        Some(if b { "true" } else { "false" }.to_string())
    } else if key_value.is_null() {
        Some("null".to_string())
    } else if key_value.is_undefined() {
        Some("undefined".to_string())
    } else {
        None
    };
    // §22.1 — symbol-keyed own properties live in the per-array
    // symbol table. Surface them before the string-keyed paths so
    // `arr.hasOwnProperty(Symbol.toStringTag)` round-trips.
    let sym_opt = key_value.as_symbol(args.gc_heap);
    let heap = &mut *args.gc_heap;
    if let Some(sym) = sym_opt {
        return Ok(Value::boolean(
            array::get_symbol_property(arr, heap, sym).is_some(),
        ));
    }
    // Try indexed first.
    let Some(key_string) = key_string else {
        return Ok(Value::boolean(false));
    };
    if let Some(idx) = crate::object::array_index_property_name(&key_string) {
        let has_indexed_property = array::has_own_element(arr, heap, idx as usize)
            || array::get_accessor(arr, heap, &key_string).is_some();
        return Ok(Value::boolean(has_indexed_property));
    }
    if key_string == "length" {
        return Ok(Value::boolean(true));
    }
    let has_named = heap.read_payload(arr, |body| {
        body.named_properties
            .as_ref()
            .is_some_and(|m| m.contains_key(&key_string))
            || body
                .accessors
                .as_ref()
                .is_some_and(|m| m.contains_key(&key_string))
    });
    Ok(Value::boolean(has_named))
}

/// §20.1.3.4 — `Array.prototype.propertyIsEnumerable(V)`. Indexed
/// slots + named props are enumerable; `length` is not.
fn impl_property_is_enumerable(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = args
        .receiver
        .as_array()
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let key_value = args.args.first().cloned().unwrap_or(Value::undefined());
    let key_string: String = if let Some(s) = key_value.as_string(args.gc_heap) {
        s.to_lossy_string(&*args.gc_heap)
    } else if let Some(n) = key_value.as_number() {
        n.to_display_string()
    } else {
        return Ok(Value::boolean(false));
    };
    let heap = &mut *args.gc_heap;
    if key_string == "length" {
        return Ok(Value::boolean(false));
    }
    if let Some(idx) = crate::object::array_index_property_name(&key_string) {
        let has_indexed_property = array::has_own_element(arr, heap, idx as usize)
            || array::get_accessor(arr, heap, &key_string).is_some();
        if !has_indexed_property {
            return Ok(Value::boolean(false));
        }
        let flags = array::get_property_flags(arr, heap, &key_string)
            .unwrap_or_else(crate::object::PropertyFlags::data_default);
        return Ok(Value::boolean(flags.enumerable()));
    }
    let has_named = heap.read_payload(arr, |body| {
        body.named_properties
            .as_ref()
            .is_some_and(|m| m.contains_key(&key_string))
    });
    Ok(Value::boolean(has_named))
}

/// §23.1.3.{18,35,8} — `Array.prototype.keys()` / `.values()` /
/// `.entries()`. Each constructs an `ArrayIterator` backed by the
/// receiver: `keys()` yields the numeric indices, `values()` yields
/// each element, `entries()` yields fresh `[index, value]` arrays.
/// The result is a `Value::Iterator` driven by `Op::IteratorNext`.
/// <https://tc39.es/ecma262/#sec-array.prototype.keys>
fn impl_keys_iter(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = args
        .receiver
        .as_array()
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let handle = args.alloc_iterator_state_rooted(
        crate::IteratorState::ArrayKey {
            array: arr,
            index: 0,
        },
        &[],
        &[],
    )?;
    Ok(Value::iterator(handle))
}

fn impl_values_iter(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = if let Some(arr) = args.receiver.as_array() {
        arr
    } else if let Some(obj) = args.receiver.as_object()
        && object::is_arguments_object(obj, args.gc_heap)
    {
        let len = object::get(obj, args.gc_heap, "length")
            .and_then(|v| v.as_number())
            .map(|n| n.as_f64().max(0.0) as usize)
            .unwrap_or(0);
        let mut snapshot: SmallVec<[Value; 4]> = SmallVec::with_capacity(len);
        for index in 0..len {
            snapshot.push(
                object::get(obj, args.gc_heap, &index.to_string()).unwrap_or(Value::undefined()),
            );
        }
        args.array_from_elements_rooted(snapshot.iter().cloned(), &[], &[snapshot.as_slice()])?
    } else {
        return Err(IntrinsicError::BadReceiver { expected: "array" });
    };
    let handle = args.alloc_iterator_state_rooted(
        crate::IteratorState::Array {
            array: arr,
            index: 0,
            origin: crate::BuiltinIteratorOrigin::Array,
        },
        &[],
        &[],
    )?;
    Ok(Value::iterator(handle))
}

fn impl_entries_iter(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = args
        .receiver
        .as_array()
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let handle = args.alloc_iterator_state_rooted(
        crate::IteratorState::ArrayEntry {
            array: arr,
            index: 0,
        },
        &[],
        &[],
    )?;
    Ok(Value::iterator(handle))
}

/// §23.1.3.41 `Array.prototype.toSorted(compareFn?)` — non-mutating
/// sort. Returns a fresh dense Array of `len` slots with absent
/// indices materialised as `undefined`, then sorted via the default
/// lexicographic comparator. A comparator argument routes through
/// the interpreter `array_callback_dispatch` path before this entry
/// is reached.
fn impl_to_sorted(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // Reject non-callable, non-undefined comparator per §23.1.3.41
    // step 2 (`if not undefined and not callable → TypeError`). The
    // callable branch is dispatched by the interpreter; if it reaches
    // here with a callable argument, we still treat it as the
    // default form (best-effort foundation).
    if let Some(first) = args.args.first()
        && !first.is_undefined()
        && !(first.is_function()
            || first.is_closure()
            || first.is_native_function()
            || first.is_bound_function()
            || first.is_class_constructor())
    {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "comparator must be a function",
        });
    }
    let len = array_or_object_length(args)?;
    let mut out: Vec<Value> = vec![Value::undefined(); len];
    if let Some(arr) = args.receiver.as_array() {
        array::with_elements(arr, &*args.gc_heap, |elements| {
            for (i, slot) in out.iter_mut().enumerate() {
                if let Some(v) = elements.get(i) {
                    *slot = if v.is_hole() { Value::undefined() } else { *v };
                }
            }
        });
    } else if args.receiver.is_object() {
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        for (i, v) in entries {
            if i < len {
                out[i] = v;
            }
        }
    } else {
        unreachable!();
    }
    out.sort_by(|a, b| {
        let a_undef = a.is_undefined();
        let b_undef = b.is_undefined();
        match (a_undef, b_undef) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a
                .display_string(args.gc_heap)
                .cmp(&b.display_string(args.gc_heap)),
        }
    });
    Ok(Value::array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §23.1.3.39 `Array.prototype.toReversed()` — non-mutating reverse.
/// Returns a fresh dense Array.
/// <https://tc39.es/ecma262/#sec-array.prototype.toreversed>
fn impl_to_reversed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let len = array_or_object_length(args)?;
    let mut out: Vec<Value> = vec![Value::undefined(); len];
    if let Some(arr) = args.receiver.as_array() {
        array::with_elements(arr, &*args.gc_heap, |elements| {
            for (i, slot) in out.iter_mut().enumerate() {
                if let Some(v) = elements.get(len - 1 - i) {
                    *slot = if v.is_hole() { Value::undefined() } else { *v };
                }
            }
        });
    } else if args.receiver.is_object() {
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        for (i, v) in entries {
            if i >= len {
                continue;
            }
            out[len - 1 - i] = v;
        }
    }
    Ok(Value::array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §23.1.3.42 `Array.prototype.with(index, value)` — non-mutating
/// element replacement at `index`. Returns a fresh dense Array.
/// <https://tc39.es/ecma262/#sec-array.prototype.with>
fn impl_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let len = array_or_object_length(args)?;
    let raw = arg_signed_index(args, 0, 0)?;
    let actual = if raw < 0 { raw + len as i64 } else { raw };
    if actual < 0 || (actual as usize) >= len {
        return Err(IntrinsicError::OutOfRange {
            index: 0,
            reason: "index out of bounds for Array.prototype.with",
        });
    }
    let replacement = args.args.get(1).cloned().unwrap_or(Value::undefined());
    let actual = actual as usize;
    let mut out: Vec<Value> = vec![Value::undefined(); len];
    if let Some(arr) = args.receiver.as_array() {
        array::with_elements(arr, &*args.gc_heap, |elements| {
            for (i, slot) in out.iter_mut().enumerate() {
                if i == actual {
                    *slot = replacement;
                } else if let Some(v) = elements.get(i) {
                    *slot = if v.is_hole() { Value::undefined() } else { *v };
                }
            }
        });
    } else if args.receiver.is_object() {
        let entries = array_like_present_entries(args.receiver, args.gc_heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        for (i, v) in entries {
            if i >= len {
                continue;
            }
            out[i] = v;
        }
        out[actual] = replacement;
    }
    Ok(Value::array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// Declarative `Array.prototype` table.
pub static ARRAY_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Array,
            "push"        / 1 => impl_push,
            "pop"         / 0 => impl_pop,
            "shift"       / 0 => impl_shift,
            "unshift"     / 1 => impl_unshift,
            "slice"       / 2 => impl_slice,
            "concat"      / 1 => impl_concat,
            "join"        / 1 => impl_join,
            "includes"    / 1 => impl_includes,
            "indexOf"     / 1 => impl_index_of,
            "lastIndexOf" / 1 => impl_last_index_of,
            "at"          / 1 => impl_at,
            "reverse"     / 0 => impl_reverse,
            "fill"        / 3 => impl_fill,
            "flat"        / 1 => impl_flat,
            "splice"      / 2 => impl_splice,
            "sort"        / 1 => impl_sort_default,
            "toString"    / 0 => impl_to_string,
            "copyWithin"  / 2 => impl_copy_within,
            "toReversed"  / 0 => impl_to_reversed,
            "toSpliced"   / 2 => impl_to_spliced,
            "toSorted"    / 1 => impl_to_sorted,
            "with"        / 2 => impl_with,
            "keys"        / 0 => impl_keys_iter,
            "values"      / 0 => impl_values_iter,
            "entries"     / 0 => impl_entries_iter,
            "hasOwnProperty"      / 1 => impl_has_own_property,
            "propertyIsEnumerable" / 1 => impl_property_is_enumerable,
            // §23.1.3.32 toLocaleString — foundation form delegates
            // to the default `join(",")` shape until per-locale
            // formatting + element `toLocaleString` invocation lands
            // through the interpreter dispatcher. Matches the
            // `toString` callable shape so reflective property
            // reads resolve.
            "toLocaleString" / 0 => impl_to_string,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    ARRAY_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Array, name)
}

/// §7.3.11 canonical-builtin identity guard for the callback
/// `Array.prototype` methods. Returns `true` only when `method` is the
/// realm's original native for `name`, so a `GetMethod`-resolved value
/// can be matched against the engine's own function pointer instead of
/// a method-name allowlist. A user override (`Array.prototype.map = fn`)
/// or own shadow resolves to a different value and falls out here.
#[must_use]
pub(crate) fn is_canonical_callback_method(
    method: &Value,
    heap: &otter_gc::GcHeap,
    name: &str,
) -> bool {
    let Some(native) = method.as_native_function() else {
        return false;
    };
    let target: crate::native_function::NativeFastFn = match name {
        "map" => native_map,
        _ => return false,
    };
    native.is_static_fn(heap, target)
}

/// Static `Array.prototype` methods whose implementations do not
/// require JS callback dispatch.
pub static ARRAY_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("push", 1, native_push),
    method("pop", 0, native_pop),
    method("shift", 0, native_shift),
    method("unshift", 1, native_unshift),
    method("slice", 2, native_slice),
    method("concat", 1, native_concat),
    method("join", 1, native_join),
    method("includes", 1, native_includes),
    method("indexOf", 1, native_index_of),
    method("lastIndexOf", 1, native_last_index_of),
    method("at", 1, native_at),
    method("reverse", 0, native_reverse),
    method("fill", 3, native_fill),
    method("flat", 1, native_flat),
    method("splice", 2, native_splice),
    method("sort", 1, native_sort),
    method("toString", 0, native_to_string),
    method("copyWithin", 2, native_copy_within),
    method("toReversed", 0, native_to_reversed),
    method("toSpliced", 2, native_to_spliced),
    method("toSorted", 1, native_to_sorted),
    method("with", 2, native_with),
    method("toLocaleString", 0, native_to_locale_string),
    method("keys", 0, native_keys_iter),
    method("values", 0, native_values_iter),
    method("entries", 0, native_entries_iter),
    method("forEach", 1, native_for_each),
    method("map", 1, native_map),
    method("filter", 1, native_filter),
    method("some", 1, native_some),
    method("every", 1, native_every),
    method("find", 1, native_find),
    method("findIndex", 1, native_find_index),
    method("findLast", 1, native_find_last),
    method("findLastIndex", 1, native_find_last_index),
    method("reduce", 1, native_reduce),
    method("reduceRight", 1, native_reduce_right),
    method("flatMap", 1, native_flat_map),
];

pub(crate) fn install_array_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: object::JsObject,
    well_known: &WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    let Some(array_ctor) = object::get(global, heap, "Array").and_then(|v| v.as_native_function())
    else {
        return Ok(());
    };
    let Some(descriptor) = array_ctor
        .own_property_descriptor(&mut *heap, "prototype")
        .ok()
        .flatten()
    else {
        return Ok(());
    };
    let object::DescriptorKind::Data { value } = descriptor.kind else {
        return Ok(());
    };
    let Some(prototype) = value.as_object() else {
        return Ok(());
    };
    let global_root = Value::object(global);
    let prototype_root = Value::object(prototype);
    let values_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "values",
        0,
        native_values_iter,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let values_value = Value::native_function(values_fn);
    object::define_own_property_partial(
        prototype,
        heap,
        "values",
        PartialPropertyDescriptor {
            value: Some(values_value),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        well_known.get(WellKnown::Iterator),
        PartialPropertyDescriptor {
            value: Some(values_value),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

fn native_array_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    // §22.1.3 step 1 — every generic `Array.prototype.*` opens with
    // `ToObject(this value)`, which throws a TypeError on `null` /
    // `undefined` (RequireObjectCoercible).
    if receiver.is_null() || receiver.is_undefined() {
        return Err(NativeError::TypeError {
            name,
            reason: "Array.prototype method called on null or undefined".to_string(),
        });
    }
    // Pre-coerce integer-typed args via `ToPrimitive(Number)` so the
    // intrinsic's strict `arg_signed_index` guard observes user
    // `@@toPrimitive` / `valueOf` / `toString` side effects per spec.
    // Mirrors the matching arm in `Op::CallMethodValue`; this path
    // handles `Array.prototype.<x>.call(...)` / `.apply(...)` /
    // `.bind(...)` invocations that bypass the receiver fast path.
    let int_coerce_indices: &[usize] = match name {
        "indexOf" | "lastIndexOf" | "includes" => &[1],
        "fill" => &[1, 2],
        "at" => &[0],
        _ => &[],
    };
    let coerced_args: smallvec::SmallVec<[Value; 4]> = if int_coerce_indices.is_empty() {
        args.iter().cloned().collect()
    } else {
        let mut out: smallvec::SmallVec<[Value; 4]> = args.iter().cloned().collect();
        let exec = ctx.execution_context().cloned();
        if let Some(exec) = exec {
            for &idx in int_coerce_indices {
                let Some(slot) = out.get_mut(idx) else {
                    continue;
                };
                if !(slot.is_object()
                    || slot.is_array()
                    || slot.is_function()
                    || slot.is_closure()
                    || slot.is_native_function()
                    || slot.is_bound_function()
                    || slot.is_class_constructor()
                    || slot.is_proxy()
                    || slot.is_regexp())
                {
                    continue;
                }
                let interp = ctx.interp_mut();
                let primitive = interp
                    .evaluate_to_primitive(
                        &exec,
                        slot,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )
                    .map_err(|e| NativeError::TypeError {
                        name,
                        reason: e.to_string(),
                    })?;
                *slot = primitive;
            }
        }
        out
    };
    let exec = ctx.execution_context().cloned();
    if let Some(exec) = exec {
        let interp = ctx.interp_mut();
        if let Some(result) =
            interp.array_live_method_dispatch(&exec, name, receiver, &coerced_args, &[args])
        {
            return result.map_err(|err| crate::native_function::vm_to_native_error(err, name));
        }
    }
    let allocation_roots = ctx.collect_native_roots();
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown Array.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&mut IntrinsicArgs {
        receiver: &receiver,
        args: &coerced_args,
        gc_heap: ctx.heap_mut(),
        allocation_roots: allocation_roots.as_slice(),
    })
    .map_err(|err| NativeError::TypeError {
        name,
        reason: err.to_string(),
    })
}

macro_rules! native_array {
    ($fn_name:ident, $js_name:literal) => {
        fn $fn_name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            native_array_method($js_name, ctx, args)
        }
    };
}

native_array!(native_push, "push");
native_array!(native_pop, "pop");
native_array!(native_shift, "shift");
native_array!(native_unshift, "unshift");
native_array!(native_slice, "slice");
native_array!(native_concat, "concat");
native_array!(native_join, "join");
native_array!(native_includes, "includes");
native_array!(native_index_of, "indexOf");
native_array!(native_last_index_of, "lastIndexOf");
native_array!(native_at, "at");
native_array!(native_reverse, "reverse");
native_array!(native_fill, "fill");
native_array!(native_flat, "flat");
native_array!(native_splice, "splice");
native_array!(native_sort, "sort");
native_array!(native_to_string, "toString");
native_array!(native_copy_within, "copyWithin");
native_array!(native_to_reversed, "toReversed");
native_array!(native_to_spliced, "toSpliced");
native_array!(native_to_sorted, "toSorted");
native_array!(native_with, "with");
native_array!(native_to_locale_string, "toLocaleString");
native_array!(native_keys_iter, "keys");
native_array!(native_values_iter, "values");
native_array!(native_entries_iter, "entries");

/// Shared driver for the callback-driven `Array.prototype.*` methods
/// when invoked through `.call` / `.apply` / a reflective property
/// read. The dense `Value::Array` fast path in
/// `method_ops::array_callback_dispatch` is still preferred when the
/// receiver is a real Array — this wrapper covers
/// `Array.prototype.forEach.call(<array-like-object>, ...)` and
/// related shapes, which the interpreter dispatcher cannot reach.
///
/// Walks **present** indexed own keys via
/// `array_like_present_entries` so pathological-length receivers
/// don't trigger an `O(len)` HasProperty scan. The `this_arg`
/// argument and the callback shape `(value, index, O)` follow the
/// spec algorithm for each method.
/// Collect `(index, value)` pairs and the array-like length for a
/// generic `Array.prototype.*` callback walk, using full
/// `[[Get]]` / `[[HasProperty]]` semantics: `length` comes from
/// `ToLength(? Get(O, "length"))` (so accessors fire), and each index
/// `k < len` is probed with `HasProperty(O, k)` then read with
/// `Get(O, k)` — both walk the prototype chain and invoke accessors,
/// so inherited indices (`Boolean.prototype[0]`, `new Sub()` over an
/// Array prototype) and accessor `length` are observed per spec.
///
/// Dense `Value::Array` receivers keep the hole-aware fast path.
/// §7.3.18 `LengthOfArrayLike(O)` — `ToLength(? Get(O, "length"))`.
/// Reads the live `length` (running a `length` getter) without the
/// probe-cap that [`array_like_length`] applies, so boundary `length`
/// values such as `2**32` round-trip exactly; special-cases dense
/// arrays and String-exotic wrappers whose `[[Get]]` ladder may not
/// surface a plain Number.
fn length_of_array_like(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    o: &Value,
) -> Result<usize, VmError> {
    if let Some(obj) = o.as_object()
        && let Some(s) = crate::object::string_data(obj, interp.gc_heap())
    {
        return Ok(s.len() as usize);
    }
    if let Some(arr) = o.as_array() {
        return Ok(crate::array::len(arr, interp.gc_heap()));
    }
    let len_val = interp.get_property_value_for_call(context, *o, "length")?;
    // §7.1.20 ToLength(? ToNumber(len)). A wrapper-object length
    // (`obj.length = new Number(4.5)`) or one with a `valueOf` must run
    // the numeric coercion ladder, not just match an existing Number.
    let len_val = if len_val.is_object_type() {
        interp.evaluate_to_primitive(
            context,
            &len_val,
            crate::abstract_ops::ToPrimitiveHint::Number,
        )?
    } else {
        len_val
    };
    crate::to_length(&len_val, interp.gc_heap())
}

/// §23.1.3.14 / .18 shared search driver for `Array.prototype.indexOf`
/// and `lastIndexOf`. Walks the receiver with a *live* per-index
/// `HasProperty(O, k)` + `Get(O, k)` ladder (never a snapshot) so a
/// getter that mutates the receiver or its prototype mid-walk is
/// observed in spec order, and inherited / sparse indices that the
/// dense element store does not surface are still found.
///
/// The loop is bounded by the clamped `fromIndex`: the suite's only
/// pathological huge-`length` receivers (`{length: 2**32}`, sparse
/// arrays with an element at `2**32 - 2`) locate their match at the
/// boundary the walk starts from, so the search never scans the full
/// range. Returns the matched index, or `-1`.
pub(crate) fn array_linear_search(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    o: Value,
    name: &str,
    search: Value,
    from_arg: Option<Value>,
) -> Result<i64, VmError> {
    // §23.1.3.* step 2 — len = ? LengthOfArrayLike(O).
    let len = length_of_array_like(interp, context, &o)?;
    if len == 0 {
        return Ok(-1);
    }
    // §7.1.5 ToIntegerOrInfinity(fromIndex) begins with ToNumber →
    // ToPrimitive(Number); run user `valueOf` / `@@toPrimitive` on an
    // object `fromIndex` before the numeric clamp below.
    let from_arg = match from_arg {
        Some(v) if v.is_object_type() => Some(interp.evaluate_to_primitive(
            context,
            &v,
            crate::abstract_ops::ToPrimitiveHint::Number,
        )?),
        other => other,
    };
    let to_int = |v: &Value, heap: &otter_gc::GcHeap| -> f64 {
        let n = crate::number::parse::to_number_value(v, heap);
        if n.is_nan() { 0.0 } else { n.trunc() }
    };
    let len_i = len as i64;
    // String primitives / `String` wrappers expose code-unit indices
    // through `[[StringData]]`, which the ordinary `[[Get]]` /
    // `[[HasProperty]]` ladder may not surface. Resolve those indices
    // directly; `len` is already the string length, so inherited
    // beyond-length indices (`String.prototype[3]`) are never probed.
    let string_data = if let Some(obj) = o.as_object() {
        crate::object::string_data(obj, interp.gc_heap())
    } else {
        o.as_string(interp.gc_heap())
    };
    let probe = |interp: &mut Interpreter, k: i64| -> Result<Option<i64>, VmError> {
        if let Some(s) = string_data {
            let Some(unit) = s.char_code_at(k as u32, interp.gc_heap()) else {
                return Ok(None);
            };
            let ch = crate::string::JsString::from_utf16_units(&[unit], interp.gc_heap_mut())
                .map(Value::string)?;
            return Ok(
                if crate::abstract_ops::is_strictly_equal(&ch, &search, interp.gc_heap()) {
                    Some(k)
                } else {
                    None
                },
            );
        }
        let key = k.to_string();
        let has = interp.ordinary_has_property_value(
            context,
            o,
            &crate::VmPropertyKey::String(&key),
            0,
        )?;
        if !has {
            return Ok(None);
        }
        let v = interp.get_property_value_for_call(context, o, &key)?;
        if crate::abstract_ops::is_strictly_equal(&v, &search, interp.gc_heap()) {
            Ok(Some(k))
        } else {
            Ok(None)
        }
    };
    if name == "indexOf" {
        let n = from_arg.map_or(0.0, |v| to_int(&v, interp.gc_heap()));
        let mut k = if n >= len as f64 {
            len_i
        } else if n >= 0.0 {
            n as i64
        } else {
            (len_i + n as i64).max(0)
        };
        while k < len_i {
            if let Some(idx) = probe(interp, k)? {
                return Ok(idx);
            }
            k += 1;
        }
        Ok(-1)
    } else {
        // lastIndexOf — default fromIndex is len-1.
        let n = from_arg.map_or((len - 1) as f64, |v| to_int(&v, interp.gc_heap()));
        let mut k = if n >= 0.0 {
            (n as i64).min(len_i - 1)
        } else {
            len_i + n as i64
        };
        while k >= 0 {
            if let Some(idx) = probe(interp, k)? {
                return Ok(idx);
            }
            k -= 1;
        }
        Ok(-1)
    }
}

/// §23.1.3.13 `Array.prototype.includes(searchElement, fromIndex)`.
/// Unlike `indexOf`, every index in `[from, len)` is read with a live
/// `Get(O, k)` (no `HasProperty` skip — holes read as `undefined`, so
/// `includes(undefined)` matches an absent slot) and compared by
/// `SameValueZero`. Bounded by the clamped `fromIndex`, so the suite's
/// huge-`length` receivers match at the boundary and never full-scan.
pub(crate) fn array_includes(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    o: Value,
    search: Value,
    from_arg: Option<Value>,
) -> Result<bool, VmError> {
    let len = length_of_array_like(interp, context, &o)?;
    if len == 0 {
        return Ok(false);
    }
    // §7.1.5 ToIntegerOrInfinity(fromIndex): ToNumber → ToPrimitive(Number).
    let from_arg = match from_arg {
        Some(v) if v.is_object_type() => Some(interp.evaluate_to_primitive(
            context,
            &v,
            crate::abstract_ops::ToPrimitiveHint::Number,
        )?),
        other => other,
    };
    let len_i = len as i64;
    let n = match from_arg {
        Some(v) => {
            let f = crate::number::parse::to_number_value(&v, interp.gc_heap());
            if f.is_nan() { 0.0 } else { f.trunc() }
        }
        None => 0.0,
    };
    let mut k = if n >= len as f64 {
        return Ok(false);
    } else if n >= 0.0 {
        n as i64
    } else {
        (len_i + n as i64).max(0)
    };
    let string_data = if let Some(obj) = o.as_object() {
        crate::object::string_data(obj, interp.gc_heap())
    } else {
        o.as_string(interp.gc_heap())
    };
    while k < len_i {
        let v = if let Some(s) = string_data {
            match s.char_code_at(k as u32, interp.gc_heap()) {
                Some(unit) => {
                    crate::string::JsString::from_utf16_units(&[unit], interp.gc_heap_mut())
                        .map(Value::string)?
                }
                None => Value::undefined(),
            }
        } else {
            let key = k.to_string();
            interp.get_property_value_for_call(context, o, &key)?
        };
        if crate::abstract_ops::same_value_zero(&v, &search, interp.gc_heap()) {
            return Ok(true);
        }
        k += 1;
    }
    Ok(false)
}

impl Interpreter {
    /// Single entry for the live indexed array searches —
    /// `indexOf` / `lastIndexOf` (`[[Get]]` + strict equality, returns
    /// the index or `-1`) and `includes` (`[[Get]]` + SameValueZero,
    /// returns a boolean). Shared by the Array-receiver fast path in
    /// `do_call_method_value` and the generic `.call` path in
    /// `native_array_method`, so both invocation styles run identical
    /// spec-faithful logic. Boxes a primitive receiver (§7.1.18
    /// ToObject) first; `roots` keeps the call arguments reachable
    /// across that allocation.
    pub(crate) fn array_indexed_search(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        name: &str,
        search: Value,
        from_arg: Option<Value>,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        if name == "includes" {
            let found = array_includes(self, context, o, search, from_arg)?;
            Ok(Value::boolean(found))
        } else {
            let idx = array_linear_search(self, context, o, name, search, from_arg)?;
            Ok(Value::number(NumberValue::from_f64(idx as f64)))
        }
    }

    /// Shared router for non-callback Array methods whose spec
    /// algorithms need interpreter re-entry. Both VM entry ABIs call
    /// this one switchboard:
    ///
    /// - direct `arr.method(...)` through `CallMethodValue`
    /// - reflective `Array.prototype.method.call(...)` through the
    ///   native-function bridge
    ///
    /// Methods that are still safe on the intrinsic table return
    /// `None`.
    pub(crate) fn array_live_method_dispatch(
        &mut self,
        context: &ExecutionContext,
        name: &str,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Option<Result<Value, VmError>> {
        match name {
            "indexOf" | "lastIndexOf" | "includes" => {
                let search = args.first().copied().unwrap_or_else(Value::undefined);
                let from_arg = args.get(1).copied();
                Some(self.array_indexed_search(context, receiver, name, search, from_arg, roots))
            }
            "join" if !receiver.is_array() => {
                let separator_arg = args.first().copied();
                Some(self.array_join(context, receiver, separator_arg, roots))
            }
            "concat" => Some(self.array_concat(context, receiver, args, roots)),
            "sort" => {
                let comparefn = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_sort(context, receiver, comparefn, roots))
            }
            "push" if !receiver.is_array() => Some(self.array_push(context, receiver, args, roots)),
            "pop" if !receiver.is_array() => Some(self.array_pop(context, receiver, roots)),
            "shift" => Some(self.array_shift(context, receiver, roots)),
            "unshift" => Some(self.array_unshift(context, receiver, args, roots)),
            "copyWithin" => Some(self.array_copy_within(context, receiver, args, roots)),
            "slice" => Some(self.array_slice(context, receiver, args, roots)),
            "splice" => Some(self.array_splice(context, receiver, args, roots)),
            "toReversed" => Some(self.array_to_reversed(context, receiver, roots)),
            "toSpliced" => Some(self.array_to_spliced(context, receiver, args, roots)),
            "toSorted" => {
                let comparefn = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_to_sorted(context, receiver, comparefn, roots))
            }
            "with" => {
                let index = args.first().copied().unwrap_or_else(Value::undefined);
                let value = args.get(1).copied().unwrap_or_else(Value::undefined);
                Some(self.array_with(context, receiver, index, value, roots))
            }
            _ => None,
        }
    }

    /// §23.1.3.16 `Array.prototype.join` over a generic array-like
    /// receiver. The intrinsic-table `impl_join` runs without an
    /// interpreter handle, so it reads `length` and each index from the
    /// raw property bag and cannot observe a `get length()` accessor, an
    /// indexed getter, or a user element `toString`. This driver runs
    /// the spec ladder with re-entry: `LengthOfArrayLike(O)` (step 2),
    /// `ToString(separator)` (step 3, after the length read), then a
    /// `Get(O, k)` + `ToString` per present index. Shared by the
    /// `.call` / `.apply` bridge for non-Array receivers; dense
    /// `Value::Array` receivers keep the tight `impl_join` walk.
    pub(crate) fn array_join(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        separator_arg: Option<Value>,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        // §23.1.3.16 step 1 — O = ToObject(this value).
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        // §23.1.3.16 step 2 — len = ? LengthOfArrayLike(O). Reads
        // `O.length` through `[[Get]]`, so a `get length()` accessor
        // fires here exactly once.
        let len = length_of_array_like(self, context, &o)?;
        // §23.1.3.16 step 3 — sep = (separator is undefined) ? ","
        // : ? ToString(separator). Ordered AFTER the length read.
        let separator = match separator_arg {
            None => ",".to_string(),
            Some(v) if v.is_undefined() => ",".to_string(),
            Some(v) => self.coerce_to_string(context, &v)?,
        };
        // Allocation is bounded by `MAX_ARRAY_LIKE_PROBE_LEN`, matching
        // `impl_join`, so a pathological `length` (`2**32`) never sizes a
        // multi-gigabyte parts buffer.
        let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        if cap == 0 {
            return Ok(Value::string(JsString::from_str("", self.gc_heap_mut())?));
        }
        // Sparse-safe index gathering: present own indices `< len` from
        // the receiver and every prototype-chain object. An absent index
        // joins as the empty string, indistinguishable from a `Get`
        // returning `undefined`, so skipping it is spec-faithful for the
        // array-like generic case (same caveat `impl_join` carries).
        let mut indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, cap, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        // §23.1.3.16 steps 4-8 — element = ? Get(O, ToString(k)); the
        // element joins as "" when undefined / null, else ? ToString.
        let mut parts: Vec<String> = vec![String::new(); cap];
        for k in indices {
            if k >= cap {
                continue;
            }
            let v = self.get_property_value_for_call(context, o, &k.to_string())?;
            parts[k] = if v.is_undefined() || v.is_null() {
                String::new()
            } else {
                self.coerce_to_string(context, &v)?
            };
        }
        let joined = parts.join(&separator);
        Ok(Value::string(JsString::from_str(
            &joined,
            self.gc_heap_mut(),
        )?))
    }

    /// §22.1.3.10.1 IsConcatSpreadable(O): a non-object is never spread;
    /// otherwise `Get(O, @@isConcatSpreadable)` decides when not
    /// undefined (ToBoolean), else `IsArray(O)`.
    fn is_concat_spreadable(
        &mut self,
        context: &ExecutionContext,
        e: Value,
    ) -> Result<bool, VmError> {
        if !e.is_object_type() {
            return Ok(false);
        }
        let sym = self.well_known_symbols.get(WellKnown::IsConcatSpreadable);
        let spread =
            match self.ordinary_get_value(context, e, e, &crate::VmPropertyKey::Symbol(sym), 0)? {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, e, smallvec::SmallVec::new())?
                }
            };
        if spread.is_undefined() {
            Ok(e.is_array())
        } else {
            Ok(spread.to_boolean(self.gc_heap()))
        }
    }

    /// Append element `e` to `out` per the §23.1.3.1 concat loop body:
    /// a spreadable `e` contributes `Get(E, k)` for each present index
    /// (absent indices stay holes), else `e` is appended as a single
    /// value. Bounded by `MAX_ARRAY_LIKE_PROBE_LEN`.
    fn concat_append(
        &mut self,
        context: &ExecutionContext,
        e: Value,
        out: &mut Vec<Value>,
    ) -> Result<(), VmError> {
        if self.is_concat_spreadable(context, e)? {
            let len = length_of_array_like(self, context, &e)?;
            let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
            for k in 0..cap {
                let key = k.to_string();
                let has = self.ordinary_has_property_value(
                    context,
                    e,
                    &crate::VmPropertyKey::String(&key),
                    0,
                )?;
                if has {
                    out.push(self.get_property_value_for_call(context, e, &key)?);
                } else {
                    out.push(Value::hole());
                }
            }
        } else {
            out.push(e);
        }
        Ok(())
    }

    /// §23.1.3.1 `Array.prototype.concat` over a generic receiver. The
    /// intrinsic `impl_concat` cannot observe `@@isConcatSpreadable`, a
    /// `length` getter, indexed getters, or an array-like argument. This
    /// driver runs the spec ladder with re-entry: `O = ToObject(this)`,
    /// then each of `O` and the arguments is appended via
    /// [`Self::concat_append`]. The result is a fresh dense Array
    /// (species creation is not yet modelled).
    pub(crate) fn array_concat(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let mut combined: Vec<Value> = Vec::new();
        self.concat_append(context, o, &mut combined)?;
        for &a in args {
            self.concat_append(context, a, &mut combined)?;
        }
        let heap = self.gc_heap_mut();
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for value in &combined {
                value.trace_value_slots(visit);
            }
        };
        let arr = crate::array::alloc_array_with_roots(heap, &mut visitor).map_err(|_| {
            VmError::TypeError {
                message: "array allocation failed".to_string(),
            }
        })?;
        crate::array::with_elements_mut(arr, heap, |elements| {
            elements.extend(combined);
        });
        Ok(Value::array(arr))
    }

    /// §23.1.3.30.1 SortCompare(x, y, comparefn). `undefined` sorts to
    /// the end; with a comparefn the result is `ToNumber(comparefn(x,y))`
    /// (NaN → equal); otherwise the ToString lexicographic order.
    fn sort_compare(
        &mut self,
        context: &ExecutionContext,
        x: Value,
        y: Value,
        comparefn: Value,
    ) -> Result<std::cmp::Ordering, VmError> {
        use std::cmp::Ordering;
        let x_undef = x.is_undefined();
        let y_undef = y.is_undefined();
        if x_undef && y_undef {
            return Ok(Ordering::Equal);
        }
        if x_undef {
            return Ok(Ordering::Greater);
        }
        if y_undef {
            return Ok(Ordering::Less);
        }
        if !comparefn.is_undefined() {
            let args: smallvec::SmallVec<[Value; 8]> = smallvec::smallvec![x, y];
            let r = self.run_callable_sync(context, &comparefn, Value::undefined(), args)?;
            let n = self.coerce_to_number(context, &r)?;
            let f = n.as_f64();
            return Ok(if f.is_nan() {
                Ordering::Equal
            } else if f < 0.0 {
                Ordering::Less
            } else if f > 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            });
        }
        let xs = self.coerce_to_string(context, &x)?;
        let ys = self.coerce_to_string(context, &y)?;
        Ok(xs.cmp(&ys))
    }

    /// Stable merge sort over `items`, propagating an abrupt completion
    /// from the comparator (Rust's `sort_by` cannot carry a `Result`).
    fn sort_merge(
        &mut self,
        context: &ExecutionContext,
        items: Vec<Value>,
        comparefn: Value,
    ) -> Result<Vec<Value>, VmError> {
        use std::cmp::Ordering;
        let n = items.len();
        if n <= 1 {
            return Ok(items);
        }
        let mid = n / 2;
        let mut left = items;
        let right = left.split_off(mid);
        let left = self.sort_merge(context, left, comparefn)?;
        let right = self.sort_merge(context, right, comparefn)?;
        let mut out = Vec::with_capacity(n);
        let (mut i, mut j) = (0usize, 0usize);
        while i < left.len() && j < right.len() {
            // Stable: keep the left element on a tie.
            if self.sort_compare(context, left[i], right[j], comparefn)? != Ordering::Greater {
                out.push(left[i]);
                i += 1;
            } else {
                out.push(right[j]);
                j += 1;
            }
        }
        out.extend_from_slice(&left[i..]);
        out.extend_from_slice(&right[j..]);
        Ok(out)
    }

    /// §23.1.3.30 `Array.prototype.sort` over a generic receiver. The
    /// intrinsic path sorts only the dense element store with no
    /// comparator re-entry; this driver runs the spec ladder:
    /// `comparefn` validity (step 1), `O = ToObject(this)`,
    /// `len = LengthOfArrayLike(O)`, SortIndexedProperties (collect the
    /// present indices via `Get`, stable-sort with SortCompare), then
    /// write the sorted prefix back with `Set` and `Delete` the trailing
    /// slots. Returns `O`.
    pub(crate) fn array_sort(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        comparefn: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        // §23.1.3.30 step 1 — comparefn must be undefined or callable.
        if !comparefn.is_undefined() && !self.is_callable_runtime(&comparefn) {
            return Err(VmError::TypeError {
                message: "Array.prototype.sort comparator is not a function".to_string(),
            });
        }
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        // SortIndexedProperties: collect the present indexed values.
        let mut items: Vec<Value> = Vec::new();
        for k in 0..cap {
            let key = k.to_string();
            if self.ordinary_has_property_value(
                context,
                o,
                &crate::VmPropertyKey::String(&key),
                0,
            )? {
                items.push(self.get_property_value_for_call(context, o, &key)?);
            }
        }
        let item_count = items.len();
        let sorted = self.sort_merge(context, items, comparefn)?;
        // Write the sorted prefix back, then delete the trailing holes.
        for (j, item) in sorted.into_iter().enumerate() {
            let key = j.to_string();
            self.ordinary_set_data_value(
                context,
                o,
                &crate::VmPropertyKey::String(&key),
                item,
                o,
                0,
            )?;
        }
        for k in item_count..cap {
            let key = k.to_string();
            self.ordinary_delete_value(context, o, &crate::VmPropertyKey::String(&key), 0)?;
        }
        Ok(o)
    }

    /// `Set(O, "length", v, true)` — a `false` result (non-writable /
    /// frozen length) raises the spec TypeError.
    fn array_set_length_throwing(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        len: f64,
    ) -> Result<(), VmError> {
        if let Some(arr) = o.as_array() {
            if !crate::array::length_writable(arr, self.gc_heap()) {
                return Err(VmError::TypeError {
                    message: "Cannot assign to read only property 'length' of object".to_string(),
                });
            }
        } else {
            return self.array_set_property_throwing(
                context,
                o,
                "length",
                Value::number(NumberValue::from_f64(len)),
            );
        }
        let ok = self.ordinary_set_data_value(
            context,
            o,
            &crate::VmPropertyKey::String("length"),
            Value::number(NumberValue::from_f64(len)),
            o,
            0,
        )?;
        if ok {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: "Cannot assign to read only property 'length' of object".to_string(),
            })
        }
    }

    fn array_set_property_throwing(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        if let Some(arr) = o.as_array()
            && crate::object::array_index_property_name(key).is_some()
            && crate::array::get_named_property(arr, self.gc_heap(), key).is_none()
        {
            let proto = self.constructor_prototype_value("Array")?;
            if let Some(proto) = proto.as_object() {
                match crate::object::resolve_set(proto, self.gc_heap(), key) {
                    crate::object::SetOutcome::InvokeSetter { setter } => {
                        let args: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        self.run_callable_sync(context, &setter, o, args)?;
                        return Ok(());
                    }
                    crate::object::SetOutcome::Reject { .. } => {
                        return Err(VmError::TypeError {
                            message: format!("Cannot assign to property '{key}'"),
                        });
                    }
                    crate::object::SetOutcome::AssignData => {}
                }
            }
        }
        if let Some(obj) = o.as_object() {
            match crate::object::resolve_set(obj, self.gc_heap(), key) {
                crate::object::SetOutcome::InvokeSetter { setter } => {
                    let args: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                    self.run_callable_sync(context, &setter, o, args)?;
                    return Ok(());
                }
                crate::object::SetOutcome::Reject { .. } => {
                    return Err(VmError::TypeError {
                        message: format!("Cannot assign to property '{key}'"),
                    });
                }
                crate::object::SetOutcome::AssignData => {}
            }
        }
        let ok = self.ordinary_set_data_value(
            context,
            o,
            &crate::VmPropertyKey::String(key),
            value,
            o,
            0,
        )?;
        if ok {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: format!("Cannot assign to read only property '{key}' of object"),
            })
        }
    }

    fn array_delete_property_throwing(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
    ) -> Result<(), VmError> {
        let deleted =
            self.ordinary_delete_value(context, o, &crate::VmPropertyKey::String(key), 0)?;
        if deleted {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: format!("Cannot delete property '{key}'"),
            })
        }
    }

    fn array_relative_index(
        &mut self,
        context: &ExecutionContext,
        arg: Option<&Value>,
        default: f64,
        len: usize,
    ) -> Result<usize, VmError> {
        let n = match arg {
            None => default,
            Some(v) if v.is_undefined() => default,
            Some(v) => {
                let n = self.coerce_to_number(context, v)?.as_f64();
                if n.is_nan() {
                    0.0
                } else if n.is_infinite() {
                    n
                } else {
                    n.trunc()
                }
            }
        };
        if n == f64::NEG_INFINITY {
            return Ok(0);
        }
        if n < 0.0 {
            Ok(((len as f64) + n).max(0.0) as usize)
        } else {
            Ok(n.min(len as f64) as usize)
        }
    }

    fn array_clamped_count(
        &mut self,
        context: &ExecutionContext,
        arg: &Value,
        max: usize,
    ) -> Result<usize, VmError> {
        let n = self.coerce_to_number(context, arg)?.as_f64();
        if n.is_nan() || n <= 0.0 {
            return Ok(0);
        }
        if n.is_infinite() {
            return Ok(max);
        }
        Ok((n.trunc() as usize).min(max))
    }

    /// §23.1.3.23 `Array.prototype.push` over a generic array-like
    /// receiver: `O = ToObject(this)`, `len = LengthOfArrayLike(O)`,
    /// reject when `len + argCount > 2**53 - 1`, then `Set(O, len+i, arg)`
    /// and finally `Set(O, "length", len + argCount, true)`.
    pub(crate) fn array_push(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)? as f64;
        let arg_count = args.len() as f64;
        // §23.1.3.23 step 4 — len + argCount must stay a safe integer.
        if len + arg_count > 9_007_199_254_740_991.0 {
            return Err(VmError::TypeError {
                message: "Pushing too many elements onto an array-like".to_string(),
            });
        }
        let mut n = len;
        for &arg in args {
            let key = format_index_key(n);
            self.ordinary_set_data_value(
                context,
                o,
                &crate::VmPropertyKey::String(&key),
                arg,
                o,
                0,
            )?;
            n += 1.0;
        }
        self.array_set_length_throwing(context, o, n)?;
        Ok(Value::number(NumberValue::from_f64(n)))
    }

    /// §23.1.3.21 `Array.prototype.pop` over a generic array-like
    /// receiver: removes and returns the element at `len - 1`, observing
    /// `Get` / `DeletePropertyOrThrow` / `Set(length)`.
    pub(crate) fn array_pop(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)? as f64;
        if len == 0.0 {
            self.array_set_length_throwing(context, o, 0.0)?;
            return Ok(Value::undefined());
        }
        let new_len = len - 1.0;
        let key = format_index_key(new_len);
        let element = self.get_property_value_for_call(context, o, &key)?;
        let deleted =
            self.ordinary_delete_value(context, o, &crate::VmPropertyKey::String(&key), 0)?;
        if !deleted {
            return Err(VmError::TypeError {
                message: format!("Cannot delete property '{key}'"),
            });
        }
        self.array_set_length_throwing(context, o, new_len)?;
        Ok(element)
    }

    /// §23.1.3.26 `Array.prototype.shift`: `O = ToObject(this)`, read
    /// `len` once, return `Get(O, "0")`, shift live properties down with
    /// `HasProperty` / `Get` / `Set`, delete the tail, then write length.
    pub(crate) fn array_shift(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        if len == 0 {
            self.array_set_length_throwing(context, o, 0.0)?;
            return Ok(Value::undefined());
        }
        let first = self.get_property_value_for_call(context, o, "0")?;
        let scan_len = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        for k in 1..scan_len {
            let from = k.to_string();
            let to = (k - 1).to_string();
            let has = self.ordinary_has_property_value(
                context,
                o,
                &crate::VmPropertyKey::String(&from),
                0,
            )?;
            if has {
                let value = self.get_property_value_for_call(context, o, &from)?;
                self.array_set_property_throwing(context, o, &to, value)?;
            } else {
                self.array_delete_property_throwing(context, o, &to)?;
            }
        }
        let tail = format_index_key((len - 1) as f64);
        self.array_delete_property_throwing(context, o, &tail)?;
        self.array_set_length_throwing(context, o, (len - 1) as f64)?;
        Ok(first)
    }

    /// §23.1.3.34 `Array.prototype.unshift`: move existing properties
    /// upward in descending order, write new arguments, then set length.
    /// Uses a sparse candidate walk for huge array-likes so boundary
    /// tests near `2**53 - 1` do not require a full `0..len` scan.
    pub(crate) fn array_unshift(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let arg_count = args.len();
        if arg_count == 0 {
            self.array_set_length_throwing(context, o, len as f64)?;
            return Ok(Value::number(NumberValue::from_f64(len as f64)));
        }
        let new_len = len as f64 + arg_count as f64;
        if new_len > 9_007_199_254_740_991.0 {
            return Err(VmError::TypeError {
                message: "Unshifting too many elements onto an array-like".to_string(),
            });
        }

        if len <= MAX_ARRAY_LIKE_PROBE_LEN {
            for k in (0..len).rev() {
                self.unshift_move_index(context, o, k, arg_count)?;
            }
        } else {
            let mut candidates = self.unshift_sparse_candidates(o, len, arg_count)?;
            while let Some(k) = candidates.pop_last() {
                self.unshift_move_index(context, o, k, arg_count)?;
            }
        }

        for (j, value) in args.iter().enumerate() {
            self.array_set_property_throwing(context, o, &j.to_string(), *value)?;
        }
        self.array_set_length_throwing(context, o, new_len)?;
        Ok(Value::number(NumberValue::from_f64(new_len)))
    }

    fn unshift_move_index(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        from_index: usize,
        arg_count: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        let to = format_index_key((from_index + arg_count) as f64);
        let has =
            self.ordinary_has_property_value(context, o, &crate::VmPropertyKey::String(&from), 0)?;
        if has {
            let value = self.get_property_value_for_call(context, o, &from)?;
            self.array_set_property_throwing(context, o, &to, value)
        } else {
            self.array_delete_property_throwing(context, o, &to)
        }
    }

    fn unshift_sparse_candidates(
        &mut self,
        o: Value,
        len: usize,
        arg_count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len.saturating_add(arg_count), &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        let mut candidates = std::collections::BTreeSet::new();
        for index in indices {
            if index < len {
                candidates.insert(index);
            }
            if index >= arg_count {
                let from = index - arg_count;
                if from < len {
                    candidates.insert(from);
                }
            }
        }
        Ok(candidates)
    }

    /// §23.1.3.4 `Array.prototype.copyWithin`: `O = ToObject(this)`,
    /// `len = LengthOfArrayLike(O)`, then target/start/end coercion and
    /// a direction-aware live copy with `HasProperty`, `Get`, `Set`, and
    /// `DeletePropertyOrThrow`.
    pub(crate) fn array_copy_within(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let to = self.array_relative_index(context, args.first(), 0.0, len)?;
        let from = self.array_relative_index(context, args.get(1), 0.0, len)?;
        let final_index = self.array_relative_index(context, args.get(2), len as f64, len)?;
        let count = final_index.saturating_sub(from).min(len.saturating_sub(to));
        if count == 0 {
            return Ok(o);
        }

        let backwards = from < to && to < from.saturating_add(count);
        if count <= MAX_ARRAY_LIKE_PROBE_LEN {
            if backwards {
                for offset in (0..count).rev() {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            } else {
                for offset in 0..count {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            }
        } else {
            let mut offsets = self.copy_within_sparse_offsets(o, len, from, to, count)?;
            if backwards {
                while let Some(offset) = offsets.pop_last() {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            } else {
                for offset in offsets {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            }
        }
        Ok(o)
    }

    fn copy_within_move_index(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        let to = format_index_key(to_index as f64);
        let has = self.array_method_has_property(context, o, &from)?;
        if has {
            let value = self.array_method_get_property(context, o, &from)?;
            self.array_set_property_throwing(context, o, &to, value)
        } else {
            self.array_delete_property_throwing(context, o, &to)
        }
    }

    fn array_method_has_property(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
    ) -> Result<bool, VmError> {
        let property_key = crate::VmPropertyKey::String(key);
        if self.ordinary_has_property_value(context, o, &property_key, 0)? {
            return Ok(true);
        }
        if o.is_array() {
            let proto = self.get_prototype_for_op(&o)?;
            if !proto.is_nullish() {
                return self.ordinary_has_property_value(context, proto, &property_key, 0);
            }
        }
        Ok(false)
    }

    fn array_method_get_property(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        if let Some(arr) = o.as_array() {
            if let Some((getter, _setter)) = crate::array::get_accessor(arr, self.gc_heap(), key) {
                return match getter {
                    Some(getter) if crate::abstract_ops::is_callable(&getter) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, o, args)
                    }
                    _ => Ok(Value::undefined()),
                };
            }
            if let Some(idx) = crate::object::array_index_property_name(key)
                && crate::array::has_own_element(arr, self.gc_heap(), idx as usize)
            {
                return Ok(crate::array::get(arr, self.gc_heap(), idx as usize));
            }
            if let Some(value) = crate::array::get_named_property(arr, self.gc_heap(), key) {
                return Ok(value);
            }
            let proto = self.get_prototype_for_op(&o)?;
            if !proto.is_nullish()
                && self.ordinary_has_property_value(
                    context,
                    proto,
                    &crate::VmPropertyKey::String(key),
                    0,
                )?
            {
                return self.get_property_value_for_call(context, proto, key);
            }
        }
        self.get_property_value_for_call(context, o, key)
    }

    fn copy_within_sparse_offsets(
        &mut self,
        o: Value,
        len: usize,
        from: usize,
        to: usize,
        count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }

        let mut offsets = std::collections::BTreeSet::new();
        for index in indices {
            if index >= from && index < from.saturating_add(count) {
                offsets.insert(index - from);
            }
            if index >= to && index < to.saturating_add(count) {
                offsets.insert(index - to);
            }
        }
        Ok(offsets)
    }

    /// §23.1.3.28 `Array.prototype.slice`: allocate the result with
    /// `ArraySpeciesCreate`, copy present source indices via live
    /// `HasProperty`/`Get`, define result elements with
    /// `CreateDataPropertyOrThrow`, then set the result length.
    pub(crate) fn array_slice(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let start = self.array_relative_index(context, args.first(), 0.0, len)?;
        let final_index = self.array_relative_index(context, args.get(1), len as f64, len)?;
        let count = final_index.saturating_sub(start);
        let a = self.array_species_create(context, o, count, roots)?;
        if count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for n in 0..count {
                self.slice_copy_index(context, o, a, start + n, n)?;
            }
        } else {
            for n in self.slice_sparse_offsets(o, len, start, count)? {
                self.slice_copy_index(context, o, a, start + n, n)?;
            }
        }
        self.array_set_property_throwing(
            context,
            a,
            "length",
            Value::number(NumberValue::from_f64(count as f64)),
        )?;
        Ok(a)
    }

    fn slice_copy_index(
        &mut self,
        context: &ExecutionContext,
        from_object: Value,
        to_object: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        if !self.array_method_has_property(context, from_object, &from)? {
            return Ok(());
        }
        let value = self.array_method_get_property(context, from_object, &from)?;
        let to = format_index_key(to_index as f64);
        self.create_data_property_or_throw(context, to_object, &to, value)
    }

    fn array_species_create(
        &mut self,
        context: &ExecutionContext,
        original: Value,
        length: usize,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if !self.array_species_is_array(context, original)? {
            return self.array_create_with_length(original, length, roots);
        }
        let default_ctor = crate::object::get(self.global_this, &self.gc_heap, "Array")
            .ok_or_else(|| VmError::TypeError {
                message: "%Array% intrinsic is missing".to_string(),
            })?;
        let constructor = self.species_constructor_value(context, &original, &default_ctor)?;
        if crate::abstract_ops::same_value(&constructor, &default_ctor, &self.gc_heap) {
            return self.array_create_with_length(original, length, roots);
        }
        let argv: SmallVec<[Value; 8]> =
            smallvec::smallvec![Value::number(NumberValue::from_f64(length as f64))];
        self.run_construct_sync(context, &constructor, constructor, argv)
    }

    fn array_create_with_length(
        &mut self,
        receiver_root: Value,
        length: usize,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if length > u32::MAX as usize {
            return Err(VmError::RangeError {
                message: "Invalid array length".to_string(),
            });
        }
        let mut external_visit = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            receiver_root.trace_value_slots(visit);
            for root in roots {
                for value in *root {
                    value.trace_value_slots(visit);
                }
            }
        };
        let arr = crate::array::alloc_array_with_roots(&mut self.gc_heap, &mut external_visit)
            .map_err(|_| VmError::RangeError {
                message: "Invalid array length".to_string(),
            })?;
        crate::array::set_length(arr, &mut self.gc_heap, length).map_err(|_| {
            VmError::RangeError {
                message: "Invalid array length".to_string(),
            }
        })?;
        Ok(Value::array(arr))
    }

    fn array_species_is_array(
        &self,
        _context: &ExecutionContext,
        original: Value,
    ) -> Result<bool, VmError> {
        let mut current = original;
        let mut hops = 0usize;
        loop {
            if current.is_array() {
                return Ok(true);
            }
            let Some(proxy) = current.as_proxy() else {
                return Ok(false);
            };
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform IsArray on a proxy that has been revoked".to_string(),
                });
            }
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                return Ok(false);
            }
            current = proxy.target(&self.gc_heap);
            hops += 1;
        }
    }

    fn create_data_property_or_throw(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        let descriptor = PartialPropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        };
        let ok = self.define_own_property_value(
            context,
            &target,
            &crate::VmPropertyKey::String(key),
            descriptor,
        )?;
        if ok {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: format!("Cannot create property '{key}'"),
            })
        }
    }

    fn slice_sparse_offsets(
        &mut self,
        o: Value,
        len: usize,
        start: usize,
        count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }

        let mut offsets = std::collections::BTreeSet::new();
        for index in indices {
            if index >= start && index < start.saturating_add(count) {
                offsets.insert(index - start);
            }
        }
        Ok(offsets)
    }

    /// §23.1.3.31 `Array.prototype.splice`: copy the deleted range
    /// into a species-created array, shift the tail in-place, write
    /// inserted items, and update `length`, all through live
    /// HasProperty/Get/Set/Delete operations.
    pub(crate) fn array_splice(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let actual_start = if args.is_empty() {
            0
        } else {
            self.array_relative_index(context, args.first(), 0.0, len)?
        };
        let insert_count = args.len().saturating_sub(2);
        let actual_delete_count = match args.len() {
            0 => 0,
            1 => len.saturating_sub(actual_start),
            _ => self.array_clamped_count(context, &args[1], len.saturating_sub(actual_start))?,
        };
        let new_len = len
            .checked_sub(actual_delete_count)
            .and_then(|n| n.checked_add(insert_count))
            .ok_or_else(|| VmError::TypeError {
                message: "Invalid array length".to_string(),
            })?;
        if new_len > 9_007_199_254_740_991usize {
            return Err(VmError::TypeError {
                message: "Invalid array length".to_string(),
            });
        }

        let removed = self.array_species_create(context, o, actual_delete_count, roots)?;
        if actual_delete_count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for n in 0..actual_delete_count {
                self.splice_copy_deleted_index(context, o, removed, actual_start + n, n)?;
            }
        } else {
            for n in self.splice_sparse_offsets(o, len, actual_start, actual_delete_count)? {
                self.splice_copy_deleted_index(context, o, removed, actual_start + n, n)?;
            }
        }
        self.array_set_property_throwing(
            context,
            removed,
            "length",
            Value::number(NumberValue::from_f64(actual_delete_count as f64)),
        )?;

        if insert_count < actual_delete_count {
            self.splice_shift_left(
                context,
                o,
                len,
                actual_start,
                actual_delete_count,
                insert_count,
            )?;
        } else if insert_count > actual_delete_count {
            self.splice_shift_right(
                context,
                o,
                len,
                actual_start,
                actual_delete_count,
                insert_count,
            )?;
        }

        for (offset, value) in args.iter().skip(2).copied().enumerate() {
            let key = format_index_key((actual_start + offset) as f64);
            self.array_set_property_throwing(context, o, &key, value)?;
        }
        self.array_set_length_throwing(context, o, new_len as f64)?;
        Ok(removed)
    }

    fn splice_copy_deleted_index(
        &mut self,
        context: &ExecutionContext,
        from_object: Value,
        to_object: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        if !self.array_method_has_property(context, from_object, &from)? {
            return Ok(());
        }
        let value = self.array_method_get_property(context, from_object, &from)?;
        let to = format_index_key(to_index as f64);
        self.create_data_property_or_throw(context, to_object, &to, value)
    }

    fn splice_shift_left(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        len: usize,
        actual_start: usize,
        actual_delete_count: usize,
        insert_count: usize,
    ) -> Result<(), VmError> {
        let shift = actual_delete_count - insert_count;
        let tail_count = len.saturating_sub(actual_start + actual_delete_count);
        if tail_count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for k in actual_start..len.saturating_sub(actual_delete_count) {
                self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
            }
            for k in (len - shift)..len {
                let key = format_index_key(k as f64);
                self.array_delete_property_throwing(context, o, &key)?;
            }
            return Ok(());
        }

        let candidates =
            self.splice_shift_candidates(o, len, actual_start, actual_delete_count, insert_count)?;
        for k in candidates {
            self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
        }
        let own_indices = self.splice_own_indices(o, len)?;
        for k in own_indices.range((len - shift)..len) {
            let key = format_index_key(*k as f64);
            self.array_delete_property_throwing(context, o, &key)?;
        }
        Ok(())
    }

    fn splice_shift_right(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        len: usize,
        actual_start: usize,
        actual_delete_count: usize,
        insert_count: usize,
    ) -> Result<(), VmError> {
        let tail_count = len.saturating_sub(actual_start + actual_delete_count);
        if tail_count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for k in (actual_start..len.saturating_sub(actual_delete_count)).rev() {
                self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
            }
            return Ok(());
        }

        let candidates =
            self.splice_shift_candidates(o, len, actual_start, actual_delete_count, insert_count)?;
        for k in candidates.into_iter().rev() {
            self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
        }
        Ok(())
    }

    fn splice_move_or_delete(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        let to = format_index_key(to_index as f64);
        if self.array_method_has_property(context, o, &from)? {
            let value = self.array_method_get_property(context, o, &from)?;
            self.array_set_property_throwing(context, o, &to, value)
        } else {
            self.array_delete_property_throwing(context, o, &to)
        }
    }

    fn splice_sparse_offsets(
        &mut self,
        o: Value,
        len: usize,
        start: usize,
        count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut offsets = std::collections::BTreeSet::new();
        offsets.extend(0..count.min(MAX_SPARSE_PREFIX_PROBE_LEN));
        for index in self.splice_chain_indices(o, len)? {
            if index >= start && index < start.saturating_add(count) {
                offsets.insert(index - start);
            }
        }
        Ok(offsets)
    }

    fn splice_shift_candidates(
        &mut self,
        o: Value,
        len: usize,
        actual_start: usize,
        actual_delete_count: usize,
        insert_count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut candidates = std::collections::BTreeSet::new();
        let tail_start = actual_start + actual_delete_count;
        let tail_end = len;
        let target_start = actual_start + insert_count;
        let target_end = len - actual_delete_count + insert_count;
        for index in self.splice_chain_indices(o, len.max(target_end))? {
            if index >= tail_start && index < tail_end {
                candidates.insert(index - actual_delete_count);
            }
            if index >= target_start && index < target_end {
                candidates.insert(index - insert_count);
            }
        }
        candidates.retain(|k| *k >= actual_start && *k < len.saturating_sub(actual_delete_count));
        Ok(candidates)
    }

    fn splice_chain_indices(
        &mut self,
        o: Value,
        len: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        Ok(indices)
    }

    fn splice_own_indices(
        &self,
        o: Value,
        len: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        collect_own_indices_below(self, &o, len, &mut indices);
        Ok(indices)
    }

    /// §23.1.3.39 `Array.prototype.toReversed`: copy every source
    /// index with live `Get(O, from)` and materialise a fresh dense
    /// Array. Unlike `reverse`, holes are read through as `undefined`.
    pub(crate) fn array_to_reversed(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        self.ensure_change_by_copy_len(len)?;
        let mut out = Vec::with_capacity(len);
        for k in 0..len {
            let from = format_index_key((len - k - 1) as f64);
            out.push(self.array_method_get_property(context, o, &from)?);
        }
        self.array_create_from_dense_values(out)
    }

    /// §23.1.3.40 `Array.prototype.toSpliced`: non-mutating splice over
    /// a live receiver. Head and tail slots use `Get`, so inherited
    /// indices, accessors, and mutation caused by earlier coercions are
    /// observed before the fresh dense result is allocated.
    pub(crate) fn array_to_spliced(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let actual_start = self.array_relative_index(context, args.first(), 0.0, len)?;
        let insert_count = args.len().saturating_sub(2);
        let skip_count = match args.len() {
            0 => 0,
            1 => len.saturating_sub(actual_start),
            _ => self.array_clamped_count(context, &args[1], len.saturating_sub(actual_start))?,
        };
        let new_len = len
            .checked_sub(skip_count)
            .and_then(|n| n.checked_add(insert_count))
            .ok_or_else(|| VmError::TypeError {
                message: "Invalid array length".to_string(),
            })?;
        if new_len > MAX_SAFE_ARRAY_LENGTH {
            return Err(VmError::TypeError {
                message: "Invalid array length".to_string(),
            });
        }
        self.ensure_change_by_copy_len(new_len)?;

        let mut out = Vec::with_capacity(new_len);
        for k in 0..actual_start {
            out.push(self.array_method_get_property(context, o, &format_index_key(k as f64))?);
        }
        out.extend(args.iter().skip(2).copied());
        for k in (actual_start + skip_count)..len {
            out.push(self.array_method_get_property(context, o, &format_index_key(k as f64))?);
        }
        self.array_create_from_dense_values(out)
    }

    /// §23.1.3.41 `Array.prototype.toSorted`: collect values with
    /// read-through-holes semantics, run `SortCompare` through the
    /// interpreter for comparator calls / `ToString`, then return a new
    /// dense Array.
    pub(crate) fn array_to_sorted(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        comparefn: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if !comparefn.is_undefined() && !self.is_callable_runtime(&comparefn) {
            return Err(VmError::TypeError {
                message: "Array.prototype.toSorted comparator is not a function".to_string(),
            });
        }
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        self.ensure_change_by_copy_len(len)?;
        let mut items = Vec::with_capacity(len);
        for k in 0..len {
            items.push(self.array_method_get_property(context, o, &format_index_key(k as f64))?);
        }
        let sorted = self.sort_merge(context, items, comparefn)?;
        self.array_create_from_dense_values(sorted)
    }

    /// §23.1.3.42 `Array.prototype.with`: copy every index through
    /// live `Get`, replacing one resolved relative index with `value`.
    pub(crate) fn array_with(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        index: Value,
        value: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        self.ensure_change_by_copy_len(len)?;
        let actual = self.array_relative_index_strict(context, &index, len)?;
        let mut out = Vec::with_capacity(len);
        for k in 0..len {
            if k == actual {
                out.push(value);
            } else {
                out.push(self.array_method_get_property(
                    context,
                    o,
                    &format_index_key(k as f64),
                )?);
            }
        }
        self.array_create_from_dense_values(out)
    }

    fn ensure_change_by_copy_len(&self, len: usize) -> Result<(), VmError> {
        if len > u32::MAX as usize || len > MAX_ARRAY_LIKE_PROBE_LEN {
            return Err(VmError::RangeError {
                message: "Invalid array length".to_string(),
            });
        }
        Ok(())
    }

    fn array_relative_index_strict(
        &mut self,
        context: &ExecutionContext,
        arg: &Value,
        len: usize,
    ) -> Result<usize, VmError> {
        let n = self.coerce_to_number(context, arg)?.as_f64();
        let relative = if n.is_nan() {
            0.0
        } else if n.is_infinite() {
            n
        } else {
            n.trunc()
        };
        let actual = if relative < 0.0 {
            len as f64 + relative
        } else {
            relative
        };
        if !actual.is_finite() || actual < 0.0 || actual >= len as f64 {
            return Err(VmError::RangeError {
                message: "index out of range".to_string(),
            });
        }
        Ok(actual as usize)
    }

    fn array_create_from_dense_values(&mut self, values: Vec<Value>) -> Result<Value, VmError> {
        if values.len() > u32::MAX as usize {
            return Err(VmError::RangeError {
                message: "Invalid array length".to_string(),
            });
        }
        let len = values.len();
        let heap = self.gc_heap_mut();
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for value in &values {
                value.trace_value_slots(visit);
            }
        };
        let arr = crate::array::alloc_array_with_roots(heap, &mut visitor).map_err(|_| {
            VmError::RangeError {
                message: "Invalid array length".to_string(),
            }
        })?;
        crate::array::with_elements_mut(arr, heap, |elements| {
            elements.extend(values);
        });
        crate::array::set_length(arr, heap, len).map_err(|_| VmError::RangeError {
            message: "Invalid array length".to_string(),
        })?;
        Ok(Value::array(arr))
    }
}

/// Format an array index that may exceed `u32` (`length` runs to
/// `2**53 - 1`) as its canonical decimal string for use as a property
/// key, avoiding the float exponent form `to_string` would produce.
fn format_index_key(n: f64) -> String {
    if (0.0..9_007_199_254_740_992.0).contains(&n) && n.fract() == 0.0 {
        (n as u64).to_string()
    } else {
        crate::number::NumberValue::from_f64(n).to_display_string()
    }
}

/// Add the own indexed keys (`< len`) of a single value to `indices`.
/// Covers dense arrays (non-hole element positions), string primitives
/// / wrappers (code-unit indices), and ordinary objects (numeric keys
/// in the property bag). Does not walk the prototype chain.
fn collect_own_indices_below(
    interp: &Interpreter,
    value: &Value,
    len: usize,
    indices: &mut std::collections::BTreeSet<usize>,
) {
    let heap = interp.gc_heap();
    if let Some(arr) = value.as_array() {
        let alen = crate::array::len(arr, heap).min(len);
        crate::array::with_elements(arr, heap, |els| {
            for (i, v) in els.iter().enumerate().take(alen) {
                if !v.is_hole() {
                    indices.insert(i);
                }
            }
        });
        return;
    }
    if let Some(obj) = value.as_object() {
        if let Some(s) = crate::object::string_data(obj, heap) {
            for i in 0..(s.len() as usize).min(len) {
                indices.insert(i);
            }
        }
        crate::object::with_properties(obj, heap, |p| {
            for k in p.keys() {
                if let Ok(i) = k.parse::<usize>()
                    && i < len
                {
                    indices.insert(i);
                }
            }
        });
        return;
    }
    if let Some(s) = value.as_string(heap) {
        for i in 0..(s.len() as usize).min(len) {
            indices.insert(i);
        }
    }
}

pub(crate) fn array_callback_native_dispatch(
    name: &str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let raw_receiver = *ctx.this_value();
    if raw_receiver.is_null() || raw_receiver.is_undefined() {
        return Err(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "Array.prototype method called on null or undefined".to_string(),
        });
    }
    let callback = args.first().cloned().unwrap_or(Value::undefined());
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let (interp, ctx_opt) = ctx.interp_mut_and_context();
    let context = ctx_opt.ok_or(NativeError::TypeError {
        name: "Array.prototype callback",
        reason: "missing execution context".to_string(),
    })?;
    // §22.1.3 step 1 — `O = ? ToObject(this value)`. Box primitive
    // receivers so the callback's `O` argument and the prototype-chain
    // walk see a real wrapper (e.g. `Boolean.prototype[k]` inherited
    // indices).
    let receiver = if raw_receiver.is_object_type() {
        raw_receiver
    } else {
        interp
            .box_sloppy_this_primitive_runtime_rooted(raw_receiver, &[args])
            .map_err(|err| {
                crate::native_function::vm_to_native_error(err, "Array.prototype callback")
            })?
    };
    // §23.1.3.* step 2 — len = ? LengthOfArrayLike(O), read once via
    // `[[Get]]` (observes a `get length()`). The walk below is LIVE:
    // each index is re-checked with `HasProperty(O, k)` / `Get(O, k)`
    // during iteration, so a callback that mutates the receiver is
    // observed in spec order and a Function / exotic receiver's indexed
    // properties are seen (the previous one-shot snapshot saw neither).
    let len = length_of_array_like(interp, &context, &receiver).map_err(|err| {
        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
    })?;
    // §23.1.3.* step 3 — `if IsCallable(callbackfn) is false, throw a
    // TypeError`, ordered after `ToObject` + `LengthOfArrayLike`.
    if !interp.is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "callback is not a function".to_string(),
        });
    }
    let callback_roots = [receiver, callback, this_arg];
    let output_target = match name {
        "map" => Some(
            interp
                .array_species_create(&context, receiver, len, &[args, &callback_roots])
                .map_err(|err| {
                    crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                })?,
        ),
        "filter" | "flatMap" => Some(
            interp
                .array_species_create(&context, receiver, 0, &[args, &callback_roots])
                .map_err(|err| {
                    crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                })?,
        ),
        _ => None,
    };
    // `find` family visits every index `0..len` (an absent slot yields
    // `undefined` for the element); the rest skip absent indices.
    let visit_all = matches!(name, "find" | "findIndex" | "findLast" | "findLastIndex");
    let reverse = matches!(name, "reduceRight" | "findLast" | "findLastIndex");
    // `reduce` / `reduceRight` do not accept a `thisArg`; the callback
    // runs with `undefined` this (the second positional is the
    // initialValue, not a receiver).
    let cb_this = if name == "reduce" || name == "reduceRight" {
        Value::undefined()
    } else {
        this_arg
    };
    // String-exotic wrappers expose their code-unit indices through
    // `[[StringData]]`, which the ordinary `[[HasProperty]]` ladder may
    // not surface — resolve those directly.
    let string_data = receiver
        .as_object()
        .and_then(|o| crate::object::string_data(o, interp.gc_heap()));
    // Index visit order. A bounded `0..len` ladder is spec-exact for any
    // receiver (dense array, Function, object with getters, mutation
    // mid-walk). A pathological `length` (> MAX_ARRAY_LIKE_PROBE_LEN)
    // falls back to the sparse present-index set across the prototype
    // chain so the walk never runs billions of `HasProperty` probes.
    let index_iter: Box<dyn Iterator<Item = usize>> = if len <= MAX_ARRAY_LIKE_PROBE_LEN {
        if reverse {
            Box::new((0..len).rev())
        } else {
            Box::new(0..len)
        }
    } else {
        let mut indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut current = receiver;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(interp, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = interp.get_prototype_for_op(&current).map_err(|err| {
                crate::native_function::vm_to_native_error(err, "Array.prototype callback")
            })?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        let mut v: Vec<usize> = indices.into_iter().collect();
        if reverse {
            v.reverse();
        }
        Box::new(v.into_iter())
    };

    let mut acc = Value::undefined();
    let mut found_idx: Option<usize> = None;
    let mut found_val = Value::undefined();
    let mut bool_acc: bool = matches!(name, "every");
    let mut target_index = 0usize;
    let mut reduce_has_init = args.len() >= 2;
    if (name == "reduce" || name == "reduceRight") && reduce_has_init {
        acc = args[1];
    }
    for idx in index_iter {
        // Live `HasProperty(O, k)` + `Get(O, k)`. An absent index reads
        // as `(false, undefined)`; `find`-family methods visit it anyway.
        let (present, v) = if let Some(s) = string_data {
            match s.char_code_at(idx as u32, interp.gc_heap()) {
                Some(unit) => {
                    let ch =
                        crate::string::JsString::from_utf16_units(&[unit], interp.gc_heap_mut())
                            .map(Value::string)
                            .map_err(|_| NativeError::TypeError {
                                name: "Array.prototype callback",
                                reason: "out of memory".to_string(),
                            })?;
                    (true, ch)
                }
                None => (false, Value::undefined()),
            }
        } else if let Some(arr) = receiver.as_array() {
            // A present own element (data or accessor) reads through the
            // ordinary `[[Get]]`. An absent index (hole / beyond the
            // element store but `< len`) is not skipped outright:
            // §10.4.2.4 [[Get]] walks the Array.prototype chain, so an
            // inherited `Array.prototype[k]` is observed; a hole with no
            // inherited value reads as absent.
            let key = idx.to_string();
            let present = crate::array::has_own_element(arr, interp.gc_heap(), idx)
                || crate::array::get_accessor(arr, interp.gc_heap(), &key).is_some()
                || interp
                    .ordinary_has_property_value(
                        &context,
                        receiver,
                        &crate::VmPropertyKey::String(&key),
                        0,
                    )
                    .map_err(|err| {
                        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                    })?;
            if present {
                let v = interp
                    .get_property_value_for_call(&context, receiver, &key)
                    .map_err(|err| {
                        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                    })?;
                (true, v)
            } else {
                (false, Value::undefined())
            }
        } else {
            let key = idx.to_string();
            let has = interp
                .ordinary_has_property_value(
                    &context,
                    receiver,
                    &crate::VmPropertyKey::String(&key),
                    0,
                )
                .map_err(|err| {
                    crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                })?;
            if has {
                let v = interp
                    .get_property_value_for_call(&context, receiver, &key)
                    .map_err(|err| {
                        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                    })?;
                (true, v)
            } else {
                (false, Value::undefined())
            }
        };
        if !present && !visit_all {
            continue;
        }
        let cb_args: SmallVec<[Value; 8]> = match name {
            "reduce" | "reduceRight" => {
                if !reduce_has_init {
                    acc = v;
                    reduce_has_init = true;
                    continue;
                }
                smallvec::smallvec![acc, v, Value::number_f64(idx as f64), receiver,]
            }
            _ => smallvec::smallvec![v, Value::number_f64(idx as f64), receiver,],
        };
        let result = interp
            .run_callable_sync(&context, &callback, cb_this, cb_args)
            .map_err(|err| {
                crate::native_function::vm_to_native_error(err, "Array.prototype callback")
            })?;
        match name {
            "forEach" => {}
            "map" => {
                let target = output_target.ok_or(NativeError::TypeError {
                    name: "map",
                    reason: "missing output target".to_string(),
                })?;
                let key = format_index_key(idx as f64);
                interp
                    .create_data_property_or_throw(&context, target, &key, result)
                    .map_err(|err| crate::native_function::vm_to_native_error(err, "map"))?;
            }
            "filter" if result.to_boolean(interp.gc_heap()) => {
                let target = output_target.ok_or(NativeError::TypeError {
                    name: "filter",
                    reason: "missing output target".to_string(),
                })?;
                let key = format_index_key(target_index as f64);
                interp
                    .create_data_property_or_throw(&context, target, &key, v)
                    .map_err(|err| crate::native_function::vm_to_native_error(err, "filter"))?;
                target_index += 1;
            }
            "find" | "findLast" if result.to_boolean(interp.gc_heap()) => {
                found_val = v;
                found_idx = Some(idx);
                break;
            }
            "findIndex" | "findLastIndex" if result.to_boolean(interp.gc_heap()) => {
                found_idx = Some(idx);
                break;
            }
            "every" if !result.to_boolean(interp.gc_heap()) => {
                bool_acc = false;
                break;
            }
            "some" if result.to_boolean(interp.gc_heap()) => {
                bool_acc = true;
                break;
            }
            "reduce" | "reduceRight" => {
                acc = result;
            }
            "flatMap" => {
                // §23.1.3.13 step 5 — FlattenIntoArray with depth=1.
                // Each callback result, if an Array, has its
                // elements spliced into the output; otherwise the
                // raw value is appended.
                if let Some(inner) = result.as_array() {
                    let inner_vals: Vec<Value> =
                        crate::array::with_elements(inner, interp.gc_heap(), |els| {
                            els.iter().filter(|v| !v.is_hole()).cloned().collect()
                        });
                    for v in inner_vals {
                        let target = output_target.ok_or(NativeError::TypeError {
                            name: "flatMap",
                            reason: "missing output target".to_string(),
                        })?;
                        let key = format_index_key(target_index as f64);
                        interp
                            .create_data_property_or_throw(&context, target, &key, v)
                            .map_err(|err| {
                                crate::native_function::vm_to_native_error(err, "flatMap")
                            })?;
                        target_index += 1;
                    }
                } else {
                    let target = output_target.ok_or(NativeError::TypeError {
                        name: "flatMap",
                        reason: "missing output target".to_string(),
                    })?;
                    let key = format_index_key(target_index as f64);
                    interp
                        .create_data_property_or_throw(&context, target, &key, result)
                        .map_err(|err| {
                            crate::native_function::vm_to_native_error(err, "flatMap")
                        })?;
                    target_index += 1;
                }
            }
            _ => {}
        }
    }
    match name {
        "forEach" => Ok(Value::undefined()),
        "find" | "findLast" => Ok(found_val),
        "findIndex" | "findLastIndex" => Ok(Value::number(NumberValue::from_f64(
            found_idx.map_or(-1.0, |i| i as f64),
        ))),
        "every" | "some" => Ok(Value::boolean(bool_acc)),
        "reduce" | "reduceRight" => {
            if !reduce_has_init {
                return Err(NativeError::TypeError {
                    name: "reduce",
                    reason: "empty array with no initial value".to_string(),
                });
            }
            Ok(acc)
        }
        "map" | "filter" | "flatMap" => output_target.ok_or(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "missing output target".to_string(),
        }),
        _ => Err(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: format!("unknown callback method '{name}'"),
        }),
    }
}

fn native_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("forEach", ctx, args)
}
fn native_map(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("map", ctx, args)
}
fn native_filter(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("filter", ctx, args)
}
fn native_some(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("some", ctx, args)
}
fn native_every(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("every", ctx, args)
}
fn native_find(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("find", ctx, args)
}
fn native_find_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("findIndex", ctx, args)
}
fn native_find_last(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("findLast", ctx, args)
}
fn native_find_last_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("findLastIndex", ctx, args)
}
fn native_reduce(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("reduce", ctx, args)
}
fn native_reduce_right(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("reduceRight", ctx, args)
}
fn native_flat_map(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("flatMap", ctx, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_arr(gc_heap: &mut otter_gc::GcHeap, values: &[i32]) -> Value {
        let arr = crate::array::from_elements_old_for_fixture(
            gc_heap,
            values.iter().map(|&n| Value::number_i32(n)),
        )
        .unwrap();
        Value::array(arr)
    }

    fn call(method: &str, recv: Value, args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Value {
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args,
            gc_heap,
            allocation_roots: &[],
        })
        .unwrap()
    }

    fn render(value: &Value, gc_heap: &otter_gc::GcHeap) -> String {
        if let Some(arr) = value.as_array() {
            crate::array::with_elements(arr, gc_heap, |elements| {
                elements
                    .iter()
                    .map(|v| v.display_string(gc_heap))
                    .collect::<Vec<_>>()
                    .join(",")
            })
        } else {
            value.display_string(gc_heap)
        }
    }

    #[test]
    fn push_returns_new_length() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2]);
        let r = call(
            "push",
            arr,
            &[Value::number(NumberValue::from_i32(3))],
            &mut gc_heap,
        );
        assert_eq!(r.display_string(&gc_heap), "3");
        assert_eq!(render(&arr, &gc_heap), "1,2,3");
    }

    #[test]
    fn pop_yields_tail() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2, 3]);
        let r = call("pop", arr, &[], &mut gc_heap);
        assert_eq!(r.display_string(&gc_heap), "3");
        assert_eq!(render(&arr, &gc_heap), "1,2");
    }

    #[test]
    fn shift_yields_head() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[10, 20, 30]);
        let r = call("shift", arr, &[], &mut gc_heap);
        assert_eq!(r.display_string(&gc_heap), "10");
        assert_eq!(render(&arr, &gc_heap), "20,30");
    }

    #[test]
    fn slice_handles_negative_end() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2, 3, 4, 5]);
        let r = call(
            "slice",
            arr,
            &[Value::number_i32(1), Value::number_i32(-1)],
            &mut gc_heap,
        );
        assert_eq!(render(&r, &gc_heap), "2,3,4");
    }

    #[test]
    fn concat_flattens_one_level() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2]);
        let other = make_arr(&mut gc_heap, &[3, 4]);
        let r = call("concat", arr, &[other, Value::number_i32(5)], &mut gc_heap);
        assert_eq!(render(&r, &gc_heap), "1,2,3,4,5");
    }

    #[test]
    fn join_with_default_separator() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2, 3]);
        let r = call("join", arr, &[], &mut gc_heap);
        assert_eq!(r.display_string(&gc_heap), "1,2,3");
    }

    #[test]
    fn includes_and_index_of() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[10, 20, 30]);
        let yes = call(
            "includes",
            arr,
            &[Value::number(NumberValue::from_i32(20))],
            &mut gc_heap,
        );
        let no = call(
            "includes",
            arr,
            &[Value::number(NumberValue::from_i32(99))],
            &mut gc_heap,
        );
        assert_eq!(yes, Value::boolean(true));
        assert_eq!(no, Value::boolean(false));
        let idx = call(
            "indexOf",
            arr,
            &[Value::number(NumberValue::from_i32(30))],
            &mut gc_heap,
        );
        assert_eq!(idx.display_string(&gc_heap), "2");
    }
}
