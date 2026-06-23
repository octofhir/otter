//! `RegExp.prototype.*` shared native helpers.
//!
//! Method dispatch is installed through the bootstrap native surface;
//! property reads (`.source`, `.flags`, `.global`, `.lastIndex`, …)
//! are handled at the `Op::LoadProperty` site since they don't go
//! through `CallMethodValue`.
//!
//! # Contents
//! - Native helpers shared by the bootstrap `RegExp.prototype`
//!   methods and string regex overloads.
//! - [`load_property`] — getter dispatch for non-method members.
//!
//! # Invariants
//! - Receivers are validated as `Value::RegExp`; non-regex native
//!   method receivers raise `TypeError`.
//! - `exec` and `test` honour the `g` and `y` flag semantics — both
//!   read and update `lastIndex`.
//! - `lastIndex` is clamped to `[0, len]` before any match attempt
//!   so a manual `re.lastIndex = -1` doesn't underflow.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp.prototype.exec>

use otter_gc::raw::RawGc;

use crate::array::JsArray;
use crate::regexp::JsRegExp;
use crate::runtime_cx::NativeCtx;
use crate::string::JsString;
use crate::{NativeError, Value, VmError};

const REGEXP_EXEC_NAME: &str = "RegExp.prototype.exec";

fn native_type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name,
        reason: reason.into(),
    }
}

fn vm_shape_error_to_native(err: VmError) -> NativeError {
    match err {
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => native_type_error(
            REGEXP_EXEC_NAME,
            format!(
                "out of memory: requested {requested_bytes} bytes with heap limit {heap_limit_bytes} bytes"
            ),
        ),
        _ => native_type_error(REGEXP_EXEC_NAME, "property shape update failed"),
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
pub(crate) fn exec_once_native(
    re: &JsRegExp,
    receiver: &Value,
    text: JsString,
    ctx: &mut NativeCtx<'_>,
    slice_roots: &[&[Value]],
) -> Result<Value, NativeError> {
    let units = text.to_utf16_vec(ctx.heap());
    let len = units.len();
    let flags = re.flags(ctx.heap());
    // §22.2.7.2 step 4 — `lastIndex = ? ToLength(? Get(R, "lastIndex"))`.
    // The read is observable (a user `lastIndex` getter fires) even
    // when the regex is neither global nor sticky.
    let last_index_val = get_property_runtime(ctx, receiver, "lastIndex", REGEXP_EXEC_NAME)?;
    let mut start = to_length_runtime(ctx, &last_index_val, REGEXP_EXEC_NAME)? as usize;
    // step 8 — a non-global, non-sticky match always starts at 0.
    if !flags.global && !flags.sticky {
        start = 0;
    } else if start > len {
        // step 12.a.i — out-of-range start resets `lastIndex` (the
        // observable `Set` throws on a non-writable `lastIndex`).
        set_property_runtime(
            ctx,
            receiver,
            "lastIndex",
            Value::number_i32(0),
            REGEXP_EXEC_NAME,
        )?;
        return Ok(Value::null());
    }
    let m = re.find_one_from_utf16(ctx.heap(), &units, start);
    let m = match m {
        Some(m) => m,
        None => {
            if flags.global || flags.sticky {
                set_property_runtime(
                    ctx,
                    receiver,
                    "lastIndex",
                    Value::number_i32(0),
                    REGEXP_EXEC_NAME,
                )?;
            }
            return Ok(Value::null());
        }
    };
    if flags.sticky && m.range.start != start {
        set_property_runtime(
            ctx,
            receiver,
            "lastIndex",
            Value::number_i32(0),
            REGEXP_EXEC_NAME,
        )?;
        return Ok(Value::null());
    }
    if flags.global || flags.sticky {
        set_property_runtime(
            ctx,
            receiver,
            "lastIndex",
            Value::number_f64(m.range.end as f64),
            REGEXP_EXEC_NAME,
        )?;
    }

    Ok(Value::array(build_match_result_native(
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
/// out of a match record. Used by `RegExp.prototype.exec` and
/// reused by `String.prototype.match` / `.matchAll` so both surfaces
/// produce identical shapes (full match + capture slots, plus
/// `index` / `input` / `groups` / optionally `indices`).
pub(crate) fn build_match_result_native(
    m: &crate::regexp::engine::Match,
    units: &[u16],
    input: JsString,
    has_indices: bool,
    ctx: &mut NativeCtx<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsArray, NativeError> {
    let full = JsString::from_utf16_units(&units[m.range.clone()], ctx.heap_mut())?;
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    out.push(Value::string(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&units[r.clone()], ctx.heap_mut())?;
                out.push(Value::string(s));
            }
            None => out.push(Value::undefined()),
        }
    }
    let input_value = Value::string(input);
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
        Value::number_i32(m.range.start as i32),
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
            .map_err(vm_shape_error_to_native)?;
        for (name, range) in named_iter {
            let value = match range {
                Some(r) => Value::string(JsString::from_utf16_units(&units[r], ctx.heap_mut())?),
                None => Value::undefined(),
            };
            ctx.set_property_with_roots(groups_obj, name, value, &roots, &slices)
                .map_err(vm_shape_error_to_native)?;
        }
        crate::array::set_named_property(arr, ctx.heap_mut(), "groups", Value::object(groups_obj))?;
    } else {
        crate::array::set_named_property(arr, ctx.heap_mut(), "groups", Value::undefined())?;
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
                None => indices_elems.push(Value::undefined()),
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
                .map_err(vm_shape_error_to_native)?;
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
                    .map_err(vm_shape_error_to_native)?;
            }
            crate::array::set_named_property(
                indices_arr,
                ctx.heap_mut(),
                "groups",
                Value::object(g_obj),
            )?;
        } else {
            crate::array::set_named_property(
                indices_arr,
                ctx.heap_mut(),
                "groups",
                Value::undefined(),
            )?;
        }
        crate::array::set_named_property(
            arr,
            ctx.heap_mut(),
            "indices",
            Value::array(indices_arr),
        )?;
    }
    Ok(arr)
}

fn pair_array_native(
    start: usize,
    end: usize,
    ctx: &mut NativeCtx<'_>,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::array(ctx.array_from_elements_with_roots(
        [
            Value::number_i32(start as i32),
            Value::number_i32(end as i32),
        ],
        value_roots,
        slice_roots,
    )?))
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
    let name = "RegExp.prototype[@@match]";
    let receiver = *ctx.this_value();
    // §22.2.6.8 step 1 — the receiver need only be an Object; the
    // matching itself flows through the observable `exec` protocol.
    if !receiver.is_object_type() {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "called on a non-object receiver".to_string(),
        });
    }
    // Step 3 — S = ? ToString(string).
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let text = coerce_to_jsstring_runtime(ctx, &arg, name)?;
    // Step 4-5 — flags = ? ToString(? Get(rx, "flags")); global = "g".
    let flags_val = get_property_runtime(ctx, &receiver, "flags", name)?;
    let flags_str = coerce_to_jsstring_runtime(ctx, &flags_val, name)?.to_lossy_string(ctx.heap());
    let global = flags_str.contains('g');
    if !global {
        // Step 6 — RegExpExec(rx, S).
        return regexp_exec_runtime(ctx, &receiver, text, name);
    }
    // Step 7 — global: reset lastIndex, then loop RegExpExec.
    let full_unicode = flags_str.contains('u') || flags_str.contains('v');
    set_property_runtime(ctx, &receiver, "lastIndex", Value::number_i32(0), name)?;
    let text_units = text.to_utf16_vec(ctx.heap());
    // Fast path: a real RegExp whose `exec` is still the native intrinsic. The
    // observable per-match protocol would build a full result object, read its
    // `"0"` element back out, and round-trip `lastIndex` through the property
    // machinery for every match — none of which is observable for a native
    // RegExp (`exec` unmodified, `lastIndex` a non-configurable data slot). So
    // collect the leftmost non-overlapping matches in one pass (which already
    // applies the global / empty-match advancement) and slice the substrings
    // directly. `flags` was read above (preserving that observable) and
    // `lastIndex` stays 0, exactly as the terminal null-returning exec leaves it.
    //
    // Guard order matters: test `as_regexp()` (no observable side effect)
    // before reading `exec`, so a non-RegExp receiver with an instrumented
    // `exec` getter is left entirely to the observable slow path. Sticky (`y`)
    // is excluded — the one-pass leftmost scan does not enforce the
    // anchored-at-`lastIndex` semantics sticky requires.
    if let Some(re) = receiver.as_regexp()
        && !flags_str.contains('y')
    {
        let exec_fn = get_property_runtime(ctx, &receiver, "exec", name)?;
        if let Some(native) = exec_fn.as_native_function()
            && native.is_static_native(ctx.heap(), crate::bootstrap_regexp::proto_exec)
        {
            let found = re.find_from_utf16(ctx.heap(), &text_units, 0);
            if found.is_empty() {
                return Ok(Value::null());
            }
            let text_value = Value::string(text);
            let mut out: Vec<Value> = Vec::with_capacity(found.len());
            for m in &found {
                let slice = &text_units[m.range.start..m.range.end];
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    for val in &out {
                        val.trace_value_slots(visitor);
                    }
                    receiver.trace_value_slots(visitor);
                    text_value.trace_value_slots(visitor);
                };
                let mstr = JsString::from_utf16_units_with_roots(slice, ctx.heap_mut(), &mut visit)
                    .map_err(|_| crate::NativeError::TypeError {
                        name,
                        reason: "out of memory".to_string(),
                    })?;
                out.push(Value::string(mstr));
            }
            let arr = ctx
                .array_from_elements_with_roots(
                    out.iter().cloned(),
                    &[&receiver, &text_value],
                    &[out.as_slice()],
                )
                .map_err(|_| crate::NativeError::TypeError {
                    name,
                    reason: "array allocation failed".to_string(),
                })?;
            return Ok(Value::array(arr));
        }
    }
    let mut matches_out: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_runtime(ctx, &receiver, text, name)?;
        if result.is_null() {
            break;
        }
        // matchStr = ? ToString(? Get(result, "0")).
        let match_val = get_property_runtime(ctx, &result, "0", name)?;
        let match_str = coerce_to_jsstring_runtime(ctx, &match_val, name)?;
        let is_empty = match_str.is_empty();
        matches_out.push(Value::string(match_str));
        if is_empty {
            // Empty match — AdvanceStringIndex via the observable
            // lastIndex so a custom `exec` / accessor sees the write.
            let last_index_val = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
            let this_index = to_length_runtime(ctx, &last_index_val, name)? as usize;
            let next_index = advance_string_index(&text_units, this_index, full_unicode);
            set_property_runtime(
                ctx,
                &receiver,
                "lastIndex",
                Value::number_f64(next_index as f64),
                name,
            )?;
        }
    }
    if matches_out.is_empty() {
        return Ok(Value::null());
    }
    let text_value = Value::string(text);
    let arr = ctx
        .array_from_elements_with_roots(
            matches_out.iter().cloned(),
            &[&receiver, &text_value],
            &[matches_out.as_slice()],
        )
        .map_err(|_| crate::NativeError::TypeError {
            name,
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
    let name = "RegExp.prototype[@@search]";
    let receiver = *ctx.this_value();
    // §22.2.6.10 step 1 — any Object receiver; matching flows through
    // the observable `exec` protocol and preserves `lastIndex`.
    if !receiver.is_object_type() {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "called on a non-object receiver".to_string(),
        });
    }
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let text = coerce_to_jsstring_runtime(ctx, &arg, name)?;
    // Steps 4-5 — save `lastIndex`, reset to 0 only if it differs.
    let previous = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
    let zero = Value::number_i32(0);
    if !crate::abstract_ops::same_value(&previous, &zero, ctx.heap()) {
        set_property_runtime(ctx, &receiver, "lastIndex", zero, name)?;
    }
    // Step 6 — RegExpExec(rx, S).
    let result = regexp_exec_runtime(ctx, &receiver, text, name)?;
    // Steps 7-8 — restore `lastIndex` if the exec changed it.
    let current = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
    if !crate::abstract_ops::same_value(&current, &previous, ctx.heap()) {
        set_property_runtime(ctx, &receiver, "lastIndex", previous, name)?;
    }
    // Steps 9-10 — null → -1, else Get(result, "index").
    if result.is_null() {
        return Ok(Value::number_i32(-1));
    }
    get_property_runtime(ctx, &result, "index", name)
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

fn vm_err_to_native<'a>(
    interp: &'a crate::Interpreter,
    name: &'static str,
) -> impl Fn(crate::VmError) -> crate::NativeError + 'a {
    move |err| match err {
        crate::VmError::Uncaught => {
            let value = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                _ => Default::default(),
            };
            crate::NativeError::Thrown {
                name,
                message: value.into(),
            }
        }
        crate::VmError::TypeError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            crate::NativeError::TypeError {
                name,
                reason: message.into(),
            }
        }
        crate::VmError::RangeError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            crate::NativeError::RangeError {
                name,
                reason: message.into(),
            }
        }
        crate::VmError::SyntaxError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            crate::NativeError::SyntaxError {
                name,
                reason: message.into(),
            }
        }
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
        .map_err(vm_err_to_native(interp, name))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
            interp
                .run_callable_sync(&exec, &getter, *receiver, args)
                .map_err(vm_err_to_native(interp, name))
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
    // Resolve accessor setters along the prototype chain first so a
    // custom `set lastIndex` (e.g. a `@@split` splitter) fires; only
    // ordinary data targets fall through to the data write.
    if let Some(obj) = receiver.as_object() {
        match crate::object::resolve_set(obj, interp.gc_heap(), key) {
            crate::object::SetOutcome::InvokeSetter { setter } => {
                if !interp.is_callable_runtime(&setter) {
                    return Err(read_only_set_error(name, key));
                }
                let args: smallvec::SmallVec<[Value; 8]> = smallvec::smallvec![value];
                interp
                    .run_callable_sync(&exec, &setter, *receiver, args)
                    .map_err(vm_err_to_native(interp, name))?;
                return Ok(());
            }
            crate::object::SetOutcome::Reject { .. } => {
                return Err(read_only_set_error(name, key));
            }
            crate::object::SetOutcome::AssignData => {}
            // The value-level funnel below dispatches exotic
            // [[Set]] overrides itself.
            crate::object::SetOutcome::ExoticParent { .. } => {}
        }
    }
    let ok = interp
        .ordinary_set_data_value(
            &exec,
            *receiver,
            &crate::VmPropertyKey::String(key),
            value,
            *receiver,
            0,
        )
        .map_err(vm_err_to_native(interp, name))?;
    // `Set(O, P, V, true)` — a `[[Set]]` returning false (e.g. a
    // non-writable `lastIndex`) is a TypeError, not a silent no-op.
    if !ok {
        return Err(read_only_set_error(name, key));
    }
    Ok(())
}

/// The §10.1.9.2 / §7.3.4 `Set(O, P, V, true)` failure: a rejected
/// write (non-writable data, accessor without a setter) is a TypeError.
fn read_only_set_error(name: &'static str, key: &str) -> crate::NativeError {
    crate::NativeError::TypeError {
        name,
        reason: format!("Cannot assign to read only property '{key}'"),
    }
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
    if value.is_symbol() {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "cannot convert a Symbol to a string".to_string(),
        });
    }
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s);
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
            .map_err(vm_err_to_native(interp, name))?
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
    // §7.1.20 ToLength = ToIntegerOrInfinity(? ToNumber(arg)) clamped to
    // [0, 2^53-1]. ToNumber throws for a Symbol / BigInt and runs (and so
    // can re-throw from) a `valueOf`, so the fallible coercion is required —
    // an infallible NaN cast would silently swallow those abrupt completions.
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
    let n = crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, value)
        .map_err(vm_err_to_native(ctx.cx.interp, name))?
        .as_f64();
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
            .map_err(vm_err_to_native(interp, name))?
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
    s: JsString,
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
        args.push(Value::string(s));
        let result = interp
            .run_callable_sync(&exec_ctx, &exec_fn, *rx, args)
            .map_err(vm_err_to_native(interp, name))?;
        if !result.is_null() && !result.is_object_type() {
            return Err(crate::NativeError::TypeError {
                name,
                reason: "exec did not return an Object or null".to_string(),
            });
        }
        return Ok(result);
    }
    // Fall back to builtin exec only when `rx` is actually a RegExp.
    if let Some(re) = rx.as_regexp() {
        return exec_once_native(&re, rx, s, ctx, &[]);
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
    input: JsString,
    global: bool,
    full_unicode: bool,
) -> Result<Option<Value>, crate::VmError> {
    let name = "RegExp String Iterator.next";
    let mut ctx = NativeCtx::new_with_call_info_and_context(
        interp,
        crate::NativeCallInfo::call(*matcher),
        Some(context),
    );
    let result = regexp_exec_runtime(&mut ctx, matcher, input, name)
        .map_err(|e| crate::native_to_vm_error(ctx.interp_mut(), e))?;
    if result.is_null() {
        return Ok(None);
    }
    if global {
        let matched_val = get_property_runtime(&mut ctx, &result, "0", name)
            .map_err(|e| crate::native_to_vm_error(ctx.interp_mut(), e))?;
        let matched_str = coerce_to_jsstring_runtime(&mut ctx, &matched_val, name)
            .map_err(|e| crate::native_to_vm_error(ctx.interp_mut(), e))?;
        if matched_str.is_empty() {
            let li_val = get_property_runtime(&mut ctx, matcher, "lastIndex", name)
                .map_err(|e| crate::native_to_vm_error(ctx.interp_mut(), e))?;
            let this_index = to_length_runtime(&mut ctx, &li_val, name)
                .map_err(|e| crate::native_to_vm_error(ctx.interp_mut(), e))?
                as usize;
            let input_units = input.to_utf16_vec(ctx.heap());
            let next_index = advance_string_index(&input_units, this_index, full_unicode);
            set_property_runtime(
                &mut ctx,
                matcher,
                "lastIndex",
                Value::number_f64(next_index as f64),
                name,
            )
            .map_err(|e| crate::native_to_vm_error(ctx.interp_mut(), e))?;
        }
    }
    Ok(Some(result))
}

/// Engine cap on string length in UTF-16 units (1 GiB of u16 —
/// matches the same order of magnitude as other engines' limits and
/// keeps a single string allocation well under the heap cap).
const MAX_STRING_UNITS: usize = 1 << 29;

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
    if !receiver.is_object_type() {
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
        set_property_runtime(ctx, &receiver, "lastIndex", Value::number_i32(0), name)?;
    }

    // Fast path: a global, non-sticky, pristine RegExp with the native `exec`
    // and a literal (`$`-free) string replacement. The replacement reads neither
    // the matched text nor captures, and `lastIndex` was just reset to a plain
    // `0`, so the per-match exec protocol and substitution machinery have no
    // observable effect: collect the matches in one engine pass and stitch the
    // source segments and literal template directly. Restricted to global so a
    // non-global `lastIndex` read (its `ToLength` may fire a `valueOf`) stays
    // observable, and to a regex with no own-property expando so an overridden
    // `unicode` / `flags` cannot change the empty-match advancement the engine
    // pass bakes in from the compiled flags. The whole loop runs on host
    // buffers; only the final string allocation touches the GC.
    if global
        && let Some(re) = receiver.as_regexp()
        && !flags_str.contains('y')
        && !functional_replace
        && re.expando(ctx.heap()).is_none()
    {
        let template = replacement_template.expect("non-functional path has a template");
        let template_units = template.to_utf16_vec(ctx.heap());
        if !template_units.contains(&0x24) {
            let exec_fn = get_property_runtime(ctx, &receiver, "exec", name)?;
            if exec_fn
                .as_native_function()
                .is_some_and(|nf| nf.is_static_native(ctx.heap(), crate::bootstrap_regexp::proto_exec))
            {
                let found = re.find_from_utf16(ctx.heap(), &s_units, 0);
                let mut accumulated: Vec<u16> = Vec::new();
                let mut next_source_position: usize = 0;
                for m in &found {
                    let position = m.range.start;
                    let match_length = m.range.end - m.range.start;
                    if position >= next_source_position {
                        let projected = accumulated
                            .len()
                            .saturating_add(position - next_source_position)
                            .saturating_add(template_units.len());
                        if projected > MAX_STRING_UNITS {
                            return Err(crate::NativeError::RangeError {
                                name,
                                reason: "Invalid string length".to_string(),
                            });
                        }
                        accumulated.extend_from_slice(&s_units[next_source_position..position]);
                        accumulated.extend_from_slice(&template_units);
                        next_source_position = position + match_length;
                    }
                }
                if next_source_position < length_s {
                    accumulated.extend_from_slice(&s_units[next_source_position..]);
                }
                return Ok(Value::string(
                    JsString::from_utf16_units(&accumulated, ctx.heap_mut()).map_err(|_| {
                        crate::NativeError::TypeError {
                            name,
                            reason: "out of memory".to_string(),
                        }
                    })?,
                ));
            }
        }
    }

    // Step 10-12 — collect results.
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_runtime(ctx, &receiver, s, name)?;
        if result.is_null() {
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
                Value::number_f64(next_index as f64),
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
            if cap_val.is_undefined() {
                captures.push(None);
            } else {
                captures.push(Some(coerce_to_jsstring_runtime(ctx, &cap_val, name)?));
            }
        }

        let named_captures = get_property_runtime(ctx, result, "groups", name)?;
        let named_captures_obj = if named_captures.is_undefined() {
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
            replacer_args.push(Value::number_f64(position as f64));
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
                        Value::undefined(),
                        replacer_args,
                    )
                    .map_err(vm_err_to_native(interp, name))?
            };
            let raw_str = coerce_to_jsstring_runtime(ctx, &raw, name)?;
            raw_str.to_utf16_vec(ctx.heap())
        } else {
            // §22.2.6.11 step 14.n.i — the non-functional path coerces a
            // present `groups` with ToObject before GetSubstitution. `null`
            // (from a monkey-patched `exec`) throws; a primitive is passed
            // through, since the `$<name>` lookups read it via the full
            // [[Get]], which boxes primitives the same way ToObject would.
            let named_captures_coerced = match named_captures_obj {
                Some(nc) if nc.is_nullish() => {
                    return Err(crate::NativeError::TypeError {
                        name,
                        reason: "named capture groups is not coercible to an Object".to_string(),
                    });
                }
                other => other,
            };
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
                named_captures_coerced.as_ref(),
                &template,
                name,
            )?
        };

        if position >= next_source_position {
            // Engine string-length cap — an unbounded global replace
            // (huge template x many matches) must surface a
            // catchable RangeError instead of asking the host for
            // tens of GB and getting OOM-killed.
            let projected = accumulated
                .len()
                .saturating_add(position - next_source_position)
                .saturating_add(replacement.len());
            if projected > MAX_STRING_UNITS {
                return Err(crate::NativeError::RangeError {
                    name,
                    reason: "Invalid string length".to_string(),
                });
            }
            accumulated.extend_from_slice(&s_units[next_source_position..position]);
            accumulated.extend_from_slice(&replacement);
            next_source_position = position + match_length;
        }
    }

    if next_source_position < length_s {
        accumulated.extend_from_slice(&s_units[next_source_position..]);
    }

    Ok(Value::string(
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
    sym: crate::symbol::JsSymbol,
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
            &crate::VmPropertyKey::Symbol(sym),
            0,
        )
        .map_err(vm_err_to_native(interp, name))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
            interp
                .run_callable_sync(&exec, &getter, *receiver, args)
                .map_err(vm_err_to_native(interp, name))
        }
    }
}

/// §7.3.21 `SpeciesConstructor(O, defaultConstructor)`. Resolves the
/// constructor to use when an algorithm needs to materialise a new
/// instance derived from `O`. Returns the default when `constructor`
/// is absent / undefined, throws TypeError on the spec-mandated
/// invalid shapes, and otherwise hands back `Symbol.species` (or the
/// constructor itself when species is absent / nullish).
pub(crate) fn species_constructor_runtime(
    ctx: &mut NativeCtx<'_>,
    obj: &Value,
    default_ctor: &Value,
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    let c = get_property_runtime(ctx, obj, "constructor", name)?;
    if c.is_undefined() {
        return Ok(*default_ctor);
    }
    if !c.is_object_type() {
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
    let s = get_symbol_property_runtime(ctx, &c, species_sym, name)?;
    // §7.3.20 step 6 — `undefined`/`null` @@species falls back to the
    // default constructor, not the resolved `constructor` value.
    if s.is_nullish() {
        return Ok(*default_ctor);
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
    if !receiver.is_object_type() {
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
            .map_err(vm_err_to_native(interp, name))?
    };

    // Step 2.f — snapshot rx.lastIndex into matcher.
    let last_index_val = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
    let last_index = to_length_runtime(ctx, &last_index_val, name)? as f64;
    set_property_runtime(
        ctx,
        &matcher,
        "lastIndex",
        Value::number_f64(last_index),
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
    if !receiver.is_object_type() {
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
            .map_err(vm_err_to_native(interp, name))?
    };

    // Step 13 — lim = limit === undefined ? 2^32 - 1 : ToUint32(limit).
    let lim: u32 = if limit_arg.is_undefined() {
        u32::MAX
    } else {
        // ToUint32 runs ToNumber, which throws for a Symbol / BigInt
        // limit rather than coercing it to zero.
        let exec =
            ctx.execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
        let n = crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, &limit_arg)
            .map_err(vm_err_to_native(ctx.cx.interp, name))?
            .as_f64();
        if n.is_nan() {
            0
        } else {
            n.trunc().rem_euclid(4_294_967_296.0) as u32
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
        let z = regexp_exec_runtime(ctx, &splitter, s, name)?;
        if !z.is_null() {
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
            Value::number_f64(q as f64),
            name,
        )?;
        let z = regexp_exec_runtime(ctx, &splitter, s, name)?;
        if z.is_null() {
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
                if named_captures.is_none() {
                    // Without named captures, `$<` is a two-code-unit literal
                    // substitution. The remaining template text is still
                    // scanned, so `$<42$1>` can substitute `$1`.
                    out.push(c);
                    out.push(n);
                    i += 2;
                    continue;
                }
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
                if let Some(nc) = named_captures {
                    let val = get_property_runtime(ctx, nc, &group_name, name)?;
                    if !val.is_undefined() {
                        let coerced = coerce_to_jsstring_runtime(ctx, &val, name)?;
                        out.extend_from_slice(&coerced.to_utf16_vec(ctx.heap()));
                    }
                    i = end + 1;
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

/// Whether `name` is installed on `RegExp.prototype`.
#[must_use]
pub fn is_builtin_method(name: &str) -> bool {
    matches!(name, "exec" | "test" | "toString" | "compile")
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
    use crate::{Interpreter, NativeCallInfo};

    fn make(pattern: &str, flags: &str, interp: &mut Interpreter) -> Value {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        Value::regexp(JsRegExp::compile(interp.gc_heap_mut(), &units, flags).unwrap())
    }

    /// Minimal `<main>` context so `exec_once_native`'s observable
    /// `Get`/`Set(lastIndex)` ladder has an execution context to run on.
    fn empty_context() -> crate::ExecutionContext {
        use otter_bytecode::{BytecodeModule, Function, Instruction, SourceKind, SpanEntry};
        crate::ExecutionContext::from_module(BytecodeModule {
            module: "regexp-proto-test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 0,
                param_count: 0,
                length: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new(),
                spans: Vec::<SpanEntry>::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    fn call(method: &str, recv: &Value, args: &[Value], interp: &mut Interpreter) -> Value {
        let re = recv.as_regexp().unwrap();
        let context = empty_context();
        let mut ctx = NativeCtx::new_with_call_info_and_context(
            interp,
            NativeCallInfo::call(*recv),
            Some(&context),
        );
        let text = string_arg_to_jsstring_for_test(args, 0, &mut ctx).unwrap();
        match method {
            "exec" => exec_once_native(&re, recv, text, &mut ctx, &[]).unwrap(),
            "test" => Value::boolean(
                !exec_once_native(&re, recv, text, &mut ctx, &[])
                    .unwrap()
                    .is_null(),
            ),
            _ => panic!("unknown regexp test method {method}"),
        }
    }

    fn string_arg_to_jsstring_for_test(
        args: &[Value],
        index: usize,
        ctx: &mut NativeCtx<'_>,
    ) -> Result<JsString, NativeError> {
        let raw = args.get(index).cloned().unwrap_or(Value::undefined());
        if let Some(s) = raw.as_string(ctx.heap()) {
            return Ok(s);
        }
        Ok(JsString::from_str(
            &raw.display_string(ctx.heap()),
            ctx.heap_mut(),
        )?)
    }

    #[test]
    fn test_returns_boolean() {
        let mut interp = Interpreter::new();
        let re = make("ab+c", "", &mut interp);
        let text = Value::string(JsString::from_str("abbbc", interp.gc_heap_mut()).unwrap());
        assert_eq!(
            call("test", &re, &[text], &mut interp),
            Value::boolean(true)
        );
        let no = Value::string(JsString::from_str("xy", interp.gc_heap_mut()).unwrap());
        assert_eq!(call("test", &re, &[no], &mut interp), Value::boolean(false));
    }

    #[test]
    fn exec_returns_array_or_null() {
        let mut interp = Interpreter::new();
        let re = make("(a)(b)", "", &mut interp);
        let text = Value::string(JsString::from_str("ab", interp.gc_heap_mut()).unwrap());
        let r = call("exec", &re, &[text], &mut interp);
        let Some(arr) = r.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(arr, interp.gc_heap()), 3);
        assert_eq!(
            crate::array::get(arr, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "ab"
        );
        assert_eq!(
            crate::array::get(arr, interp.gc_heap(), 1).display_string(interp.gc_heap()),
            "a"
        );
        assert_eq!(
            crate::array::get(arr, interp.gc_heap(), 2).display_string(interp.gc_heap()),
            "b"
        );
        let miss = call(
            "exec",
            &re,
            &[Value::string(
                JsString::from_str("zz", interp.gc_heap_mut()).unwrap(),
            )],
            &mut interp,
        );
        assert!(miss.is_null());
    }

    #[test]
    fn exec_result_arrays_use_native_rooted_allocation() {
        let mut interp = Interpreter::new();
        let re = make("(?<first>a)(b)", "d", &mut interp);
        let text = Value::string(JsString::from_str("ab", interp.gc_heap_mut()).unwrap());
        let before = interp.gc_heap().stats().new_allocated_bytes;
        let result = call("exec", &re, std::slice::from_ref(&text), &mut interp);
        let after = interp.gc_heap().stats().new_allocated_bytes;

        assert!(
            after > before,
            "RegExp exec result arrays, groups, and indices should allocate through native roots"
        );
        let Some(arr) = result.as_array() else {
            panic!("expected RegExp exec result array");
        };
        assert!(
            crate::array::get_named_property(arr, interp.gc_heap(), "indices")
                .is_some_and(|v| v.is_array())
        );
        assert!(
            crate::array::get_named_property(arr, interp.gc_heap(), "groups")
                .is_some_and(|v| v.is_object())
        );
    }

    #[test]
    fn exec_global_walks_through_text() {
        let mut interp = Interpreter::new();
        let re = make("a", "g", &mut interp);
        let text = Value::string(JsString::from_str("abab", interp.gc_heap_mut()).unwrap());
        // First call → match at 0, lastIndex moves to 1.
        let r1 = call("exec", &re, std::slice::from_ref(&text), &mut interp);
        let (Some(arr), Some(rx)) = (r1.as_array(), re.as_regexp()) else {
            panic!();
        };
        assert_eq!(
            crate::array::get(arr, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "a"
        );
        assert_eq!(rx.last_index(interp.gc_heap()), 1);
        // Second call → match at 2, lastIndex → 3.
        call("exec", &re, std::slice::from_ref(&text), &mut interp);
        if let Some(rx) = re.as_regexp() {
            assert_eq!(rx.last_index(interp.gc_heap()), 3);
        }
        // Third call → no match, lastIndex → 0.
        let r3 = call("exec", &re, &[text], &mut interp);
        assert!(r3.is_null());
        if let Some(rx) = re.as_regexp() {
            assert_eq!(rx.last_index(interp.gc_heap()), 0);
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
            Value::boolean(true)
        );
        assert_eq!(
            load_property(&re, &mut gc_heap, "ignoreCase"),
            Value::boolean(true)
        );
        assert_eq!(
            load_property(&re, &mut gc_heap, "multiline"),
            Value::boolean(false)
        );
    }

    #[test]
    fn last_index_writable() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let re =
            JsRegExp::compile(&mut gc_heap, &"a".encode_utf16().collect::<Vec<_>>(), "g").unwrap();
        store_property(&re, &mut gc_heap, "lastIndex", Value::number_i32(7));
        assert_eq!(re.last_index(&gc_heap), 7);
        // Numeric execution coercion clamps negative values to 0,
        // while the JS-visible property preserves the written value.
        store_property(&re, &mut gc_heap, "lastIndex", Value::number_i32(-3));
        assert_eq!(re.last_index(&gc_heap), 0);
        assert_eq!(
            load_property(&re, &mut gc_heap, "lastIndex"),
            Value::number_i32(-3)
        );
        // String writes are observable, and execution coerces them
        // numerically when needed.
        let written = JsString::from_str("9", &mut gc_heap).unwrap();
        store_property(&re, &mut gc_heap, "lastIndex", Value::string(written));
        assert_eq!(
            load_property(&re, &mut gc_heap, "lastIndex"),
            Value::string(written)
        );
        assert_eq!(re.last_index(&gc_heap), 9);
        // Non-lastIndex names are silently ignored.
        let nope = Value::string(JsString::from_str("nope", &mut gc_heap).unwrap());
        store_property(&re, &mut gc_heap, "source", nope);
    }
}
