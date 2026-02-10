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
pub use provider::{
    NodeModuleProvider, create_nodejs_provider, create_nodejs_provider_for_profile,
    create_nodejs_safe_provider,
};

/// Node.js API compatibility profile.
///
/// - `None`: Node APIs are disabled.
/// - `SafeCore`: only non-host-control modules for embedded-safe usage.
/// - `Full`: full currently-implemented Node compatibility surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeApiProfile {
    #[default]
    None,
    SafeCore,
    Full,
}

const FULL_BUILTIN_MODULES: &[&str] = &[
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
];

const SAFE_BUILTIN_MODULES: &[&str] = &[
    "node:buffer",
    "node:events",
    "node:path",
    "node:util",
    "node:stream",
    "node:assert",
];

/// Create the main Node.js compatibility extension.
///
/// This bundles all Node.js API modules into a single extension
/// that can be registered with the Otter runtime.
pub fn create_nodejs_extension() -> Extension {
    create_nodejs_extension_for_profile(NodeApiProfile::Full)
}

/// Create a Node.js extension configured for a specific profile.
pub fn create_nodejs_extension_for_profile(profile: NodeApiProfile) -> Extension {
    match profile {
        NodeApiProfile::None => Extension::new("nodejs"),
        NodeApiProfile::SafeCore => Extension::new("nodejs")
            .with_ops(
                vec![
                    // Buffer ops
                    buffer::buffer_ops(),
                    // Path ops
                    path::path_ops(),
                ]
                .into_iter()
                .flatten()
                .collect(),
            )
            .with_js(include_str!("js/init_safe.js")),
        NodeApiProfile::Full => Extension::new("nodejs")
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
            .with_js(include_str!("js/init.js")),
    }
}

/// Create Node.js extension for embedded-safe profile.
pub fn create_nodejs_safe_extension() -> Extension {
    create_nodejs_extension_for_profile(NodeApiProfile::SafeCore)
}

/// Get the list of supported built-in module specifiers.
pub fn builtin_modules() -> &'static [&'static str] {
    FULL_BUILTIN_MODULES
}

/// Get the list of safe profile built-in module specifiers.
pub fn safe_builtin_modules() -> &'static [&'static str] {
    SAFE_BUILTIN_MODULES
}

fn normalize_builtin_name(specifier: &str) -> &str {
    specifier.strip_prefix("node:").unwrap_or(specifier)
}

/// Check if a specifier refers to a Node.js built-in module.
///
/// Supports both prefixed (`node:fs`) and bare (`fs`) specifiers.
pub fn is_builtin(specifier: &str) -> bool {
    is_builtin_for_profile(specifier, NodeApiProfile::Full)
}

/// Check if a specifier is allowed for the safe profile.
pub fn is_safe_builtin(specifier: &str) -> bool {
    is_builtin_for_profile(specifier, NodeApiProfile::SafeCore)
}

/// Check if a builtin is available for a specific profile.
pub fn is_builtin_for_profile(specifier: &str, profile: NodeApiProfile) -> bool {
    let name = normalize_builtin_name(specifier);
    let modules = match profile {
        NodeApiProfile::None => return false,
        NodeApiProfile::SafeCore => SAFE_BUILTIN_MODULES,
        NodeApiProfile::Full => FULL_BUILTIN_MODULES,
    };

    modules
        .iter()
        .filter_map(|module| module.strip_prefix("node:"))
        .any(|module| module == name)
}

/// Get the ESM source code for a Node.js built-in module.
///
/// Returns `Some(source)` if the module is supported, `None` otherwise.
pub fn get_builtin_source(name: &str) -> Option<&'static str> {
    get_builtin_source_for_profile(name, NodeApiProfile::Full)
}

/// Get ESM source code for a builtin under a given profile.
pub fn get_builtin_source_for_profile(name: &str, profile: NodeApiProfile) -> Option<&'static str> {
    let name = normalize_builtin_name(name);
    if !is_builtin_for_profile(name, profile) {
        return None;
    }

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
