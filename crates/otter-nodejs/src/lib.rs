//! Node.js API compatibility for Otter VM
//!
//! This crate provides Node.js-compatible APIs as extensions for the Otter VM.
//!
//! # Modules
//!
//! - `buffer` - Binary data handling (Buffer class)
//! - `process` - Process object (env, cwd, exit, etc.)
//! - `events` - EventEmitter class
//! - `fs` - File system operations
//! - `path` - Path manipulation utilities
//!
//! # Usage
//!
//! ```rust,ignore
//! use otter_nodejs::create_nodejs_extension;
//!
//! let extension = create_nodejs_extension();
//! runtime.register_extension(extension)?;
//! ```

pub mod buffer;
pub mod events;
pub mod fs;
pub mod path;
pub mod process;
pub mod provider;

use otter_vm_runtime::Extension;
pub use provider::{NodeModuleProvider, create_nodejs_provider};

/// Create the main Node.js compatibility extension.
///
/// This bundles all Node.js API modules into a single extension
/// that can be registered with the Otter runtime.
pub fn create_nodejs_extension() -> Extension {
    Extension::new("nodejs")
        .with_ops(
            vec![
                // Buffer ops
                buffer::buffer_ops(),
                // Process ops
                process::process_ops(),
                // FS ops
                fs::fs_ops(),
                // Path ops
                path::path_ops(),
            ]
            .into_iter()
            .flatten()
            .collect(),
        )
        .with_js(include_str!("js/init.js"))
}

/// Get the list of supported built-in module specifiers.
pub fn builtin_modules() -> &'static [&'static str] {
    &[
        "node:buffer",
        "node:events",
        "node:fs",
        "node:fs/promises",
        "node:path",
        "node:process",
        "node:util",
        "node:stream",
        "node:assert",
        "node:os",
    ]
}

/// Check if a specifier refers to a Node.js built-in module.
///
/// Supports both prefixed (`node:fs`) and bare (`fs`) specifiers.
pub fn is_builtin(specifier: &str) -> bool {
    // Strip optional "node:" prefix
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    matches!(
        name,
        "buffer"
            | "events"
            | "fs"
            | "fs/promises"
            | "path"
            | "process"
            | "util"
            | "stream"
            | "assert"
            | "os"
    )
}

/// Get the ESM source code for a Node.js built-in module.
///
/// Returns `Some(source)` if the module is supported, `None` otherwise.
pub fn get_builtin_source(name: &str) -> Option<&'static str> {
    match name {
        "fs" => Some(include_str!("js/node_fs.js")),
        "fs/promises" => Some(include_str!("js/node_fs_promises.js")),
        "path" => Some(include_str!("js/node_path.js")),
        "buffer" => Some(include_str!("js/node_buffer.js")),
        "events" => Some(include_str!("js/node_events.js")),
        "process" => Some(include_str!("js/node_process.js")),
        "util" => Some(include_str!("js/node_util.js")),
        "stream" => Some(include_str!("js/node_stream.js")),
        "assert" => Some(include_str!("js/node_assert.js")),
        "os" => Some(include_str!("js/node_os.js")),
        _ => None,
    }
}
