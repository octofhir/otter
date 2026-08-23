//! `internal/otter/error_source` hosted module.
//!
//! Backs the one native `internal/errors/error_source` reaches for.
//! `assert` builds its "expression evaluated to a falsy value" message by
//! capturing a stack onto a bare object, asking where the top frame points,
//! and tokenizing that source line — so the answer has to come from the
//! structured frame snapshot, not from the rendered `stack` string, which
//! that path deliberately never touches.
//!
//! # Contents
//! - [`error_source_cjs_value`] — the module's CommonJS export, one
//!   `getErrorSourcePositions` function.
//!
//! # Invariants
//! - A value with no captured frames, or one whose script the isolate no
//!   longer holds source for, answers empty coordinates rather than nothing:
//!   the caller destructures the result unconditionally.
//! - `startColumn` is a 0-based offset into `sourceLine`, which is what the
//!   caller indexes the line by.
//!
//! # See also
//! - `nodelib/internal/errors/error_source.js` — the vendored consumer.

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
    RuntimeValue as Value,
};

/// `getErrorSourcePositions(error)` — where the error's top captured frame
/// points, as `{ sourceLine, scriptResourceName, lineNumber, startColumn }`.
fn get_error_source_positions(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let error = args.first().copied().unwrap_or_else(Value::undefined);
    let position = ctx.error_source_position(&error);
    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let (source_line, script_name, line_number, start_column) = match &position {
            Some(position) => (
                position.source_line.as_str(),
                position.script_name.as_str(),
                position.line_number,
                position.start_column,
            ),
            // The caller destructures unconditionally, so an unlocatable
            // error answers with empty coordinates rather than nothing:
            // there is no source line to read, which is a different thing
            // from a failure to look.
            None => ("", "", 0, 0),
        };
        let source_line = scope.string(source_line)?;
        scope.set(result, "sourceLine", source_line)?;
        let script = scope.string(script_name)?;
        scope.set(result, "scriptResourceName", script)?;
        let line = scope.number(f64::from(line_number));
        scope.set(result, "lineNumber", line)?;
        let column = scope.number(f64::from(start_column));
        scope.set(result, "startColumn", column)?;
        Ok(scope.finish(result))
    })
}

/// Build the `internal/otter/error_source` CommonJS export.
///
/// # Errors
/// Returns a native error when the export object or its function cannot be
/// allocated.
pub fn error_source_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    let getter = scope.native_closure(
        "getErrorSourcePositions",
        1,
        &[],
        |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            get_error_source_positions(ctx, args)
        },
    )?;
    scope.set(object, "getErrorSourcePositions", getter)?;
    Ok(object)
}
