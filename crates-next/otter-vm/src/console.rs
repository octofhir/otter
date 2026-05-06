//! Runtime-backed `console` global for the active VM.
//!
//! The console surface is intentionally small and host-facing:
//! `console.log` / `info` / `debug` write to stdout, `warn` /
//! `error` / `assert` write to stderr, and every method returns
//! `undefined`.
//!
//! # Contents
//! - [`CONSOLE_SPEC`] — static namespace spec used by bootstrap.
//! - [`install`] — allocate and attach the `console` object through
//!   the JS surface builder backend.
//! - Native method bodies for the common console methods.
//! - Formatting helpers shared by stdout and stderr paths.
//!
//! # Invariants
//! - Native functions receive the explicit [`crate::NativeCtx`]
//!   mutator context; no thread-local heap lookup is used.
//! - Console methods use static native function pointers and keep no
//!   hidden JS handles. The installed functions are strongly
//!   reachable only through `globalThis`.
//! - Error-shaped objects render through the same
//!   `Error.prototype.toString` helper used by uncaught exception
//!   diagnostics.
//!
//! # See also
//! - <https://console.spec.whatwg.org/>

use std::sync::Arc;

use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec};
use crate::{NativeCall, NativeCtx, NativeError, Value, error_classes, object};

/// Static namespace spec installed by the centralized bootstrap
/// registry.
pub static CONSOLE_SPEC: NamespaceSpec = NamespaceSpec {
    name: "console",
    methods: CONSOLE_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

/// Console output level selected by the invoked console method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConsoleLevel {
    /// `console.log`.
    Log,
    /// `console.info`.
    Info,
    /// `console.debug`.
    Debug,
    /// `console.warn`.
    Warn,
    /// `console.error`.
    Error,
    /// `console.trace`.
    Trace,
    /// Failed `console.assert`.
    Assert,
}

/// Embedder-overridable console sink.
///
/// The default sink writes `log` / `info` / `debug` to stdout and
/// `warn` / `error` / `trace` / failed `assert` to stderr using
/// `println!` / `eprintln!`.
pub trait ConsoleSink: Send + Sync + std::fmt::Debug + 'static {
    /// Write one console event. `fields` are already rendered in
    /// JavaScript argument order.
    fn write(&self, level: ConsoleLevel, fields: &[String]);
}

/// Shared console sink handle.
pub type ConsoleSinkHandle = Arc<dyn ConsoleSink>;

/// Default console sink backed by `println!` / `eprintln!`.
#[derive(Debug, Default)]
pub struct StdConsoleSink;

impl ConsoleSink for StdConsoleSink {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        let line = fields.join(" ");
        match level {
            ConsoleLevel::Log | ConsoleLevel::Info | ConsoleLevel::Debug => println!("{line}"),
            ConsoleLevel::Warn
            | ConsoleLevel::Error
            | ConsoleLevel::Trace
            | ConsoleLevel::Assert => {
                eprintln!("{line}")
            }
        }
    }
}

/// Build the default stdout/stderr console sink.
#[must_use]
pub fn default_console_sink() -> ConsoleSinkHandle {
    Arc::new(StdConsoleSink)
}

const CONSOLE_METHODS: &[MethodSpec] = &[
    method("log", console_log),
    method("info", console_info),
    method("debug", console_debug),
    method("warn", console_warn),
    method("error", console_error),
    method("trace", console_trace),
    method("assert", console_assert),
];

const fn method(
    name: &'static str,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length: 0,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

/// Install `globalThis.console`.
pub(crate) fn install(
    global_this: crate::JsObject,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<(), JsSurfaceError> {
    let console = NamespaceBuilder::from_spec(gc_heap, &CONSOLE_SPEC)?.build()?;
    if !object::define_own_property(
        global_this,
        gc_heap,
        CONSOLE_SPEC.name,
        crate::object::PropertyDescriptor::data(
            Value::Object(console),
            CONSOLE_SPEC.attrs.writable,
            CONSOLE_SPEC.attrs.enumerable,
            CONSOLE_SPEC.attrs.configurable,
        ),
    ) {
        return Err(JsSurfaceError::DefinePropertyFailed(CONSOLE_SPEC.name));
    }
    Ok(())
}

fn console_log(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write(ctx, ConsoleLevel::Log, args);
    Ok(Value::Undefined)
}

fn console_info(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write(ctx, ConsoleLevel::Info, args);
    Ok(Value::Undefined)
}

fn console_debug(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write(ctx, ConsoleLevel::Debug, args);
    Ok(Value::Undefined)
}

fn console_warn(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write(ctx, ConsoleLevel::Warn, args);
    Ok(Value::Undefined)
}

fn console_error(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    write(ctx, ConsoleLevel::Error, args);
    Ok(Value::Undefined)
}

fn console_trace(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let mut values = Vec::with_capacity(args.len() + 1);
    values.push("Trace".to_string());
    values.extend(format_args(ctx, args));
    ctx.interp_mut()
        .console_sink()
        .write(ConsoleLevel::Trace, &values);
    Ok(Value::Undefined)
}

fn console_assert(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if args.first().is_some_and(Value::to_boolean) {
        return Ok(Value::Undefined);
    }

    let mut values = Vec::new();
    values.push("Assertion failed".to_string());
    values.extend(format_args(ctx, args.get(1..).unwrap_or(&[])));
    ctx.interp_mut()
        .console_sink()
        .write(ConsoleLevel::Assert, &values);
    Ok(Value::Undefined)
}

fn write(ctx: &mut NativeCtx<'_>, level: ConsoleLevel, args: &[Value]) {
    let values = format_args(ctx, args);
    ctx.interp_mut().console_sink().write(level, &values);
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
