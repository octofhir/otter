//! EventEmitter behavior for the runtime-owned `process` global.
//!
//! Listener state is represented by JS-owned records stored on `process`.
//! Native methods only orchestrate those records through [`NativeCtx`] handle
//! scopes; no raw VM handles escape an allocating operation. Warnings are
//! queued as microtasks so listeners registered later in the current turn see
//! them, matching Node's observable `emitWarning` timing.
//!
//! # Contents
//! - [`install`] creates hidden listener state and installs EventEmitter methods.
//! - Listener add/remove/query operations over stable JS-owned records.
//! - Deferred `process.emitWarning` construction and delivery.
//!
//! # Invariants
//! - Incoming `Value`s enter a handle scope before the first allocation.
//! - Emission snapshots listeners before invoking user code.
//! - Once-listeners are deactivated before their callback is invoked.
//! - Invalid Node arguments surface structured `ERR_INVALID_ARG_TYPE` errors.
//!
//! # See also
//! - [`crate::process::install_global`]
//! - Node.js `lib/events.js` and `lib/internal/process/warning.js`.

use smallvec::smallvec;

use otter_vm::{Attr, ErrorKind, Local, NativeCall, NativeCtx, NativeError, NativeScope, Value};

const EVENTS_SLOT: &str = "__otter_process_events__";
const MAX_LISTENERS_SLOT: &str = "__otter_process_max_listeners__";
const RECORD_EVENT: &str = "event";
const RECORD_LISTENER: &str = "listener";
const RECORD_ONCE: &str = "once";
const RECORD_ACTIVE: &str = "active";

pub(crate) fn install(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
) -> Result<(), NativeError> {
    let listeners = scope.array(0)?;
    scope.define(
        process,
        EVENTS_SLOT,
        listeners,
        Attr {
            writable: true,
            enumerable: false,
            configurable: false,
        }
        .to_flags(),
    )?;
    let max = scope.number(10.0);
    scope.define(
        process,
        MAX_LISTENERS_SLOT,
        max,
        Attr {
            writable: true,
            enumerable: false,
            configurable: false,
        }
        .to_flags(),
    )?;
    let count = scope.number(0.0);
    scope.set(process, "_eventsCount", count)?;

    for (name, length, call) in [
        ("on", 2, process_on as _),
        ("addListener", 2, process_on as _),
        ("once", 2, process_once as _),
        ("prependListener", 2, process_prepend as _),
        ("prependOnceListener", 2, process_prepend_once as _),
        ("off", 2, process_remove_listener as _),
        ("removeListener", 2, process_remove_listener as _),
        ("removeAllListeners", 1, process_remove_all_listeners as _),
        ("emit", 1, process_emit as _),
        ("listenerCount", 1, process_listener_count as _),
        ("listeners", 1, process_listeners as _),
        ("rawListeners", 1, process_listeners as _),
        ("eventNames", 0, process_event_names as _),
        ("setMaxListeners", 1, process_set_max_listeners as _),
        ("getMaxListeners", 0, process_get_max_listeners as _),
        ("emitWarning", 1, process_emit_warning as _),
    ] {
        let function = scope.native_call(name, length, NativeCall::Static(call))?;
        scope.define(process, name, function, Attr::builtin_function().to_flags())?;
    }
    Ok(())
}

fn invalid_arg(message: impl Into<String>) -> NativeError {
    NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_TYPE",
        message: message.into(),
    }
}

fn validate_event(value: Value) -> Result<Value, NativeError> {
    if value.is_string() || value.is_symbol() {
        Ok(value)
    } else {
        Err(invalid_arg(
            "The \"type\" argument must be of type string or symbol",
        ))
    }
}

fn validate_listener(value: Value) -> Result<Value, NativeError> {
    if value.is_callable() {
        Ok(value)
    } else {
        Err(invalid_arg(
            "The \"listener\" argument must be of type function",
        ))
    }
}

fn array_length(scope: &NativeScope<'_, '_>, array: Local<'_>) -> Result<usize, NativeError> {
    scope.array_length(array)
}

fn same_value(scope: &NativeScope<'_, '_>, left: Local<'_>, right: Local<'_>) -> bool {
    scope.strict_equals(left, right)
}

fn record_is_active(
    scope: &mut NativeScope<'_, '_>,
    record: Local<'_>,
) -> Result<bool, NativeError> {
    let active = scope.get(record, RECORD_ACTIVE)?;
    Ok(scope.boolean_value(active).unwrap_or(false))
}

fn record_matches_event(
    scope: &mut NativeScope<'_, '_>,
    record: Local<'_>,
    event: Local<'_>,
) -> Result<bool, NativeError> {
    if !record_is_active(scope, record)? {
        return Ok(false);
    }
    let stored = scope.get(record, RECORD_EVENT)?;
    Ok(same_value(scope, stored, event))
}

fn active_event_count<'s>(
    scope: &mut NativeScope<'s, '_>,
    events: Local<'_>,
) -> Result<usize, NativeError> {
    let length = array_length(scope, events)?;
    let mut names: Vec<Local<'s>> = Vec::new();
    for index in 0..length {
        let record = scope.index(events, index)?;
        if !record_is_active(scope, record)? {
            continue;
        }
        let event = scope.get(record, RECORD_EVENT)?;
        if !names.iter().any(|name| same_value(scope, *name, event)) {
            names.push(event);
        }
    }
    Ok(names.len())
}

fn refresh_event_count(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    events: Local<'_>,
) -> Result<(), NativeError> {
    let count = active_event_count(scope, events)?;
    let count = scope.number(count as f64);
    scope.set(process, "_eventsCount", count)
}

fn add_listener(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    prepend: bool,
    once: bool,
) -> Result<Value, NativeError> {
    let event = validate_event(args.first().copied().unwrap_or_else(Value::undefined))?;
    let listener = validate_listener(args.get(1).copied().unwrap_or_else(Value::undefined))?;
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let event = scope.value(event);
        let listener = scope.value(listener);
        let events = scope.get(process, EVENTS_SLOT)?;
        let record = scope.bare_object()?;
        scope.set(record, RECORD_EVENT, event)?;
        scope.set(record, RECORD_LISTENER, listener)?;
        let once = scope.boolean(once);
        scope.set(record, RECORD_ONCE, once)?;
        let active = scope.boolean(true);
        scope.set(record, RECORD_ACTIVE, active)?;
        let length = array_length(&scope, events)?;
        if prepend {
            for index in (0..length).rev() {
                let previous = scope.index(events, index)?;
                scope.set_index(events, index + 1, previous)?;
            }
            scope.set_index(events, 0, record)?;
        } else {
            scope.set_index(events, length, record)?;
        }
        refresh_event_count(&mut scope, process, events)?;
        Ok(scope.finish(process))
    })
}

fn process_on(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    add_listener(ctx, args, false, false)
}

fn process_once(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    add_listener(ctx, args, false, true)
}

fn process_prepend(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    add_listener(ctx, args, true, false)
}

fn process_prepend_once(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    add_listener(ctx, args, true, true)
}

fn process_remove_listener(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let event = validate_event(args.first().copied().unwrap_or_else(Value::undefined))?;
    let listener = validate_listener(args.get(1).copied().unwrap_or_else(Value::undefined))?;
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let event = scope.value(event);
        let listener = scope.value(listener);
        let events = scope.get(process, EVENTS_SLOT)?;
        let length = array_length(&scope, events)?;
        for index in (0..length).rev() {
            let record = scope.index(events, index)?;
            if !record_matches_event(&mut scope, record, event)? {
                continue;
            }
            let stored = scope.get(record, RECORD_LISTENER)?;
            if same_value(&scope, stored, listener) {
                let inactive = scope.boolean(false);
                scope.set(record, RECORD_ACTIVE, inactive)?;
                break;
            }
        }
        refresh_event_count(&mut scope, process, events)?;
        Ok(scope.finish(process))
    })
}

fn process_remove_all_listeners(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let selected = args.first().copied().map(validate_event).transpose()?;
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let selected = selected.map(|event| scope.value(event));
        let events = scope.get(process, EVENTS_SLOT)?;
        let length = array_length(&scope, events)?;
        for index in 0..length {
            let record = scope.index(events, index)?;
            let remove = match selected {
                Some(event) => record_matches_event(&mut scope, record, event)?,
                None => record_is_active(&mut scope, record)?,
            };
            if remove {
                let inactive = scope.boolean(false);
                scope.set(record, RECORD_ACTIVE, inactive)?;
            }
        }
        refresh_event_count(&mut scope, process, events)?;
        Ok(scope.finish(process))
    })
}

fn emit_values(
    ctx: &mut NativeCtx<'_>,
    process: Value,
    event: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let event = validate_event(event)?;
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let event = scope.value(event);
        let call_args: Vec<Local<'_>> = args
            .iter()
            .copied()
            .map(|value| scope.value(value))
            .collect();
        let events = scope.get(process, EVENTS_SLOT)?;
        let length = array_length(&scope, events)?;
        let snapshot = scope.array(0)?;
        let mut matched = 0usize;
        for index in 0..length {
            let record = scope.index(events, index)?;
            if !record_matches_event(&mut scope, record, event)? {
                continue;
            }
            let listener = scope.get(record, RECORD_LISTENER)?;
            let once = scope.get(record, RECORD_ONCE)?;
            let entry = scope.bare_object()?;
            scope.set(entry, "record", record)?;
            scope.set(entry, RECORD_LISTENER, listener)?;
            scope.set(entry, RECORD_ONCE, once)?;
            scope.set_index(snapshot, matched, entry)?;
            matched += 1;
        }

        for index in 0..matched {
            let entry = scope.index(snapshot, index)?;
            let record = scope.get(entry, "record")?;
            let once = scope.get(entry, RECORD_ONCE)?;
            if scope.boolean_value(once).unwrap_or(false) {
                let inactive = scope.boolean(false);
                scope.set(record, RECORD_ACTIVE, inactive)?;
            }
            let listener = scope.get(entry, RECORD_LISTENER)?;
            scope.call(listener, process, &call_args)?;
        }
        if matched > 0 {
            refresh_event_count(&mut scope, process, events)?;
        }
        let result = scope.boolean(matched > 0);
        Ok(scope.finish(result))
    })
}

fn process_emit(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let event = args.first().copied().unwrap_or_else(Value::undefined);
    emit_values(
        ctx,
        *ctx.this_value(),
        event,
        args.get(1..).unwrap_or_default(),
    )
}

fn listener_values(
    ctx: &mut NativeCtx<'_>,
    event: Value,
    filter: Option<Value>,
    return_array: bool,
) -> Result<Value, NativeError> {
    let event = validate_event(event)?;
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let event = scope.value(event);
        let filter = filter.map(|value| scope.value(value));
        let events = scope.get(process, EVENTS_SLOT)?;
        let length = array_length(&scope, events)?;
        let output = return_array.then(|| scope.array(0)).transpose()?;
        let mut count = 0usize;
        for index in 0..length {
            let record = scope.index(events, index)?;
            if !record_matches_event(&mut scope, record, event)? {
                continue;
            }
            let listener = scope.get(record, RECORD_LISTENER)?;
            if let Some(filter) = filter
                && !same_value(&scope, listener, filter)
            {
                continue;
            }
            if let Some(output) = output {
                scope.set_index(output, count, listener)?;
            }
            count += 1;
        }
        let result = match output {
            Some(output) => output,
            None => scope.number(count as f64),
        };
        Ok(scope.finish(result))
    })
}

fn process_listener_count(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    listener_values(
        ctx,
        args.first().copied().unwrap_or_else(Value::undefined),
        args.get(1).copied(),
        false,
    )
}

fn process_listeners(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    listener_values(
        ctx,
        args.first().copied().unwrap_or_else(Value::undefined),
        None,
        true,
    )
}

fn process_event_names(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let events = scope.get(process, EVENTS_SLOT)?;
        let length = array_length(&scope, events)?;
        let output = scope.array(0)?;
        let mut names: Vec<Local<'_>> = Vec::new();
        for index in 0..length {
            let record = scope.index(events, index)?;
            if !record_is_active(&mut scope, record)? {
                continue;
            }
            let event = scope.get(record, RECORD_EVENT)?;
            if names.iter().any(|name| same_value(&scope, *name, event)) {
                continue;
            }
            names.push(event);
            scope.set_index(output, names.len() - 1, event)?;
        }
        Ok(scope.finish(output))
    })
}

fn process_set_max_listeners(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let value = args
        .first()
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| invalid_arg("The \"n\" argument must be a non-negative number"))?;
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let value = scope.number(value);
        scope.set(process, MAX_LISTENERS_SLOT, value)?;
        Ok(scope.finish(process))
    })
}

fn process_get_max_listeners(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let process = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(process);
        let value = scope.get(process, MAX_LISTENERS_SLOT)?;
        Ok(scope.finish(value))
    })
}

fn value_string(scope: &NativeScope<'_, '_>, value: Local<'_>) -> Option<String> {
    scope
        .is_string(value)
        .then(|| scope.string_value(value).ok())
        .flatten()
}

fn process_emit_warning(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = args.first().copied().unwrap_or_else(Value::undefined);
    let process = *ctx.this_value();
    let error_constructor = ctx
        .global_value("Error")
        .ok_or_else(|| invalid_arg("Error constructor is unavailable"))?;
    let captures = ctx.scope(|mut scope| {
        let process = scope.value(process);
        let input = scope.value(input);
        let error_constructor = scope.value(error_constructor);
        let arguments: Vec<Local<'_>> = args
            .iter()
            .copied()
            .map(|value| scope.value(value))
            .collect();
        let input_is_error = scope.is_instance_of(input, error_constructor)?;
        let warning = if input_is_error {
            input
        } else {
            let message = value_string(&scope, input).ok_or_else(|| {
                invalid_arg("The \"warning\" argument must be of type string or an Error")
            })?;
            let mut warning_type = "Warning".to_string();
            let mut code = None;
            let mut detail = None;
            if let Some(second) = arguments.get(1).copied() {
                if let Some(value) = value_string(&scope, second) {
                    warning_type = if value.is_empty() {
                        "Warning".to_string()
                    } else {
                        value
                    };
                    if let Some(third) = arguments.get(2).copied() {
                        if let Some(value) = value_string(&scope, third) {
                            code = Some(value);
                        } else if !scope.is_undefined(third) && !scope.is_callable(third) {
                            return Err(invalid_arg(
                                "The \"code\" argument must be of type string",
                            ));
                        }
                    }
                } else if scope.is_object(second) && !scope.is_callable(second) {
                    let kind = scope.get(second, "type")?;
                    if let Some(value) = value_string(&scope, kind)
                        && !value.is_empty()
                    {
                        warning_type = value;
                    }
                    let option_code = scope.get(second, "code")?;
                    code = value_string(&scope, option_code);
                    let option_detail = scope.get(second, "detail")?;
                    detail = value_string(&scope, option_detail);
                } else if !scope.is_callable(second) && !scope.is_undefined(second) {
                    return Err(invalid_arg("The \"type\" argument must be of type string"));
                }
            }
            let message_value = scope.string(&message)?;
            let warning = scope.construct(error_constructor, &[message_value])?;
            let name = scope.string(&warning_type)?;
            scope.set(warning, "name", name)?;
            if let Some(code) = code {
                let code = scope.string(&code)?;
                scope.set(warning, "code", code)?;
            }
            if let Some(detail) = detail {
                let detail = scope.string(&detail)?;
                scope.set(warning, "detail", detail)?;
            }
            warning
        };

        let name = scope.get(warning, "name")?;
        let no_deprecation = scope.get(process, "noDeprecation")?;
        if value_string(&scope, name).as_deref() == Some("DeprecationWarning")
            && scope.boolean_value(no_deprecation).unwrap_or(false)
        {
            return Ok::<Option<Value>, NativeError>(None);
        }

        let captures = scope.array(2)?;
        scope.set_index(captures, 0, process)?;
        scope.set_index(captures, 1, warning)?;
        Ok(Some(scope.finish(captures)))
    })?;
    let Some(captures) = captures else {
        return Ok(Value::undefined());
    };
    let task = ctx.native_value(
        "process warning dispatch",
        smallvec![captures],
        |ctx, _args, captures| {
            let capture = captures
                .first()
                .copied()
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_arg("missing process warning captures"))?;
            let process = otter_vm::array::get(capture, ctx.heap(), 0);
            let warning = otter_vm::array::get(capture, ctx.heap(), 1);
            let event = ctx.scope(|mut scope| {
                let event = scope.string("warning")?;
                Ok::<Value, NativeError>(scope.finish(event))
            })?;
            emit_values(ctx, process, event, &[warning])?;
            Ok(Value::undefined())
        },
    )?;
    ctx.scope(|mut scope| {
        let task = scope.value(task);
        scope.queue_microtask(task, &[])?;
        let undefined = scope.undefined();
        Ok(scope.finish(undefined))
    })
}
