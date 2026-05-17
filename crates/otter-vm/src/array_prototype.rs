//! `Array.prototype.*` non-callback intrinsic implementations.
//!
//! This module hosts methods that do **not** invoke a JS callback:
//! `push`, `pop`, `shift`, `unshift`, `slice`, `concat`, `join`,
//! `includes`, `indexOf`, `lastIndexOf`, `at`, `reverse`, `fill`,
//! `flat`, `splice`, `sort` (default lexicographic). The callback-
//! driven family (`forEach`, `map`, `filter`, `reduce`, `find`,
//! `findIndex`, `every`, `some`, `flatMap`, `sort` with comparator)
//! is dispatched by the interpreter in `do_call_method_value` so
//! the callbacks run on the active VM stack via `run_callable_sync`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>
//!
//! # Contents
//! - [`ARRAY_PROTOTYPE_TABLE`] — declarative registry built with
//!   the `intrinsics!` macro.
//! - One private `impl_*` function per method.
//!
//! # Invariants
//! - Receivers must be `Value::Array`; non-arrays raise
//!   `IntrinsicError::BadReceiver`.
//! - Spec-mandated argument coercion (e.g., `slice` clamping
//!   negatives) follows the foundation subset; rare edge cases
//!   are documented inline.

use smallvec::SmallVec;

use crate::Value;
use crate::array::{self, JsArray};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::js_surface::{Attr, MethodSpec};
use crate::number::NumberValue;
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError};

fn receiver_array(args: &IntrinsicArgs<'_>) -> Result<JsArray, IntrinsicError> {
    match args.receiver {
        Value::Array(a) => Ok(*a),
        _ => Err(IntrinsicError::BadReceiver { expected: "array" }),
    }
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

/// §7.3.18 LengthOfArrayLike — read `O.length`, ToLength-coerce,
/// clamp to [`MAX_ARRAY_LIKE_PROBE_LEN`].
fn read_array_like_length(obj: crate::object::JsObject, heap: &otter_gc::GcHeap) -> usize {
    let len_val = crate::object::get(obj, heap, "length").unwrap_or(Value::Undefined);
    match len_val {
        Value::Number(n) => {
            let f = n.as_f64();
            if f.is_nan() || f <= 0.0 {
                0
            } else if f >= MAX_ARRAY_LIKE_PROBE_LEN as f64 {
                MAX_ARRAY_LIKE_PROBE_LEN
            } else {
                f as usize
            }
        }
        _ => 0,
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
    heap: &otter_gc::GcHeap,
) -> Option<Vec<(usize, Value)>> {
    match receiver {
        Value::Array(arr) => {
            // Dense Array path — Value::Hole encodes "absent"; the
            // sparse-aware filter drops it so the index/value pairs
            // match what HasProperty would observe.
            Some(array::with_elements(*arr, heap, |els| {
                els.iter()
                    .enumerate()
                    .filter_map(|(i, v)| match v {
                        Value::Hole => None,
                        other => Some((i, other.clone())),
                    })
                    .collect()
            }))
        }
        Value::Object(obj) => {
            let len = read_array_like_length(*obj, heap);
            if len == 0 {
                return Some(Vec::new());
            }
            let mut idx_keys: Vec<usize> = crate::object::with_properties(*obj, heap, |p| {
                p.keys()
                    .filter_map(|k| k.parse::<usize>().ok())
                    .filter(|&i| i < len)
                    .collect()
            });
            idx_keys.sort_unstable();
            idx_keys.dedup();
            Some(
                idx_keys
                    .into_iter()
                    .map(|i| {
                        let key = i.to_string();
                        let v = crate::object::get(*obj, heap, &key).unwrap_or(Value::Undefined);
                        (i, v)
                    })
                    .collect(),
            )
        }
        // §7.1.18 ToObject — primitive receivers coerce to their
        // wrapper. The wrapper carries no own indexed properties
        // for Boolean / Number / Symbol / BigInt (length undefined
        // → 0), so the callable methods see an empty walk and short-
        // circuit with the spec result for an empty array-like.
        Value::Boolean(_) | Value::Number(_) | Value::Symbol(_) | Value::BigInt(_) => {
            Some(Vec::new())
        }
        // Callable receivers (Function / Closure / NativeFunction /
        // BoundFunction / ClassConstructor) carry a `length` and no
        // observable own indexed properties unless the user
        // installs them. Generic Array.prototype callbacks treat
        // them as empty array-likes with `length = funcLength`,
        // matching ToObject + LengthOfArrayLike. The callback
        // observes `undefined` slots if `length > 0`.
        Value::Function { .. }
        | Value::Closure { .. }
        | Value::NativeFunction(_)
        | Value::BoundFunction(_)
        | Value::ClassConstructor(_) => Some(Vec::new()),
        // String primitive: walks code-unit slots up to `length`.
        // Each unit materialises as a single-unit `JsString`.
        Value::String(s) => {
            let units = s.to_utf16_vec();
            Some(
                units
                    .into_iter()
                    .enumerate()
                    .map(|(i, u)| {
                        let s = crate::string::JsString::from_utf16_units(
                            &[u],
                            // Reuse a thread-local empty heap is not
                            // available without context; fall back to
                            // constructing a fresh `StringHeap`. The
                            // callback path materialises these on the
                            // fly so the leak surface is bounded by
                            // the receiver's length.
                            &crate::string::StringHeap::default(),
                        )
                        .map(Value::String)
                        .unwrap_or(Value::Undefined);
                        (i, s)
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// §7.3.18 reachable length helper for receivers whose `.length`
/// we trust to be observable but where we only need the upper bound
/// for `fromIndex` clamping — does not allocate, just reads.
pub(crate) fn array_like_length(receiver: &Value, heap: &otter_gc::GcHeap) -> usize {
    match receiver {
        Value::Array(arr) => array::len(*arr, heap),
        Value::Object(obj) => read_array_like_length(*obj, heap),
        Value::String(s) => s.len() as usize,
        _ => 0,
    }
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
    match args.args.get(index as usize) {
        None | Some(Value::Undefined) => Ok(default),
        Some(Value::Number(n)) => match n.as_smi() {
            Some(v) => Ok(v as i64),
            None => {
                let f = n.as_f64();
                if f.is_nan() {
                    Ok(0)
                } else if f.is_infinite() {
                    Ok(if f.is_sign_negative() {
                        i64::MIN
                    } else {
                        i64::MAX
                    })
                } else {
                    Ok(f.trunc() as i64)
                }
            }
        },
        Some(Value::Boolean(true)) => Ok(1),
        Some(Value::Boolean(false)) | Some(Value::Null) => Ok(0),
        Some(Value::String(s)) => {
            let text = s.to_lossy_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(0)
            } else {
                Ok(trimmed.parse::<i64>().unwrap_or(0))
            }
        }
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a number",
        }),
    }
}

fn impl_push(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.20 — `len = ? LengthOfArrayLike(O)`; for each item set
    // `O[len + idx]` then update `O.length`. Iterates only the
    // argument list (no `0..len` scan), so safe for any `len`.
    if let Value::Array(arr) = args.receiver {
        let mut new_len = array::len(*arr, &*args.gc_heap);
        let values: Vec<Value> = args.args.to_vec();
        for v in values {
            new_len = args.array_push_rooted(*arr, v)?;
        }
        return Ok(Value::Number(NumberValue::from_i32(new_len as i32)));
    }
    if let Value::Object(obj) = args.receiver {
        let heap = &mut *args.gc_heap;
        let base_len = read_array_like_length(*obj, heap);
        let added = args.args.len();
        // §22.1.3.20 step 5.b — RangeError when the resulting length
        // would exceed 2^53 - 1. We surface the inner heap cap via
        // `read_array_like_length`'s ToLength clamp; the explicit
        // check here guards the final write to `length`.
        let new_len = base_len.saturating_add(added);
        for (i, v) in args.args.iter().enumerate() {
            let key = (base_len + i).to_string();
            crate::object::set(*obj, heap, &key, v.clone());
        }
        crate::object::set(
            *obj,
            heap,
            "length",
            Value::Number(NumberValue::from_f64(new_len as f64)),
        );
        return Ok(Value::Number(NumberValue::from_f64(new_len as f64)));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_pop(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.19 — read length, peel the last present element, write
    // length back. For Array we keep the existing dense fast path
    // (`array::pop`).
    if let Value::Array(arr) = args.receiver {
        let heap = &mut *args.gc_heap;
        return Ok(array::pop(*arr, heap));
    }
    if let Value::Object(obj) = args.receiver {
        let heap = &mut *args.gc_heap;
        let len = read_array_like_length(*obj, heap);
        if len == 0 {
            crate::object::set(
                *obj,
                heap,
                "length",
                Value::Number(NumberValue::from_i32(0)),
            );
            return Ok(Value::Undefined);
        }
        let last_idx = len - 1;
        let key = last_idx.to_string();
        let element = crate::object::get(*obj, heap, &key).unwrap_or(Value::Undefined);
        let _ = crate::object::delete(*obj, heap, &key);
        crate::object::set(
            *obj,
            heap,
            "length",
            Value::Number(NumberValue::from_f64(last_idx as f64)),
        );
        return Ok(element);
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_shift(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.26 — read element at 0, shift indices 1..len down by 1
    // (skipping absent slots), drop the last, decrement length. For
    // Array we keep the existing dense `Vec::remove(0)` path.
    if let Value::Array(arr) = args.receiver {
        let heap = &mut *args.gc_heap;
        return Ok(array::with_elements_mut(*arr, heap, |elements| {
            if elements.is_empty() {
                Value::Undefined
            } else {
                // §23.1.3.26: a leading hole shifts to `undefined`.
                match elements.remove(0) {
                    Value::Hole => Value::Undefined,
                    other => other,
                }
            }
        }));
    }
    if let Value::Object(obj) = args.receiver {
        let heap_ref = &*args.gc_heap;
        let len = read_array_like_length(*obj, heap_ref);
        if len == 0 {
            let heap = &mut *args.gc_heap;
            crate::object::set(
                *obj,
                heap,
                "length",
                Value::Number(NumberValue::from_i32(0)),
            );
            return Ok(Value::Undefined);
        }
        // Walk pre-shift present own indices in ascending order. The
        // post-shift state is the same set with each index decremented
        // by 1 (and the element at index 0 returned). Indices that
        // were present pre-shift but whose decremented form clashes
        // with nothing post-shift have their original key deleted to
        // match HasProperty results.
        let entries = array_like_present_entries(args.receiver, heap_ref)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        let heap = &mut *args.gc_heap;
        let mut first: Option<Value> = None;
        let mut post_present: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        for (i, v) in &entries {
            if *i == 0 {
                first = Some(v.clone());
                continue;
            }
            if *i < len {
                let new_idx = *i - 1;
                let new_key = new_idx.to_string();
                crate::object::set(*obj, heap, &new_key, v.clone());
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
                let _ = crate::object::delete(*obj, heap, &i.to_string());
            }
        }
        // Always remove the trailing slot — even if it wasn't present,
        // delete is idempotent.
        let _ = crate::object::delete(*obj, heap, &(len - 1).to_string());
        crate::object::set(
            *obj,
            heap,
            "length",
            Value::Number(NumberValue::from_f64((len - 1) as f64)),
        );
        return Ok(first.unwrap_or(Value::Undefined));
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
    if let Value::Array(arr) = args.receiver {
        let heap = &mut *args.gc_heap;
        let existing_len = array::len(*arr, heap);
        let mut values: Vec<Value> = args.args.to_vec();
        array::with_elements(*arr, heap, |elements| {
            values.extend(elements.iter().cloned())
        });
        array::with_elements_mut(*arr, heap, |elements| {
            elements.clear();
            elements.extend(values);
        });
        return Ok(Value::Number(NumberValue::from_i32(
            (existing_len + arg_count) as i32,
        )));
    }
    if let Value::Object(obj) = args.receiver {
        let heap_ref = &*args.gc_heap;
        let existing_len = read_array_like_length(*obj, heap_ref);
        if arg_count == 0 {
            // Spec still requires writing `length` back (no-op if it
            // already equals `existing_len`).
            let heap = &mut *args.gc_heap;
            crate::object::set(
                *obj,
                heap,
                "length",
                Value::Number(NumberValue::from_f64(existing_len as f64)),
            );
            return Ok(Value::Number(NumberValue::from_f64(existing_len as f64)));
        }
        let entries = array_like_present_entries(args.receiver, heap_ref)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        let heap = &mut *args.gc_heap;
        // Walk descending so destination slot is never live yet.
        for (i, v) in entries.into_iter().rev() {
            let new_idx = i + arg_count;
            crate::object::set(*obj, heap, &new_idx.to_string(), v);
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
                let _ = crate::object::delete(*obj, heap, &i.to_string());
            }
        }
        // Prepend the new items at indices 0..arg_count.
        for (i, v) in args.args.iter().enumerate() {
            crate::object::set(*obj, heap, &i.to_string(), v.clone());
        }
        let new_len = existing_len + arg_count;
        crate::object::set(
            *obj,
            heap,
            "length",
            Value::Number(NumberValue::from_f64(new_len as f64)),
        );
        return Ok(Value::Number(NumberValue::from_f64(new_len as f64)));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

fn impl_slice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.28 — generic over array-likes via ToObject(this) +
    // LengthOfArrayLike. The dense `Value::Array` path stays on the
    // contiguous slice copy; non-array receivers walk present indexed
    // own keys and materialise undefined for absent positions inside
    // the requested range (matching `HasProperty` + `Get` semantics).
    let heap = &*args.gc_heap;
    if let Value::Array(arr) = args.receiver {
        let len = array::len(*arr, heap);
        let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
        let end_default = len as i64;
        let end_raw = arg_signed_index(args, 1, end_default)?;
        let end = clamp_index(end_raw, len);
        let slice: Vec<Value> = array::with_elements(*arr, heap, |elements| {
            if start >= end {
                Vec::new()
            } else {
                elements[start..end].to_vec()
            }
        });
        return Ok(Value::Array(args.array_from_elements_rooted(
            slice.iter().cloned(),
            &[],
            &[slice.as_slice()],
        )?));
    }
    let len = array_like_length(args.receiver, heap);
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    let end_default = len as i64;
    let end_raw = arg_signed_index(args, 1, end_default)?;
    let end = clamp_index(end_raw, len);
    let entries = array_like_present_entries(args.receiver, heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let slice_len = end.saturating_sub(start);
    let mut out = vec![Value::Undefined; slice_len];
    for (i, v) in entries {
        if i < start || i >= end {
            continue;
        }
        out[i - start] = v;
    }
    Ok(Value::Array(args.array_from_elements_rooted(
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
    let heap = &*args.gc_heap;
    let mut combined: Vec<Value> = match args.receiver {
        Value::Array(arr) => array::with_elements(*arr, heap, |elements| elements.to_vec()),
        Value::Object(_) => {
            let len = array_like_length(args.receiver, heap);
            let entries = array_like_present_entries(args.receiver, heap)
                .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
            let mut out = vec![Value::Undefined; len];
            for (i, v) in entries {
                if i < len {
                    out[i] = v;
                }
            }
            out
        }
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    for v in args.args {
        match v {
            Value::Array(other) => {
                array::with_elements(*other, heap, |elements| {
                    combined.extend(elements.iter().cloned());
                });
            }
            other => combined.push(other.clone()),
        }
    }
    Ok(Value::Array(args.array_from_elements_rooted(
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
    let heap = &*args.gc_heap;
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
    };
    if let Value::Array(arr) = args.receiver {
        let parts: Vec<String> = array::with_elements(*arr, heap, |elements| {
            elements
                .iter()
                .map(|v| match v {
                    Value::Undefined | Value::Null | Value::Hole => String::new(),
                    other => other.display_string(),
                })
                .collect()
        });
        let joined = parts.join(&separator);
        return Ok(Value::String(JsString::from_str(
            &joined,
            args.string_heap,
        )?));
    }
    if let Value::Object(obj) = args.receiver {
        let len = read_array_like_length(*obj, heap);
        if len == 0 {
            return Ok(Value::String(JsString::from_str("", args.string_heap)?));
        }
        // Materialise present indices into a sparse lookup; absent
        // slots produce empty-string parts so the final `join` keeps
        // separator placement correct. We bound the `parts` length by
        // `len` (already clamped to `MAX_ARRAY_LIKE_PROBE_LEN`); no
        // unbounded probe.
        let entries = array_like_present_entries(args.receiver, heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let mut parts: Vec<String> = vec![String::new(); len];
        for (i, v) in entries {
            if i >= len {
                continue;
            }
            parts[i] = match v {
                Value::Undefined | Value::Null | Value::Hole => String::new(),
                other => other.display_string(),
            };
        }
        let joined = parts.join(&separator);
        return Ok(Value::String(JsString::from_str(
            &joined,
            args.string_heap,
        )?));
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
    let heap = &*args.gc_heap;
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    let needle_is_undefined = matches!(needle, Value::Undefined);
    if let Value::Array(arr) = args.receiver {
        let found = array::with_elements(*arr, heap, |elements| {
            elements.iter().any(|v| match v {
                Value::Hole => needle_is_undefined,
                other => crate::abstract_ops::same_value_zero(other, &needle),
            })
        });
        return Ok(Value::Boolean(found));
    }
    let entries = array_like_present_entries(args.receiver, heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    let found = entries
        .iter()
        .any(|(_, v)| crate::abstract_ops::same_value_zero(v, &needle));
    Ok(Value::Boolean(found))
}

fn impl_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.14 — generic over array-likes. The dense `Value::Array`
    // path keeps the existing tight `with_elements` walk so common
    // dense-array calls don't pay a snapshot allocation.
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    let from_raw = arg_signed_index(args, 1, 0)?;
    let heap = &*args.gc_heap;
    if let Value::Array(arr) = args.receiver {
        let len = array::len(*arr, heap);
        let from = clamp_index(from_raw, len);
        let found = array::with_elements(*arr, heap, |elements| {
            elements
                .iter()
                .enumerate()
                .skip(from)
                .find_map(|(i, v)| if v == &needle { Some(i) } else { None })
        });
        if let Some(i) = found {
            return Ok(Value::Number(NumberValue::from_i32(i as i32)));
        }
        return Ok(Value::Number(NumberValue::from_i32(-1)));
    }
    let len = array_like_length(args.receiver, heap);
    let from = clamp_index(from_raw, len);
    let entries = array_like_present_entries(args.receiver, heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    for (i, v) in entries {
        if i < from {
            continue;
        }
        if v == needle {
            return Ok(Value::Number(NumberValue::from_i32(i as i32)));
        }
    }
    Ok(Value::Number(NumberValue::from_i32(-1)))
}

/// §23.1.3.1 `Array.prototype.at(index)` — clamp negative indexing.
/// <https://tc39.es/ecma262/#sec-array.prototype.at>
fn impl_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §23.1.3.1 — generic via ToObject(this) + LengthOfArrayLike,
    // then a single indexed Get. Constant-time regardless of `len`.
    let heap = &*args.gc_heap;
    if let Value::Array(arr) = args.receiver {
        let len = array::len(*arr, heap) as i64;
        let raw = arg_signed_index(args, 0, 0)?;
        let idx = if raw < 0 { len + raw } else { raw };
        if idx < 0 || idx >= len {
            return Ok(Value::Undefined);
        }
        return Ok(array::get(*arr, heap, idx as usize));
    }
    if let Value::Object(obj) = args.receiver {
        let len = read_array_like_length(*obj, heap) as i64;
        let raw = arg_signed_index(args, 0, 0)?;
        let idx = if raw < 0 { len + raw } else { raw };
        if idx < 0 || idx >= len {
            return Ok(Value::Undefined);
        }
        let key = (idx as usize).to_string();
        return Ok(crate::object::get(*obj, heap, &key).unwrap_or(Value::Undefined));
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.18 `Array.prototype.lastIndexOf(value, fromIndex?)`.
/// Generic over array-likes; dense `Value::Array` keeps the existing
/// tight reverse walk to avoid a snapshot allocation on hot paths.
/// <https://tc39.es/ecma262/#sec-array.prototype.lastindexof>
fn impl_last_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let heap = &*args.gc_heap;
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    if let Value::Array(arr) = args.receiver {
        let len = array::len(*arr, heap);
        let from_default = (len as i64).saturating_sub(1);
        let from_raw = arg_signed_index(args, 1, from_default)?;
        let from = if from_raw < 0 {
            let v = (len as i64) + from_raw;
            if v < 0 {
                return Ok(Value::Number(NumberValue::from_i32(-1)));
            }
            v as usize
        } else if (from_raw as usize) >= len {
            len.saturating_sub(1)
        } else {
            from_raw as usize
        };
        let found = array::with_elements(*arr, heap, |elements| {
            if elements.is_empty() {
                return None;
            }
            let mut i = from as i64;
            while i >= 0 {
                if elements[i as usize] == needle {
                    return Some(i as i32);
                }
                i -= 1;
            }
            None
        });
        if let Some(i) = found {
            return Ok(Value::Number(NumberValue::from_i32(i)));
        }
        return Ok(Value::Number(NumberValue::from_i32(-1)));
    }
    let len = array_like_length(args.receiver, heap);
    let from_default = (len as i64).saturating_sub(1);
    let from_raw = arg_signed_index(args, 1, from_default)?;
    let from = if from_raw < 0 {
        let v = (len as i64) + from_raw;
        if v < 0 {
            return Ok(Value::Number(NumberValue::from_i32(-1)));
        }
        v as usize
    } else if (from_raw as usize) >= len {
        len.saturating_sub(1)
    } else {
        from_raw as usize
    };
    let entries = array_like_present_entries(args.receiver, heap)
        .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
    // Reverse walk over the sorted entries; first hit with `i <= from`
    // wins. Entries are ascending so we iterate in reverse.
    for (i, v) in entries.into_iter().rev() {
        if i > from {
            continue;
        }
        if v == needle {
            return Ok(Value::Number(NumberValue::from_i32(i as i32)));
        }
    }
    Ok(Value::Number(NumberValue::from_i32(-1)))
}

/// §23.1.3.27 `Array.prototype.reverse()` — in-place.
/// Generic over array-likes; sparse Object receivers swap only the
/// pairs `(i, len-1-i)` where at least one side is present (matching
/// the spec's `HasProperty` short-circuit).
/// <https://tc39.es/ecma262/#sec-array.prototype.reverse>
fn impl_reverse(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    if let Value::Array(arr) = args.receiver {
        let heap = &mut *args.gc_heap;
        array::with_elements_mut(*arr, heap, |elements| elements.reverse());
        return Ok(Value::Array(*arr));
    }
    if let Value::Object(obj) = args.receiver {
        let heap_ref = &*args.gc_heap;
        let len = read_array_like_length(*obj, heap_ref);
        if len < 2 {
            return Ok(args.receiver.clone());
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
            let v_i = crate::object::get(*obj, heap, &key_i).unwrap_or(Value::Undefined);
            let mirror_present = pre_present.contains(&mirror);
            if mirror_present {
                let v_m = crate::object::get(*obj, heap, &key_m).unwrap_or(Value::Undefined);
                crate::object::set(*obj, heap, &key_i, v_m);
                crate::object::set(*obj, heap, &key_m, v_i);
            } else {
                // Mirror absent — migrate i → mirror, delete i.
                crate::object::set(*obj, heap, &key_m, v_i);
                let _ = crate::object::delete(*obj, heap, &key_i);
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
            let v_i = crate::object::get(*obj, heap, &key_i).unwrap_or(Value::Undefined);
            crate::object::set(*obj, heap, &key_m, v_i);
            let _ = crate::object::delete(*obj, heap, &key_i);
        }
        return Ok(args.receiver.clone());
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.7 `Array.prototype.fill(value, start?, end?)` — in-place.
/// Generic over array-likes via ToObject(this) + LengthOfArrayLike.
/// <https://tc39.es/ecma262/#sec-array.prototype.fill>
fn impl_fill(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let value = args.args.first().cloned().unwrap_or(Value::Undefined);
    if let Value::Array(arr) = args.receiver {
        let len = array::len(*arr, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 1, 0)?, len);
        let end = clamp_index(arg_signed_index(args, 2, len as i64)?, len);
        if start < end {
            let heap = &mut *args.gc_heap;
            array::with_elements_mut(*arr, heap, |elements| {
                for slot in elements.iter_mut().take(end).skip(start) {
                    *slot = value.clone();
                }
            });
        }
        return Ok(Value::Array(*arr));
    }
    if let Value::Object(obj) = args.receiver {
        let len = read_array_like_length(*obj, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 1, 0)?, len);
        let end = clamp_index(arg_signed_index(args, 2, len as i64)?, len);
        // Cap defensively — `MAX_ARRAY_LIKE_PROBE_LEN` is already
        // applied to `len` via `read_array_like_length`, so the
        // bounded `start..end` walk is safe.
        let heap = &mut *args.gc_heap;
        for k in start..end {
            crate::object::set(*obj, heap, &k.to_string(), value.clone());
        }
        return Ok(args.receiver.clone());
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.11 `Array.prototype.flat(depth?)` — flattens at most
/// `depth` levels (default 1). Sparse holes are dropped — foundation
/// arrays are dense, so the spec's `IsConcatSpreadable` short-circuit
/// reduces to "is `Value::Array`".
/// <https://tc39.es/ecma262/#sec-array.prototype.flat>
fn impl_flat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let heap = &*args.gc_heap;
    let depth = match args.args.first() {
        None | Some(Value::Undefined) => 1i64,
        Some(Value::Number(n)) => match n.as_smi() {
            Some(v) if v >= 0 => v as i64,
            Some(_) => 0,
            None => n.as_f64() as i64,
        },
        _ => 1,
    };
    fn walk(out: &mut Vec<Value>, heap: &otter_gc::GcHeap, body: &[Value], depth: i64) {
        for v in body {
            match v {
                Value::Hole => {}
                Value::Array(a) if depth > 0 => {
                    array::with_elements(*a, heap, |inner| walk(out, heap, inner, depth - 1));
                }
                other => out.push(other.clone()),
            }
        }
    }
    let elements: Vec<Value> = match args.receiver {
        Value::Array(arr) => array::with_elements(*arr, heap, |els| els.to_vec()),
        Value::Object(obj) => {
            let len = read_array_like_length(*obj, heap);
            (0..len)
                .map(|i| crate::object::get(*obj, heap, &i.to_string()).unwrap_or(Value::Undefined))
                .collect()
        }
        _ => Vec::new(),
    };
    let mut out: Vec<Value> = Vec::with_capacity(elements.len());
    walk(&mut out, heap, &elements, depth);
    Ok(Value::Array(args.array_from_elements_rooted(
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
    if let Value::Array(arr) = args.receiver {
        let len = array::len(*arr, &*args.gc_heap);
        let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
        let delete_count = match args.args.get(1) {
            None | Some(Value::Undefined) => len.saturating_sub(start),
            Some(Value::Number(n)) => {
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
            }
            _ => 0,
        };
        let inserts: Vec<Value> = args.args.iter().skip(2).cloned().collect();
        let heap = &mut *args.gc_heap;
        let removed = array::with_elements_mut(*arr, heap, |elements| {
            let mut removed: Vec<Value> = Vec::with_capacity(delete_count);
            for _ in 0..delete_count {
                removed.push(elements.remove(start));
            }
            for (i, v) in inserts.into_iter().enumerate() {
                elements.insert(start + i, v);
            }
            removed
        });
        return Ok(Value::Array(args.array_from_elements_rooted(
            removed.iter().cloned(),
            &[],
            &[removed.as_slice()],
        )?));
    }
    if let Value::Object(obj) = args.receiver {
        let heap_ref = &*args.gc_heap;
        let len = read_array_like_length(*obj, heap_ref);
        let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
        let delete_count = match args.args.get(1) {
            None | Some(Value::Undefined) => len.saturating_sub(start),
            Some(Value::Number(n)) => {
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
            }
            _ => 0,
        };
        let item_count = args.args.len().saturating_sub(2);
        let inserts: Vec<Value> = args.args.iter().skip(2).cloned().collect();
        // Pre-shift present indices.
        let entries = array_like_present_entries(args.receiver, heap_ref)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let pre_present: std::collections::BTreeSet<usize> =
            entries.iter().map(|(i, _)| *i).collect();
        // Snapshot the deleted region so we can return it.
        let mut removed: Vec<Value> = vec![Value::Undefined; delete_count];
        for (i, v) in &entries {
            if *i >= start && *i < start + delete_count {
                removed[*i - start] = v.clone();
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
                crate::object::set(*obj, heap, &new_idx.to_string(), v.clone());
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
                    let _ = crate::object::delete(*obj, heap, &i.to_string());
                }
            }
        } else if item_count > delete_count {
            // Grow — walk present indices in [start+delete_count, len)
            // descending so writes don't clobber yet-to-relocate values.
            let shift = item_count - delete_count;
            let tail: Vec<(usize, Value)> = entries
                .iter()
                .filter(|(i, _)| *i >= start + delete_count && *i < len)
                .map(|(i, v)| (*i, v.clone()))
                .collect();
            for (i, v) in tail.iter().rev() {
                let new_idx = i + shift;
                crate::object::set(*obj, heap, &new_idx.to_string(), v.clone());
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
                    let _ = crate::object::delete(*obj, heap, &i.to_string());
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
            crate::object::set(*obj, heap, &(start + k).to_string(), v);
        }
        // Update length.
        let new_len = len - delete_count + item_count;
        crate::object::set(
            *obj,
            heap,
            "length",
            Value::Number(NumberValue::from_f64(new_len as f64)),
        );
        return Ok(Value::Array(args.array_from_elements_rooted(
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
    if let Some(Value::Undefined) | None = args.args.first() {
        let heap = &mut *args.gc_heap;
        // §23.1.3.30.2 SortCompare (no comparator) — undefined values
        // sort to the end; remaining values compare by their
        // ToString result.
        array::with_elements_mut(arr, heap, |elements| {
            elements.sort_by(|a, b| {
                let a_undef = matches!(a, Value::Undefined);
                let b_undef = matches!(b, Value::Undefined);
                match (a_undef, b_undef) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    (false, false) => a.display_string().cmp(&b.display_string()),
                }
            })
        });
        Ok(Value::Array(arr))
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
    let heap_ref = &*args.gc_heap;
    let len = match args.receiver {
        Value::Array(arr) => array::len(*arr, heap_ref),
        Value::Object(_) => array_like_length(args.receiver, heap_ref),
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let to_raw = arg_signed_index(args, 0, 0)?;
    let from_raw = arg_signed_index(args, 1, 0)?;
    let end_raw = arg_signed_index(args, 2, len as i64)?;
    let to = clamp_index(to_raw, len);
    let from = clamp_index(from_raw, len);
    let end = clamp_index(end_raw, len);
    let count = end.saturating_sub(from).min(len.saturating_sub(to));
    if count == 0 {
        return Ok(args.receiver.clone());
    }
    if let Value::Array(arr) = args.receiver {
        let heap = &mut *args.gc_heap;
        array::with_elements_mut(*arr, heap, |elements| {
            // Snapshot source range — std::vec::Vec doesn't have
            // `copy_within` for non-Copy types, so a transient
            // buffer is the cleanest correct path.
            let src: Vec<Value> = elements[from..from + count].to_vec();
            for (i, v) in src.into_iter().enumerate() {
                elements[to + i] = v;
            }
        });
        return Ok(Value::Array(*arr));
    }
    if let Value::Object(obj) = args.receiver {
        // Snapshot the source range using only present indices so
        // pathological-sparse receivers don't trigger an
        // `O(count)` HasProperty scan; afterwards write to `to..`,
        // deleting positions whose source was absent.
        let entries = array_like_present_entries(args.receiver, heap_ref)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        let mut src: Vec<Option<Value>> = vec![None; count];
        for (i, v) in &entries {
            if *i >= from && *i < from + count {
                src[*i - from] = Some(v.clone());
            }
        }
        let heap = &mut *args.gc_heap;
        for (i, slot) in src.into_iter().enumerate() {
            let key = (to + i).to_string();
            match slot {
                Some(v) => crate::object::set(*obj, heap, &key, v),
                None => {
                    let _ = crate::object::delete(*obj, heap, &key);
                }
            }
        }
        return Ok(args.receiver.clone());
    }
    Err(IntrinsicError::BadReceiver { expected: "array" })
}

/// §23.1.3.40 `Array.prototype.toSpliced(start, skipCount?, ...items)`
/// — non-mutating splice. Returns a fresh dense Array with the spec
/// `[len - skipCount + itemCount]` shape.
/// <https://tc39.es/ecma262/#sec-array.prototype.tospliced>
fn impl_to_spliced(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let heap = &*args.gc_heap;
    let len = match args.receiver {
        Value::Array(arr) => array::len(*arr, heap),
        Value::Object(_) => array_like_length(args.receiver, heap),
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    let skip_count = match args.args.get(1) {
        None | Some(Value::Undefined) => len.saturating_sub(start),
        Some(Value::Number(n)) => {
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
        }
        _ => 0,
    };
    let item_count = args.args.len().saturating_sub(2);
    let new_len = len - skip_count + item_count;
    let mut out: Vec<Value> = vec![Value::Undefined; new_len];
    // Materialise present source values into `src[0..len]`.
    let mut src: Vec<Value> = vec![Value::Undefined; len];
    match args.receiver {
        Value::Array(arr) => {
            array::with_elements(*arr, heap, |elements| {
                for (i, slot) in src.iter_mut().enumerate() {
                    if let Some(v) = elements.get(i) {
                        *slot = match v {
                            Value::Hole => Value::Undefined,
                            other => other.clone(),
                        };
                    }
                }
            });
        }
        Value::Object(_) => {
            let entries = array_like_present_entries(args.receiver, heap)
                .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
            for (i, v) in entries {
                if i < len {
                    src[i] = v;
                }
            }
        }
        _ => unreachable!(),
    }
    // Write the head [0, start).
    for k in 0..start {
        out[k] = src[k].clone();
    }
    // Write the inserts at [start, start+item_count).
    for (k, v) in args.args.iter().skip(2).enumerate() {
        out[start + k] = v.clone();
    }
    // Write the tail [start+skip_count, len) shifted to
    // [start+item_count, new_len).
    let mut dst = start + item_count;
    let mut srcidx = start + skip_count;
    while srcidx < len {
        out[dst] = src[srcidx].clone();
        dst += 1;
        srcidx += 1;
    }
    Ok(Value::Array(args.array_from_elements_rooted(
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
    let Value::Array(arr) = args.receiver else {
        return Err(IntrinsicError::BadReceiver { expected: "array" });
    };
    let key_value = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    // Try indexed first.
    let key_string: String = match &key_value {
        Value::String(s) => s.to_lossy_string(),
        Value::Number(n) => n.to_display_string(),
        Value::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Null => "null".to_string(),
        Value::Undefined => "undefined".to_string(),
        _ => return Ok(Value::Boolean(false)),
    };
    if let Ok(idx) = key_string.parse::<usize>() {
        return Ok(Value::Boolean(array::has_own_element(*arr, heap, idx)));
    }
    if key_string == "length" {
        return Ok(Value::Boolean(true));
    }
    let has_named = heap.read_payload(*arr, |body| {
        body.named_properties
            .as_ref()
            .is_some_and(|m| m.contains_key(&key_string))
    });
    Ok(Value::Boolean(has_named))
}

/// §20.1.3.3 — `Array.prototype.isPrototypeOf(V)`. Foundation: arrays
/// are not in a real prototype chain target; always returns false.
fn impl_is_prototype_of(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::Boolean(false))
}

/// §20.1.3.4 — `Array.prototype.propertyIsEnumerable(V)`. Indexed
/// slots + named props are enumerable; `length` is not.
fn impl_property_is_enumerable(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let Value::Array(arr) = args.receiver else {
        return Err(IntrinsicError::BadReceiver { expected: "array" });
    };
    let key_value = args.args.first().cloned().unwrap_or(Value::Undefined);
    let heap = &*args.gc_heap;
    let key_string: String = match &key_value {
        Value::String(s) => s.to_lossy_string(),
        Value::Number(n) => n.to_display_string(),
        _ => return Ok(Value::Boolean(false)),
    };
    if key_string == "length" {
        return Ok(Value::Boolean(false));
    }
    if let Ok(idx) = key_string.parse::<usize>() {
        return Ok(Value::Boolean(array::has_own_element(*arr, heap, idx)));
    }
    let has_named = heap.read_payload(*arr, |body| {
        body.named_properties
            .as_ref()
            .is_some_and(|m| m.contains_key(&key_string))
    });
    Ok(Value::Boolean(has_named))
}

/// §23.1.3.{18,35,8} — `Array.prototype.keys()` / `.values()` /
/// `.entries()`. Each constructs an `ArrayIterator` backed by the
/// receiver: `keys()` yields the numeric indices, `values()` yields
/// each element, `entries()` yields fresh `[index, value]` arrays.
/// The result is a `Value::Iterator` driven by `Op::IteratorNext`.
/// <https://tc39.es/ecma262/#sec-array.prototype.keys>
fn impl_keys_iter(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = match args.receiver {
        Value::Array(arr) => *arr,
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let handle = args.alloc_iterator_state_rooted(
        crate::IteratorState::ArrayKey {
            array: arr,
            index: 0,
        },
        &[],
        &[],
    )?;
    Ok(Value::Iterator(handle))
}

fn impl_values_iter(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = match args.receiver {
        Value::Array(arr) => *arr,
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let handle = args.alloc_iterator_state_rooted(
        crate::IteratorState::Array {
            array: arr,
            index: 0,
        },
        &[],
        &[],
    )?;
    Ok(Value::Iterator(handle))
}

fn impl_entries_iter(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = match args.receiver {
        Value::Array(arr) => *arr,
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let handle = args.alloc_iterator_state_rooted(
        crate::IteratorState::ArrayEntry {
            array: arr,
            index: 0,
        },
        &[],
        &[],
    )?;
    Ok(Value::Iterator(handle))
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
        && !matches!(first, Value::Undefined)
        && !matches!(
            first,
            Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
        )
    {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "comparator must be a function",
        });
    }
    let heap = &*args.gc_heap;
    let len = match args.receiver {
        Value::Array(arr) => array::len(*arr, heap),
        Value::Object(_) => array_like_length(args.receiver, heap),
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let mut out: Vec<Value> = vec![Value::Undefined; len];
    match args.receiver {
        Value::Array(arr) => {
            array::with_elements(*arr, heap, |elements| {
                for (i, slot) in out.iter_mut().enumerate() {
                    if let Some(v) = elements.get(i) {
                        *slot = match v {
                            Value::Hole => Value::Undefined,
                            other => other.clone(),
                        };
                    }
                }
            });
        }
        Value::Object(_) => {
            let entries = array_like_present_entries(args.receiver, heap)
                .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
            for (i, v) in entries {
                if i < len {
                    out[i] = v;
                }
            }
        }
        _ => unreachable!(),
    }
    out.sort_by(|a, b| {
        let a_undef = matches!(a, Value::Undefined);
        let b_undef = matches!(b, Value::Undefined);
        match (a_undef, b_undef) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.display_string().cmp(&b.display_string()),
        }
    });
    Ok(Value::Array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §23.1.3.39 `Array.prototype.toReversed()` — non-mutating reverse.
/// Returns a fresh dense Array.
/// <https://tc39.es/ecma262/#sec-array.prototype.toreversed>
fn impl_to_reversed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let heap = &*args.gc_heap;
    let len = match args.receiver {
        Value::Array(arr) => array::len(*arr, heap),
        Value::Object(_) => array_like_length(args.receiver, heap),
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let mut out: Vec<Value> = vec![Value::Undefined; len];
    if let Value::Array(arr) = args.receiver {
        array::with_elements(*arr, heap, |elements| {
            for (i, slot) in out.iter_mut().enumerate() {
                if let Some(v) = elements.get(len - 1 - i) {
                    *slot = match v {
                        Value::Hole => Value::Undefined,
                        other => other.clone(),
                    };
                }
            }
        });
    } else if let Value::Object(_) = args.receiver {
        let entries = array_like_present_entries(args.receiver, heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        for (i, v) in entries {
            if i >= len {
                continue;
            }
            out[len - 1 - i] = v;
        }
    }
    Ok(Value::Array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §23.1.3.42 `Array.prototype.with(index, value)` — non-mutating
/// element replacement at `index`. Returns a fresh dense Array.
/// <https://tc39.es/ecma262/#sec-array.prototype.with>
fn impl_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let heap = &*args.gc_heap;
    let len = match args.receiver {
        Value::Array(arr) => array::len(*arr, heap),
        Value::Object(_) => array_like_length(args.receiver, heap),
        _ => return Err(IntrinsicError::BadReceiver { expected: "array" }),
    };
    let raw = arg_signed_index(args, 0, 0)?;
    let actual = if raw < 0 { raw + len as i64 } else { raw };
    if actual < 0 || (actual as usize) >= len {
        return Err(IntrinsicError::OutOfRange {
            index: 0,
            reason: "index out of bounds for Array.prototype.with",
        });
    }
    let replacement = args.args.get(1).cloned().unwrap_or(Value::Undefined);
    let actual = actual as usize;
    let mut out: Vec<Value> = vec![Value::Undefined; len];
    if let Value::Array(arr) = args.receiver {
        array::with_elements(*arr, heap, |elements| {
            for (i, slot) in out.iter_mut().enumerate() {
                if i == actual {
                    *slot = replacement.clone();
                } else if let Some(v) = elements.get(i) {
                    *slot = match v {
                        Value::Hole => Value::Undefined,
                        other => other.clone(),
                    };
                }
            }
        });
    } else if let Value::Object(_) = args.receiver {
        let entries = array_like_present_entries(args.receiver, heap)
            .ok_or(IntrinsicError::BadReceiver { expected: "array" })?;
        for (i, v) in entries {
            if i >= len {
                continue;
            }
            out[i] = v;
        }
        out[actual] = replacement;
    }
    Ok(Value::Array(args.array_from_elements_rooted(
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
            "isPrototypeOf"       / 1 => impl_is_prototype_of,
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
    let receiver = ctx.this_value().clone();
    // Pre-coerce integer-typed args via `ToPrimitive(Number)` so the
    // intrinsic's strict `arg_signed_index` guard observes user
    // `@@toPrimitive` / `valueOf` / `toString` side effects per spec.
    // Mirrors the matching arm in `Op::CallMethodValue`; this path
    // handles `Array.prototype.<x>.call(...)` / `.apply(...)` /
    // `.bind(...)` invocations that bypass the receiver fast path.
    let int_coerce_indices: &[usize] = match name {
        "indexOf" | "lastIndexOf" | "includes" => &[1],
        "fill" => &[1, 2],
        "copyWithin" => &[0, 1, 2],
        "slice" => &[0, 1],
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
                if !matches!(
                    slot,
                    Value::Object(_)
                        | Value::Array(_)
                        | Value::Function { .. }
                        | Value::Closure { .. }
                        | Value::NativeFunction(_)
                        | Value::BoundFunction(_)
                        | Value::ClassConstructor(_)
                        | Value::Proxy(_)
                        | Value::RegExp(_)
                ) {
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
    let (string_heap, allocation_roots) = {
        let interp = ctx.interp_mut();
        (interp.string_heap_clone(), interp.collect_runtime_roots())
    };
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown Array.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&mut IntrinsicArgs {
        receiver: &receiver,
        args: &coerced_args,
        string_heap: &string_heap,
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
fn array_callback_native_dispatch(
    name: &str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = ctx.this_value().clone();
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let (interp, ctx_opt) = ctx.interp_mut_and_context();
    let context = ctx_opt.ok_or(NativeError::TypeError {
        name: "Array.prototype callback",
        reason: "missing execution context".to_string(),
    })?;
    let entries = {
        let heap = interp.gc_heap();
        array_like_present_entries(&receiver, heap).ok_or(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "receiver is not array-like".to_string(),
        })?
    };
    let len = array_like_length(&receiver, interp.gc_heap());
    let mut acc = Value::Undefined;
    let mut out: Vec<(usize, Value)> = Vec::new();
    let mut found_idx: Option<usize> = None;
    let mut found_val = Value::Undefined;
    let mut bool_acc: bool = match name {
        "every" => true,
        "some" => false,
        _ => false,
    };
    let mut reduce_has_init = args.len() >= 2;
    if name == "reduce" || name == "reduceRight" {
        acc = if reduce_has_init {
            args[1].clone()
        } else {
            Value::Undefined
        };
    }
    let iter: Box<dyn Iterator<Item = (usize, Value)>> =
        if name == "reduceRight" || name == "findLast" || name == "findLastIndex" {
            Box::new(entries.into_iter().rev())
        } else {
            Box::new(entries.into_iter())
        };
    for (idx, v) in iter {
        let cb_args: SmallVec<[Value; 8]> = match name {
            "reduce" | "reduceRight" => {
                if !reduce_has_init {
                    acc = v.clone();
                    reduce_has_init = true;
                    continue;
                }
                smallvec::smallvec![
                    acc.clone(),
                    v.clone(),
                    Value::Number(NumberValue::from_f64(idx as f64)),
                    receiver.clone(),
                ]
            }
            _ => smallvec::smallvec![
                v.clone(),
                Value::Number(NumberValue::from_f64(idx as f64)),
                receiver.clone(),
            ],
        };
        let result = interp
            .run_callable_sync(&context, &callback, this_arg.clone(), cb_args)
            .map_err(|err| NativeError::TypeError {
                name: "Array.prototype callback",
                reason: err.to_string(),
            })?;
        match name {
            "forEach" => {}
            "map" => out.push((idx, result)),
            "filter" => {
                if result.to_boolean() {
                    out.push((idx, v));
                }
            }
            "find" | "findLast" => {
                if result.to_boolean() {
                    found_val = v.clone();
                    found_idx = Some(idx);
                    break;
                }
            }
            "findIndex" | "findLastIndex" => {
                if result.to_boolean() {
                    found_idx = Some(idx);
                    break;
                }
            }
            "every" => {
                if !result.to_boolean() {
                    bool_acc = false;
                    break;
                }
            }
            "some" => {
                if result.to_boolean() {
                    bool_acc = true;
                    break;
                }
            }
            "reduce" | "reduceRight" => {
                acc = result;
            }
            "flatMap" => {
                // §23.1.3.13 step 5 — FlattenIntoArray with depth=1.
                // Each callback result, if an Array, has its
                // elements spliced into the output; otherwise the
                // raw value is appended.
                match result {
                    Value::Array(inner) => {
                        let inner_vals: Vec<Value> =
                            crate::array::with_elements(inner, interp.gc_heap(), |els| {
                                els.iter()
                                    .filter(|v| !matches!(v, Value::Hole))
                                    .cloned()
                                    .collect()
                            });
                        for v in inner_vals {
                            out.push((out.len(), v));
                        }
                    }
                    other => out.push((out.len(), other)),
                }
            }
            _ => {}
        }
    }
    match name {
        "forEach" => Ok(Value::Undefined),
        "find" | "findLast" => Ok(found_val),
        "findIndex" | "findLastIndex" => Ok(Value::Number(NumberValue::from_f64(
            found_idx.map_or(-1.0, |i| i as f64),
        ))),
        "every" | "some" => Ok(Value::Boolean(bool_acc)),
        "reduce" | "reduceRight" => {
            if !reduce_has_init {
                return Err(NativeError::TypeError {
                    name: "reduce",
                    reason: "empty array with no initial value".to_string(),
                });
            }
            Ok(acc)
        }
        "map" => {
            // Materialise into a dense array of length `len`. Absent
            // positions stay `undefined`.
            let mut buf: Vec<Value> = vec![Value::Undefined; len];
            for (i, v) in out {
                if i < len {
                    buf[i] = v;
                }
            }
            let (interp, _) = ctx.interp_mut_and_context();
            let heap = interp.gc_heap_mut();
            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for value in &buf {
                    value.trace_value_slots(visit);
                }
            };
            let arr = crate::array::alloc_array_with_roots(heap, &mut visitor).map_err(|_| {
                NativeError::TypeError {
                    name: "map",
                    reason: "array allocation failed".to_string(),
                }
            })?;
            crate::array::with_elements_mut(arr, heap, |elements| {
                elements.extend(buf);
            });
            Ok(Value::Array(arr))
        }
        "filter" | "flatMap" => {
            let buf: Vec<Value> = out.into_iter().map(|(_, v)| v).collect();
            let (interp, _) = ctx.interp_mut_and_context();
            let heap = interp.gc_heap_mut();
            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                for value in &buf {
                    value.trace_value_slots(visit);
                }
            };
            let arr = crate::array::alloc_array_with_roots(heap, &mut visitor).map_err(|_| {
                NativeError::TypeError {
                    name: "filter",
                    reason: "array allocation failed".to_string(),
                }
            })?;
            crate::array::with_elements_mut(arr, heap, |elements| {
                elements.extend(buf);
            });
            Ok(Value::Array(arr))
        }
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
    use crate::string::StringHeap;

    fn make_arr(gc_heap: &mut otter_gc::GcHeap, values: &[i32]) -> Value {
        let arr = crate::array::from_elements_old_for_fixture(
            gc_heap,
            values
                .iter()
                .map(|&n| Value::Number(NumberValue::from_i32(n))),
        )
        .unwrap();
        Value::Array(arr)
    }

    fn call(method: &str, recv: Value, args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Value {
        let heap = StringHeap::default();
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args,
            string_heap: &heap,
            gc_heap,
            allocation_roots: &[],
        })
        .unwrap()
    }

    fn render(value: &Value, gc_heap: &otter_gc::GcHeap) -> String {
        match value {
            Value::Array(arr) => crate::array::with_elements(*arr, gc_heap, |elements| {
                elements
                    .iter()
                    .map(Value::display_string)
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            other => other.display_string(),
        }
    }

    #[test]
    fn push_returns_new_length() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2]);
        let r = call(
            "push",
            arr.clone(),
            &[Value::Number(NumberValue::from_i32(3))],
            &mut gc_heap,
        );
        assert_eq!(r.display_string(), "3");
        assert_eq!(render(&arr, &gc_heap), "1,2,3");
    }

    #[test]
    fn pop_yields_tail() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2, 3]);
        let r = call("pop", arr.clone(), &[], &mut gc_heap);
        assert_eq!(r.display_string(), "3");
        assert_eq!(render(&arr, &gc_heap), "1,2");
    }

    #[test]
    fn shift_yields_head() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[10, 20, 30]);
        let r = call("shift", arr.clone(), &[], &mut gc_heap);
        assert_eq!(r.display_string(), "10");
        assert_eq!(render(&arr, &gc_heap), "20,30");
    }

    #[test]
    fn slice_handles_negative_end() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2, 3, 4, 5]);
        let r = call(
            "slice",
            arr,
            &[
                Value::Number(NumberValue::from_i32(1)),
                Value::Number(NumberValue::from_i32(-1)),
            ],
            &mut gc_heap,
        );
        assert_eq!(render(&r, &gc_heap), "2,3,4");
    }

    #[test]
    fn concat_flattens_one_level() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2]);
        let other = make_arr(&mut gc_heap, &[3, 4]);
        let r = call(
            "concat",
            arr,
            &[other, Value::Number(NumberValue::from_i32(5))],
            &mut gc_heap,
        );
        assert_eq!(render(&r, &gc_heap), "1,2,3,4,5");
    }

    #[test]
    fn join_with_default_separator() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[1, 2, 3]);
        let r = call("join", arr, &[], &mut gc_heap);
        assert_eq!(r.display_string(), "1,2,3");
    }

    #[test]
    fn includes_and_index_of() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let arr = make_arr(&mut gc_heap, &[10, 20, 30]);
        let yes = call(
            "includes",
            arr.clone(),
            &[Value::Number(NumberValue::from_i32(20))],
            &mut gc_heap,
        );
        let no = call(
            "includes",
            arr.clone(),
            &[Value::Number(NumberValue::from_i32(99))],
            &mut gc_heap,
        );
        assert_eq!(yes, Value::Boolean(true));
        assert_eq!(no, Value::Boolean(false));
        let idx = call(
            "indexOf",
            arr,
            &[Value::Number(NumberValue::from_i32(30))],
            &mut gc_heap,
        );
        assert_eq!(idx.display_string(), "2");
    }
}
