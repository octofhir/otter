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
//! - Observable getters, setters, `exec`, species construction, and iterator
//!   advancement run on the current activation stack; every receiver and
//!   intermediate result is re-read from a traced anchor after re-entry.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp.prototype.exec>

use crate::regexp::JsRegExp;
use crate::runtime_cx::{NativeCtx, NativeScope};
use crate::string::JsString;
use crate::{Local, NativeError, Value};

const REGEXP_EXEC_NAME: &str = "RegExp.prototype.exec";

fn native_type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name,
        reason: reason.into(),
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
    receiver: &Value,
    text: JsString,
    ctx: &mut NativeCtx<'_>,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.value(*receiver);
        let input = scope.value(Value::string(text));

        let result = (|| -> Result<Local<'_>, NativeError> {
            let len = scope
                .raw(input)
                .as_string(scope.context().heap())
                .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "input is not a string"))?
                .len() as usize;

            // §22.2.7.2 step 3 — the observable lastIndex read and coercion
            // may run arbitrary JS. Receiver, subject, and the returned value
            // all live in the handle arena before either step starts.
            let receiver_value = scope.raw(receiver);
            let last_index = get_property_runtime(
                scope.context(),
                &receiver_value,
                "lastIndex",
                REGEXP_EXEC_NAME,
            )?;
            let last_index = scope.value(last_index);
            let last_index_value = scope.raw(last_index);
            let mut start =
                to_length_runtime(scope.context(), &last_index_value, REGEXP_EXEC_NAME)? as usize;

            // A lastIndex getter may recompile the RegExp. Re-read the
            // authoritative receiver slot only after that getter/coercion
            // ladder completes; never retain the incoming raw JsRegExp handle.
            let receiver_value = scope.raw(receiver);
            let re = receiver_value
                .as_regexp()
                .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "this is not a RegExp"))?;
            let flags = re.flags(scope.context().heap());
            if !flags.global && !flags.sticky {
                start = 0;
            } else if start > len {
                let receiver_value = scope.raw(receiver);
                set_property_runtime(
                    scope.context(),
                    &receiver_value,
                    "lastIndex",
                    Value::number_i32(0),
                    REGEXP_EXEC_NAME,
                )?;
                return Ok(scope.null());
            }

            // Matching itself is non-allocating. Resolve both raw handles from
            // their Locals immediately before the shared heap borrow.
            let receiver_value = scope.raw(receiver);
            let re = receiver_value
                .as_regexp()
                .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "this is not a RegExp"))?;
            let input_value = scope.raw(input);
            let text = input_value
                .as_string(scope.context().heap())
                .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "input is not a string"))?;
            let heap = scope.context().heap();
            let matched = text.with_utf16(heap, |units| re.find_one_from_utf16(heap, units, start));
            let matched = match matched {
                Some(matched) => matched,
                None => {
                    if flags.global || flags.sticky {
                        let receiver_value = scope.raw(receiver);
                        set_property_runtime(
                            scope.context(),
                            &receiver_value,
                            "lastIndex",
                            Value::number_i32(0),
                            REGEXP_EXEC_NAME,
                        )?;
                    }
                    return Ok(scope.null());
                }
            };
            if flags.sticky && matched.range.start != start {
                let receiver_value = scope.raw(receiver);
                set_property_runtime(
                    scope.context(),
                    &receiver_value,
                    "lastIndex",
                    Value::number_i32(0),
                    REGEXP_EXEC_NAME,
                )?;
                return Ok(scope.null());
            }
            if flags.global || flags.sticky {
                let receiver_value = scope.raw(receiver);
                set_property_runtime(
                    scope.context(),
                    &receiver_value,
                    "lastIndex",
                    Value::number_f64(matched.range.end as f64),
                    REGEXP_EXEC_NAME,
                )?;
            }

            build_match_result_native(&matched, input, flags.has_indices, &mut scope)
        })()?;
        Ok(scope.finish(result))
    })
}

/// §22.2.7.2 steps 26–32 — build the JS-visible match-result array
/// out of a match record. Used by `RegExp.prototype.exec` and
/// reused by `String.prototype.match` / `.matchAll` so both surfaces
/// produce identical shapes (full match + capture slots, plus
/// `index` / `input` / `groups` / optionally `indices`).
/// Slice `range` out of the subject's cached UTF-16 units and allocate the
/// substring. Copies the (small) range out under a shared borrow before the
/// allocation, so the widened subject buffer is never held live across a
/// heap mutation that could move it.
fn slice_subject_string<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    input: Local<'_>,
    range: std::ops::Range<usize>,
) -> Result<Local<'scope>, NativeError> {
    let input_value = scope.raw(input);
    let input = input_value
        .as_string(scope.context().heap())
        .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "input root is not a string"))?;
    let units = input.with_utf16(scope.context().heap(), |units| units[range].to_vec());
    let string = JsString::from_utf16_units(&units, scope.context().heap_mut())?;
    Ok(scope.value(Value::string(string)))
}

fn build_match_result_native<'scope>(
    m: &crate::regexp::engine::Match,
    input: Local<'_>,
    has_indices: bool,
    scope: &mut NativeScope<'scope, '_>,
) -> Result<Local<'scope>, NativeError> {
    let result = scope.array(1 + m.captures.len())?;
    let full = slice_subject_string(scope, input, m.range.clone())?;
    scope.set_index(result, 0, full)?;
    for (index, capture) in m.captures.iter().enumerate() {
        let value = match capture {
            Some(range) => slice_subject_string(scope, input, range.clone())?,
            None => scope.undefined(),
        };
        scope.set_index(result, index + 1, value)?;
    }

    let mut named = m.named_groups();
    let groups = if let Some((name, range)) = named.next() {
        let groups = scope.bare_object()?;
        let value = match range {
            Some(range) => slice_subject_string(scope, input, range)?,
            None => scope.undefined(),
        };
        scope.set(groups, name, value)?;
        for (name, range) in named {
            let value = match range {
                Some(range) => slice_subject_string(scope, input, range)?,
                None => scope.undefined(),
            };
            scope.set(groups, name, value)?;
        }
        groups
    } else {
        scope.undefined()
    };

    // Re-read the array and every stored value only after all group
    // allocations. The arena is the sole authority; no raw JsArray/JsObject
    // survives a collection point.
    let result_array = scope
        .raw(result)
        .as_array()
        .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "result root is not an array"))?;
    let input_value = scope.raw(input);
    let groups_value = scope.raw(groups);
    crate::array::set_match_result_props(
        result_array,
        scope.context().heap_mut(),
        Value::number_i32(m.range.start as i32),
        input_value,
        groups_value,
    )?;

    if has_indices {
        let indices = scope.array(1 + m.captures.len())?;
        let pair = pair_array_native(scope, m.range.start, m.range.end)?;
        scope.set_index(indices, 0, pair)?;
        for (index, capture) in m.captures.iter().enumerate() {
            let value = match capture {
                Some(range) => pair_array_native(scope, range.start, range.end)?,
                None => scope.undefined(),
            };
            scope.set_index(indices, index + 1, value)?;
        }

        let mut named = m.named_groups();
        let index_groups = if let Some((name, range)) = named.next() {
            let groups = scope.bare_object()?;
            let value = match range {
                Some(range) => pair_array_native(scope, range.start, range.end)?,
                None => scope.undefined(),
            };
            scope.set(groups, name, value)?;
            for (name, range) in named {
                let value = match range {
                    Some(range) => pair_array_native(scope, range.start, range.end)?,
                    None => scope.undefined(),
                };
                scope.set(groups, name, value)?;
            }
            groups
        } else {
            scope.undefined()
        };

        let indices_array = scope
            .raw(indices)
            .as_array()
            .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "indices root is not an array"))?;
        let index_groups = scope.raw(index_groups);
        crate::array::set_named_property(
            indices_array,
            scope.context().heap_mut(),
            "groups",
            index_groups,
        )?;
        let result_array = scope
            .raw(result)
            .as_array()
            .ok_or_else(|| native_type_error(REGEXP_EXEC_NAME, "result root is not an array"))?;
        let indices = scope.raw(indices);
        crate::array::set_named_property(
            result_array,
            scope.context().heap_mut(),
            "indices",
            indices,
        )?;
    }
    Ok(result)
}

fn pair_array_native<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    start: usize,
    end: usize,
) -> Result<Local<'scope>, NativeError> {
    let pair = scope.array(2)?;
    let start = scope.number(start as f64);
    let end = scope.number(end as f64);
    scope.set_index(pair, 0, start)?;
    scope.set_index(pair, 1, end)?;
    Ok(pair)
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
    const NAME: &str = "RegExp.prototype[@@match]";
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        if !scope.raw(receiver).is_object_type() {
            return Err(crate::NativeError::TypeError {
                name: NAME,
                reason: "called on a non-object receiver".to_string(),
            });
        }
        let input_arg = scope.argument(args, 0);
        let input_arg_value = scope.raw(input_arg);
        let input = coerce_to_jsstring_runtime(scope.context(), &input_arg_value, NAME)?;
        let input = scope.value(Value::string(input));

        let receiver_value = scope.raw(receiver);
        let flags = get_property_runtime(scope.context(), &receiver_value, "flags", NAME)?;
        let flags = scope.value(flags);
        let flags_value = scope.raw(flags);
        let flags = coerce_to_jsstring_runtime(scope.context(), &flags_value, NAME)?;
        let flags = scope.value(Value::string(flags));
        let flags_value = scope.raw(flags);
        let flags_string = flags_value
            .as_string(scope.context().heap())
            .ok_or_else(|| native_type_error(NAME, "flags root is not a string"))?;
        let (global, full_unicode, sticky) =
            flags_string.with_utf16(scope.context().heap(), |units| {
                (
                    units.contains(&(b'g' as u16)),
                    units.contains(&(b'u' as u16)) || units.contains(&(b'v' as u16)),
                    units.contains(&(b'y' as u16)),
                )
            });

        if !global {
            let receiver_value = scope.raw(receiver);
            let input_value = scope.raw(input);
            let input_string = input_value
                .as_string(scope.context().heap())
                .ok_or_else(|| native_type_error(NAME, "input root is not a string"))?;
            let result = regexp_exec_runtime(scope.context(), &receiver_value, input_string, NAME)?;
            let result = scope.value(result);
            return Ok(scope.finish(result));
        }

        let receiver_value = scope.raw(receiver);
        set_property_runtime(
            scope.context(),
            &receiver_value,
            "lastIndex",
            Value::number_i32(0),
            NAME,
        )?;
        let input_value = scope.raw(input);
        let input_string = input_value
            .as_string(scope.context().heap())
            .ok_or_else(|| native_type_error(NAME, "input root is not a string"))?;
        let input_units = input_string.to_utf16_vec(scope.context().heap());

        // Keep the allocation-free builtin fast path, but treat the arena as
        // authoritative after the observable `exec` lookup. An accessor can
        // allocate, relocate, or even recompile the receiver before returning
        // the builtin function.
        if scope.raw(receiver).as_regexp().is_some() && !sticky {
            let receiver_value = scope.raw(receiver);
            let exec = get_property_runtime(scope.context(), &receiver_value, "exec", NAME)?;
            let exec = scope.value(exec);
            let exec_value = scope.raw(exec);
            if exec_value.as_native_function().is_some_and(|native| {
                native.is_static_native(scope.context().heap(), crate::bootstrap_regexp::proto_exec)
            }) {
                let receiver_value = scope.raw(receiver);
                let re = receiver_value
                    .as_regexp()
                    .ok_or_else(|| native_type_error(NAME, "receiver root is not a RegExp"))?;
                // A custom exec getter may have recompiled the RegExp into
                // another mode or changed lastIndex. The one-pass scan models
                // only an unmodified global builtin starting from zero.
                let current_flags = re.flags(scope.context().heap());
                let current_last_index = re.last_index_value(scope.context().heap());
                if current_flags.global
                    && !current_flags.sticky
                    && crate::abstract_ops::same_value(
                        &current_last_index,
                        &Value::number_i32(0),
                        scope.context().heap(),
                    )
                {
                    let found = re.find_from_utf16(scope.context().heap(), &input_units, 0);
                    if found.is_empty() {
                        return Ok(Value::null());
                    }
                    let mut output = Vec::with_capacity(found.len());
                    for matched in &found {
                        output.push(slice_subject_string(
                            &mut scope,
                            input,
                            matched.range.clone(),
                        )?);
                    }
                    let array = scope.array(output.len())?;
                    for (index, value) in output.iter().copied().enumerate() {
                        scope.set_index(array, index, value)?;
                    }
                    return Ok(scope.finish(array));
                }
            }
        }

        let mut output: Vec<Local<'_>> = Vec::new();
        loop {
            let receiver_value = scope.raw(receiver);
            let input_value = scope.raw(input);
            let input_string = input_value
                .as_string(scope.context().heap())
                .ok_or_else(|| native_type_error(NAME, "input root is not a string"))?;
            let result = regexp_exec_runtime(scope.context(), &receiver_value, input_string, NAME)?;
            let result = scope.value(result);
            if scope.is_null(result) {
                break;
            }

            let result_value = scope.raw(result);
            let matched = get_property_runtime(scope.context(), &result_value, "0", NAME)?;
            let matched = scope.value(matched);
            let matched_value = scope.raw(matched);
            let matched = coerce_to_jsstring_runtime(scope.context(), &matched_value, NAME)?;
            let matched = scope.value(Value::string(matched));
            let matched_value = scope.raw(matched);
            let is_empty = matched_value
                .as_string(scope.context().heap())
                .ok_or_else(|| native_type_error(NAME, "match root is not a string"))?
                .is_empty();
            output.push(matched);

            if is_empty {
                let receiver_value = scope.raw(receiver);
                let last_index =
                    get_property_runtime(scope.context(), &receiver_value, "lastIndex", NAME)?;
                let last_index = scope.value(last_index);
                let last_index_value = scope.raw(last_index);
                let this_index =
                    to_length_runtime(scope.context(), &last_index_value, NAME)? as usize;
                let next_index = advance_string_index(&input_units, this_index, full_unicode);
                let receiver_value = scope.raw(receiver);
                set_property_runtime(
                    scope.context(),
                    &receiver_value,
                    "lastIndex",
                    Value::number_f64(next_index as f64),
                    NAME,
                )?;
            }
        }
        if output.is_empty() {
            return Ok(Value::null());
        }
        let array = scope.array(output.len())?;
        for (index, value) in output.iter().copied().enumerate() {
            scope.set_index(array, index, value)?;
        }
        Ok(scope.finish(array))
    })
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
    const NAME: &str = "RegExp.prototype[@@search]";
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        if !scope.raw(receiver).is_object_type() {
            return Err(crate::NativeError::TypeError {
                name: NAME,
                reason: "called on a non-object receiver".to_string(),
            });
        }
        let input_arg = scope.argument(args, 0);
        let input_arg_value = scope.raw(input_arg);
        let input = coerce_to_jsstring_runtime(scope.context(), &input_arg_value, NAME)?;
        let input = scope.value(Value::string(input));

        let receiver_value = scope.raw(receiver);
        let previous = get_property_runtime(scope.context(), &receiver_value, "lastIndex", NAME)?;
        let previous = scope.value(previous);
        let zero = Value::number_i32(0);
        if !crate::abstract_ops::same_value(&scope.raw(previous), &zero, scope.context().heap()) {
            let receiver_value = scope.raw(receiver);
            set_property_runtime(scope.context(), &receiver_value, "lastIndex", zero, NAME)?;
        }

        let receiver_value = scope.raw(receiver);
        let input_value = scope.raw(input);
        let input_string = input_value
            .as_string(scope.context().heap())
            .ok_or_else(|| native_type_error(NAME, "input root is not a string"))?;
        let result = regexp_exec_runtime(scope.context(), &receiver_value, input_string, NAME)?;
        let result = scope.value(result);

        let receiver_value = scope.raw(receiver);
        let current = get_property_runtime(scope.context(), &receiver_value, "lastIndex", NAME)?;
        let current = scope.value(current);
        if !crate::abstract_ops::same_value(
            &scope.raw(current),
            &scope.raw(previous),
            scope.context().heap(),
        ) {
            let receiver_value = scope.raw(receiver);
            let previous_value = scope.raw(previous);
            set_property_runtime(
                scope.context(),
                &receiver_value,
                "lastIndex",
                previous_value,
                NAME,
            )?;
        }

        if scope.is_null(result) {
            return Ok(Value::number_i32(-1));
        }
        let result_value = scope.raw(result);
        let index = get_property_runtime(scope.context(), &result_value, "index", NAME)?;
        let index = scope.value(index);
        Ok(scope.finish(index))
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
    const RECEIVER: usize = 0;
    const RESULT: usize = 1;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(*receiver);
    ctx.interp_mut().push_iteration_anchor(Value::undefined());

    let result = (|| -> Result<Value, crate::NativeError> {
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let exec =
            ctx.execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
        let outcome = ctx.with_turn_parts(|interp, stack| {
            interp
                .ordinary_get_value(
                    stack,
                    &exec,
                    receiver,
                    receiver,
                    &crate::VmPropertyKey::String(key),
                    0,
                )
                .map_err(vm_err_to_native(interp, name))
        })?;
        match outcome {
            crate::VmGetOutcome::Value(value) => {
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + RESULT, value);
            }
            crate::VmGetOutcome::InvokeGetter { getter } => {
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + RESULT, getter);
                let getter = ctx.interp_mut().iteration_anchor(anchor_base + RESULT);
                let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
                let value = ctx.call(getter, receiver, &[])?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + RESULT, value);
            }
        }
        Ok(ctx.interp_mut().iteration_anchor(anchor_base + RESULT))
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
    const RECEIVER: usize = 0;
    const VALUE: usize = 1;
    const SETTER: usize = 2;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(*receiver);
    ctx.interp_mut().push_iteration_anchor(value);
    ctx.interp_mut().push_iteration_anchor(Value::undefined());

    let result = (|| -> Result<(), crate::NativeError> {
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        // Resolve accessor setters along the prototype chain first so a
        // custom `set lastIndex` (e.g. a `@@split` splitter) fires; only
        // ordinary data targets fall through to the data write.
        if let Some(obj) = receiver.as_object() {
            match crate::object::resolve_set(obj, ctx.heap(), key) {
                crate::object::SetOutcome::InvokeSetter { setter } => {
                    if !ctx.interp_mut().is_callable_runtime(&setter) {
                        return Err(read_only_set_error(name, key));
                    }
                    ctx.interp_mut()
                        .set_iteration_anchor(anchor_base + SETTER, setter);
                    let setter = ctx.interp_mut().iteration_anchor(anchor_base + SETTER);
                    let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
                    let value = ctx.interp_mut().iteration_anchor(anchor_base + VALUE);
                    ctx.call(setter, receiver, &[value])?;
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

        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let value = ctx.interp_mut().iteration_anchor(anchor_base + VALUE);
        let exec =
            ctx.execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
        let ok = ctx.with_turn_parts(|interp, stack| {
            interp
                .ordinary_set_data_value(
                    stack,
                    &exec,
                    receiver,
                    &crate::VmPropertyKey::String(key),
                    value,
                    receiver,
                    0,
                )
                .map_err(vm_err_to_native(interp, name))
        })?;
        // `Set(O, P, V, true)` — a `[[Set]]` returning false (e.g. a
        // non-writable `lastIndex`) is a TypeError, not a silent no-op.
        if !ok {
            return Err(read_only_set_error(name, key));
        }
        Ok(())
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
    ctx.scope(|mut scope| {
        let value = scope.value(*value);
        let current = scope.raw(value);
        if current.is_symbol() {
            return Err(crate::NativeError::TypeError {
                name,
                reason: "cannot convert a Symbol to a string".to_string(),
            });
        }
        if let Some(string) = current.as_string(scope.context().heap()) {
            return Ok(string);
        }
        let primitive = if crate::abstract_ops::is_primitive(&current) {
            current
        } else {
            let exec = scope
                .context()
                .execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
            scope.with_turn_parts(|interp, stack| {
                interp
                    .evaluate_to_primitive(
                        stack,
                        &exec,
                        &current,
                        crate::abstract_ops::ToPrimitiveHint::String,
                    )
                    .map_err(vm_err_to_native(interp, name))
            })?
        };
        let primitive = scope.value(primitive);
        let primitive = scope.raw(primitive);
        crate::conversion::to_js_string_primitive(&primitive, scope.context().heap_mut()).map_err(
            |error| crate::NativeError::TypeError {
                name,
                reason: format!("ToString failed: {error:?}"),
            },
        )
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
    ctx.scope(|mut scope| {
        let value = scope.value(*value);
        let current = scope.raw(value);
        let primitive = if crate::abstract_ops::is_primitive(&current) {
            current
        } else {
            let exec = scope
                .context()
                .execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
            scope.with_turn_parts(|interp, stack| {
                interp
                    .evaluate_to_primitive(
                        stack,
                        &exec,
                        &current,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )
                    .map_err(vm_err_to_native(interp, name))
            })?
        };
        let primitive = scope.value(primitive);
        let primitive = scope.raw(primitive);
        let n = scope
            .with_turn_parts(|interp, _| {
                crate::coerce::primitive_to_number(interp, &primitive)
                    .map_err(vm_err_to_native(interp, name))
            })?
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
    })
}

/// §7.1.5 `ToIntegerOrInfinity`. `NaN` / `+0` / `-0` → 0; ±Infinity
/// pass through; finite values truncate toward zero.
fn to_integer_or_infinity_runtime(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    name: &'static str,
) -> Result<f64, crate::NativeError> {
    ctx.scope(|mut scope| {
        let value = scope.value(*value);
        let current = scope.raw(value);
        let primitive = if crate::abstract_ops::is_primitive(&current) {
            current
        } else {
            let exec = scope
                .context()
                .execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
            scope.with_turn_parts(|interp, stack| {
                interp
                    .evaluate_to_primitive(
                        stack,
                        &exec,
                        &current,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )
                    .map_err(vm_err_to_native(interp, name))
            })?
        };
        let primitive = scope.value(primitive);
        let primitive = scope.raw(primitive);
        let n = scope
            .with_turn_parts(|interp, _| {
                crate::coerce::primitive_to_number(interp, &primitive)
                    .map_err(vm_err_to_native(interp, name))
            })?
            .as_f64();
        if n.is_nan() || n == 0.0 {
            return Ok(0.0);
        }
        if n.is_infinite() {
            return Ok(n);
        }
        Ok(n.trunc())
    })
}

/// §22.2.7.1 `RegExpExec(R, S)`. Dispatches through
/// `Get(R, "exec")` so user-overridable execs are honoured before
/// falling back to the builtin §22.2.7.2 algorithm when `R` is a
/// `Value::RegExp` instance.
pub(crate) fn regexp_exec_runtime(
    ctx: &mut NativeCtx<'_>,
    rx: &Value,
    s: JsString,
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    const RECEIVER: usize = 0;
    const INPUT: usize = 1;
    const EXEC: usize = 2;
    const RESULT: usize = 3;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(*rx);
    ctx.interp_mut().push_iteration_anchor(Value::string(s));
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    ctx.interp_mut().push_iteration_anchor(Value::undefined());

    let outcome = (|| -> Result<Value, crate::NativeError> {
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let exec_fn = get_property_runtime(ctx, &receiver, "exec", name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + EXEC, exec_fn);
        let exec_fn = ctx.interp_mut().iteration_anchor(anchor_base + EXEC);
        if ctx.interp_mut().is_callable_runtime(&exec_fn) {
            let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
            let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
            let result = ctx.call(exec_fn, receiver, &[input])?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + RESULT, result);
            let result = ctx.interp_mut().iteration_anchor(anchor_base + RESULT);
            if !result.is_null() && !result.is_object_type() {
                return Err(crate::NativeError::TypeError {
                    name,
                    reason: "exec did not return an Object or null".to_string(),
                });
            }
            return Ok(result);
        }

        // Fall back to builtin exec only when `rx` is actually a RegExp.
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        if receiver.as_regexp().is_some() {
            let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
            let input =
                input
                    .as_string(ctx.heap())
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "RegExpExec input root is not a string".to_string(),
                    })?;
            let result = exec_once_native(&receiver, input, ctx)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + RESULT, result);
            return Ok(ctx.interp_mut().iteration_anchor(anchor_base + RESULT));
        }
        Err(crate::NativeError::TypeError {
            name,
            reason: "exec is not callable and receiver is not a RegExp".to_string(),
        })
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    outcome
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
    stack: &mut crate::ActivationStack,
    context: &crate::ExecutionContext,
    matcher: &Value,
    input: JsString,
    global: bool,
    full_unicode: bool,
) -> Result<Option<Value>, crate::VmError> {
    let name = "RegExp String Iterator.next";
    const MATCHER: usize = 0;
    const INPUT: usize = 1;
    const RESULT: usize = 2;
    const INTERMEDIATE: usize = 3;
    let anchor_base = interp.iteration_anchors_for_trace().len();
    interp.push_iteration_anchor(*matcher);
    interp.push_iteration_anchor(Value::string(input));
    interp.push_iteration_anchor(Value::undefined());
    interp.push_iteration_anchor(Value::undefined());

    let outcome = {
        let rooted_matcher = interp.iteration_anchor(anchor_base + MATCHER);
        let call_info = crate::NativeCallInfo::call(rooted_matcher);
        let roots = crate::runtime_cx::NativeCallRoots::new(&call_info, &[], &[]);
        let _call_roots = interp
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(interp, stack);
        let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
        let ctx = &mut ctx;

        (|| -> Result<Option<Value>, crate::VmError> {
            let matcher = ctx.interp_mut().iteration_anchor(anchor_base + MATCHER);
            let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
            let input = input
                .as_string(ctx.heap())
                .ok_or(crate::VmError::InvalidOperand)?;
            let result = regexp_exec_runtime(ctx, &matcher, input, name)
                .map_err(|e| ctx.native_error_to_vm(e))?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + RESULT, result);
            if ctx
                .interp_mut()
                .iteration_anchor(anchor_base + RESULT)
                .is_null()
            {
                return Ok(None);
            }
            if global {
                let result = ctx.interp_mut().iteration_anchor(anchor_base + RESULT);
                let matched = get_property_runtime(ctx, &result, "0", name)
                    .map_err(|e| ctx.native_error_to_vm(e))?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + INTERMEDIATE, matched);
                let matched = ctx
                    .interp_mut()
                    .iteration_anchor(anchor_base + INTERMEDIATE);
                let matched_str = coerce_to_jsstring_runtime(ctx, &matched, name)
                    .map_err(|e| ctx.native_error_to_vm(e))?;
                if matched_str.is_empty() {
                    let matcher = ctx.interp_mut().iteration_anchor(anchor_base + MATCHER);
                    let last_index = get_property_runtime(ctx, &matcher, "lastIndex", name)
                        .map_err(|e| ctx.native_error_to_vm(e))?;
                    ctx.interp_mut()
                        .set_iteration_anchor(anchor_base + INTERMEDIATE, last_index);
                    let last_index = ctx
                        .interp_mut()
                        .iteration_anchor(anchor_base + INTERMEDIATE);
                    let this_index = to_length_runtime(ctx, &last_index, name)
                        .map_err(|e| ctx.native_error_to_vm(e))?
                        as usize;
                    let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
                    let input = input
                        .as_string(ctx.heap())
                        .ok_or(crate::VmError::InvalidOperand)?;
                    let next_index = input.with_utf16(ctx.heap(), |units| {
                        advance_string_index(units, this_index, full_unicode)
                    });
                    let matcher = ctx.interp_mut().iteration_anchor(anchor_base + MATCHER);
                    set_property_runtime(
                        ctx,
                        &matcher,
                        "lastIndex",
                        Value::number_f64(next_index as f64),
                        name,
                    )
                    .map_err(|e| ctx.native_error_to_vm(e))?;
                }
            }
            Ok(Some(
                ctx.interp_mut().iteration_anchor(anchor_base + RESULT),
            ))
        })()
    };
    interp.pop_iteration_anchors_to(anchor_base);
    outcome
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
    const RECEIVER: usize = 0;
    const INPUT: usize = 1;
    const REPLACER: usize = 2;
    const TEMPLATE: usize = 3;
    const FLAGS: usize = 4;
    const MATCHED: usize = 5;
    const NAMED: usize = 6;
    const TEMP: usize = 7;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(receiver);
    ctx.interp_mut().push_iteration_anchor(string_arg);
    ctx.interp_mut().push_iteration_anchor(replace_value_arg);
    for _ in TEMPLATE..=TEMP {
        ctx.interp_mut().push_iteration_anchor(Value::undefined());
    }

    let result = (|| -> Result<Value, crate::NativeError> {
        // Step 3 — S = ? ToString(string).
        let string_arg = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
        let s = coerce_to_jsstring_runtime(ctx, &string_arg, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + INPUT, Value::string(s));
        let s_units = s.to_utf16_vec(ctx.heap());
        let length_s = s_units.len();

        // Step 4 — functionalReplace = IsCallable(replaceValue).
        let replacer = ctx.interp_mut().iteration_anchor(anchor_base + REPLACER);
        let functional_replace = ctx.interp_mut().is_callable_runtime(&replacer);

        // Step 5 — non-callable replacements are ToString-coerced once.
        if !functional_replace {
            let replacer = ctx.interp_mut().iteration_anchor(anchor_base + REPLACER);
            let template = coerce_to_jsstring_runtime(ctx, &replacer, name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + TEMPLATE, Value::string(template));
        }

        // Step 6 — flags = ? ToString(? Get(rx, "flags")).
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let flags = get_property_runtime(ctx, &receiver, "flags", name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + FLAGS, flags);
        let flags = ctx.interp_mut().iteration_anchor(anchor_base + FLAGS);
        let flags = coerce_to_jsstring_runtime(ctx, &flags, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + FLAGS, Value::string(flags));
        let (global, full_unicode, sticky) = flags.with_utf16(ctx.heap(), |units| {
            (
                units.contains(&(b'g' as u16)),
                units.contains(&(b'u' as u16)) || units.contains(&(b'v' as u16)),
                units.contains(&(b'y' as u16)),
            )
        });

        // Step 9 — if global, Set(rx, "lastIndex", 0, true).
        if global {
            let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
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
        let fast_receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        if global
            && let Some(re) = fast_receiver.as_regexp()
            && !sticky
            && !functional_replace
            && re.expando(ctx.heap()).is_none()
        {
            let template = ctx.interp_mut().iteration_anchor(anchor_base + TEMPLATE);
            let template =
                template
                    .as_string(ctx.heap())
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "replace template root is not a string".to_string(),
                    })?;
            let template_units = template.to_utf16_vec(ctx.heap());
            if !template_units.contains(&0x24) {
                let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
                let exec_fn = get_property_runtime(ctx, &receiver, "exec", name)?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + TEMP, exec_fn);
                let exec_fn = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
                if exec_fn.as_native_function().is_some_and(|nf| {
                    nf.is_static_native(ctx.heap(), crate::bootstrap_regexp::proto_exec)
                }) {
                    let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
                    let re = receiver
                        .as_regexp()
                        .ok_or_else(|| crate::NativeError::TypeError {
                            name,
                            reason: "replace receiver root is not a RegExp".to_string(),
                        })?;
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
        let mut result_indices: Vec<usize> = Vec::new();
        loop {
            let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
            let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
            let input =
                input
                    .as_string(ctx.heap())
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "replace input root is not a string".to_string(),
                    })?;
            let result = regexp_exec_runtime(ctx, &receiver, input, name)?;
            if result.is_null() {
                break;
            }
            let result_index = ctx.interp_mut().push_iteration_anchor(result) - 1;
            result_indices.push(result_index);
            if !global {
                break;
            }
            // Step 11.d.iii — empty match: advance lastIndex by one
            // (or two for paired surrogates under `u` / `v`).
            let result = ctx.interp_mut().iteration_anchor(result_index);
            let matched_val = get_property_runtime(ctx, &result, "0", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + MATCHED, matched_val);
            let matched_val = ctx.interp_mut().iteration_anchor(anchor_base + MATCHED);
            let matched_str = coerce_to_jsstring_runtime(ctx, &matched_val, name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + MATCHED, Value::string(matched_str));
            if matched_str.is_empty() {
                let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
                let last_index_val = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + TEMP, last_index_val);
                let last_index_val = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
                let this_index = to_length_runtime(ctx, &last_index_val, name)? as usize;
                let next_index = advance_string_index(&s_units, this_index, full_unicode);
                let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
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

        for &result_index in &result_indices {
            let capture_anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
            let result = ctx.interp_mut().iteration_anchor(result_index);
            let length_val = get_property_runtime(ctx, &result, "length", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + TEMP, length_val);
            let length_val = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
            let result_length = to_length_runtime(ctx, &length_val, name)? as usize;
            let n_captures = result_length.saturating_sub(1);

            let result = ctx.interp_mut().iteration_anchor(result_index);
            let matched_val = get_property_runtime(ctx, &result, "0", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + MATCHED, matched_val);
            let matched_val = ctx.interp_mut().iteration_anchor(anchor_base + MATCHED);
            let matched_str = coerce_to_jsstring_runtime(ctx, &matched_val, name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + MATCHED, Value::string(matched_str));
            let match_units = matched_str.to_utf16_vec(ctx.heap());
            let match_length = match_units.len();

            let result = ctx.interp_mut().iteration_anchor(result_index);
            let index_val = get_property_runtime(ctx, &result, "index", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + TEMP, index_val);
            let index_val = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
            let position_raw = to_integer_or_infinity_runtime(ctx, &index_val, name)?;
            let position = position_raw.max(0.0).min(length_s as f64) as usize;

            let mut captures: Vec<Option<usize>> = Vec::with_capacity(n_captures);
            for i in 1..=n_captures {
                let cap_key = i.to_string();
                let result = ctx.interp_mut().iteration_anchor(result_index);
                let cap_val = get_property_runtime(ctx, &result, &cap_key, name)?;
                if cap_val.is_undefined() {
                    captures.push(None);
                } else {
                    ctx.interp_mut()
                        .set_iteration_anchor(anchor_base + TEMP, cap_val);
                    let cap_val = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
                    let capture = coerce_to_jsstring_runtime(ctx, &cap_val, name)?;
                    let capture_index = ctx
                        .interp_mut()
                        .push_iteration_anchor(Value::string(capture))
                        - 1;
                    captures.push(Some(capture_index));
                }
            }

            let result = ctx.interp_mut().iteration_anchor(result_index);
            let named_captures = get_property_runtime(ctx, &result, "groups", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + NAMED, named_captures);
            let named_captures = ctx.interp_mut().iteration_anchor(anchor_base + NAMED);
            let named_captures_index = if named_captures.is_undefined() {
                None
            } else {
                Some(anchor_base + NAMED)
            };

            let replacement: Vec<u16> = if functional_replace {
                let mut replacer_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                replacer_args.push(ctx.interp_mut().iteration_anchor(anchor_base + MATCHED));
                for cap in &captures {
                    replacer_args.push(match cap {
                        Some(index) => ctx.interp_mut().iteration_anchor(*index),
                        None => Value::undefined(),
                    });
                }
                replacer_args.push(Value::number_f64(position as f64));
                replacer_args.push(ctx.interp_mut().iteration_anchor(anchor_base + INPUT));
                if let Some(index) = named_captures_index {
                    replacer_args.push(ctx.interp_mut().iteration_anchor(index));
                }
                let replacer = ctx.interp_mut().iteration_anchor(anchor_base + REPLACER);
                let raw = ctx.call(replacer, Value::undefined(), &replacer_args)?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + TEMP, raw);
                let raw = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
                let raw_str = coerce_to_jsstring_runtime(ctx, &raw, name)?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + TEMP, Value::string(raw_str));
                raw_str.to_utf16_vec(ctx.heap())
            } else {
                // §22.2.6.11 step 14.n.i — the non-functional path coerces a
                // present `groups` with ToObject before GetSubstitution. `null`
                // (from a monkey-patched `exec`) throws; a primitive is passed
                // through, since the `$<name>` lookups read it via the full
                // [[Get]], which boxes primitives the same way ToObject would.
                let named_captures_coerced = match named_captures_index {
                    Some(index) if ctx.interp_mut().iteration_anchor(index).is_nullish() => {
                        return Err(crate::NativeError::TypeError {
                            name,
                            reason: "named capture groups is not coercible to an Object"
                                .to_string(),
                        });
                    }
                    other => other,
                };
                let template = ctx.interp_mut().iteration_anchor(anchor_base + TEMPLATE);
                let template = template.as_string(ctx.heap()).ok_or_else(|| {
                    crate::NativeError::TypeError {
                        name,
                        reason: "replace template root is not a string".to_string(),
                    }
                })?;
                let template = template.to_utf16_vec(ctx.heap());
                get_substitution(
                    ctx,
                    &match_units,
                    &s_units,
                    position,
                    &captures,
                    named_captures_coerced,
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
            ctx.interp_mut()
                .pop_iteration_anchors_to(capture_anchor_base);
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
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
    const RECEIVER: usize = 0;
    const RESULT: usize = 1;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(*receiver);
    ctx.interp_mut().push_iteration_anchor(Value::undefined());

    let result = (|| -> Result<Value, crate::NativeError> {
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let exec =
            ctx.execution_context()
                .cloned()
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
        let outcome = ctx.with_turn_parts(|interp, stack| {
            interp
                .ordinary_get_value(
                    stack,
                    &exec,
                    receiver,
                    receiver,
                    &crate::VmPropertyKey::Symbol(sym),
                    0,
                )
                .map_err(vm_err_to_native(interp, name))
        })?;
        match outcome {
            crate::VmGetOutcome::Value(value) => {
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + RESULT, value);
            }
            crate::VmGetOutcome::InvokeGetter { getter } => {
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + RESULT, getter);
                let getter = ctx.interp_mut().iteration_anchor(anchor_base + RESULT);
                let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
                let value = ctx.call(getter, receiver, &[])?;
                ctx.interp_mut()
                    .set_iteration_anchor(anchor_base + RESULT, value);
            }
        }
        Ok(ctx.interp_mut().iteration_anchor(anchor_base + RESULT))
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
    const OBJECT: usize = 0;
    const DEFAULT_CTOR: usize = 1;
    const CONSTRUCTOR: usize = 2;
    const SPECIES: usize = 3;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(*obj);
    ctx.interp_mut().push_iteration_anchor(*default_ctor);
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    ctx.interp_mut().push_iteration_anchor(Value::undefined());

    let result = (|| -> Result<Value, crate::NativeError> {
        let object = ctx.interp_mut().iteration_anchor(anchor_base + OBJECT);
        let constructor = get_property_runtime(ctx, &object, "constructor", name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + CONSTRUCTOR, constructor);
        let constructor = ctx.interp_mut().iteration_anchor(anchor_base + CONSTRUCTOR);
        if constructor.is_undefined() {
            return Ok(ctx
                .interp_mut()
                .iteration_anchor(anchor_base + DEFAULT_CTOR));
        }
        if !constructor.is_object_type() {
            return Err(crate::NativeError::TypeError {
                name,
                reason: "constructor is not an Object".to_string(),
            });
        }
        let species_sym = ctx
            .interp_mut()
            .well_known_symbols()
            .get(crate::symbol::WellKnown::Species);
        let species = get_symbol_property_runtime(ctx, &constructor, species_sym, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + SPECIES, species);
        let species = ctx.interp_mut().iteration_anchor(anchor_base + SPECIES);
        // §7.3.20 step 6 — `undefined`/`null` @@species falls back to the
        // default constructor, not the resolved `constructor` value.
        if species.is_nullish() {
            return Ok(ctx
                .interp_mut()
                .iteration_anchor(anchor_base + DEFAULT_CTOR));
        }
        let is_constructor = {
            let (interp, exec) = ctx.interp_mut_and_context();
            let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
                name,
                reason: "missing execution context".to_string(),
            })?;
            crate::abstract_ops::is_constructor(&species, &exec, &interp.gc_heap)
        };
        if is_constructor {
            return Ok(ctx.interp_mut().iteration_anchor(anchor_base + SPECIES));
        }
        Err(crate::NativeError::TypeError {
            name,
            reason: "Symbol.species is not a constructor".to_string(),
        })
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
    const RECEIVER: usize = 0;
    const INPUT: usize = 1;
    const DEFAULT_CTOR: usize = 2;
    const CONSTRUCTOR: usize = 3;
    const MATCHER: usize = 4;
    const TEMP: usize = 5;
    const RESULT: usize = 6;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(receiver);
    ctx.interp_mut().push_iteration_anchor(string_arg);
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    ctx.interp_mut().push_iteration_anchor(Value::undefined());

    let result = (|| -> Result<Value, crate::NativeError> {
        let string_arg = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
        let input = coerce_to_jsstring_runtime(ctx, &string_arg, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + INPUT, Value::string(input));

        let default_ctor = {
            let interp = &ctx.cx.interp;
            crate::object::get(interp.global_this, &interp.gc_heap, "RegExp").ok_or_else(|| {
                crate::NativeError::TypeError {
                    name,
                    reason: "%RegExp% intrinsic missing".to_string(),
                }
            })?
        };
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + DEFAULT_CTOR, default_ctor);
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let default_ctor = ctx
            .interp_mut()
            .iteration_anchor(anchor_base + DEFAULT_CTOR);
        let constructor = species_constructor_runtime(ctx, &receiver, &default_ctor, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + CONSTRUCTOR, constructor);

        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let flags = get_property_runtime(ctx, &receiver, "flags", name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + TEMP, flags);
        let flags = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
        let flags = coerce_to_jsstring_runtime(ctx, &flags, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + TEMP, Value::string(flags));
        let (global, full_unicode) = flags.with_utf16(ctx.heap(), |units| {
            let global = units.contains(&(b'g' as u16));
            let full_unicode = units.contains(&(b'u' as u16)) || units.contains(&(b'v' as u16));
            (global, full_unicode)
        });
        let constructor = ctx.interp_mut().iteration_anchor(anchor_base + CONSTRUCTOR);
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let flags = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
        let matcher = ctx.construct(constructor, &[receiver, flags])?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + MATCHER, matcher);

        // Step 2.f — snapshot rx.lastIndex into matcher.
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let last_index = get_property_runtime(ctx, &receiver, "lastIndex", name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + TEMP, last_index);
        let last_index = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
        let last_index = to_length_runtime(ctx, &last_index, name)? as f64;
        let matcher = ctx.interp_mut().iteration_anchor(anchor_base + MATCHER);
        set_property_runtime(
            ctx,
            &matcher,
            "lastIndex",
            Value::number_f64(last_index),
            name,
        )?;

        let matcher = ctx.interp_mut().iteration_anchor(anchor_base + MATCHER);
        let input_root = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
        let input =
            input_root
                .as_string(ctx.heap())
                .ok_or_else(|| crate::NativeError::TypeError {
                    name,
                    reason: "matchAll input root is not a string".to_string(),
                })?;
        let iter_state = crate::IteratorState::RegExpString {
            matcher,
            input,
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
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + RESULT, Value::iterator(handle));
        Ok(ctx.interp_mut().iteration_anchor(anchor_base + RESULT))
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
}

/// Materialize values retained by iteration-anchor index into a JS array.
fn array_from_iteration_anchor_indices(
    ctx: &mut NativeCtx<'_>,
    indices: &[usize],
    name: &'static str,
) -> Result<Value, crate::NativeError> {
    let mut elements = Vec::with_capacity(indices.len());
    for &index in indices {
        elements.push(ctx.interp_mut().iteration_anchor(index));
    }
    let arr = ctx
        .array_from_elements_with_roots(elements.iter().copied(), &[], &[elements.as_slice()])
        .map_err(|_| crate::NativeError::TypeError {
            name,
            reason: "array allocation failed".to_string(),
        })?;
    Ok(Value::array(arr))
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
    const RECEIVER: usize = 0;
    const INPUT: usize = 1;
    const LIMIT: usize = 2;
    const DEFAULT_CTOR: usize = 3;
    const CONSTRUCTOR: usize = 4;
    const SPLITTER: usize = 5;
    const MATCH_RESULT: usize = 6;
    const TEMP: usize = 7;
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(receiver);
    ctx.interp_mut().push_iteration_anchor(string_arg);
    ctx.interp_mut().push_iteration_anchor(limit_arg);
    for _ in DEFAULT_CTOR..=TEMP {
        ctx.interp_mut().push_iteration_anchor(Value::undefined());
    }

    let result = (|| -> Result<Value, crate::NativeError> {
        // Step 4 — S = ? ToString(string).
        let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
        let input = coerce_to_jsstring_runtime(ctx, &input, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + INPUT, Value::string(input));
        let s_units = input.to_utf16_vec(ctx.heap());
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
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + DEFAULT_CTOR, default_ctor);
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let default_ctor = ctx
            .interp_mut()
            .iteration_anchor(anchor_base + DEFAULT_CTOR);
        let constructor = species_constructor_runtime(ctx, &receiver, &default_ctor, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + CONSTRUCTOR, constructor);

        // Step 6-8 — append sticky `y`, preserving the coerced WTF-16 flags.
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let flags = get_property_runtime(ctx, &receiver, "flags", name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + TEMP, flags);
        let flags = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
        let flags = coerce_to_jsstring_runtime(ctx, &flags, name)?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + TEMP, Value::string(flags));
        let mut new_flags = flags.to_utf16_vec(ctx.heap());
        let unicode_matching =
            new_flags.contains(&(b'u' as u16)) || new_flags.contains(&(b'v' as u16));
        if !new_flags.contains(&(b'y' as u16)) {
            new_flags.push(b'y' as u16);
        }
        let new_flags = JsString::from_utf16_units(&new_flags, ctx.heap_mut()).map_err(|_| {
            crate::NativeError::TypeError {
                name,
                reason: "out of memory".to_string(),
            }
        })?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + TEMP, Value::string(new_flags));

        // Step 9 — splitter = ? Construct(C, [rx, newFlags]) on this turn.
        let constructor = ctx.interp_mut().iteration_anchor(anchor_base + CONSTRUCTOR);
        let receiver = ctx.interp_mut().iteration_anchor(anchor_base + RECEIVER);
        let flags = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
        let splitter = ctx.construct(constructor, &[receiver, flags])?;
        ctx.interp_mut()
            .set_iteration_anchor(anchor_base + SPLITTER, splitter);

        // Step 13 — lim = limit === undefined ? 2^32 - 1 : ToUint32(limit).
        let limit = ctx.interp_mut().iteration_anchor(anchor_base + LIMIT);
        let lim: u32 = if limit.is_undefined() {
            u32::MAX
        } else {
            let exec =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "missing execution context".to_string(),
                    })?;
            let n = ctx
                .with_turn_parts(|interp, stack| {
                    crate::coerce::to_number_or_throw(interp, stack, &exec, &limit)
                        .map_err(vm_err_to_native(interp, name))
                })?
                .as_f64();
            if n.is_nan() {
                0
            } else {
                n.trunc().rem_euclid(4_294_967_296.0) as u32
            }
        };

        let mut output_indices: Vec<usize> = Vec::new();
        if lim == 0 {
            return array_from_iteration_anchor_indices(ctx, &output_indices, name);
        }

        if size == 0 {
            let splitter = ctx.interp_mut().iteration_anchor(anchor_base + SPLITTER);
            let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
            let input =
                input
                    .as_string(ctx.heap())
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "split input root is not a string".to_string(),
                    })?;
            let matched = regexp_exec_runtime(ctx, &splitter, input, name)?;
            if !matched.is_null() {
                return array_from_iteration_anchor_indices(ctx, &output_indices, name);
            }
            output_indices.push(anchor_base + INPUT);
            return array_from_iteration_anchor_indices(ctx, &output_indices, name);
        }

        let mut p: usize = 0;
        let mut q: usize = 0;
        while q < size {
            let splitter = ctx.interp_mut().iteration_anchor(anchor_base + SPLITTER);
            set_property_runtime(
                ctx,
                &splitter,
                "lastIndex",
                Value::number_f64(q as f64),
                name,
            )?;
            let splitter = ctx.interp_mut().iteration_anchor(anchor_base + SPLITTER);
            let input = ctx.interp_mut().iteration_anchor(anchor_base + INPUT);
            let input =
                input
                    .as_string(ctx.heap())
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name,
                        reason: "split input root is not a string".to_string(),
                    })?;
            let matched = regexp_exec_runtime(ctx, &splitter, input, name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + MATCH_RESULT, matched);
            if ctx
                .interp_mut()
                .iteration_anchor(anchor_base + MATCH_RESULT)
                .is_null()
            {
                q = advance_string_index(&s_units, q, unicode_matching);
                continue;
            }
            let splitter = ctx.interp_mut().iteration_anchor(anchor_base + SPLITTER);
            let last_index = get_property_runtime(ctx, &splitter, "lastIndex", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + TEMP, last_index);
            let last_index = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
            let e = (to_length_runtime(ctx, &last_index, name)? as usize).min(size);
            if e == p {
                q = advance_string_index(&s_units, q, unicode_matching);
                continue;
            }
            let part =
                JsString::from_utf16_units(&s_units[p..q], ctx.heap_mut()).map_err(|_| {
                    crate::NativeError::TypeError {
                        name,
                        reason: "out of memory".to_string(),
                    }
                })?;
            let part_index = ctx.interp_mut().push_iteration_anchor(Value::string(part)) - 1;
            output_indices.push(part_index);
            if output_indices.len() as u32 == lim {
                return array_from_iteration_anchor_indices(ctx, &output_indices, name);
            }
            p = e;
            let matched = ctx
                .interp_mut()
                .iteration_anchor(anchor_base + MATCH_RESULT);
            let length = get_property_runtime(ctx, &matched, "length", name)?;
            ctx.interp_mut()
                .set_iteration_anchor(anchor_base + TEMP, length);
            let length = ctx.interp_mut().iteration_anchor(anchor_base + TEMP);
            let number_of_captures =
                (to_length_runtime(ctx, &length, name)? as usize).saturating_sub(1);
            for i in 1..=number_of_captures {
                let matched = ctx
                    .interp_mut()
                    .iteration_anchor(anchor_base + MATCH_RESULT);
                let capture = get_property_runtime(ctx, &matched, &i.to_string(), name)?;
                let capture_index = ctx.interp_mut().push_iteration_anchor(capture) - 1;
                output_indices.push(capture_index);
                if output_indices.len() as u32 == lim {
                    return array_from_iteration_anchor_indices(ctx, &output_indices, name);
                }
            }
            q = p;
        }

        let tail = JsString::from_utf16_units(&s_units[p..size], ctx.heap_mut()).map_err(|_| {
            crate::NativeError::TypeError {
                name,
                reason: "out of memory".to_string(),
            }
        })?;
        let tail_index = ctx.interp_mut().push_iteration_anchor(Value::string(tail)) - 1;
        output_indices.push(tail_index);
        array_from_iteration_anchor_indices(ctx, &output_indices, name)
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
    captures: &[Option<usize>],
    named_captures: Option<usize>,
    template: &[u16],
    name: &'static str,
) -> Result<Vec<u16>, crate::NativeError> {
    let anchor_base = ctx.interp_mut().iteration_anchors_for_trace().len();
    ctx.interp_mut().push_iteration_anchor(Value::undefined());
    let result = (|| -> Result<Vec<u16>, crate::NativeError> {
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
                        if let Some(Some(cap_index)) = captures.get(group_index - 1) {
                            let capture = ctx.interp_mut().iteration_anchor(*cap_index);
                            let capture = capture.as_string(ctx.heap()).ok_or_else(|| {
                                crate::NativeError::TypeError {
                                    name,
                                    reason: "replace capture root is not a string".to_string(),
                                }
                            })?;
                            out.extend_from_slice(&capture.to_utf16_vec(ctx.heap()));
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
                    if let Some(nc_index) = named_captures {
                        let named = ctx.interp_mut().iteration_anchor(nc_index);
                        let val = get_property_runtime(ctx, &named, &group_name, name)?;
                        ctx.interp_mut().set_iteration_anchor(anchor_base, val);
                        let val = ctx.interp_mut().iteration_anchor(anchor_base);
                        if !val.is_undefined() {
                            let coerced = coerce_to_jsstring_runtime(ctx, &val, name)?;
                            ctx.interp_mut()
                                .set_iteration_anchor(anchor_base, Value::string(coerced));
                            let coerced = ctx.interp_mut().iteration_anchor(anchor_base);
                            let coerced = coerced.as_string(ctx.heap()).ok_or_else(|| {
                                crate::NativeError::TypeError {
                                    name,
                                    reason: "named replacement root is not a string".to_string(),
                                }
                            })?;
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
    })();
    ctx.interp_mut().pop_iteration_anchors_to(anchor_base);
    result
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
            let escaped = escape_regexp_pattern_utf16(&re.pattern_utf16(gc_heap));
            match JsString::from_utf16_units(&escaped, gc_heap) {
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

/// §22.2.3.2.4 `EscapeRegExpPattern(src, flags)` — emit a code-unit
/// sequence that, when re-parsed as a Pattern, matches the same set
/// of strings as the original. Empty source maps to `"(?:)"`; bare
/// `/` and line terminators are escaped; everything else passes
/// through. Operates at the UTF-16 code-unit level so lone surrogates
/// in the pattern body (e.g. `/\u{D800}/.source`) survive — a `&str`
/// round-trip would replace them with U+FFFD. Every escaped code unit
/// is ASCII, so matching on raw `u16` values is exact. Shared by
/// `RegExp.prototype.source` / `RegExp.prototype.toString` / direct
/// property loads.
///
/// <https://tc39.es/ecma262/#sec-escaperegexppattern>
#[must_use]
pub fn escape_regexp_pattern_utf16(units: &[u16]) -> Vec<u16> {
    if units.is_empty() {
        return "(?:)".encode_utf16().collect();
    }
    let mut out = Vec::with_capacity(units.len());
    let mut in_class = false;
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        match u {
            0x5C => {
                // Backslash: emit it, then the escaped code unit. A line
                // terminator after `\` (an identity escape of the
                // terminator) must still be rendered in letter / `\u` form
                // — the already-emitted backslash makes `\n`, `\r`,
                // ` `, ` ` — so `source` stays a one-line literal.
                out.push(0x5C);
                if let Some(&next) = units.get(i + 1) {
                    match next {
                        0x0A => out.push(b'n' as u16),
                        0x0D => out.push(b'r' as u16),
                        0x2028 => out.extend("u2028".encode_utf16()),
                        0x2029 => out.extend("u2029".encode_utf16()),
                        other => out.push(other),
                    }
                    i += 1;
                }
            }
            0x5B => {
                in_class = true;
                out.push(0x5B);
            }
            0x5D => {
                in_class = false;
                out.push(0x5D);
            }
            0x2F if !in_class => {
                out.push(0x5C);
                out.push(0x2F);
            }
            0x0A => out.extend_from_slice(&[0x5C, b'n' as u16]),
            0x0D => out.extend_from_slice(&[0x5C, b'r' as u16]),
            0x2028 => out.extend("\\u2028".encode_utf16()),
            0x2029 => out.extend("\\u2029".encode_utf16()),
            other => out.push(other),
        }
        i += 1;
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
                uses_arguments_callee: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new().into(),
                spans: Vec::<SpanEntry>::new(),
                number_hint_sites: Vec::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    fn call(method: &str, recv: &Value, args: &[Value], interp: &mut Interpreter) -> Value {
        recv.as_regexp().expect("RegExp receiver");
        let context = empty_context();
        NativeCtx::with_host_context(interp, NativeCallInfo::call(*recv), Some(&context), |ctx| {
            let text = string_arg_to_jsstring_for_test(args, 0, ctx).unwrap();
            match method {
                "exec" => exec_once_native(recv, text, ctx).unwrap(),
                "test" => Value::boolean(!exec_once_native(recv, text, ctx).unwrap().is_null()),
                _ => panic!("unknown regexp test method {method}"),
            }
        })
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
