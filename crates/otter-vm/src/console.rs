//! Console API — ES/WHATWG Console Standard.
//!
//! # Architecture
//!
//! The console implementation is split into two layers:
//!
//! 1. **`ConsoleBackend` trait** — defines where console output goes.
//!    Embedders implement this to route output to their own logging system
//!    (e.g., Axum → tracing, WASM → browser devtools, test harness → capture buffer).
//!
//! 2. **`StdioConsoleBackend`** — default implementation that writes to
//!    stdout/stderr via `println!`/`eprintln!`. Used by the CLI.
//!
//! The `RuntimeState` holds a `Box<dyn ConsoleBackend>` which the console
//! native functions delegate to. Embedders swap the backend before execution.

use crate::descriptors::{
    NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor, VmNativeCallError,
};
use crate::interpreter::RuntimeState;
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

// ---------------------------------------------------------------------------
// ConsoleBackend trait
// ---------------------------------------------------------------------------

/// Backend for console output. Embedders implement this to redirect
/// console.log/warn/error to their own logging infrastructure.
pub trait ConsoleBackend: Send {
    /// `console.log(...args)` — informational output.
    fn log(&self, message: &str);
    /// `console.warn(...args)` — warning output.
    fn warn(&self, message: &str);
    /// `console.error(...args)` — error output.
    fn error(&self, message: &str);
    /// `console.info(...args)` — informational (same as log in most impls).
    fn info(&self, message: &str) {
        self.log(message);
    }
    /// `console.debug(...args)` — debug output (same as log in most impls).
    fn debug(&self, message: &str) {
        self.log(message);
    }
    /// `console.trace(...args)` — output with stack trace (simplified: just message).
    fn trace(&self, message: &str) {
        self.log(message);
    }
    /// `console.dir(obj)` — object inspection (simplified: same as log).
    fn dir(&self, message: &str) {
        self.log(message);
    }
    /// `console.assert(condition, ...args)` — assertion.
    fn assert(&self, condition: bool, message: &str) {
        if !condition {
            self.error(&format!("Assertion failed: {message}"));
        }
    }
}

/// Default console backend that writes to stdout (log/info/debug)
/// and stderr (warn/error). Used by the CLI.
pub struct StdioConsoleBackend;

impl ConsoleBackend for StdioConsoleBackend {
    fn log(&self, message: &str) {
        println!("{message}");
    }

    fn warn(&self, message: &str) {
        eprintln!("{message}");
    }

    fn error(&self, message: &str) {
        eprintln!("{message}");
    }
}

/// Console backend that captures all output into a buffer.
/// Used for testing and test262 harness.
pub struct CaptureConsoleBackend {
    output: std::sync::Mutex<Vec<CapturedLine>>,
}

/// A single captured console output line.
#[derive(Debug, Clone)]
pub struct CapturedLine {
    pub level: ConsoleLevel,
    pub message: String,
}

/// Console output level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Warn,
    Error,
    Info,
    Debug,
}

impl CaptureConsoleBackend {
    pub fn new() -> Self {
        Self {
            output: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn lines(&self) -> Vec<CapturedLine> {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn text(&self) -> String {
        self.lines()
            .iter()
            .map(|l| l.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear(&self) {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl Default for CaptureConsoleBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleBackend for CaptureConsoleBackend {
    fn log(&self, message: &str) {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedLine {
                level: ConsoleLevel::Log,
                message: message.to_string(),
            });
    }

    fn warn(&self, message: &str) {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedLine {
                level: ConsoleLevel::Warn,
                message: message.to_string(),
            });
    }

    fn error(&self, message: &str) {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedLine {
                level: ConsoleLevel::Error,
                message: message.to_string(),
            });
    }
}

// ---------------------------------------------------------------------------
// Value formatting (args → string)
// ---------------------------------------------------------------------------

/// Formats a RegisterValue as a human-readable string for console output.
/// Handles numbers, strings, booleans, null, undefined, objects, arrays.
///
/// Takes `&RuntimeState` (immutable) so it can be called from console methods
/// without requiring mutable access to the runtime.
pub fn format_value(value: RegisterValue, runtime: &RuntimeState) -> String {
    if value == RegisterValue::undefined() {
        return "undefined".to_string();
    }
    if value == RegisterValue::null() {
        return "null".to_string();
    }
    if let Some(b) = value.as_bool() {
        return b.to_string();
    }
    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n.is_infinite() {
            return if n.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_string();
        }
        if n == 0.0 {
            return "0".to_string();
        }
        return format!("{n}");
    }
    if let Some(handle_id) = value.as_object_handle() {
        let handle = ObjectHandle(handle_id);
        // Try string value.
        if let Ok(Some(s)) = runtime.objects().string_value(handle) {
            return s.to_string();
        }
        // Array — show length since element access requires &mut.
        if let Ok(HeapValueKind::Array) = runtime.objects().kind(handle)
            && let Ok(Some(len)) = runtime.objects().array_length(handle)
        {
            return format!("[Array({len})]");
        }
        // Promise.
        if let Ok(HeapValueKind::Promise) = runtime.objects().kind(handle)
            && let Some(promise) = runtime.objects().get_promise(handle)
        {
            return match &promise.state {
                crate::promise::PromiseState::Pending => "Promise { <pending> }".to_string(),
                crate::promise::PromiseState::Fulfilled(v) => {
                    format!("Promise {{ {} }}", format_value(*v, runtime))
                }
                crate::promise::PromiseState::Rejected(v) => {
                    format!("Promise {{ <rejected> {} }}", format_value(*v, runtime))
                }
            };
        }
        // Function / closure.
        if let Ok(HeapValueKind::HostFunction | HeapValueKind::Closure) =
            runtime.objects().kind(handle)
        {
            return "[Function]".to_string();
        }
        // Generic object.
        return "[object Object]".to_string();
    }
    String::new()
}

/// Formats multiple arguments space-separated (like console.log does).
pub fn format_args(args: &[RegisterValue], runtime: &RuntimeState) -> String {
    args.iter()
        .map(|v| format_value(*v, runtime))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Console native functions
// ---------------------------------------------------------------------------

/// Returns all console method binding descriptors for installation on the
/// console object.
pub fn console_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("log", 0, console_log),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("warn", 0, console_warn),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("error", 0, console_error),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("info", 0, console_info),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("debug", 0, console_debug),
        ),
    ]
}

fn console_log(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = format_args(args, runtime);
    runtime.console().log(&message);
    Ok(RegisterValue::undefined())
}

fn console_warn(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = format_args(args, runtime);
    runtime.console().warn(&message);
    Ok(RegisterValue::undefined())
}

fn console_error(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = format_args(args, runtime);
    runtime.console().error(&message);
    Ok(RegisterValue::undefined())
}

fn console_info(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = format_args(args, runtime);
    runtime.console().info(&message);
    Ok(RegisterValue::undefined())
}

fn console_debug(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = format_args(args, runtime);
    runtime.console().debug(&message);
    Ok(RegisterValue::undefined())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_backend_doesnt_panic() {
        let backend = StdioConsoleBackend;
        backend.log("test log");
        backend.warn("test warn");
        backend.error("test error");
        backend.info("test info");
        backend.debug("test debug");
    }

    #[test]
    fn capture_backend_collects_output() {
        let backend = CaptureConsoleBackend::new();
        backend.log("hello");
        backend.warn("caution");
        backend.error("oops");

        let lines = backend.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].level, ConsoleLevel::Log);
        assert_eq!(lines[0].message, "hello");
        assert_eq!(lines[1].level, ConsoleLevel::Warn);
        assert_eq!(lines[2].level, ConsoleLevel::Error);
    }

    #[test]
    fn capture_backend_text_joins_lines() {
        let backend = CaptureConsoleBackend::new();
        backend.log("a");
        backend.log("b");
        assert_eq!(backend.text(), "a\nb");
    }

    #[test]
    fn capture_backend_clear() {
        let backend = CaptureConsoleBackend::new();
        backend.log("x");
        backend.clear();
        assert!(backend.lines().is_empty());
    }

    #[test]
    fn format_primitives() {
        let runtime = RuntimeState::new();
        assert_eq!(
            format_value(RegisterValue::undefined(), &runtime),
            "undefined"
        );
        assert_eq!(format_value(RegisterValue::null(), &runtime), "null");
        assert_eq!(
            format_value(RegisterValue::from_bool(true), &runtime),
            "true"
        );
        assert_eq!(
            format_value(RegisterValue::from_bool(false), &runtime),
            "false"
        );
        assert_eq!(format_value(RegisterValue::from_i32(42), &runtime), "42");
        assert_eq!(
            format_value(RegisterValue::from_number(1.25), &runtime),
            "1.25"
        );
        assert_eq!(
            format_value(RegisterValue::from_number(f64::NAN), &runtime),
            "NaN"
        );
        assert_eq!(
            format_value(RegisterValue::from_number(f64::INFINITY), &runtime),
            "Infinity"
        );
    }

    #[test]
    fn format_args_space_separated() {
        let runtime = RuntimeState::new();
        let result = format_args(
            &[
                RegisterValue::from_i32(1),
                RegisterValue::from_bool(true),
                RegisterValue::undefined(),
            ],
            &runtime,
        );
        assert_eq!(result, "1 true undefined");
    }
}
