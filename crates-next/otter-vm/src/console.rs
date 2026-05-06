//! Runtime-backed `console` global for the active VM.
//!
//! The console surface is intentionally small and host-facing:
//! `console.log` / `info` / `debug` write to stdout, `warn` /
//! `error` / `assert` write to stderr, and every method returns
//! `undefined`.
//!
//! # Contents
//! - [`install`] — allocate and attach the `console` object.
//! - Native method bodies for the common console methods.
//! - Formatting helpers shared by stdout and stderr paths.
//!
//! # Invariants
//! - Native functions receive the explicit [`crate::NativeCtx`]
//!   mutator context; no thread-local heap lookup is used.
//! - Console closures keep no hidden JS handles. The installed
//!   functions are strongly reachable only through `globalThis`.
//! - Error-shaped objects render through the same
//!   `Error.prototype.toString` helper used by uncaught exception
//!   diagnostics.
//!
//! # See also
//! - <https://console.spec.whatwg.org/>

use crate::{NativeCtx, NativeError, NativeFunction, Value, error_classes, object};

/// Install `globalThis.console`.
pub(crate) fn install(
    global_this: crate::JsObject,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<(), otter_gc::OutOfMemory> {
    let console = object::alloc_object(gc_heap)?;
    install_method(gc_heap, console, "log", console_log)?;
    install_method(gc_heap, console, "info", console_info)?;
    install_method(gc_heap, console, "debug", console_debug)?;
    install_method(gc_heap, console, "warn", console_warn)?;
    install_method(gc_heap, console, "error", console_error)?;
    install_method(gc_heap, console, "trace", console_trace)?;
    install_method(gc_heap, console, "assert", console_assert)?;
    object::set(global_this, gc_heap, "console", Value::Object(console));
    Ok(())
}

type ConsoleFn = for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;

fn install_method(
    gc_heap: &mut otter_gc::GcHeap,
    console: crate::JsObject,
    name: &'static str,
    call: ConsoleFn,
) -> Result<(), otter_gc::OutOfMemory> {
    let native = NativeFunction::new(gc_heap, name, call)?;
    object::set(console, gc_heap, name, Value::NativeFunction(native));
    Ok(())
}

fn console_log(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_stdout(ctx, args);
    Ok(Value::Undefined)
}

fn console_info(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_stdout(ctx, args);
    Ok(Value::Undefined)
}

fn console_debug(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_stdout(ctx, args);
    Ok(Value::Undefined)
}

fn console_warn(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_stderr(ctx, args);
    Ok(Value::Undefined)
}

fn console_error(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write_stderr(ctx, args);
    Ok(Value::Undefined)
}

fn console_trace(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut values = Vec::with_capacity(args.len() + 1);
    values.push("Trace".to_string());
    values.extend(format_args(ctx, args));
    eprintln!("{}", values.join(" "));
    Ok(Value::Undefined)
}

fn console_assert(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if args.first().is_some_and(Value::to_boolean) {
        return Ok(Value::Undefined);
    }

    let mut values = Vec::new();
    values.push("Assertion failed".to_string());
    values.extend(format_args(ctx, args.get(1..).unwrap_or(&[])));
    eprintln!("{}", values.join(" "));
    Ok(Value::Undefined)
}

fn write_stdout(ctx: &mut NativeCtx<'_>, args: &[Value]) {
    println!("{}", format_args(ctx, args).join(" "));
}

fn write_stderr(ctx: &mut NativeCtx<'_>, args: &[Value]) {
    eprintln!("{}", format_args(ctx, args).join(" "));
}

fn format_args(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Vec<String> {
    let heap = ctx.heap();
    args.iter()
        .map(|value| match value {
            Value::Object(obj)
                if object::get(*obj, heap, "name").is_some()
                    || object::get(*obj, heap, "message").is_some() =>
            {
                let rendered = error_classes::render_error_to_string(value, heap);
                if rendered.is_empty() {
                    value.display_string()
                } else {
                    rendered
                }
            }
            _ => value.display_string(),
        })
        .collect()
}
