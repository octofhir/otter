//! Stack frame snapshots for V8-parity error stack traces.
//!
//! ECMAScript leaves `Error.prototype.stack` unspecified, but Node/V8 expose
//! captured frame metadata so user code (and the CLI reporter) can format
//! Java-style multi-line traces. This module owns the snapshot type and a
//! shadow stack threaded through `RuntimeState`.
//!
//! Spec references:
//! - §6.2.5 Execution contexts (the "running execution context" stack).
//!   <https://tc39.es/ecma262/#sec-execution-contexts>
//! - V8 stack trace API:
//!   <https://v8.dev/docs/stack-trace-api>

use std::fmt::Write as _;

use crate::bytecode::ProgramCounter;
use crate::module::{FunctionIndex, Module};
use crate::object::ObjectHandle;

/// Snapshot of a single execution-context activation captured at a point in
/// time, used to format `.stack` strings and CLI error reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrameInfo {
    /// Module that owns the function executing in this frame.
    pub module: Module,
    /// Function index inside the module.
    pub function_index: FunctionIndex,
    /// Pre-resolved function name, captured at frame entry. The function on
    /// the module may be renamed in source maps, but most installers set this
    /// at compile time, so we capture it eagerly.
    pub function_name: Option<Box<str>>,
    /// Program counter for this frame at the moment of capture. For the
    /// topmost frame this is `activation.pc()`; for parent frames this is
    /// the PC of the call site at which the parent descended into a child.
    pub pc: ProgramCounter,
    /// Closure handle for this frame, if any. `None` for host/top-level
    /// frames. Used by `Error.captureStackTrace(obj, constructorOpt)` to
    /// match the user-supplied frame to skip.
    pub closure_handle: Option<ObjectHandle>,
    /// Whether this frame represents a host (Rust) function.
    pub is_native: bool,
    /// Whether this frame is an async function call.
    pub is_async: bool,
    /// Whether this frame is a `[[Construct]]` invocation.
    pub is_construct: bool,
}

impl StackFrameInfo {
    /// Returns the resolved function display name, or `<anonymous>` when no
    /// name was captured.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.function_name.as_deref().unwrap_or("<anonymous>")
    }

    /// Returns the module URL/name as it should appear in stack traces.
    #[must_use]
    pub fn module_url(&self) -> &str {
        self.module.name().unwrap_or("<unknown>")
    }
}

/// Formats a captured shadow stack as a V8/Node.js compatible `Error.stack`
/// payload.
///
/// Shape:
/// ```text
/// <ErrorName>: <message>
///     at <fnName> (<url>:<line>:<col>)
///     at async <fnName> (<url>:<line>:<col>)
/// ```
///
/// Reference: <https://v8.dev/docs/stack-trace-api>
#[must_use]
pub fn format_v8_stack(error_name: &str, message: &str, frames: &[StackFrameInfo]) -> String {
    let mut out = String::new();
    if message.is_empty() {
        out.push_str(error_name);
    } else if error_name.is_empty() {
        out.push_str(message);
    } else {
        let _ = write!(out, "{error_name}: {message}");
    }
    for frame in frames {
        let fn_name = frame.display_name();
        let url = frame.module_url();
        let location = frame
            .module
            .function(frame.function_index)
            .and_then(|function| function.source_map().lookup(frame.pc));
        let (line, column) = match location {
            Some(loc) => (loc.line(), loc.column()),
            None => (0, 0),
        };
        let prefix = if frame.is_async { "    at async " } else { "    at " };
        if fn_name == "<anonymous>" {
            let _ = write!(out, "\n{prefix}{url}:{line}:{column}");
        } else {
            let _ = write!(out, "\n{prefix}{fn_name} ({url}:{line}:{column})");
        }
    }
    out
}
