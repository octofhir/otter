//! Node.js API compatibility for Otter VM
//!
//! This crate provides Node.js-compatible APIs as native extensions for the Otter VM.
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
//! use otter_nodejs::nodejs_extensions;
//!
//! for ext in nodejs_extensions() {
//!     runtime.register_native_extension(ext);
//! }
//! ```

pub mod assert_ext;
pub mod buffer;
pub mod buffer_ext;
pub mod events_ext;
mod fs_core;
pub mod fs_ext;
mod module_registry;
pub mod os_ext;
pub mod path_ext;
pub mod process_ext;
pub mod provider;
mod security;
pub mod util_ext;

pub use module_registry::NodeModuleEntry;
use otter_vm_runtime::extension_v2::OtterExtension;
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

/// Get the list of supported built-in module specifiers.
pub fn builtin_modules() -> &'static [&'static str] {
    module_registry::builtin_modules()
}

/// Get the list of safe profile built-in module specifiers.
pub fn safe_builtin_modules() -> &'static [&'static str] {
    module_registry::safe_builtin_modules()
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
    module_registry::is_builtin_for_profile(specifier, profile)
}

/// Get the built-in module entry for a given profile.
pub fn get_builtin_entry_for_profile(
    name: &str,
    profile: NodeApiProfile,
) -> Option<&'static NodeModuleEntry> {
    module_registry::module_entry_for_profile(name, profile)
}

/// Get all native extensions for Node.js module compatibility.
///
/// These are zero-JS-shim extensions that provide native implementations
/// of Node.js modules (path, os, etc.).
pub fn nodejs_extensions() -> Vec<Box<dyn OtterExtension>> {
    vec![
        path_ext::node_path_extension(),
        os_ext::node_os_extension(),
        process_ext::node_process_extension(),
        fs_ext::node_fs_extension(),
        events_ext::node_events_extension(),
        util_ext::node_util_extension(),
        assert_ext::node_assert_extension(),
        buffer_ext::node_buffer_extension(),
    ]
}
