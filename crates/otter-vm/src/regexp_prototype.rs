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
use crate::VmError;
use crate::array::JsArray;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::regexp::JsRegExp;
use crate::runtime_cx::NativeCtx;
use crate::string::JsString;

fn receiver_regexp<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsRegExp, IntrinsicError> {
    match args.receiver {
        Value::RegExp(r) => Ok(r),
        _ => Err(IntrinsicError::BadReceiver { expected: "regexp" }),
    }
}

fn vm_shape_error_to_intrinsic(err: VmError) -> IntrinsicError {
    match err {
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        _ => IntrinsicError::BadArgument {
            index: 0,
            reason: "property shape update failed",
        },
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
    let units = text.to_utf16_vec(args.gc_heap);
    let len = units.len();
    let flags = re.flags(args.gc_heap);
    let mut start = re.last_index(args.gc_heap) as usize;
    if (flags.global || flags.sticky) && start > len {
        re.set_last_index(args.gc_heap, 0);
        return Ok(Value::null());
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
            return Ok(Value::null());
        }
    };
    if flags.sticky && m.range.start != start {
        re.set_last_index(args.gc_heap, 0);
        return Ok(Value::null());
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
    ctx: &mut NativeCtx<'_>,
    slice_roots: &[&[Value]],
) -> Result<Value, IntrinsicError> {
    let units = text.to_utf16_vec(ctx.heap());
    let len = units.len();
    let flags = re.flags(ctx.heap());
    let mut start = re.last_index(ctx.heap()) as usize;
    if (flags.global || flags.sticky) && start > len {
        re.set_last_index(ctx.heap_mut(), 0);
        return Ok(Value::null());
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
            return Ok(Value::null());
        }
    };
    if flags.sticky && m.range.start != start {
        re.set_last_index(ctx.heap_mut(), 0);
        return Ok(Value::null());
    }
    if flags.global || flags.sticky {
        re.set_last_index(ctx.heap_mut(), m.range.end as u32);
    }

    Ok(Value::Array(build_match_result_native(
        &m,
        &units,
        text,
        flags.has_indices,
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
    let full = JsString::from_utf16_units(&units[m.range.clone()], args.gc_heap)?;
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    out.push(Value::string(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&units[r.clone()], args.gc_heap)?;
                out.push(Value::string(s));
            }
            None => out.push(Value::Undefined),
        }
    }
    let input_value = Value::string(*input);
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
    crate::array::set_named_property(arr, args.gc_heap, "input", input_value)?;

    let mut named_iter = m.named_groups();
    let first_named = named_iter.next();
    if let Some((name, range)) = first_named {
        let arr_value = Value::array(arr);
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.push(&input_value);
        roots.push(&arr_value);
        roots.extend_from_slice(value_roots);
        let groups_obj = args.alloc_object_rooted(&roots, &slices)?;
        crate::object::set_prototype(groups_obj, args.gc_heap, None);
        let value = match range {
            Some(r) => Value::string(JsString::from_utf16_units(&units[r], args.gc_heap)?),
            None => Value::undefined(),
        };
        crate::object::set(groups_obj, args.gc_heap, name, value);
        for (name, range) in named_iter {
            let value = match range {
                Some(r) => Value::string(JsString::from_utf16_units(&units[r], args.gc_heap)?),
                None => Value::undefined(),
            };
            crate::object::set(groups_obj, args.gc_heap, name, value);
        }
        crate::array::set_named_property(arr, args.gc_heap, "groups", Value::Object(groups_obj))?;
    } else {
        crate::array::set_named_property(arr, args.gc_heap, "groups", Value::Undefined)?;
    }

    if has_indices {
        let arr_value = Value::array(arr);
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
            let indices_value = Value::array(indices_arr);
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
                None => Value::undefined(),
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
                    None => Value::undefined(),
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
    ctx: &mut NativeCtx<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsArray, IntrinsicError> {
    let full = JsString::from_utf16_units(&units[m.range.clone()], ctx.heap_mut())?;
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    out.push(Value::string(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&units[r.clone()], ctx.heap_mut())?;
                out.push(Value::string(s));
            }
            None => out.push(Value::Undefined),
        }
    }
    let input_value = Value::string(*input);
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
    crate::array::set_named_property(arr, ctx.heap_mut(), "input", input_value)?;

    let mut named_iter = m.named_groups();
    let first_named = named_iter.next();
    if let Some((name, range)) = first_named {
        let arr_value = Value::array(arr);
        let mut roots = Vec::with_capacity(value_roots.len() + 2);
        roots.push(&input_value);
        roots.push(&arr_value);
        roots.extend_from_slice(value_roots);
        let groups_obj = ctx.alloc_object_with_roots(&roots, &slices)?;
        crate::object::set_prototype(groups_obj, ctx.heap_mut(), None);
        let value = match range {
            Some(r) => Value::string(JsString::from_utf16_units(&units[r], ctx.heap_mut())?),
            None => Value::undefined(),
        };
        ctx.set_property_with_roots(groups_obj, name, value, &roots, &slices)
            .map_err(vm_shape_error_to_intrinsic)?;
        for (name, range) in named_iter {
            let value = match range {
                Some(r) => Value::string(JsString::from_utf16_units(&units[r], ctx.heap_mut())?),
                None => Value::undefined(),
            };
            ctx.set_property_with_roots(groups_obj, name, value, &roots, &slices)
                .map_err(vm_shape_error_to_intrinsic)?;
        }
        crate::array::set_named_property(arr, ctx.heap_mut(), "groups", Value::Object(groups_obj))?;
    } else {
        crate::array::set_named_property(arr, ctx.heap_mut(), "groups", Value::Undefined)?;
    }

    if has_indices {
        let arr_value = Value::array(arr);
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
            let indices_value = Value::array(indices_arr);
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
                None => Value::undefined(),
            };
            ctx.set_property_with_roots(g_obj, name, v, &roots, &index_slices)
                .map_err(vm_shape_error_to_intrinsic)?;
            for (name, range) in named_iter {
                let v = match range {
                    Some(r) => pair_array_native(
                        r.start,
                        r.end,
                        ctx,
                        &roots,
                        &[out.as_slice(), indices_elems.as_slice()],
                    )?,
                    None => Value::undefined(),
                };
                ctx.set_property_with_roots(g_obj, name, v, &roots, &index_slices)
                    .map_err(vm_shape_error_to_intrinsic)?;
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
    let re_clone = *receiver_regexp(args)?;
    let text = arg_to_string_primitive(args, 0)?;
    exec_once(&re_clone, &text, args)
}

fn impl_test(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re_clone = *receiver_regexp(args)?;
    let text = arg_to_string_primitive(args, 0)?;
    let result = exec_once(&re_clone, &text, args)?;
    Ok(Value::boolean(!matches!(result, Value::Null)))
}

/// §22.2.7.1 step 4 — `Let S be ? ToString(string)`. Coerces every
/// primitive shape (Number / Boolean / Null / Undefined / BigInt /
/// String) to a fresh `JsString`. Object operands fall back to
/// "[object Object]" matching V8 for the no-context intrinsic path
/// (full `@@toPrimitive` / `valueOf` / `toString` coercion belongs
/// in the runtime entry point that already routes
/// `RegExp.prototype.exec` calls through `evaluate_to_primitive`).
fn arg_to_string_primitive(
    args: &mut IntrinsicArgs<'_>,
    index: usize,
) -> Result<JsString, IntrinsicError> {
    let raw = args.args.get(index).cloned().unwrap_or(Value::undefined());
    let text: String = match &raw {
        Value::String(s) => return Ok(*s),
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Boolean(true) => "true".to_string(),
        Value::Boolean(false) => "false".to_string(),
        Value::Number(n) => n.to_display_string(),
        Value::BigInt(b) => b.to_decimal_string(&*args.gc_heap),
        Value::Symbol(_) => {
            return Err(IntrinsicError::BadArgument {
                index: index as u16,
                reason: "cannot convert a Symbol to a string",
            });
        }
        other => other.display_string(&*args.gc_heap),
    };
    Ok(JsString::from_str(&text, args.gc_heap)?)
}

fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let heap = &*args.gc_heap;
    let rendered = format!("/{}/{}", re.source(heap), re.flags(heap).to_js_string());
    Ok(Value::string(JsString::from_str(&rendered, args.gc_heap)?))
}

/// §22.2.6.8 `RegExp.prototype[@@match](string)` — invoked by
/// `String.prototype.match(re)` when a user installs a custom
/// matcher, and directly when user code calls
/// `re[Symbol.match]("…")`.
///
/// 1. Let `rx` be the this value; must be Object.
/// 2. Let `S` be `? ToString(string)`.
/// 3. Let `flags` be `? ToString(? Get(rx, "flags"))`.
/// 4. If `flags` contains "g", run the global match loop returning
///    an array of full matches or `null` when no match exists.
/// 5. Otherwise return `? RegExpExec(rx, S)`.
///
/// The foundation drives the engine directly via
/// [`JsRegExp::find_from_utf16`] so the spec-mandated `lastIndex`
/// updates and `AdvanceStringIndex` semantics fire correctly for
/// both global and non-global receivers. Unicode awareness is keyed
/// off the `u` and `v` flags per §22.2.5.2 step 18.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-regexp.prototype-@@match>
pub fn native_regexp_symbol_match(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let receiver = *ctx.this_value();
    let Value::RegExp(re) = receiver else {
        return Err(crate::NativeError::TypeError {
            name: "RegExp.prototype[@@match]",
            reason: "called on a non-RegExp receiver".to_string(),
        });
    };

    let text = string_arg_to_jsstring(ctx, args, 0, "RegExp.prototype[@@match]")?;
    let flags = re.flags(ctx.heap());
    let units = text.to_utf16_vec(ctx.heap());
    if !flags.global {
        return exec_once_native(&re, &text, ctx, &[])
            .map_err(intrinsic_to_native_error("RegExp.prototype[@@match]"));
    }
    let full_unicode = flags.unicode || flags.unicode_sets;
    re.set_last_index(ctx.heap_mut(), 0);
    let mut cursor: usize = 0;
    let mut matches_out: Vec<Value> = Vec::new();
    loop {
        let mut iter = re.find_from_utf16(ctx.heap(), &units, cursor).into_iter();
        let m = match iter.next() {
            Some(m) => m,
            None => break,
        };
        let match_str = JsString::from_utf16_units(&units[m.range.clone()], ctx.heap_mut())
            .map_err(|_| crate::NativeError::TypeError {
                name: "RegExp.prototype[@@match]",
                reason: "out of memory".to_string(),
            })?;
        matches_out.push(Value::string(match_str));
        if m.range.start == m.range.end {
            cursor = advance_string_index(&units, m.range.end, full_unicode);
        } else {
            cursor = m.range.end;
        }
        if cursor > units.len() {
            break;
        }
    }
    re.set_last_index(ctx.heap_mut(), 0);
    if matches_out.is_empty() {
        return Ok(Value::null());
    }
    let receiver_value = Value::regexp(re);
    let text_value = Value::string(text);
    let arr = ctx
        .array_from_elements_with_roots(
            matches_out.iter().cloned(),
            &[&receiver_value, &text_value],
            &[matches_out.as_slice()],
        )
        .map_err(|_| crate::NativeError::TypeError {
            name: "RegExp.prototype[@@match]",
            reason: "array allocation failed".to_string(),
        })?;
    Ok(Value::array(arr))
}

/// §22.2.6.10 `RegExp.prototype[@@search](string)` — returns the
/// 0-based index of the first match or `-1`, preserving the
/// receiver's pre-call `lastIndex`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-regexp.prototype-@@search>
pub fn native_regexp_symbol_search(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let receiver = *ctx.this_value();
    let Value::RegExp(re) = receiver else {
        return Err(crate::NativeError::TypeError {
            name: "RegExp.prototype[@@search]",
            reason: "called on a non-RegExp receiver".to_string(),
        });
    };

    let text = string_arg_to_jsstring(ctx, args, 0, "RegExp.prototype[@@search]")?;
    let previous = re.last_index_value(ctx.heap());
    re.set_last_index(ctx.heap_mut(), 0);
    let units = text.to_utf16_vec(ctx.heap());
    let result = re.find_from_utf16(ctx.heap(), &units, 0).into_iter().next();
    re.set_last_index_value(ctx.heap_mut(), previous);
    Ok(match result {
        Some(m) => Value::number(NumberValue::from_i32(m.range.start as i32)),
        None => Value::number(NumberValue::from_i32(-1)),
    })
}

/// §22.2.7.3 `AdvanceStringIndex(S, index, unicode)`. When `unicode`
/// is true and the current code unit is a high surrogate followed
/// by a low surrogate, the index advances by two; otherwise by one.
fn advance_string_index(units: &[u16], index: usize, unicode: bool) -> usize {
    if !unicode || index + 1 >= units.len() {
        return index + 1;
    }
    let cp = units[index];
    if (0xD800..=0xDBFF).contains(&cp) {
        let next = units[index + 1];
        if (0xDC00..=0xDFFF).contains(&next) {
            return index + 2;
        }
    }
    index + 1
}

fn string_arg_to_jsstring(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
    method_name: &'static str,
) -> Result<JsString, crate::NativeError> {
    let raw = args.get(index).cloned().unwrap_or(Value::undefined());
    let text: String = match &raw {
        Value::String(s) => return Ok(*s),
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Boolean(true) => "true".to_string(),
        Value::Boolean(false) => "false".to_string(),
        Value::Number(n) => n.to_display_string(),
        Value::BigInt(b) => b.to_decimal_string(ctx.heap()),
        Value::Symbol(_) => {
            return Err(crate::NativeError::TypeError {
                name: method_name,
                reason: "cannot convert a Symbol to a string".to_string(),
            });
        }
        other => other.display_string(ctx.heap()),
    };
    JsString::from_str(&text, ctx.heap_mut()).map_err(|_| crate::NativeError::TypeError {
        name: method_name,
        reason: "out of memory".to_string(),
    })
}

fn intrinsic_to_native_error(
    method_name: &'static str,
) -> impl Fn(IntrinsicError) -> crate::NativeError {
    move |err| crate::NativeError::TypeError {
        name: method_name,
        reason: err.to_string(),
    }
}

fn vm_err_to_native(name: &'static str) -> impl Fn(crate::VmError) -> crate::NativeError {
    move |err| match err {
        crate::VmError::Uncaught { value } => crate::NativeError::Thrown {
            name,
            message: value,
        },
        crate::VmError::TypeError { message } => crate::NativeError::TypeError {
            name,
            reason: message,
        },
        crate::VmError::RangeError { message } => crate::NativeError::RangeError {
            name,
            reason: message,
        },
        crate::VmError::SyntaxError { message } => crate::NativeError::SyntaxError {
            name,
            reason: message,
        },
        other => crate::NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

/// `? Get(value, key)` driven through `ordinary_get_value` so accessor
/// getters fire observably. Used by the @@replace ladder to call
/// user-overridable `flags` / `exec` / `lastIndex` / `index` / etc.
pub(crate) fn get_property_runtime(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    key: &str,
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let outcome = interp
        .ordinary_get_value(
            &exec,
            *receiver,
            *receiver,
            &crate::VmPropertyKey::String(key),
            0,
        )
        .map_err(vm_err_to_native(name))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
            interp
                .run_callable_sync(&exec, &getter, *receiver, args)
                .map_err(vm_err_to_native(name))
        }
    }
}

/// `? Set(value, key, v, true)` driven through `ordinary_set_data_value`
/// so the spec-mandated observable write fires through the normal
/// `[[Set]]` ladder (including the JsRegExp `lastIndex` storage path).
fn set_property_runtime(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    key: &str,
    value: Value,
    name: &'static str,
) -> Result<(), crate::NativeError> {
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    interp
        .ordinary_set_data_value(
            &exec,
            *receiver,
            &crate::VmPropertyKey::String(key),
            value,
            *receiver,
            0,
        )
        .map_err(vm_err_to_native(name))?;
    Ok(())
}

/// §7.1.17 `ToString` synchronous bridge. Primitives go through
/// `to_string_primitive`; objects run the §7.1.1 / §7.1.1.1
/// `[Symbol.toPrimitive]` → `toString` → `valueOf` ladder via the
/// interpreter helper.
pub(crate) fn coerce_to_jsstring_runtime(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    name: &'static str,
) -> Result<JsString, crate::NativeError> {
    if matches!(value, Value::Symbol(_)) {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "cannot convert a Symbol to a string".to_string(),
        });
    }
    if let Value::String(s) = value {
        return Ok(*s);
    }
    let primitive = if crate::abstract_ops::is_primitive(value) {
        *value
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        interp
            .evaluate_to_primitive(&exec, value, crate::abstract_ops::ToPrimitiveHint::String)
            .map_err(vm_err_to_native(name))?
    };

    crate::conversion::to_js_string_primitive(&primitive, ctx.heap_mut()).map_err(|e| {
        crate::NativeError::TypeError {
            name,
            reason: format!("ToString failed: {e:?}"),
        }
    })
}

/// §7.1.20 `ToLength(value)`. Coerces via `ToNumber` (which goes
/// through `ToPrimitive(hint=number)` for object operands) then
/// clamps the integer result to `[0, 2^53 - 1]`.
fn to_length_runtime(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    name: &'static str,
) -> Result<u64, crate::NativeError> {
    let primitive = if crate::abstract_ops::is_primitive(value) {
        *value
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        interp
            .evaluate_to_primitive(&exec, value, crate::abstract_ops::ToPrimitiveHint::Number)
            .map_err(vm_err_to_native(name))?
    };
    let n = crate::number::to_number_value(&primitive, ctx.heap());
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    if n.is_infinite() {
        return Ok(9_007_199_254_740_991);
    }
    let trunc = n.trunc();
    let clamped = trunc.min(9_007_199_254_740_991.0);
    Ok(clamped as u64)
}

/// §7.1.5 `ToIntegerOrInfinity`. `NaN` / `+0` / `-0` → 0; ±Infinity
/// pass through; finite values truncate toward zero.
fn to_integer_or_infinity_runtime(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    name: &'static str,
) -> Result<f64, crate::NativeError> {
    let primitive = if crate::abstract_ops::is_primitive(value) {
        *value
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        interp
            .evaluate_to_primitive(&exec, value, crate::abstract_ops::ToPrimitiveHint::Number)
            .map_err(vm_err_to_native(name))?
    };
    let n = crate::number::to_number_value(&primitive, ctx.heap());
    if n.is_nan() {
        return Ok(0.0);
    }
    if n == 0.0 {
        return Ok(0.0);
    }
    if n.is_infinite() {
        return Ok(n);
    }
    Ok(n.trunc())
}

/// §22.2.7.1 `RegExpExec(R, S)`. Dispatches through
/// `Get(R, "exec")` so user-overridable execs are honoured before
/// falling back to the builtin §22.2.7.2 algorithm when `R` is a
/// `Value::RegExp` instance.
fn regexp_exec_runtime(
    ctx: &mut NativeCtx<'_>,
    rx: &Value,
    s: &JsString,
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    let exec_fn = get_property_runtime(ctx, rx, "exec", name)?;
    let is_callable = ctx.cx.interp.is_callable_runtime(&exec_fn);
    if is_callable {
        let (interp, exec_ctx) = ctx.interp_mut_and_context();
        let exec_ctx = exec_ctx.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        let mut args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        args.push(Value::string(*s));
        let result = interp
            .run_callable_sync(&exec_ctx, &exec_fn, *rx, args)
            .map_err(vm_err_to_native(name))?;
        if !matches!(result, Value::Null) && !crate::value_kind::is_object_like_value(&result) {
            return Err(crate::NativeError::TypeError {
                name,
                reason: "exec did not return an Object or null".to_string(),
            });
        }
        return Ok(result);
    }
    // Fall back to builtin exec only when `rx` is actually a RegExp.
    if let Value::RegExp(re) = rx {
        return exec_once_native(re, s, ctx, &[]).map_err(intrinsic_to_native_error(name));
    }
    Err(crate::NativeError::TypeError {
        name,
        reason: "exec is not callable and receiver is not a RegExp".to_string(),
    })
}

/// Advance a lazy RegExp String Iterator one step.
///
/// This is the §22.2.7.2 step body shared by reflective
/// `%RegExpStringIteratorPrototype%.next` calls and VM-level
/// `IteratorNext`. The observable operations deliberately route
/// through the native runtime bridges used by the rest of
/// `RegExp.prototype`: `RegExpExec`, `Get(match, "0")`, `ToString`,
/// `Get(R, "lastIndex")`, `ToLength`, and `Set(R, "lastIndex", …)`.
pub(crate) fn regexp_string_iterator_next_runtime(
    interp: &mut crate::Interpreter,
    context: &crate::ExecutionContext,
    matcher: &Value,
    input: &JsString,
    global: bool,
    full_unicode: bool,
) -> Result<Option<Value>, crate::VmError> {
    let name = "RegExp String Iterator.next";
    let mut ctx = NativeCtx::new_with_call_info_and_context(
        interp,
        crate::NativeCallInfo::call(*matcher),
        Some(context.clone()),
    );
    let result =
        regexp_exec_runtime(&mut ctx, matcher, input, name).map_err(crate::native_to_vm_error)?;
    if matches!(result, Value::Null) {
        return Ok(None);
    }
    if global {
        let matched_val = get_property_runtime(&mut ctx, &result, "0", name)
            .map_err(crate::native_to_vm_error)?;
        let matched_str = coerce_to_jsstring_runtime(&mut ctx, &matched_val, name)
            .map_err(crate::native_to_vm_error)?;
        if matched_str.is_empty() {
            let li_val = get_property_runtime(&mut ctx, matcher, "lastIndex", name)
                .map_err(crate::native_to_vm_error)?;
            let this_index = to_length_runtime(&mut ctx, &li_val, name)
                .map_err(crate::native_to_vm_error)? as usize;
            let input_units = input.to_utf16_vec(ctx.heap());
            let next_index = advance_string_index(&input_units, this_index, full_unicode);
            set_property_runtime(
                &mut ctx,
                matcher,
                "lastIndex",
                Value::Number(NumberValue::from_f64(next_index as f64)),
                name,
            )
            .map_err(crate::native_to_vm_error)?;
        }
    }
    Ok(Some(result))
}

/// §22.2.6.11 `RegExp.prototype[@@replace](string, replaceValue)`.
///
/// Walks the user-overridable protocol: `Get(rx, "flags")`,
/// `Get(rx, "exec")`, `Set(rx, "lastIndex", …)` and friends are
/// routed through the interpreter helpers so accessor getters /
/// setters, monkey-patched `exec`, and proxy `[[Set]]` traps observe
/// every read and write in the spec-mandated order. The
/// `GetSubstitution` (§22.2.6.11.1) tail handles `$$`, `$&`, `` $` ``,
/// `$'`, `$1`-`$nn`, `$<name>` with proper truncation and named-group
/// resolution.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-regexp.prototype-@@replace>
/// - <https://tc39.es/ecma262/#sec-getsubstitution>
pub fn native_regexp_symbol_replace(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let name = "RegExp.prototype[@@replace]";
    let receiver = *ctx.this_value();
    if !crate::value_kind::is_object_like_value(&receiver) {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "called on a non-object receiver".to_string(),
        });
    }

    let string_arg = args.first().cloned().unwrap_or(Value::undefined());
    let replace_value_arg = args.get(1).cloned().unwrap_or(Value::undefined());

    // Step 3 — S = ? ToString(string).
    let s = coerce_to_jsstring_runtime(ctx, &string_arg, name)?;
    let s_units = s.to_utf16_vec(ctx.heap());
    let length_s = s_units.len();

    // Step 4 — functionalReplace = IsCallable(replaceValue).
    let functional_replace = ctx.cx.interp.is_callable_runtime(&replace_value_arg);

    // Step 5 — non-callable replacements are ToString-coerced once.
    let replacement_template = if functional_replace {
        None
    } else {
        Some(coerce_to_jsstring_runtime(ctx, &replace_value_arg, name)?)
    };

    // Step 6 — flags = ? ToString(? Get(rx, "flags")).
    let flags_val = get_property_runtime(ctx, &receiver, "flags", name)?;
    let flags_str = coerce_to_jsstring_runtime(ctx, &flags_val, name)?.to_lossy_string(ctx.heap());
    let global = flags_str.contains('g');
    let full_unicode = flags_str.contains('u') || flags_str.contains('v');

    // Step 9 — if global, Set(rx, "lastIndex", 0, true).
    if global {
        set_property_runtime(
            ctx,
            &receiver,
            "lastIndex",
            Value::Number(NumberValue::from_i32(0)),
            name,
        )?;
    }

    // Step 10-12 — collect results.
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_runtime(ctx, &receiver, &s, name)?;
        if matches!(result, Value::Null) {
            break;
        }
        results.push(result);
        if !global {
            break;
        }
        // Step 11.d.iii — empty match: advance lastIndex by one
        // (or two for paired surrogates under `u` / `v`).
        let matched_val = get_property_runtime(ctx, &result, "0", name)?;
        let matched_str = coerce_to_jsstring_runtime(ctx, &matched_val, name)?;
        if matched_str.is_empty() {
            let last_index_val = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
            let this_index = to_length_runtime(ctx, &last_index_val, name)? as usize;
            let next_index = advance_string_index(&s_units, this_index, full_unicode);
            set_property_runtime(
                ctx,
                &receiver,
                "lastIndex",
                Value::Number(NumberValue::from_f64(next_index as f64)),
                name,
            )?;
        }
    }

    // Step 13-15 — build the accumulated replacement.
    let mut accumulated: Vec<u16> = Vec::new();
    let mut next_source_position: usize = 0;

    for result in &results {
        let length_val = get_property_runtime(ctx, result, "length", name)?;
        let result_length = to_length_runtime(ctx, &length_val, name)? as usize;
        let n_captures = result_length.saturating_sub(1);

        let matched_val = get_property_runtime(ctx, result, "0", name)?;
        let matched_str = coerce_to_jsstring_runtime(ctx, &matched_val, name)?;
        let match_units = matched_str.to_utf16_vec(ctx.heap());
        let match_length = match_units.len();

        let index_val = get_property_runtime(ctx, result, "index", name)?;
        let position_raw = to_integer_or_infinity_runtime(ctx, &index_val, name)?;
        let position = position_raw.max(0.0).min(length_s as f64) as usize;

        let mut captures: Vec<Option<JsString>> = Vec::with_capacity(n_captures);
        for i in 1..=n_captures {
            let cap_key = i.to_string();
            let cap_val = get_property_runtime(ctx, result, &cap_key, name)?;
            if matches!(cap_val, Value::Undefined) {
                captures.push(None);
            } else {
                captures.push(Some(coerce_to_jsstring_runtime(ctx, &cap_val, name)?));
            }
        }

        let named_captures = get_property_runtime(ctx, result, "groups", name)?;
        let named_captures_obj = if matches!(named_captures, Value::Undefined) {
            None
        } else {
            Some(named_captures)
        };

        let replacement: Vec<u16> = if functional_replace {
            let mut replacer_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
            replacer_args.push(Value::string(matched_str));
            for cap in &captures {
                replacer_args.push(match cap {
                    Some(s) => Value::string(*s),
                    None => Value::undefined(),
                });
            }
            replacer_args.push(Value::number(NumberValue::from_f64(position as f64)));
            replacer_args.push(Value::string(s));
            if let Some(nc) = &named_captures_obj {
                replacer_args.push(*nc);
            }
            let exec_ctx =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "missing execution context".to_string(),
                    })?;
            let raw = {
                let (interp, _) = ctx.interp_mut_and_context();
                interp
                    .run_callable_sync(
                        &exec_ctx,
                        &replace_value_arg,
                        Value::Undefined,
                        replacer_args,
                    )
                    .map_err(vm_err_to_native(name))?
            };
            let raw_str = coerce_to_jsstring_runtime(ctx, &raw, name)?;
            raw_str.to_utf16_vec(ctx.heap())
        } else {
            let template = replacement_template
                .as_ref()
                .expect("non-functional path has a template")
                .to_utf16_vec(ctx.heap());
            get_substitution(
                ctx,
                &match_units,
                &s_units,
                position,
                &captures,
                named_captures_obj.as_ref(),
                &template,
                name,
            )?
        };

        if position >= next_source_position {
            accumulated.extend_from_slice(&s_units[next_source_position..position]);
            accumulated.extend_from_slice(&replacement);
            next_source_position = position + match_length;
        }
    }

    if next_source_position < length_s {
        accumulated.extend_from_slice(&s_units[next_source_position..]);
    }

    Ok(Value::String(
        JsString::from_utf16_units(&accumulated, ctx.heap_mut()).map_err(|_| {
            crate::NativeError::TypeError {
                name,
                reason: "out of memory".to_string(),
            }
        })?,
    ))
}

/// `? Get(value, Symbol.someKey)` driven through `ordinary_get_value`
/// so accessor getters on symbol-keyed slots fire observably. Mirror
/// of [`get_property_runtime`] for `Symbol.species` /
/// `Symbol.toPrimitive` resolution inside @@split.
pub(crate) fn get_symbol_property_runtime(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    sym: &crate::symbol::JsSymbol,
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let outcome = interp
        .ordinary_get_value(
            &exec,
            *receiver,
            *receiver,
            &crate::VmPropertyKey::Symbol(*sym),
            0,
        )
        .map_err(vm_err_to_native(name))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
            interp
                .run_callable_sync(&exec, &getter, *receiver, args)
                .map_err(vm_err_to_native(name))
        }
    }
}

/// §7.3.21 `SpeciesConstructor(O, defaultConstructor)`. Resolves the
/// constructor to use when an algorithm needs to materialise a new
/// instance derived from `O`. Returns the default when `constructor`
/// is absent / undefined, throws TypeError on the spec-mandated
/// invalid shapes, and otherwise hands back `Symbol.species` (or the
/// constructor itself when species is absent / nullish).
fn species_constructor_runtime(
    ctx: &mut NativeCtx<'_>,
    obj: &Value,
    default_ctor: &Value,
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    let c = get_property_runtime(ctx, obj, "constructor", name)?;
    if matches!(c, Value::Undefined) {
        return Ok(*default_ctor);
    }
    if !crate::value_kind::is_object_like_value(&c) {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "constructor is not an Object".to_string(),
        });
    }
    let species_sym = ctx
        .cx
        .interp
        .well_known_symbols()
        .get(crate::symbol::WellKnown::Species);
    let s = get_symbol_property_runtime(ctx, &c, &species_sym, name)?;
    if matches!(s, Value::Undefined | Value::Null) {
        return Ok(c);
    }
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    if crate::abstract_ops::is_constructor(&s, &exec, &interp.gc_heap) {
        return Ok(s);
    }
    Err(crate::NativeError::TypeError {
        name,
        reason: "Symbol.species is not a constructor".to_string(),
    })
}

/// §22.2.6.9 `RegExp.prototype[@@matchAll](string)`. Resolves
/// `SpeciesConstructor(rx, %RegExp%)` (§7.3.21), builds the matcher
/// with the receiver's flags, snapshots the receiver's `lastIndex`
/// into the matcher, and returns a lazy RegExp String Iterator.
///
/// `lastIndex` is cached at call time per §22.2.6.9 step 2.f — the
/// matcher's `lastIndex` is read from the receiver before iteration
/// starts, so mutating `rx.lastIndex` between the @@matchAll call
/// and the iterator drain is observably ignored. Subsequent `exec`,
/// match-result `"0"`, and matcher `lastIndex` operations happen at
/// `.next()` time per `CreateRegExpStringIterator`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-regexp.prototype-@@matchall>
/// - <https://tc39.es/ecma262/#sec-createregexpstringiterator>
pub fn native_regexp_symbol_match_all(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let name = "RegExp.prototype[@@matchAll]";
    let receiver = *ctx.this_value();
    if !crate::value_kind::is_object_like_value(&receiver) {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "called on a non-object receiver".to_string(),
        });
    }
    let string_arg = args.first().cloned().unwrap_or(Value::undefined());
    let s = coerce_to_jsstring_runtime(ctx, &string_arg, name)?;

    let default_ctor = {
        let interp = &ctx.cx.interp;
        crate::object::get(interp.global_this, &interp.gc_heap, "RegExp").ok_or_else(|| {
            crate::NativeError::TypeError {
                name,
                reason: "%RegExp% intrinsic missing".to_string(),
            }
        })?
    };
    let c = species_constructor_runtime(ctx, &receiver, &default_ctor, name)?;

    let flags_val = get_property_runtime(ctx, &receiver, "flags", name)?;
    let flags_str = coerce_to_jsstring_runtime(ctx, &flags_val, name)?.to_lossy_string(ctx.heap());
    let global = flags_str.contains('g');
    let full_unicode = flags_str.contains('u') || flags_str.contains('v');

    let flags_js = {
        JsString::from_str(&flags_str, ctx.heap_mut()).map_err(|_| {
            crate::NativeError::TypeError {
                name,
                reason: "out of memory".to_string(),
            }
        })?
    };
    let matcher = {
        let mut ctor_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        ctor_args.push(receiver);
        ctor_args.push(Value::string(flags_js));
        let (interp, exec_ctx) = ctx.interp_mut_and_context();
        let exec_ctx = exec_ctx.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        interp
            .run_construct_sync(&exec_ctx, &c, c, ctor_args)
            .map_err(vm_err_to_native(name))?
    };

    // Step 2.f — snapshot rx.lastIndex into matcher.
    let last_index_val = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
    let last_index = to_length_runtime(ctx, &last_index_val, name)? as f64;
    set_property_runtime(
        ctx,
        &matcher,
        "lastIndex",
        Value::Number(NumberValue::from_f64(last_index)),
        name,
    )?;

    let input_root = Value::string(s);
    let iter_state = crate::IteratorState::RegExpString {
        matcher,
        input: s,
        global,
        full_unicode,
        done: false,
    };
    let handle = ctx
        .alloc_iterator_state(iter_state, &[&matcher, &input_root], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name,
            reason: "iterator allocation failed".to_string(),
        })?;
    Ok(Value::iterator(handle))
}

/// §22.2.6.14 `RegExp.prototype[@@split](string, limit)`.
///
/// Walks the spec-mandated `SpeciesConstructor` → `Construct(C, [rx,
/// newFlags])` → looped `RegExpExec(splitter, S)` ladder. The
/// splitter is forced into sticky mode so each step probes one
/// position; `lastIndex` is read / written observably through the
/// interpreter helpers so user-defined accessors / proxies see every
/// transition. Empty-match positions advance via
/// `AdvanceStringIndex` (§22.2.7.3) keyed off the `u` / `v` flags.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-regexp.prototype-@@split>
pub fn native_regexp_symbol_split(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let name = "RegExp.prototype[@@split]";
    let receiver = *ctx.this_value();
    if !crate::value_kind::is_object_like_value(&receiver) {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "called on a non-object receiver".to_string(),
        });
    }
    let string_arg = args.first().cloned().unwrap_or(Value::undefined());
    let limit_arg = args.get(1).cloned().unwrap_or(Value::undefined());

    // Step 4 — S = ? ToString(string).
    let s = coerce_to_jsstring_runtime(ctx, &string_arg, name)?;
    let s_units = s.to_utf16_vec(ctx.heap());
    let size = s_units.len();

    // Step 5 — C = ? SpeciesConstructor(rx, %RegExp%).
    let default_ctor = {
        let interp = &ctx.cx.interp;
        crate::object::get(interp.global_this, &interp.gc_heap, "RegExp").ok_or_else(|| {
            crate::NativeError::TypeError {
                name,
                reason: "%RegExp% intrinsic missing".to_string(),
            }
        })?
    };
    let c = species_constructor_runtime(ctx, &receiver, &default_ctor, name)?;

    // Step 6-8 — flags = ToString(Get(rx, "flags")). newFlags appends
    // `y` so the splitter probes exactly the requested position each
    // step. `unicodeMatching` keys off `u` or `v`.
    let flags_val = get_property_runtime(ctx, &receiver, "flags", name)?;
    let flags_str = coerce_to_jsstring_runtime(ctx, &flags_val, name)?.to_lossy_string(ctx.heap());
    let unicode_matching = flags_str.contains('u') || flags_str.contains('v');
    let new_flags = if flags_str.contains('y') {
        flags_str.clone()
    } else {
        let mut combined = String::with_capacity(flags_str.len() + 1);
        combined.push_str(&flags_str);
        combined.push('y');
        combined
    };
    let new_flags_js = {
        JsString::from_str(&new_flags, ctx.heap_mut()).map_err(|_| {
            crate::NativeError::TypeError {
                name,
                reason: "out of memory".to_string(),
            }
        })?
    };

    // Step 9 — splitter = ? Construct(C, [rx, newFlags]).
    let splitter = {
        let mut ctor_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        ctor_args.push(receiver);
        ctor_args.push(Value::string(new_flags_js));
        let (interp, exec_ctx) = ctx.interp_mut_and_context();
        let exec_ctx = exec_ctx.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        interp
            .run_construct_sync(&exec_ctx, &c, c, ctor_args)
            .map_err(vm_err_to_native(name))?
    };

    // Step 13 — lim = limit === undefined ? 2^32 - 1 : ToUint32(limit).
    let lim: u32 = if matches!(limit_arg, Value::Undefined) {
        u32::MAX
    } else {
        let primitive = if crate::abstract_ops::is_primitive(&limit_arg) {
            limit_arg
        } else {
            let (interp, exec) = ctx.interp_mut_and_context();
            let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
                name,
                reason: "missing execution context".to_string(),
            })?;
            interp
                .evaluate_to_primitive(
                    &exec,
                    &limit_arg,
                    crate::abstract_ops::ToPrimitiveHint::Number,
                )
                .map_err(vm_err_to_native(name))?
        };
        let n = crate::number::to_number_value(&primitive, ctx.heap());
        if n.is_nan() {
            0
        } else {
            // ToUint32 modulo 2^32.
            let truncated = n.trunc();
            let modulo = truncated.rem_euclid(4_294_967_296.0);
            modulo as u32
        }
    };

    let mut out_elements: Vec<Value> = Vec::new();

    // Step 14 — lim == 0 short-circuits to an empty array.
    if lim == 0 {
        let arr = ctx
            .array_from_elements_with_roots(out_elements, &[&splitter], &[])
            .map_err(|_| crate::NativeError::TypeError {
                name,
                reason: "array allocation failed".to_string(),
            })?;
        return Ok(Value::array(arr));
    }

    // Step 16 — empty S: one probe; if exec yields a match, return
    // empty array, otherwise return `[S]`.
    if size == 0 {
        let z = regexp_exec_runtime(ctx, &splitter, &s, name)?;
        if !matches!(z, Value::Null) {
            let arr = ctx
                .array_from_elements_with_roots(out_elements, &[&splitter], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name,
                    reason: "array allocation failed".to_string(),
                })?;
            return Ok(Value::array(arr));
        }
        out_elements.push(Value::string(s));
        let arr = ctx
            .array_from_elements_with_roots(
                out_elements.iter().cloned(),
                &[&splitter],
                &[out_elements.as_slice()],
            )
            .map_err(|_| crate::NativeError::TypeError {
                name,
                reason: "array allocation failed".to_string(),
            })?;
        return Ok(Value::array(arr));
    }

    // Step 17-21 — main loop.
    let mut p: usize = 0;
    let mut q: usize = 0;
    while q < size {
        set_property_runtime(
            ctx,
            &splitter,
            "lastIndex",
            Value::Number(NumberValue::from_f64(q as f64)),
            name,
        )?;
        let z = regexp_exec_runtime(ctx, &splitter, &s, name)?;
        if matches!(z, Value::Null) {
            q = advance_string_index(&s_units, q, unicode_matching);
            continue;
        }
        let last_index_val = get_property_runtime(ctx, &splitter, "lastIndex", name)?;
        let e_raw = to_length_runtime(ctx, &last_index_val, name)? as usize;
        let e = e_raw.min(size);
        if e == p {
            q = advance_string_index(&s_units, q, unicode_matching);
            continue;
        }
        let part = JsString::from_utf16_units(&s_units[p..q], ctx.cx.interp.gc_heap_mut())
            .map_err(|_| crate::NativeError::TypeError {
                name,
                reason: "out of memory".to_string(),
            })?;
        out_elements.push(Value::string(part));
        if out_elements.len() as u32 == lim {
            let arr = ctx
                .array_from_elements_with_roots(
                    out_elements.iter().cloned(),
                    &[&splitter],
                    &[out_elements.as_slice()],
                )
                .map_err(|_| crate::NativeError::TypeError {
                    name,
                    reason: "array allocation failed".to_string(),
                })?;
            return Ok(Value::array(arr));
        }
        p = e;
        let length_val = get_property_runtime(ctx, &z, "length", name)?;
        let number_of_captures =
            (to_length_runtime(ctx, &length_val, name)? as usize).saturating_sub(1);
        for i in 1..=number_of_captures {
            let cap_key = i.to_string();
            let next_capture = get_property_runtime(ctx, &z, &cap_key, name)?;
            out_elements.push(next_capture);
            if out_elements.len() as u32 == lim {
                let arr = ctx
                    .array_from_elements_with_roots(
                        out_elements.iter().cloned(),
                        &[&splitter],
                        &[out_elements.as_slice()],
                    )
                    .map_err(|_| crate::NativeError::TypeError {
                        name,
                        reason: "array allocation failed".to_string(),
                    })?;
                return Ok(Value::array(arr));
            }
        }
        q = p;
    }

    // Step 22 — trailing slice from `p` to end.
    let tail = JsString::from_utf16_units(&s_units[p..size], ctx.cx.interp.gc_heap_mut()).map_err(
        |_| crate::NativeError::TypeError {
            name,
            reason: "out of memory".to_string(),
        },
    )?;
    out_elements.push(Value::string(tail));
    let arr = ctx
        .array_from_elements_with_roots(
            out_elements.iter().cloned(),
            &[&splitter],
            &[out_elements.as_slice()],
        )
        .map_err(|_| crate::NativeError::TypeError {
            name,
            reason: "array allocation failed".to_string(),
        })?;
    Ok(Value::array(arr))
}

/// §22.2.6.11.1 `GetSubstitution(matched, str, position, captures,
/// namedCaptures, replacementTemplate)`. Translates the `$$`, `$&`,
/// `` $` ``, `$'`, `$n`, `$nn`, `$<name>` substitution markers into
/// the rendered replacement UTF-16 buffer.
#[allow(clippy::too_many_arguments)]
fn get_substitution(
    ctx: &mut NativeCtx<'_>,
    matched: &[u16],
    str_units: &[u16],
    position: usize,
    captures: &[Option<JsString>],
    named_captures: Option<&Value>,
    template: &[u16],
    name: &'static str,
) -> Result<Vec<u16>, crate::NativeError> {
    let mut out: Vec<u16> = Vec::with_capacity(template.len());
    let match_length = matched.len();
    let str_length = str_units.len();
    let tail_position = (position + match_length).min(str_length);
    let m = captures.len();
    let mut i = 0;
    while i < template.len() {
        let c = template[i];
        if c != b'$' as u16 || i + 1 >= template.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let next = template[i + 1];
        match next {
            n if n == b'$' as u16 => {
                out.push(b'$' as u16);
                i += 2;
            }
            n if n == b'&' as u16 => {
                out.extend_from_slice(matched);
                i += 2;
            }
            n if n == b'`' as u16 => {
                out.extend_from_slice(&str_units[..position]);
                i += 2;
            }
            n if n == b'\'' as u16 => {
                out.extend_from_slice(&str_units[tail_position..]);
                i += 2;
            }
            n if (b'0' as u16..=b'9' as u16).contains(&n) => {
                // §22.2.6.11.1 step 11: parse one- or two-digit
                // group index. Use two digits when the result is a
                // valid (in-range, non-zero) capture index;
                // otherwise fall back to single digit. `$0` is
                // emitted literally per the spec table.
                let first = (n - b'0' as u16) as usize;
                let second = template
                    .get(i + 2)
                    .copied()
                    .filter(|&c| (b'0' as u16..=b'9' as u16).contains(&c))
                    .map(|c| (c - b'0' as u16) as usize);
                let (idx, consumed) = match (first, second) {
                    (0, None) => (None, 2),
                    (0, Some(0)) => (None, 3),
                    (0, Some(d)) if d > 0 && d <= m => (Some(d), 3),
                    (0, Some(_)) => (None, 2),
                    (a, Some(b)) => {
                        let two = a * 10 + b;
                        if two > 0 && two <= m {
                            (Some(two), 3)
                        } else if a > 0 && a <= m {
                            (Some(a), 2)
                        } else {
                            (None, 2)
                        }
                    }
                    (a, None) => {
                        if a > 0 && a <= m {
                            (Some(a), 2)
                        } else {
                            (None, 2)
                        }
                    }
                };
                if let Some(group_index) = idx {
                    if let Some(Some(cap)) = captures.get(group_index - 1) {
                        out.extend_from_slice(&cap.to_utf16_vec(ctx.heap()));
                    }
                    // Undefined capture group → emit nothing.
                } else {
                    // Out-of-range or `$0` → emit verbatim.
                    out.push(c);
                    for k in 1..consumed {
                        out.push(template[i + k]);
                    }
                }
                i += consumed;
            }
            n if n == b'<' as u16 => {
                // §22.2.6.11.1 step 12 — named capture reference.
                let mut end = i + 2;
                while end < template.len() && template[end] != b'>' as u16 {
                    end += 1;
                }
                if end >= template.len() {
                    // No closing `>`; emit literally.
                    out.push(c);
                    i += 1;
                    continue;
                }
                let group_name_units = &template[i + 2..end];
                let group_name = String::from_utf16_lossy(group_name_units);
                match named_captures {
                    None => {
                        // No named groups at all → emit literally
                        // including the `<…>` payload.
                        out.push(c);
                        for k in 1..=(end - i) {
                            out.push(template[i + k]);
                        }
                        i = end + 1;
                        continue;
                    }
                    Some(nc) => {
                        let val = get_property_runtime(ctx, nc, &group_name, name)?;
                        if !matches!(val, Value::Undefined) {
                            let coerced = coerce_to_jsstring_runtime(ctx, &val, name)?;
                            out.extend_from_slice(&coerced.to_utf16_vec(ctx.heap()));
                        }
                        i = end + 1;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
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
pub fn load_property(re: &JsRegExp, gc_heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    match name {
        "source" => {
            let raw = re.source(gc_heap);
            let escaped = escape_regexp_pattern(&raw);
            match JsString::from_str(&escaped, gc_heap) {
                Ok(s) => Value::string(s),
                Err(_) => Value::undefined(),
            }
        }
        "flags" => match JsString::from_str(&re.flags(gc_heap).to_js_string(), gc_heap) {
            Ok(s) => Value::string(s),
            Err(_) => Value::undefined(),
        },
        "hasIndices" => Value::boolean(re.flags(gc_heap).has_indices),
        "global" => Value::boolean(re.flags(gc_heap).global),
        "ignoreCase" => Value::boolean(re.flags(gc_heap).ignore_case),
        "multiline" => Value::boolean(re.flags(gc_heap).multiline),
        "dotAll" => Value::boolean(re.flags(gc_heap).dot_all),
        "unicode" => Value::boolean(re.flags(gc_heap).unicode),
        "sticky" => Value::boolean(re.flags(gc_heap).sticky),
        "unicodeSets" => Value::boolean(re.flags(gc_heap).unicode_sets),
        "lastIndex" => re.last_index_value(gc_heap),
        _ => Value::undefined(),
    }
}

/// Mutate a JS-visible property on a `RegExp`. Currently only
/// `lastIndex` is writable; everything else is silently ignored
/// (foundation: the spec marks accessors non-writable, so a real
/// `TypeError` belongs in a later strict-mode slice).
pub fn store_property(re: &JsRegExp, gc_heap: &mut otter_gc::GcHeap, name: &str, value: Value) {
    if name == "lastIndex" && re.last_index_writable(gc_heap) {
        re.set_last_index_value(gc_heap, value);
    }
}

/// §22.2.3.2.4 `EscapeRegExpPattern(src, flags)` — emit a string
/// that, when re-parsed as a Pattern, matches the same set of
/// strings as the original. Empty source maps to `"(?:)"`; bare
/// `/` and line terminators are escaped; everything else passes
/// through. Shared by `RegExp.prototype.source` /
/// `RegExp.prototype.toString` / direct property loads.
///
/// <https://tc39.es/ecma262/#sec-escaperegexppattern>
pub fn escape_regexp_pattern(raw: &str) -> String {
    if raw.is_empty() {
        return "(?:)".to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut in_class = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push('\\');
                if let Some(&next) = chars.peek() {
                    out.push(next);
                    chars.next();
                }
            }
            '[' => {
                in_class = true;
                out.push('[');
            }
            ']' => {
                in_class = false;
                out.push(']');
            }
            '/' if !in_class => {
                out.push('\\');
                out.push('/');
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(pattern: &str, flags: &str, gc_heap: &mut otter_gc::GcHeap) -> Value {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        Value::RegExp(JsRegExp::compile(gc_heap, &units, flags).unwrap())
    }

    fn call(method: &str, recv: &Value, args: &[Value], gc_heap: &mut otter_gc::GcHeap) -> Value {
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: recv,
            args,
            gc_heap,
            allocation_roots: &[],
        })
        .unwrap()
    }

    #[test]
    fn test_returns_boolean() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("ab+c", "", &mut gc_heap);
        let text = Value::string(JsString::from_str("abbbc", &mut gc_heap).unwrap());
        assert_eq!(
            call("test", &re, &[text], &mut gc_heap),
            Value::Boolean(true)
        );
        let no = Value::string(JsString::from_str("xy", &mut gc_heap).unwrap());
        assert_eq!(
            call("test", &re, &[no], &mut gc_heap),
            Value::Boolean(false)
        );
    }

    #[test]
    fn exec_returns_array_or_null() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("(a)(b)", "", &mut gc_heap);
        let text = Value::string(JsString::from_str("ab", &mut gc_heap).unwrap());
        let r = call("exec", &re, &[text], &mut gc_heap);
        match r {
            Value::Array(arr) => {
                assert_eq!(crate::array::len(arr, &gc_heap), 3);
                assert_eq!(
                    crate::array::get(arr, &gc_heap, 0).display_string(&gc_heap),
                    "ab"
                );
                assert_eq!(
                    crate::array::get(arr, &gc_heap, 1).display_string(&gc_heap),
                    "a"
                );
                assert_eq!(
                    crate::array::get(arr, &gc_heap, 2).display_string(&gc_heap),
                    "b"
                );
            }
            _ => panic!("expected array"),
        }
        let miss = call(
            "exec",
            &re,
            &[Value::String(
                JsString::from_str("zz", &mut gc_heap).unwrap(),
            )],
            &mut gc_heap,
        );
        assert_eq!(miss, Value::Null);
    }

    #[test]
    fn exec_result_arrays_use_intrinsic_rooted_allocation() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("(?<first>a)(b)", "d", &mut gc_heap);
        let text = Value::string(JsString::from_str("ab", &mut gc_heap).unwrap());
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = make("a", "g", &mut gc_heap);
        let text = Value::string(JsString::from_str("abab", &mut gc_heap).unwrap());
        // First call → match at 0, lastIndex moves to 1.
        let r1 = call("exec", &re, std::slice::from_ref(&text), &mut gc_heap);
        match (&r1, &re) {
            (Value::Array(arr), Value::RegExp(rx)) => {
                assert_eq!(
                    crate::array::get(*arr, &gc_heap, 0).display_string(&gc_heap),
                    "a"
                );
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re = JsRegExp::compile(
            &mut gc_heap,
            &"ab+c".encode_utf16().collect::<Vec<_>>(),
            "gi",
        )
        .unwrap();
        let src = load_property(&re, &mut gc_heap, "source");
        assert_eq!(src.display_string(&gc_heap), "ab+c");
        let flags = load_property(&re, &mut gc_heap, "flags");
        assert_eq!(flags.display_string(&gc_heap), "gi");
        assert_eq!(
            load_property(&re, &mut gc_heap, "global"),
            Value::Boolean(true)
        );
        assert_eq!(
            load_property(&re, &mut gc_heap, "ignoreCase"),
            Value::Boolean(true)
        );
        assert_eq!(
            load_property(&re, &mut gc_heap, "multiline"),
            Value::Boolean(false)
        );
    }

    #[test]
    fn last_index_writable() {
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
            load_property(&re, &mut gc_heap, "lastIndex"),
            Value::Number(NumberValue::from_i32(-3))
        );
        // String writes are observable, and execution coerces them
        // numerically when needed.
        let written = JsString::from_str("9", &mut gc_heap).unwrap();
        store_property(&re, &mut gc_heap, "lastIndex", Value::String(written));
        assert_eq!(
            load_property(&re, &mut gc_heap, "lastIndex"),
            Value::String(written)
        );
        assert_eq!(re.last_index(&gc_heap), 9);
        // Non-lastIndex names are silently ignored.
        let nope = Value::string(JsString::from_str("nope", &mut gc_heap).unwrap());
        store_property(&re, &mut gc_heap, "source", nope);
    }
}
