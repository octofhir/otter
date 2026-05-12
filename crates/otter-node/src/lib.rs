//! Opt-in Node.js-compatible hosted modules.
//!
//! This crate owns Node-specific module surfaces such as `node:fs` and `fs`.
//! It is intentionally separate from `otter-runtime`: embedders only receive
//! Node compatibility when they depend on this crate and call
//! [`NodeApiBuilderExt::with_node_apis`].
//!
//! # Contents
//! - [`fs`] - permission-gated `node:fs` / `fs` helpers.
//! - [`HOSTED_MODULES`] - static Node hosted-module specs.
//! - [`NodeApiBuilderExt`] - convenience helper for runtime builders.
//!
//! # Invariants
//! - Node modules are opt-in and are not installed by `otter-runtime` itself.
//! - Permission checks happen at the Rust boundary before host resources open.
//! - Host state is owned Rust data; no VM values, handles, or contexts are
//!   stored in futures or long-lived module state.

pub mod fs;

use otter_runtime::{HostedModule, HostedModuleInstall, OtterBuilder, RuntimeBuilder};

/// Active Node-compatible hosted modules in deterministic install order.
pub const HOSTED_MODULES: &[HostedModule] = &[
    HostedModule::new("node:fs", HostedModuleInstall::new(fs::install_fs_module)),
    HostedModule::new("fs", HostedModuleInstall::new(fs::install_fs_module)),
];

/// Return active Node hosted module installers.
#[must_use]
pub const fn hosted_modules() -> &'static [HostedModule] {
    HOSTED_MODULES
}

/// Builder extension for opting into Node-compatible modules.
pub trait NodeApiBuilderExt: Sized {
    /// Install the active Node-compatible hosted modules.
    fn with_node_apis(self) -> Self;
}

impl NodeApiBuilderExt for RuntimeBuilder {
    fn with_node_apis(self) -> Self {
        self.hosted_modules(HOSTED_MODULES.iter().copied())
    }
}

impl NodeApiBuilderExt for OtterBuilder {
    fn with_node_apis(self) -> Self {
        self.hosted_modules(HOSTED_MODULES.iter().copied())
    }
}

pub(crate) fn type_error(
    name: &'static str,
    reason: impl Into<String>,
) -> otter_runtime::RuntimeNativeError {
    otter_runtime::runtime_type_error(name, reason)
}

pub(crate) fn arg_string(
    args: &[otter_runtime::RuntimeValue],
    index: usize,
    _name: &'static str,
) -> Result<String, otter_runtime::RuntimeNativeError> {
    Ok(otter_runtime::runtime_arg_to_string(args, index))
}

pub(crate) fn string_value(
    ctx: &mut otter_runtime::RuntimeNativeCtx<'_>,
    value: &str,
) -> Result<otter_runtime::RuntimeValue, otter_runtime::RuntimeNativeError> {
    otter_runtime::runtime_string_value(ctx, value)
}
