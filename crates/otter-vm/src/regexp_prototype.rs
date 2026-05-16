//! `RegExp.prototype.*` intrinsic implementations.
//!
//! Slice 31. Method dispatch goes through the
//! [`crate::intrinsics`] table; property reads (`.source`,
//! `.flags`, `.global`, `.lastIndex`, …) handled at the
//! `Op::LoadProperty` site since they don't go through
//! `CallMethodValue`.
//!
//! # Contents
//! - [`REGEXP_PROTOTYPE_TABLE`] — declarative registry built with
//!   the `intrinsics!` macro.
//! - One private `impl_*` function per method.
//! - [`load_property`] — getter dispatch for non-method members.
//!
//! # Invariants
//! - Receivers are validated as `Value::RegExp`; non-regex
//!   receivers raise [`crate::intrinsics::IntrinsicError::BadReceiver`].
//! - `exec` and `test` honour the `g` and `y` flag semantics — both
//!   read and update `lastIndex`.
//! - `lastIndex` is clamped to `[0, len]` before any match attempt
//!   so a manual `re.lastIndex = -1` doesn't underflow.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp.prototype.exec>

use crate::Value;
use crate::array::JsArray;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::regexp::JsRegExp;
use crate::runtime_cx::NativeCtx;
use crate::string::{JsString, StringHeap};

fn receiver_regexp<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsRegExp, IntrinsicError> {
    match args.receiver {
        Value::RegExp(r) => Ok(r),
        _ => Err(IntrinsicError::BadReceiver { expected: "regexp" }),
    }
}

fn arg_string<'a>(args: &'a IntrinsicArgs<'_>, index: u16) -> Result<&'a JsString, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        }),
        None => Err(IntrinsicError::BadArgument {
            index,
            reason: "is required",
        }),
    }
}

/// Run a single match attempt and return the resulting JS array
/// (`[fullMatch, ...captureGroups]`) or `Value::Null` for no match.
/// Honours the `g` / `y` flag state stored on the receiver.
///
/// Per §22.2.7.2 [`RegExpBuiltinExec`](https://tc39.es/ecma262/#sec-regexpbuiltinexec)
/// the result array also carries `index`, `input`, and `groups`
/// own properties — and, when the receiver has the `d` flag, an
/// `indices` companion array (§22.2.7.7
/// [`MakeMatchIndicesIndexPairArray`](https://tc39.es/ecma262/#sec-makematchindicesindexpairarray)).
pub(crate) fn exec_once(
    re: &JsRegExp,
    text: &JsString,
    args: &mut IntrinsicArgs<'_>,
) -> Result<Value, IntrinsicError> {
    let units = text.to_utf16_vec();
    let len = units.len();
    let flags = re.flags(args.gc_heap);
    let mut start = re.last_index(args.gc_heap) as usize;
    if (flags.global || flags.sticky) && start > len {
        re.set_last_index(args.gc_heap, 0);
        return Ok(Value::Null);
    }
    if !flags.global && !flags.sticky {
        start = 0;
    }
    let m = re
        .find_from_utf16(args.gc_heap, &units, start)
        .into_iter()
        .next();
    let m = match m {
        Some(m) => m,
        None => {
            if flags.global || flags.sticky {
                re.set_last_index(args.gc_heap, 0);
            }
            return Ok(Value::Null);
        }
    };
    if flags.sticky && m.range.start != start {
        re.set_last_index(args.gc_heap, 0);
        return Ok(Value::Null);
    }
    if flags.global || flags.sticky {
        re.set_last_index(args.gc_heap, m.range.end as u32);
    }

    Ok(Value::Array(build_match_result(
        &m,
        &units,
        text,
        flags.has_indices,
        args,
        &[],
        &[],
    )?))
}

pub(crate) fn exec_once_native(
    re: &JsRegExp,
    text: &JsString,
    string_heap: &StringHeap,
    ctx: &mut NativeCtx<'_>,
    slice_roots: &[&[Value]],
) -> Result<Value, IntrinsicError> {
    let units = text.to_utf16_vec();
    let len = units.len();
    let flags = re.flags(ctx.heap());
    let mut start = re.last_index(ctx.heap()) as usize;
    if (flags.global || flags.sticky) && start > len {
        re.set_last_index(ctx.heap_mut(), 0);
        return Ok(Value::Null);
    }
    if !flags.global && !flags.sticky {
        start = 0;
    }
    let m = re
        .find_from_utf16(ctx.heap(), &units, start)
        .into_iter()
        .next();
    let m = match m {
        Some(m) => m,
        None => {
            if flags.global || flags.sticky {
                re.set_last_index(ctx.heap_mut(), 0);
            }
            return Ok(Value::Null);
        }
    };
    if flags.sticky && m.range.start != start {
        re.set_last_index(ctx.heap_mut(), 0);
        return Ok(Value::Null);
    }
    if flags.global || flags.sticky {
        re.set_last_index(ctx.heap_mut(), m.range.end as u32);
    }

    Ok(Value::Array(build_match_result_native(
        &m,
        &units,
        text,
        flags.has_indices,
        string_heap,
        ctx,
        &[],
        slice_roots,
    )?))
}

/// §22.2.7.2 steps 26–32 — build the JS-visible match-result array
/// out of a `regress::Match`. Used by `RegExp.prototype.exec` and
/// reused by `String.prototype.match` / `.matchAll` so both surfaces
/// produce identical shapes (full match + capture slots, plus
/// `index` / `input` / `groups` / optionally `indices`).
pub(crate) fn build_match_result(
    m: &regress::Match,
    units: &[u16],
    input: &JsString,
    has_indices: bool,
    args: &mut IntrinsicArgs<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsArray, IntrinsicError> {
    let full = JsString::from_utf16_units(&units[m.range.clone()], args.string_heap)?;
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    out.push(Value::String(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&units[r.clone()], args.string_heap)?;
                out.push(Value::String(s));
            }
            None => out.push(Value::Undefined),
        }
    }
    let input_value = Value::String(input.clone());
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&input_value);
    roots.extend_from_slice(value_roots);
    let mut slices = Vec::with_capacity(slice_roots.len() + 1);
    slices.push(out.as_slice());
    slices.extend_from_slice(slice_roots);
    let arr = args.array_from_elements_rooted(out.iter().cloned(), &roots, &slices)?;

    crate::array::set_named_property(
        arr,
        args.gc_heap,
        "index",
        Value::Number(NumberValue::from_i32(m.range.start as i32)),
    )?;
    crate::array::set_named_property(arr, args.gc_heap, "input", input_value.clone())?;

    let mut named_iter = m.named_groups();
    let first_named = named_iter.next();
    if let Some((name, range)) = first_named {
        let arr_value = Value::Array(arr);
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.push(&input_value);
        roots.push(&arr_value);
        roots.extend_from_slice(value_roots);
        let groups_obj = args.alloc_object_rooted(&roots, &slices)?;
        crate::object::set_prototype(groups_obj, args.gc_heap, None);
        let value = match range {
            Some(r) => Value::String(JsString::from_utf16_units(&units[r], args.string_heap)?),
            None => Value::Undefined,
        };
        crate::object::set(groups_obj, args.gc_heap, name, value);
        for (name, range) in named_iter {
            let value = match range {
                Some(r) => Value::String(JsString::from_utf16_units(&units[r], args.string_heap)?),
                None => Value::Undefined,
            };
            crate::object::set(groups_obj, args.gc_heap, name, value);
        }
        crate::array::set_named_property(arr, args.gc_heap, "groups", Value::Object(groups_obj))?;
    } else {
        crate::array::set_named_property(arr, args.gc_heap, "groups", Value::Undefined)?;
    }

    if has_indices {
        let arr_value = Value::Array(arr);
        let mut indices_elems: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
        indices_elems.push(pair_array(
            m.range.start,
            m.range.end,
            args,
            &[&input_value, &arr_value],
            &[out.as_slice()],
        )?);
        for cap in &m.captures {
            match cap {
                Some(r) => indices_elems.push(pair_array(
                    r.start,
                    r.end,
                    args,
                    &[&input_value, &arr_value],
                    &[out.as_slice(), indices_elems.as_slice()],
                )?),
                None => indices_elems.push(Value::Undefined),
            }
        }
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.push(&input_value);
        roots.push(&arr_value);
        roots.extend_from_slice(value_roots);
        let mut index_slices = Vec::with_capacity(slice_roots.len() + 2);
        index_slices.push(out.as_slice());
        index_slices.push(indices_elems.as_slice());
        index_slices.extend_from_slice(slice_roots);
        let indices_arr =
            args.array_from_elements_rooted(indices_elems.iter().cloned(), &roots, &index_slices)?;
        let mut named_iter = m.named_groups();
        let first_named = named_iter.next();
        if let Some((name, range)) = first_named {
            let indices_value = Value::Array(indices_arr);
            let mut roots = Vec::with_capacity(value_roots.len() + 3);
            roots.push(&input_value);
            roots.push(&arr_value);
            roots.push(&indices_value);
            roots.extend_from_slice(value_roots);
            let g_obj = args.alloc_object_rooted(&roots, &index_slices)?;
            crate::object::set_prototype(g_obj, args.gc_heap, None);
            let v = match range {
                Some(r) => pair_array(
                    r.start,
                    r.end,
                    args,
                    &roots,
                    &[out.as_slice(), indices_elems.as_slice()],
                )?,
                None => Value::Undefined,
            };
            crate::object::set(g_obj, args.gc_heap, name, v);
            for (name, range) in named_iter {
                let v = match range {
                    Some(r) => pair_array(
                        r.start,
                        r.end,
                        args,
                        &roots,
                        &[out.as_slice(), indices_elems.as_slice()],
                    )?,
                    None => Value::Undefined,
                };
                crate::object::set(g_obj, args.gc_heap, name, v);
            }
            crate::array::set_named_property(
                indices_arr,
                args.gc_heap,
                "groups",
                Value::Object(g_obj),
            )?;
        } else {
            crate::array::set_named_property(
                indices_arr,
                args.gc_heap,
                "groups",
                Value::Undefined,
            )?;
        }
        crate::array::set_named_property(arr, args.gc_heap, "indices", Value::Array(indices_arr))?;
    }
    Ok(arr)
}

pub(crate) fn build_match_result_native(
    m: &regress::Match,
    units: &[u16],
    input: &JsString,
    has_indices: bool,
    string_heap: &StringHeap,
    ctx: &mut NativeCtx<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsArray, IntrinsicError> {
    let full = JsString::from_utf16_units(&units[m.range.clone()], string_heap)?;
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    out.push(Value::String(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&units[r.clone()], string_heap)?;
                out.push(Value::String(s));
            }
            None => out.push(Value::Undefined),
        }
    }
    let input_value = Value::String(input.clone());
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&input_value);
    roots.extend_from_slice(value_roots);
    let mut slices = Vec::with_capacity(slice_roots.len() + 1);
    slices.push(out.as_slice());
    slices.extend_from_slice(slice_roots);
    let arr = ctx.array_from_elements_with_roots(out.iter().cloned(), &roots, &slices)?;

    crate::array::set_named_property(
        arr,
        ctx.heap_mut(),
        "index",
        Value::Number(NumberValue::from_i32(m.range.start as i32)),
    )?;
    crate::array::set_named_property(arr, ctx.heap_mut(), "input", input_value.clone())?;

    let mut named_iter = m.named_groups();
    let first_named = named_iter.next();
    if let Some((name, range)) = first_named {
        let arr_value = Value::Array(arr);
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.push(&input_value);
        roots.push(&arr_value);
        roots.extend_from_slice(value_roots);
        let groups_obj = ctx.alloc_object_with_roots(&roots, &slices)?;
        crate::object::set_prototype(groups_obj, ctx.heap_mut(), None);
        let value = match range {
            Some(r) => Value::String(JsString::from_utf16_units(&units[r], string_heap)?),
            None => Value::Undefined,
        };
        crate::object::set(groups_obj, ctx.heap_mut(), name, value);
        for (name, range) in named_iter {
            let value = match range {
                Some(r) => Value::String(JsString::from_utf16_units(&units[r], string_heap)?),
                None => Value::Undefined,
            };
            crate::object::set(groups_obj, ctx.heap_mut(), name, value);
        }
        crate::array::set_named_property(arr, ctx.heap_mut(), "groups", Value::Object(groups_obj))?;
    } else {
        crate::array::set_named_property(arr, ctx.heap_mut(), "groups", Value::Undefined)?;
    }

    if has_indices {
        let arr_value = Value::Array(arr);
        let mut indices_elems: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
        indices_elems.push(pair_array_native(
            m.range.start,
            m.range.end,
            ctx,
            &[&input_value, &arr_value],
            &[out.as_slice()],
        )?);
        for cap in &m.captures {
            match cap {
                Some(r) => indices_elems.push(pair_array_native(
                    r.start,
                    r.end,
                    ctx,
                    &[&input_value, &arr_value],
                    &[out.as_slice(), indices_elems.as_slice()],
                )?),
                None => indices_elems.push(Value::Undefined),
            }
        }
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.push(&input_value);
        roots.push(&arr_value);
        roots.extend_from_slice(value_roots);
        let mut index_slices = Vec::with_capacity(slice_roots.len() + 2);
        index_slices.push(out.as_slice());
        index_slices.push(indices_elems.as_slice());
        index_slices.extend_from_slice(slice_roots);
        let indices_arr = ctx.array_from_elements_with_roots(
            indices_elems.iter().cloned(),
            &roots,
            &index_slices,
        )?;
        let mut named_iter = m.named_groups();
        let first_named = named_iter.next();
        if let Some((name, range)) = first_named {
            let indices_value = Value::Array(indices_arr);
            let mut roots = Vec::with_capacity(value_roots.len() + 3);
            roots.push(&input_value);
            roots.push(&arr_value);
            roots.push(&indices_value);
            roots.extend_from_slice(value_roots);
            let g_obj = ctx.alloc_object_with_roots(&roots, &index_slices)?;
            crate::object::set_prototype(g_obj, ctx.heap_mut(), None);
            let v = match range {
                Some(r) => pair_array_native(
                    r.start,
                    r.end,
                    ctx,
                    &roots,
                    &[out.as_slice(), indices_elems.as_slice()],
                )?,
                None => Value::Undefined,
            };
            crate::object::set(g_obj, ctx.heap_mut(), name, v);
            for (name, range) in named_iter {
                let v = match range {
                    Some(r) => pair_array_native(
                        r.start,
                        r.end,
                        ctx,
                        &roots,
                        &[out.as_slice(), indices_elems.as_slice()],
                    )?,
                    None => Value::Undefined,
                };
                crate::object::set(g_obj, ctx.heap_mut(), name, v);
            }
            crate::array::set_named_property(
                indices_arr,
                ctx.heap_mut(),
                "groups",
                Value::Object(g_obj),
            )?;
        } else {
            crate::array::set_named_property(
                indices_arr,
                ctx.heap_mut(),
                "groups",
                Value::Undefined,
            )?;
        }
        crate::array::set_named_property(
            arr,
            ctx.heap_mut(),
            "indices",
            Value::Array(indices_arr),
        )?;
    }
    Ok(arr)
}

/// Build a `[start, end]` two-element array used by the `d`-flag
/// indices companion (§22.2.7.7).
fn pair_array(
    start: usize,
    end: usize,
    args: &mut IntrinsicArgs<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::Array(args.array_from_elements_rooted(
        [
            Value::Number(NumberValue::from_i32(start as i32)),
            Value::Number(NumberValue::from_i32(end as i32)),
        ],
        value_roots,
        slice_roots,
    )?))
}

fn pair_array_native(
    start: usize,
    end: usize,
    ctx: &mut NativeCtx<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::Array(ctx.array_from_elements_with_roots(
        [
            Value::Number(NumberValue::from_i32(start as i32)),
            Value::Number(NumberValue::from_i32(end as i32)),
        ],
        value_roots,
        slice_roots,
    )?))
}

fn impl_exec(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let text = arg_string(args, 0)?.clone();
    let re_clone = *re;
    exec_once(&re_clone, &text, args)
}

fn impl_test(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let text = arg_string(args, 0)?.clone();
    let re_clone = *re;
    let result = exec_once(&re_clone, &text, args)?;
    Ok(Value::Boolean(!matches!(result, Value::Null)))
}

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let heap = &*args.gc_heap;
    let rendered = format!("/{}/{}", re.source(heap), re.flags(heap).to_js_string());
    Ok(Value::String(JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

/// Declarative `RegExp.prototype` table.
pub static REGEXP_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            RegExp,
            "exec"     / 1 => impl_exec,
            "test"     / 1 => impl_test,
            "toString" / 0 => impl_to_string,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    REGEXP_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::RegExp, name)
}

/// Resolve a JS-visible property of a `RegExp` value. `None` when
/// the property is not a recognised RegExp member — callers fall
/// back to `Value::Undefined`. `lastIndex` reads and writes flow
/// through here too.
#[must_use]
pub fn load_property(
    re: &JsRegExp,
    gc_heap: &otter_gc::GcHeap,
    name: &str,
    string_heap: &crate::string::StringHeap,
) -> Value {
    match name {
        "source" => match JsString::from_str(&re.source(gc_heap), string_heap) {
            Ok(s) => Value::String(s),
            Err(_) => Value::Undefined,
        },
        "flags" => match JsString::from_str(&re.flags(gc_heap).to_js_string(), string_heap) {
            Ok(s) => Value::String(s),
            Err(_) => Value::Undefined,
        },
        "hasIndices" => Value::Boolean(re.flags(gc_heap).has_indices),
        "global" => Value::Boolean(re.flags(gc_heap).global),
        "ignoreCase" => Value::Boolean(re.flags(gc_heap).ignore_case),
        "multiline" => Value::Boolean(re.flags(gc_heap).multiline),
        "dotAll" => Value::Boolean(re.flags(gc_heap).dot_all),
        "unicode" => Value::Boolean(re.flags(gc_heap).unicode),
        "sticky" => Value::Boolean(re.flags(gc_heap).sticky),
        "unicodeSets" => Value::Boolean(re.flags(gc_heap).unicode_sets),
        "lastIndex" => re.last_index_value(gc_heap),
        _ => Value::Undefined,
    }
}

/// Mutate a JS-visible property on a `RegExp`. Currently only
/// `lastIndex` is writable; everything else is silently ignored
/// (foundation: the spec marks accessors non-writable, so a real
/// `TypeError` belongs in a later strict-mode slice).
pub fn store_property(re: &JsRegExp, gc_heap: &mut otter_gc::GcHeap, name: &str, value: Value) {
    if name == "lastIndex" {
        re.set_last_index_value(gc_heap, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    fn make(pattern: &str, flags: &str, gc_heap: &mut otter_gc::GcHeap) -> Value {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        Value::RegExp(JsRegExp::compile(gc_heap, &units, flags).unwrap())
    }

    fn call(method: &str, recv: &Value, args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Value {
        let heap = StringHeap::default();
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: recv,
            args,
            string_heap: &heap,
            gc_heap,
            allocation_roots: &[],
        })
        .unwrap()
    }

    #[test]
    fn test_returns_boolean() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("ab+c", "", &mut gc_heap);
        let text = Value::String(JsString::from_str("abbbc", &heap).unwrap());
        assert_eq!(
            call("test", &re, &[text], &mut gc_heap),
            Value::Boolean(true)
        );
        let no = Value::String(JsString::from_str("xy", &heap).unwrap());
        assert_eq!(
            call("test", &re, &[no], &mut gc_heap),
            Value::Boolean(false)
        );
    }

    #[test]
    fn exec_returns_array_or_null() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("(a)(b)", "", &mut gc_heap);
        let text = Value::String(JsString::from_str("ab", &heap).unwrap());
        let r = call("exec", &re, &[text], &mut gc_heap);
        match r {
            Value::Array(arr) => {
                assert_eq!(crate::array::len(arr, &gc_heap), 3);
                assert_eq!(crate::array::get(arr, &gc_heap, 0).display_string(), "ab");
                assert_eq!(crate::array::get(arr, &gc_heap, 1).display_string(), "a");
                assert_eq!(crate::array::get(arr, &gc_heap, 2).display_string(), "b");
            }
            _ => panic!("expected array"),
        }
        let miss = call(
            "exec",
            &re,
            &[Value::String(JsString::from_str("zz", &heap).unwrap())],
            &mut gc_heap,
        );
        assert_eq!(miss, Value::Null);
    }

    #[test]
    fn exec_result_arrays_use_intrinsic_rooted_allocation() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("(?<first>a)(b)", "d", &mut gc_heap);
        let text = Value::String(JsString::from_str("ab", &heap).unwrap());
        let before = gc_heap.stats().new_allocated_bytes;
        let result = call("exec", &re, std::slice::from_ref(&text), &mut gc_heap);
        let after = gc_heap.stats().new_allocated_bytes;

        assert!(
            after > before,
            "RegExp exec result arrays, groups, and indices should allocate through intrinsic roots"
        );
        let Value::Array(arr) = result else {
            panic!("expected RegExp exec result array");
        };
        assert!(matches!(
            crate::array::get_named_property(arr, &gc_heap, "indices"),
            Some(Value::Array(_))
        ));
        assert!(matches!(
            crate::array::get_named_property(arr, &gc_heap, "groups"),
            Some(Value::Object(_))
        ));
    }

    #[test]
    fn exec_global_walks_through_text() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("a", "g", &mut gc_heap);
        let text = Value::String(JsString::from_str("abab", &heap).unwrap());
        // First call → match at 0, lastIndex moves to 1.
        let r1 = call("exec", &re, std::slice::from_ref(&text), &mut gc_heap);
        match (&r1, &re) {
            (Value::Array(arr), Value::RegExp(rx)) => {
                assert_eq!(crate::array::get(*arr, &gc_heap, 0).display_string(), "a");
                assert_eq!(rx.last_index(&gc_heap), 1);
            }
            _ => panic!(),
        }
        // Second call → match at 2, lastIndex → 3.
        call("exec", &re, std::slice::from_ref(&text), &mut gc_heap);
        if let Value::RegExp(rx) = &re {
            assert_eq!(rx.last_index(&gc_heap), 3);
        }
        // Third call → no match, lastIndex → 0.
        let r3 = call("exec", &re, &[text], &mut gc_heap);
        assert_eq!(r3, Value::Null);
        if let Value::RegExp(rx) = &re {
            assert_eq!(rx.last_index(&gc_heap), 0);
        }
    }

    #[test]
    fn property_lookups() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = JsRegExp::compile(
            &mut gc_heap,
            &"ab+c".encode_utf16().collect::<Vec<_>>(),
            "gi",
        )
        .unwrap();
        let src = load_property(&re, &gc_heap, "source", &heap);
        assert_eq!(src.display_string(), "ab+c");
        let flags = load_property(&re, &gc_heap, "flags", &heap);
        assert_eq!(flags.display_string(), "gi");
        assert_eq!(
            load_property(&re, &gc_heap, "global", &heap),
            Value::Boolean(true)
        );
        assert_eq!(
            load_property(&re, &gc_heap, "ignoreCase", &heap),
            Value::Boolean(true)
        );
        assert_eq!(
            load_property(&re, &gc_heap, "multiline", &heap),
            Value::Boolean(false)
        );
    }

    #[test]
    fn last_index_writable() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re =
            JsRegExp::compile(&mut gc_heap, &"a".encode_utf16().collect::<Vec<_>>(), "g").unwrap();
        store_property(
            &re,
            &mut gc_heap,
            "lastIndex",
            Value::Number(NumberValue::from_i32(7)),
        );
        assert_eq!(re.last_index(&gc_heap), 7);
        // Numeric execution coercion clamps negative values to 0,
        // while the JS-visible property preserves the written value.
        store_property(
            &re,
            &mut gc_heap,
            "lastIndex",
            Value::Number(NumberValue::from_i32(-3)),
        );
        assert_eq!(re.last_index(&gc_heap), 0);
        assert_eq!(
            load_property(&re, &gc_heap, "lastIndex", &heap),
            Value::Number(NumberValue::from_i32(-3))
        );
        // String writes are observable, and execution coerces them
        // numerically when needed.
        let written = JsString::from_str("9", &heap).unwrap();
        store_property(
            &re,
            &mut gc_heap,
            "lastIndex",
            Value::String(written.clone()),
        );
        assert_eq!(
            load_property(&re, &gc_heap, "lastIndex", &heap),
            Value::String(written)
        );
        assert_eq!(re.last_index(&gc_heap), 9);
        // Non-lastIndex names are silently ignored.
        store_property(
            &re,
            &mut gc_heap,
            "source",
            Value::String(JsString::from_str("nope", &heap).unwrap()),
        );
    }
}
