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

fn impl_push(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let mut new_len = array::len(arr, &heap);
    for v in args.args {
        new_len = array::push(arr, &mut heap, v.clone())?;
    }
    Ok(Value::Number(NumberValue::from_i32(new_len as i32)))
}

fn impl_pop(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    Ok(array::pop(arr, &mut heap))
}

fn impl_shift(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    Ok(array::with_elements_mut(arr, &mut heap, |elements| {
        if elements.is_empty() {
            Value::Undefined
        } else {
            elements.remove(0)
        }
    }))
}

fn impl_unshift(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let existing_len = array::len(arr, &heap);
    let mut values: Vec<Value> = args.args.to_vec();
    array::with_elements(arr, &heap, |elements| {
        values.extend(elements.iter().cloned())
    });
    let replacement = array::from_elements(&mut heap, values)?;
    let copied = array::with_elements(replacement, &heap, |elements| elements.to_vec());
    array::with_elements_mut(arr, &mut heap, |elements| {
        elements.clear();
        elements.extend(copied);
    });
    Ok(Value::Number(NumberValue::from_i32(
        (existing_len + args.args.len()) as i32,
    )))
}

fn impl_slice(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let len = array::len(arr, &heap);
    let start = clamp_index(arg_signed_index(args, 0, 0)?, len);
    let end_default = len as i64;
    let end_raw = arg_signed_index(args, 1, end_default)?;
    let end = clamp_index(end_raw, len);
    let slice: Vec<Value> = array::with_elements(arr, &heap, |elements| {
        if start >= end {
            Vec::new()
        } else {
            elements[start..end].to_vec()
        }
    });
    Ok(Value::Array(array::from_elements(&mut heap, slice)?))
}

fn impl_concat(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    // Spec: result starts as a copy of the receiver; for each
    // argument, if it's an array, append its elements; otherwise
    // append the value itself.
    let mut combined: Vec<Value> = array::with_elements(arr, &heap, |elements| elements.to_vec());
    for v in args.args {
        match v {
            Value::Array(other) => {
                array::with_elements(*other, &heap, |elements| {
                    combined.extend(elements.iter().cloned());
                });
            }
            other => combined.push(other.clone()),
        }
    }
    Ok(Value::Array(array::from_elements(&mut heap, combined)?))
}

fn impl_join(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = args.gc_heap.borrow();
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
    };
    let parts: Vec<String> = array::with_elements(arr, &heap, |elements| {
        elements
            .iter()
            .map(|v| match v {
                Value::Undefined | Value::Null => String::new(),
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

fn impl_includes(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = args.gc_heap.borrow();
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    let found = array::with_elements(arr, &heap, |elements| elements.iter().any(|v| v == &needle));
    Ok(Value::Boolean(found))
}

fn impl_index_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
    let from_raw = arg_signed_index(args, 1, 0)?;
    let heap = args.gc_heap.borrow();
    let len = array::len(arr, &heap);
    let from = clamp_index(from_raw, len);
    let found = array::with_elements(arr, &heap, |elements| {
        elements
            .iter()
            .enumerate()
            .skip(from)
            .find_map(|(i, v)| if v == &needle { Some(i) } else { None })
    });
    if let Some(i) = found {
        return Ok(Value::Number(NumberValue::from_i32(i as i32)));
    }
    Ok(Value::Number(NumberValue::from_i32(-1)))
}

/// §23.1.3.1 `Array.prototype.at(index)` — clamp negative indexing.
/// <https://tc39.es/ecma262/#sec-array.prototype.at>
fn impl_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = args.gc_heap.borrow();
    let len = array::len(arr, &heap) as i64;
    let raw = arg_signed_index(args, 0, 0)?;
    let idx = if raw < 0 { len + raw } else { raw };
    if idx < 0 || idx >= len {
        return Ok(Value::Undefined);
    }
    Ok(array::get(arr, &heap, idx as usize))
}

/// §23.1.3.18 `Array.prototype.lastIndexOf(value, fromIndex?)`.
/// <https://tc39.es/ecma262/#sec-array.prototype.lastindexof>
fn impl_last_index_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let heap = args.gc_heap.borrow();
    let len = array::len(arr, &heap);
    let needle = args.args.first().cloned().unwrap_or(Value::Undefined);
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
    let found = array::with_elements(arr, &heap, |elements| {
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
    if len == 0 {
        return Ok(Value::Number(NumberValue::from_i32(-1)));
    }
    Ok(Value::Number(NumberValue::from_i32(-1)))
}

/// §23.1.3.27 `Array.prototype.reverse()` — in-place.
/// <https://tc39.es/ecma262/#sec-array.prototype.reverse>
fn impl_reverse(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    array::with_elements_mut(arr, &mut heap, |elements| elements.reverse());
    Ok(Value::Array(arr))
}

/// §23.1.3.7 `Array.prototype.fill(value, start?, end?)` — in-place.
/// <https://tc39.es/ecma262/#sec-array.prototype.fill>
fn impl_fill(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let len = array::len(arr, &heap);
    let value = args.args.first().cloned().unwrap_or(Value::Undefined);
    let start = clamp_index(arg_signed_index(args, 1, 0)?, len);
    let end = clamp_index(arg_signed_index(args, 2, len as i64)?, len);
    if start < end {
        array::with_elements_mut(arr, &mut heap, |elements| {
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
fn impl_flat(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
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
                Value::Array(a) if depth > 0 => {
                    array::with_elements(*a, heap, |inner| walk(out, heap, inner, depth - 1));
                }
                other => out.push(other.clone()),
            }
        }
    }
    let mut out: Vec<Value> = Vec::with_capacity(array::len(arr, &heap));
    array::with_elements(arr, &heap, |elements| {
        walk(&mut out, &heap, elements, depth)
    });
    Ok(Value::Array(array::from_elements(&mut heap, out)?))
}

/// §23.1.3.31 `Array.prototype.splice(start, deleteCount?, ...items)`.
/// Mutates the receiver in place; returns the removed elements.
/// <https://tc39.es/ecma262/#sec-array.prototype.splice>
fn impl_splice(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    let mut heap = args.gc_heap.borrow_mut();
    let len = array::len(arr, &heap);
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
    let removed = array::with_elements_mut(arr, &mut heap, |elements| {
        let mut removed: Vec<Value> = Vec::with_capacity(delete_count);
        for _ in 0..delete_count {
            removed.push(elements.remove(start));
        }
        for (i, v) in inserts.into_iter().enumerate() {
            elements.insert(start + i, v);
        }
        removed
    });
    Ok(Value::Array(array::from_elements(&mut heap, removed)?))
}

/// §23.1.3.30 `Array.prototype.sort()` — default lexicographic
/// comparator (calls `String(a)` / `String(b)` and compares as
/// UTF-16). Comparator-driven sort is interpreter-dispatched.
/// <https://tc39.es/ecma262/#sec-array.prototype.sort>
fn impl_sort_default(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let arr = receiver_array(args)?;
    if let Some(Value::Undefined) | None = args.args.first() {
        let mut heap = args.gc_heap.borrow_mut();
        // §23.1.3.30.2 SortCompare (no comparator) — undefined values
        // sort to the end; remaining values compare by their
        // ToString result.
        array::with_elements_mut(arr, &mut heap, |elements| {
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
    let string_heap = ctx.interp_mut().string_heap_clone();
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown Array.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&IntrinsicArgs {
        receiver: &receiver,
        args,
        string_heap: &string_heap,
        gc_heap: std::cell::RefCell::new(ctx.heap_mut()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    fn make_arr(gc_heap: &mut otter_gc::GcHeap, values: &[i32]) -> Value {
        let arr = crate::array::from_elements(
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
        (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args,
            string_heap: &heap,
            gc_heap: std::cell::RefCell::new(gc_heap),
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
