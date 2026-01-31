//! Built-in JavaScript objects and functions for Otter VM
//!
//! This crate provides standard JavaScript built-in objects:
//! - `Object` - Object.keys(), Object.values(), Object.entries(), etc.
//! - `Array` - Array.isArray(), Array.from(), Array.prototype methods
//! - `Boolean` - Boolean constructor and Boolean.prototype methods
//! - `Date` - Date constructor and all Date.prototype methods
//! - `JSON` - JSON.parse(), JSON.stringify() with ES2024+ rawJSON support
//! - `Map` - Map and WeakMap collections (ES2026)
//! - `Math` - All ES2025 Math methods and constants
//! - `Number` - Number static methods and Number.prototype methods
//! - `RegExp` - Regular expressions with ES2026 RegExp.escape()
//! - `Set` - Set and WeakSet collections with ES2025 set methods
//! - `String` - String.prototype methods and String constructor methods
//! - `Promise` - Promise constructor, static methods, and prototype methods
//! - `Proxy` - Proxy constructor and Proxy.revocable()
//! - `Reflect` - Reflect static methods for metaprogramming
//! - `Temporal` - Modern date/time API (Temporal.Instant, Temporal.ZonedDateTime, etc.)
//! - `console` - Logging with pluggable adapter support

#![warn(clippy::all)]

pub mod array;
pub mod array_buffer;
pub mod boolean;
pub mod console;
pub mod data_view;
pub mod date;
pub mod error;
pub mod fetch;
pub mod function;
pub mod global;
pub mod http;
pub mod iterator;
pub mod json;
pub mod map;
pub mod math;
pub mod number;
pub mod object;
pub mod promise;
pub mod proxy;
pub mod reflect;
pub mod regexp;
pub mod set;
pub mod string;
pub mod symbol;
pub mod temporal;
pub mod typed_array;

pub use array::ArrayBuiltin;
pub use console::{ConsoleAdapter, LogLevel, StdConsole};
pub use object::ObjectBuiltin;

// Re-export CapabilitiesGuard from otter-vm-runtime for convenience
pub use otter_vm_runtime::CapabilitiesGuard;

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

    ops.extend(console::console_ops_with_adapter(adapter));
    ops.extend(object::ops());
    ops.extend(array::ops());
    ops.extend(array_buffer::ops());
    ops.extend(boolean::ops());
    ops.extend(date::ops());
    ops.extend(error::ops());
    // ops.extend(function::ops()); // Removed - conflicts with intrinsics
    ops.extend(global::ops());
    ops.extend(iterator::ops());
    ops.extend(json::ops());
    ops.extend(map::ops());
    ops.extend(math::ops());
    ops.extend(number::ops());
    ops.extend(promise::ops());
    ops.extend(proxy::ops());
    ops.extend(reflect::ops());
    ops.extend(regexp::ops());
    ops.extend(set::ops());
    ops.extend(string::ops());
    ops.extend(symbol::ops());
    ops.extend(temporal::ops());
    ops.extend(typed_array::ops());
    ops.extend(data_view::ops());
    ops.extend(fetch::ops());

    Extension::new("builtins").with_ops(ops)
}

/// Create HTTP server extension
///
/// This extension requires an event channel and active server counter
/// that integrate with the event loop.
///
/// # Example
/// ```ignore
/// use otter_vm_builtins::create_http_extension;
/// use tokio::sync::mpsc;
///
/// let (event_tx, event_rx) = mpsc::unbounded_channel();
/// let (ws_event_tx, ws_event_rx) = mpsc::unbounded_channel();
/// let ext = create_http_extension(event_tx, ws_event_tx, event_loop.get_active_server_count());
/// event_loop.set_http_receiver(event_rx);
/// event_loop.set_ws_receiver(ws_event_rx);
/// ```
pub fn create_http_extension(
    event_tx: tokio::sync::mpsc::UnboundedSender<otter_vm_runtime::HttpEvent>,
    ws_event_tx: tokio::sync::mpsc::UnboundedSender<otter_vm_runtime::WsEvent>,
    active_count: otter_vm_runtime::ActiveServerCount,
) -> Extension {
    Extension::new("http")
        .with_ops(http::ops(event_tx, ws_event_tx, active_count))
        .with_js(http::JS_SHIM)
}
