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
fn array_like_present_entries(
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
        _ => None,
    }
}

/// §7.3.18 reachable length helper for receivers whose `.length`
/// we trust to be observable but where we only need the upper bound
/// for `fromIndex` clamping — does not allocate, just reads.
fn array_like_length(receiver: &Value, heap: &otter_gc::GcHeap) -> usize {
    match receiver {
        Value::Array(arr) => array::len(*arr, heap),
        Value::Object(obj) => read_array_like_length(*obj, heap),
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
    match args.args.get(index as usize) {
        None => Ok(default),
        Some(Value::Number(n)) => match n.as_smi() {
            Some(v) => Ok(v as i64),
            None => Ok(n.as_f64() as i64),
        },
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a number",
        }),
    }
}

fn impl_push(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut new_len = array::len(arr, &*args.gc_heap);
    let values: Vec<Value> = args.args.to_vec();
    for v in values {
        new_len = args.array_push_rooted(arr, v)?;
    }
    Ok(Value::Number(NumberValue::from_i32(new_len as i32)))
}

fn impl_pop(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = &mut *args.gc_heap;
    Ok(array::pop(arr, heap))
}

fn impl_shift(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = &mut *args.gc_heap;
    Ok(array::with_elements_mut(arr, heap, |elements| {
        if elements.is_empty() {
            Value::Undefined
        } else {
            // §23.1.3.26: a leading hole shifts to `undefined`.
            match elements.remove(0) {
                Value::Hole => Value::Undefined,
                other => other,
            }
        }
    }))
}

fn impl_unshift(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
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
    Ok(Value::Number(NumberValue::from_i32(
        (existing_len + args.args.len()) as i32,
    )))
}

fn impl_slice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
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
    Ok(Value::Array(args.array_from_elements_rooted(
        slice.iter().cloned(),
        &[],
        &[slice.as_slice()],
    )?))
}

fn impl_concat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = &*args.gc_heap;
    // Spec: result starts as a copy of the receiver; for each
    // argument, if it's an array, append its elements; otherwise
    // append the value itself.
    let mut combined: Vec<Value> = array::with_elements(arr, heap, |elements| elements.to_vec());
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
    let arr = receiver_array(args)?;
    let heap = &*args.gc_heap;
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
    };
    let parts: Vec<String> = array::with_elements(arr, heap, |elements| {
        elements
            .iter()
            .map(|v| match v {
                // §23.1.3.16: holes serialize the same as `undefined`
                // / `null` — i.e. an empty string between separators.
                Value::Undefined | Value::Null | Value::Hole => String::new(),
                other => other.display_string(),
            })
            .collect()
    });
    let joined = parts.join(&separator);
    Ok(Value::String(JsString::from_str(
        &joined,
        args.string_heap,
    )?))
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
    let arr = receiver_array(args)?;
    let len = array::len(arr, &*args.gc_heap) as i64;
    let raw = arg_signed_index(args, 0, 0)?;
    let idx = if raw < 0 { len + raw } else { raw };
    if idx < 0 || idx >= len {
        return Ok(Value::Undefined);
    }
    let heap = &*args.gc_heap;
    Ok(array::get(arr, heap, idx as usize))
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
/// <https://tc39.es/ecma262/#sec-array.prototype.reverse>
fn impl_reverse(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = &mut *args.gc_heap;
    array::with_elements_mut(arr, heap, |elements| elements.reverse());
    Ok(Value::Array(arr))
}

/// §23.1.3.7 `Array.prototype.fill(value, start?, end?)` — in-place.
/// <https://tc39.es/ecma262/#sec-array.prototype.fill>
fn impl_fill(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let len = array::len(arr, &*args.gc_heap);
    let value = args.args.first().cloned().unwrap_or(Value::Undefined);
    let start = clamp_index(arg_signed_index(args, 1, 0)?, len);
    let end = clamp_index(arg_signed_index(args, 2, len as i64)?, len);
    if start < end {
        let heap = &mut *args.gc_heap;
        array::with_elements_mut(arr, heap, |elements| {
            for slot in elements.iter_mut().take(end).skip(start) {
                *slot = value.clone();
            }
        });
    }
    Ok(Value::Array(arr))
}

/// §23.1.3.11 `Array.prototype.flat(depth?)` — flattens at most
/// `depth` levels (default 1). Sparse holes are dropped — foundation
/// arrays are dense, so the spec's `IsConcatSpreadable` short-circuit
/// reduces to "is `Value::Array`".
/// <https://tc39.es/ecma262/#sec-array.prototype.flat>
fn impl_flat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
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
                // §23.1.3.12 step 4.b — `flat` skips array holes
                // (`HasProperty(O, P)` is `false`).
                Value::Hole => {}
                Value::Array(a) if depth > 0 => {
                    array::with_elements(*a, heap, |inner| walk(out, heap, inner, depth - 1));
                }
                other => out.push(other.clone()),
            }
        }
    }
    let mut out: Vec<Value> = Vec::with_capacity(array::len(arr, heap));
    array::with_elements(arr, heap, |elements| walk(&mut out, heap, elements, depth));
    Ok(Value::Array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §23.1.3.31 `Array.prototype.splice(start, deleteCount?, ...items)`.
/// Mutates the receiver in place; returns the removed elements.
/// <https://tc39.es/ecma262/#sec-array.prototype.splice>
fn impl_splice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let len = array::len(arr, &*args.gc_heap);
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    // §23.1.3.31 step 6 — when `deleteCount` is omitted (foundation
    // accepts `undefined`), splice removes through the tail.
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
    // `SmallVec` lacks a `splice` API — perform the equivalent by
    // hand: drain the removed slice, then insert the new items at
    // `start`.
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
    Ok(Value::Array(args.array_from_elements_rooted(
        removed.iter().cloned(),
        &[],
        &[removed.as_slice()],
    )?))
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
        args,
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
