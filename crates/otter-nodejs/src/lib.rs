#![allow(missing_docs)]

//! Parked Node.js API compatibility shim.
//!
//! The legacy Node.js implementation depended on the retired VM stack. This
//! crate now stays compileable as a thin parking layer while the real Node.js
//! surface is redesigned on top of `otter-runtime` + `otter-vm`.

mod module_registry;

use std::cell::RefCell;
use std::sync::Arc;

use otter_runtime::HostedExtension;

pub use module_registry::NodeModuleEntry;

/// Node.js API compatibility profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeApiProfile {
    #[default]
    None,
    SafeCore,
    Full,
}

/// Placeholder provider retained only so parked callers can still construct a
/// profile-specific marker without pulling the legacy stack back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeModuleProvider {
    profile: NodeApiProfile,
}

impl NodeModuleProvider {
    #[must_use]
    pub const fn profile(self) -> NodeApiProfile {
        self.profile
    }
}

/// Get the list of supported built-in module specifiers from the parked table.
pub fn builtin_modules() -> &'static [&'static str] {
    module_registry::builtin_modules()
}

/// Get the list of safe profile built-in module specifiers from the parked table.
pub fn safe_builtin_modules() -> &'static [&'static str] {
    module_registry::safe_builtin_modules()
}

/// Check if a specifier refers to a parked Node.js built-in module.
pub fn is_builtin(specifier: &str) -> bool {
    is_builtin_for_profile(specifier, NodeApiProfile::Full)
}

/// Check if a specifier is allowed for the parked safe profile.
pub fn is_safe_builtin(specifier: &str) -> bool {
    is_builtin_for_profile(specifier, NodeApiProfile::SafeCore)
}

/// Check if a parked builtin is available for a specific profile.
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

/// The active runtime no longer loads Node.js extensions from this crate.
pub fn nodejs_extensions() -> Vec<Arc<dyn HostedExtension>> {
    Vec::new()
}

/// Create a parked provider marker for the full profile.
pub fn create_nodejs_provider() -> Arc<NodeModuleProvider> {
    create_nodejs_provider_for_profile(NodeApiProfile::Full)
}

/// Create a parked provider marker for the safe profile.
pub fn create_nodejs_safe_provider() -> Arc<NodeModuleProvider> {
    create_nodejs_provider_for_profile(NodeApiProfile::SafeCore)
}

/// Create a parked provider marker for the requested profile.
pub fn create_nodejs_provider_for_profile(profile: NodeApiProfile) -> Arc<NodeModuleProvider> {
    Arc::new(NodeModuleProvider { profile })
}

thread_local! {
    static PROCESS_ARGV_OVERRIDE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    static PROCESS_EXEC_ARGV_OVERRIDE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    static AUTO_SELECT_FAMILY: RefCell<Option<bool>> = const { RefCell::new(None) };
    static AUTO_SELECT_FAMILY_TIMEOUT: RefCell<Option<u64>> = const { RefCell::new(None) };
}

/// Override `process.argv` for parked compatibility tooling.
pub fn set_process_argv_override(argv: Option<Vec<String>>) {
    PROCESS_ARGV_OVERRIDE.with(|value| *value.borrow_mut() = argv);
}

/// Read the parked `process.argv` override.
pub fn get_process_argv_override() -> Option<Vec<String>> {
    PROCESS_ARGV_OVERRIDE.with(|value| value.borrow().clone())
}

/// Override `process.execArgv` for parked compatibility tooling.
pub fn set_process_exec_argv_override(argv: Option<Vec<String>>) {
    PROCESS_EXEC_ARGV_OVERRIDE.with(|value| *value.borrow_mut() = argv);
}

/// Read the parked `process.execArgv` override.
pub fn get_process_exec_argv_override() -> Option<Vec<String>> {
    PROCESS_EXEC_ARGV_OVERRIDE.with(|value| value.borrow().clone())
}

/// Override `net.setDefaultAutoSelectFamily` for parked compatibility tooling.
pub fn set_default_auto_select_family_override(value: Option<bool>) {
    AUTO_SELECT_FAMILY.with(|slot| *slot.borrow_mut() = value);
}

/// Override `net.setDefaultAutoSelectFamilyAttemptTimeout` for parked tooling.
pub fn set_default_auto_select_family_attempt_timeout_override(value: Option<u64>) {
    AUTO_SELECT_FAMILY_TIMEOUT.with(|slot| *slot.borrow_mut() = value);
}
