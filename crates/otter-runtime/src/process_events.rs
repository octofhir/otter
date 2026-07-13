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

use smallvec::{SmallVec, smallvec};

use otter_vm::{Attr, ErrorKind, HandleScope, NativeCall, NativeCtx, NativeError, Scoped, Value};

const EVENTS_SLOT: &str = "__otter_process_events__";
const MAX_LISTENERS_SLOT: &str = "__otter_process_max_listeners__";
const RECORD_EVENT: &str = "event";
const RECORD_LISTENER: &str = "listener";
const RECORD_ONCE: &str = "once";
const RECORD_ACTIVE: &str = "active";

pub(crate) fn install(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
    process: Scoped<'_>,
) -> Result<(), NativeError> {
    let listeners = ctx.scoped_array(scope, 0)?;
    ctx.scoped_define_data(
        scope,
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
    let max = ctx.scoped_number(scope, 10.0);
    ctx.scoped_define_data(
        scope,
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
    let count = ctx.scoped_number(scope, 0.0);
    ctx.scoped_set(scope, process, "_eventsCount", count)?;

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
        let function = ctx.scoped_native_call(scope, name, length, NativeCall::Static(call))?;
        ctx.scoped_define_data(
            scope,
            process,
            name,
            function,
            Attr::builtin_function().to_flags(),
        )?;
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

fn array_length(
    ctx: &mut NativeCtx<'_>,
    _scope: &HandleScope,
    array: Scoped<'_>,
) -> Result<usize, NativeError> {
    ctx.scoped_array_length(array)
}

fn same_value(ctx: &NativeCtx<'_>, left: Value, right: Value) -> bool {
    match (left.as_string(ctx.heap()), right.as_string(ctx.heap())) {
        (Some(left), Some(right)) => {
            left.to_utf16_vec(ctx.heap()) == right.to_utf16_vec(ctx.heap())
        }
        (None, None) => left == right,
        _ => false,
    }
}

fn record_is_active(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
    record: Scoped<'_>,
) -> Result<bool, NativeError> {
    let active = ctx.scoped_get(scope, record, RECORD_ACTIVE)?;
    Ok(ctx.escape(active).as_boolean().unwrap_or(false))
}

fn record_matches_event(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
    record: Scoped<'_>,
    event: Scoped<'_>,
) -> Result<bool, NativeError> {
    if !record_is_active(ctx, scope, record)? {
        return Ok(false);
    }
    let stored = ctx.scoped_get(scope, record, RECORD_EVENT)?;
    Ok(same_value(ctx, ctx.escape(stored), ctx.escape(event)))
}

fn active_event_count(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
    events: Scoped<'_>,
) -> Result<usize, NativeError> {
    let length = array_length(ctx, scope, events)?;
    let mut names: Vec<Value> = Vec::new();
    for index in 0..length {
        let record = ctx.scoped_get_index(scope, events, index)?;
        if !record_is_active(ctx, scope, record)? {
            continue;
        }
        let event = ctx.scoped_get(scope, record, RECORD_EVENT)?;
        let event = ctx.escape(event);
        if !names.iter().any(|name| same_value(ctx, *name, event)) {
            names.push(event);
        }
    }
    Ok(names.len())
}

fn refresh_event_count(
    ctx: &mut NativeCtx<'_>,
    scope: &HandleScope,
    process: Scoped<'_>,
    events: Scoped<'_>,
) -> Result<(), NativeError> {
    let count = active_event_count(ctx, scope, events)?;
    let count = ctx.scoped_number(scope, count as f64);
    ctx.scoped_set(scope, process, "_eventsCount", count)
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
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let event = ctx.scoped_value(scope, event);
        let listener = ctx.scoped_value(scope, listener);
        let events = ctx.scoped_get(scope, process, EVENTS_SLOT)?;
        let record = ctx.scoped_object_bare(scope)?;
        ctx.scoped_set(scope, record, RECORD_EVENT, event)?;
        ctx.scoped_set(scope, record, RECORD_LISTENER, listener)?;
        let once = ctx.scoped_boolean(scope, once);
        ctx.scoped_set(scope, record, RECORD_ONCE, once)?;
        let active = ctx.scoped_boolean(scope, true);
        ctx.scoped_set(scope, record, RECORD_ACTIVE, active)?;
        let length = array_length(ctx, scope, events)?;
        if prepend {
            for index in (0..length).rev() {
                let previous = ctx.scoped_get_index(scope, events, index)?;
                ctx.scoped_set_index(scope, events, index + 1, previous)?;
            }
            ctx.scoped_set_index(scope, events, 0, record)?;
        } else {
            ctx.scoped_set_index(scope, events, length, record)?;
        }
        refresh_event_count(ctx, scope, process, events)?;
        Ok(ctx.escape(process))
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
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let event = ctx.scoped_value(scope, event);
        let listener = ctx.scoped_value(scope, listener);
        let events = ctx.scoped_get(scope, process, EVENTS_SLOT)?;
        let length = array_length(ctx, scope, events)?;
        for index in (0..length).rev() {
            let record = ctx.scoped_get_index(scope, events, index)?;
            if !record_matches_event(ctx, scope, record, event)? {
                continue;
            }
            let stored = ctx.scoped_get(scope, record, RECORD_LISTENER)?;
            if same_value(ctx, ctx.escape(stored), ctx.escape(listener)) {
                let inactive = ctx.scoped_boolean(scope, false);
                ctx.scoped_set(scope, record, RECORD_ACTIVE, inactive)?;
                break;
            }
        }
        refresh_event_count(ctx, scope, process, events)?;
        Ok(ctx.escape(process))
    })
}

fn process_remove_all_listeners(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let selected = args.first().copied().map(validate_event).transpose()?;
    let process = *ctx.this_value();
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let selected = selected.map(|event| ctx.scoped_value(scope, event));
        let events = ctx.scoped_get(scope, process, EVENTS_SLOT)?;
        let length = array_length(ctx, scope, events)?;
        for index in 0..length {
            let record = ctx.scoped_get_index(scope, events, index)?;
            let remove = match selected {
                Some(event) => record_matches_event(ctx, scope, record, event)?,
                None => record_is_active(ctx, scope, record)?,
            };
            if remove {
                let inactive = ctx.scoped_boolean(scope, false);
                ctx.scoped_set(scope, record, RECORD_ACTIVE, inactive)?;
            }
        }
        refresh_event_count(ctx, scope, process, events)?;
        Ok(ctx.escape(process))
    })
}

fn emit_values(
    ctx: &mut NativeCtx<'_>,
    process: Value,
    event: Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let event = validate_event(event)?;
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let event = ctx.scoped_value(scope, event);
        let call_args: Vec<Scoped<'_>> = args
            .iter()
            .copied()
            .map(|value| ctx.scoped_value(scope, value))
            .collect();
        let events = ctx.scoped_get(scope, process, EVENTS_SLOT)?;
        let length = array_length(ctx, scope, events)?;
        let snapshot = ctx.scoped_array(scope, 0)?;
        let mut matched = 0usize;
        for index in 0..length {
            let record = ctx.scoped_get_index(scope, events, index)?;
            if !record_matches_event(ctx, scope, record, event)? {
                continue;
            }
            let listener = ctx.scoped_get(scope, record, RECORD_LISTENER)?;
            let once = ctx.scoped_get(scope, record, RECORD_ONCE)?;
            let entry = ctx.scoped_object_bare(scope)?;
            ctx.scoped_set(scope, entry, "record", record)?;
            ctx.scoped_set(scope, entry, RECORD_LISTENER, listener)?;
            ctx.scoped_set(scope, entry, RECORD_ONCE, once)?;
            ctx.scoped_set_index(scope, snapshot, matched, entry)?;
            matched += 1;
        }

        for index in 0..matched {
            let entry = ctx.scoped_get_index(scope, snapshot, index)?;
            let record = ctx.scoped_get(scope, entry, "record")?;
            let once = ctx.scoped_get(scope, entry, RECORD_ONCE)?;
            if ctx.escape(once).as_boolean().unwrap_or(false) {
                let inactive = ctx.scoped_boolean(scope, false);
                ctx.scoped_set(scope, record, RECORD_ACTIVE, inactive)?;
            }
            let listener = ctx.scoped_get(scope, entry, RECORD_LISTENER)?;
            let args: SmallVec<[Value; 8]> =
                call_args.iter().map(|handle| ctx.escape(*handle)).collect();
            ctx.call(ctx.escape(listener), ctx.escape(process), &args)?;
        }
        if matched > 0 {
            refresh_event_count(ctx, scope, process, events)?;
        }
        Ok(Value::boolean(matched > 0))
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
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let event = ctx.scoped_value(scope, event);
        let filter = filter.map(|value| ctx.scoped_value(scope, value));
        let events = ctx.scoped_get(scope, process, EVENTS_SLOT)?;
        let length = array_length(ctx, scope, events)?;
        let output = return_array
            .then(|| ctx.scoped_array(scope, 0))
            .transpose()?;
        let mut count = 0usize;
        for index in 0..length {
            let record = ctx.scoped_get_index(scope, events, index)?;
            if !record_matches_event(ctx, scope, record, event)? {
                continue;
            }
            let listener = ctx.scoped_get(scope, record, RECORD_LISTENER)?;
            if let Some(filter) = filter
                && !same_value(ctx, ctx.escape(listener), ctx.escape(filter))
            {
                continue;
            }
            if let Some(output) = output {
                ctx.scoped_set_index(scope, output, count, listener)?;
            }
            count += 1;
        }
        Ok(match output {
            Some(output) => ctx.escape(output),
            None => Value::number_f64(count as f64),
        })
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
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let events = ctx.scoped_get(scope, process, EVENTS_SLOT)?;
        let length = array_length(ctx, scope, events)?;
        let output = ctx.scoped_array(scope, 0)?;
        let mut names: Vec<Value> = Vec::new();
        for index in 0..length {
            let record = ctx.scoped_get_index(scope, events, index)?;
            if !record_is_active(ctx, scope, record)? {
                continue;
            }
            let event = ctx.scoped_get(scope, record, RECORD_EVENT)?;
            let value = ctx.escape(event);
            if names.iter().any(|name| same_value(ctx, *name, value)) {
                continue;
            }
            names.push(value);
            ctx.scoped_set_index(scope, output, names.len() - 1, event)?;
        }
        Ok(ctx.escape(output))
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
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let value = ctx.scoped_number(scope, value);
        ctx.scoped_set(scope, process, MAX_LISTENERS_SLOT, value)?;
        Ok(ctx.escape(process))
    })
}

fn process_get_max_listeners(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let process = *ctx.this_value();
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let value = ctx.scoped_get(scope, process, MAX_LISTENERS_SLOT)?;
        Ok(ctx.escape(value))
    })
}

fn value_string(ctx: &NativeCtx<'_>, value: Value) -> Option<String> {
    value
        .as_string(ctx.heap())
        .map(|string| string.to_lossy_string(ctx.heap()))
}

fn process_emit_warning(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = args.first().copied().unwrap_or_else(Value::undefined);
    let process = *ctx.this_value();
    let error_constructor = ctx
        .global_value("Error")
        .ok_or_else(|| invalid_arg("Error constructor is unavailable"))?;
    ctx.scope(|ctx, scope| {
        let process = ctx.scoped_value(scope, process);
        let input = ctx.scoped_value(scope, input);
        let error_constructor = ctx.scoped_value(scope, error_constructor);
        let arguments: Vec<Scoped<'_>> = args
            .iter()
            .copied()
            .map(|value| ctx.scoped_value(scope, value))
            .collect();
        let input_is_error =
            ctx.is_instance_of(ctx.escape(input), ctx.escape(error_constructor))?;
        let warning = if input_is_error {
            input
        } else {
            let message = value_string(ctx, ctx.escape(input)).ok_or_else(|| {
                invalid_arg("The \"warning\" argument must be of type string or an Error")
            })?;
            let mut warning_type = "Warning".to_string();
            let mut code = None;
            let mut detail = None;
            if let Some(second) = arguments.get(1).copied() {
                let second_value = ctx.escape(second);
                if let Some(value) = value_string(ctx, second_value) {
                    warning_type = if value.is_empty() {
                        "Warning".to_string()
                    } else {
                        value
                    };
                    if let Some(third) = arguments.get(2).copied() {
                        let third_value = ctx.escape(third);
                        if let Some(value) = value_string(ctx, third_value) {
                            code = Some(value);
                        } else if !third_value.is_undefined() && !third_value.is_callable() {
                            return Err(invalid_arg(
                                "The \"code\" argument must be of type string",
                            ));
                        }
                    }
                } else if second_value.as_object().is_some() && !second_value.is_callable() {
                    let kind = ctx.scoped_get(scope, second, "type")?;
                    if let Some(value) = value_string(ctx, ctx.escape(kind))
                        && !value.is_empty()
                    {
                        warning_type = value;
                    }
                    let option_code = ctx.scoped_get(scope, second, "code")?;
                    code = value_string(ctx, ctx.escape(option_code));
                    let option_detail = ctx.scoped_get(scope, second, "detail")?;
                    detail = value_string(ctx, ctx.escape(option_detail));
                } else if !second_value.is_callable() && !second_value.is_undefined() {
                    return Err(invalid_arg("The \"type\" argument must be of type string"));
                }
            }
            let message_value = ctx.scoped_string(scope, &message)?;
            let warning_value =
                ctx.construct(ctx.escape(error_constructor), &[ctx.escape(message_value)])?;
            let warning = ctx.scoped_value(scope, warning_value);
            let name = ctx.scoped_string(scope, &warning_type)?;
            ctx.scoped_set(scope, warning, "name", name)?;
            if let Some(code) = code {
                let code = ctx.scoped_string(scope, &code)?;
                ctx.scoped_set(scope, warning, "code", code)?;
            }
            if let Some(detail) = detail {
                let detail = ctx.scoped_string(scope, &detail)?;
                ctx.scoped_set(scope, warning, "detail", detail)?;
            }
            warning
        };

        let name = ctx.scoped_get(scope, warning, "name")?;
        let no_deprecation = ctx.scoped_get(scope, process, "noDeprecation")?;
        if value_string(ctx, ctx.escape(name)).as_deref() == Some("DeprecationWarning")
            && ctx.escape(no_deprecation).as_boolean().unwrap_or(false)
        {
            return Ok(Value::undefined());
        }

        let captures = smallvec![ctx.escape(process), ctx.escape(warning)];
        let task = ctx.native_value(
            "process warning dispatch",
            captures,
            |ctx, _args, captures| {
                let event = ctx.scope(|ctx, scope| {
                    let event = ctx.scoped_string(scope, "warning")?;
                    Ok::<Value, NativeError>(ctx.escape(event))
                })?;
                emit_values(ctx, captures[0], event, &captures[1..2])?;
                Ok(Value::undefined())
            },
        )?;
        let task = ctx.scoped_value(scope, task);
        ctx.queue_microtask(ctx.escape(task), std::iter::empty())?;
        Ok(Value::undefined())
    })
}
