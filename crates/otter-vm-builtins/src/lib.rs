//! Built-in JavaScript objects and functions for Otter VM
//!
//! This crate provides standard JavaScript built-in objects:
//! - `Object` - Object.keys(), Object.values(), Object.entries(), etc.
//! - `Array` - Array.isArray(), Array.from(), Array.prototype methods
//! - `Math` - All ES2025 Math methods and constants
//! - `String` - String.prototype methods and String constructor methods
//! - `Number` - Number static methods and Number.prototype methods
//! - `console` - Logging with pluggable adapter support

#![warn(clippy::all)]

pub mod array;
pub mod console;
pub mod math;
pub mod number;
pub mod object;
pub mod string;

pub use array::ArrayBuiltin;
pub use console::{ConsoleAdapter, LogLevel, StdConsole};
pub use object::ObjectBuiltin;

use otter_vm_runtime::Extension;

/// Create extension with all built-ins (default console using println!)
pub fn create_builtins_extension() -> Extension {
    create_builtins_extension_with_console(StdConsole::default())
}

/// Create extension with custom console adapter
///
/// # Example
/// ```ignore
/// use otter_vm_builtins::{create_builtins_extension_with_console, ConsoleAdapter, LogLevel};
///
/// struct TracingConsole;
/// impl ConsoleAdapter for TracingConsole {
///     fn log(&self, level: LogLevel, message: &str) {
///         // Use tracing crate here
///     }
///     // ... other methods
/// }
///
/// let ext = create_builtins_extension_with_console(TracingConsole);
/// ```
pub fn create_builtins_extension_with_console<A: ConsoleAdapter>(adapter: A) -> Extension {
    let mut ops = Vec::new();

    ops.extend(object::ops());
    ops.extend(array::ops());
    ops.extend(math::ops());
    ops.extend(string::ops());
    ops.extend(number::ops());
    ops.extend(console::console_ops_with_adapter(adapter));

    Extension::new("builtins")
        .with_ops(ops)
        .with_js(include_str!("builtins.js"))
}
