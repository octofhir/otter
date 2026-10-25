//! Structured stack traces: `Error.prepareStackTrace` and the call-site
//! objects it receives.
//!
//! When the realm's `Error` constructor carries a callable
//! `prepareStackTrace`, an error's `stack` is whatever that hook returns for
//! `(error, callSites)` instead of the default rendering. Each call site is
//! an ordinary object inheriting `%CallSite.prototype%`, whose methods
//! (`getFileName`, `getLineNumber`, `getFunctionName`, …) read the one frame
//! the call site carries.
//!
//! # Contents
//! - [`METHODS`] — the `%CallSite.prototype%` method table the error class
//!   registry installs at bootstrap.
//! - [`prepare_stack_trace`] — run the hook for an error's captured frames.
//!
//! # Invariants
//! - A call site keeps its frame in the same managed error-stack sidecar an
//!   error keeps its frames in: one frame, written once, no GC references.
//!   A call site has no `[[ErrorData]]`, so its own `stack` stays undefined.
//! - The hook never re-enters: a stack read while it runs renders with the
//!   default formatter.
//! - The hook runs with the `Error` constructor as its receiver, and its
//!   result is the `stack` value whatever its type.
//!
//! # See also
//! - [`crate::error_classes`] — the `stack` accessor and
//!   `Error.captureStackTrace`, the two places a stack is rendered.

use crate::native_function::NativeFastFn;
use crate::string::JsString;
use crate::{Local, NativeCtx, NativeError, NativeScope, Value, ValueWindow};

/// The receiver's frame: function name, module, `(line, zero-based
/// column)`, source offset.
type Frame = (String, String, Option<(u32, u32)>, u32);

fn frame(ctx: &NativeCtx<'_>, method: &'static str) -> Result<Frame, NativeError> {
    ctx.this_value()
        .as_object()
        .and_then(|obj| crate::object::error_stack_top_frame(obj, ctx.heap()))
        .ok_or_else(|| NativeError::TypeError {
            name: method,
            reason: "receiver is not a CallSite".to_string(),
        })
}

fn string(ctx: &mut NativeCtx<'_>, text: &str) -> Result<Value, NativeError> {
    Ok(Value::string(
        JsString::from_str(text, ctx.heap_mut()).map_err(NativeError::from)?,
    ))
}

fn number(value: f64) -> Value {
    Value::number(crate::number::NumberValue::from_f64(value))
}

/// Code compiled from a string (`eval`, `Function`) has no script name.
fn is_synthetic(module: &str) -> bool {
    matches!(module, "" | "<eval>" | "<anonymous>")
}

fn is_anonymous(name: &str) -> bool {
    name.is_empty() || name.starts_with('<')
}

fn get_file_name(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (_, module, _, _) = frame(ctx, "CallSite.prototype.getFileName")?;
    if is_synthetic(&module) {
        return Ok(Value::undefined());
    }
    string(ctx, &module)
}

fn get_line_number(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (_, _, position, _) = frame(ctx, "CallSite.prototype.getLineNumber")?;
    Ok(position.map_or_else(Value::null, |(line, _)| number(f64::from(line))))
}

fn get_column_number(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (_, _, position, _) = frame(ctx, "CallSite.prototype.getColumnNumber")?;
    Ok(position.map_or_else(Value::null, |(_, column)| number(f64::from(column) + 1.0)))
}

fn get_position(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (_, _, _, offset) = frame(ctx, "CallSite.prototype.getPosition")?;
    Ok(number(f64::from(offset)))
}

fn get_function_name(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (name, _, _, _) = frame(ctx, "CallSite.prototype.getFunctionName")?;
    if is_anonymous(&name) {
        return Ok(Value::null());
    }
    string(ctx, &name)
}

fn is_eval(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (_, module, _, _) = frame(ctx, "CallSite.prototype.isEval")?;
    Ok(Value::boolean(module == "<eval>"))
}

fn is_toplevel(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    frame(ctx, "CallSite.prototype.isToplevel")?;
    Ok(Value::boolean(true))
}

/// Facts a captured frame does not record: no receiver, method or promise
/// combinator is known, so these answer as for a plain strict-mode call.
fn answers_null(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    frame(ctx, "CallSite.prototype")?;
    Ok(Value::null())
}

fn answers_undefined(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    frame(ctx, "CallSite.prototype")?;
    Ok(Value::undefined())
}

fn answers_false(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    frame(ctx, "CallSite.prototype")?;
    Ok(Value::boolean(false))
}

fn get_script_hash(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    frame(ctx, "CallSite.prototype.getScriptHash")?;
    string(ctx, "")
}

fn to_string(ctx: &mut NativeCtx<'_>, _: ValueWindow<'_>) -> Result<Value, NativeError> {
    let (name, module, position, _) = frame(ctx, "CallSite.prototype.toString")?;
    let mut line = String::new();
    crate::error_classes::append_stack_frame(&mut line, &name, &module, position);
    let rendered = line.trim_start_matches("\n    at ").to_owned();
    string(ctx, &rendered)
}

/// `%CallSite.prototype%`: method name, `length`, native body.
pub(crate) const METHODS: &[(&str, u8, NativeFastFn)] = &[
    ("getFileName", 0, get_file_name),
    ("getScriptNameOrSourceURL", 0, get_file_name),
    ("getLineNumber", 0, get_line_number),
    ("getColumnNumber", 0, get_column_number),
    ("getEnclosingLineNumber", 0, get_line_number),
    ("getEnclosingColumnNumber", 0, get_column_number),
    ("getPosition", 0, get_position),
    ("getFunctionName", 0, get_function_name),
    ("getMethodName", 0, answers_null),
    ("getTypeName", 0, answers_null),
    ("getPromiseIndex", 0, answers_null),
    ("getThis", 0, answers_undefined),
    ("getFunction", 0, answers_undefined),
    ("getEvalOrigin", 0, answers_undefined),
    ("getScriptHash", 0, get_script_hash),
    ("isEval", 0, is_eval),
    ("isToplevel", 0, is_toplevel),
    ("isNative", 0, answers_false),
    ("isConstructor", 0, answers_false),
    ("isAsync", 0, answers_false),
    ("isPromiseAll", 0, answers_false),
    ("toString", 0, to_string),
];

/// Run `Error.prepareStackTrace(error, callSites)` over the frames `error`
/// captured. `None` when the realm's `Error` has no callable hook, or the
/// hook is already running.
pub(crate) fn prepare_stack_trace<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    error: Local<'_>,
) -> Result<Option<Local<'scope>>, NativeError> {
    if scope.with_turn_parts(|interp, _| interp.preparing_stack_trace) {
        return Ok(None);
    }
    let Some(constructor) = scope.global("Error") else {
        return Ok(None);
    };
    if !scope.is_object(constructor) {
        return Ok(None);
    }
    let hook = scope.get(constructor, "prepareStackTrace")?;
    if !scope.is_callable(hook) {
        return Ok(None);
    }
    let Some(error_obj) = scope.raw(error).as_object() else {
        return Ok(None);
    };
    let drafts = scope.with_turn_parts(|interp, _| {
        crate::object::error_stack_frame_drafts(error_obj, &interp.gc_heap)
    });
    let prototype = scope
        .with_turn_parts(|interp, _| Value::object(interp.error_classes.call_site_prototype()));
    let prototype = scope.value(prototype);
    let mut sites = Vec::with_capacity(drafts.len());
    for draft in &drafts {
        let site = scope.object_with_prototype(prototype)?;
        let raw = scope.raw(site);
        scope.with_turn_parts(|interp, _| {
            let mut obj = raw.as_object().expect("a call site is an object");
            crate::object::set_error_stack(&mut obj, &mut interp.gc_heap, draft)
        })?;
        sites.push(site);
    }
    let sites = scope.array_from(&sites)?;
    scope.with_turn_parts(|interp, _| interp.preparing_stack_trace = true);
    let result = scope.call(hook, constructor, &[error, sites]);
    scope.with_turn_parts(|interp, _| interp.preparing_stack_trace = false);
    result.map(Some)
}
