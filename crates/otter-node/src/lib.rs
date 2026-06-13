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

pub mod assert;
pub mod buffer;
pub mod events;
pub mod fs;
pub mod globals;
pub mod node_test;
pub mod os;
pub mod path;
pub mod querystring;
pub mod stream;
pub mod string_decoder;
pub mod stubs;
pub mod util;

pub use otter_runtime::otter_gc;
use otter_runtime::{HostedModule, HostedModuleInstall, OtterBuilder, RuntimeBuilder};

/// Active Node-compatible hosted modules in deterministic install order.
pub const HOSTED_MODULES: &[HostedModule] = &[
    HostedModule::new("node:fs", HostedModuleInstall::new(fs::install_fs_module)),
    HostedModule::new("fs", HostedModuleInstall::new(fs::install_fs_module)),
    HostedModule::new_with_cjs_value(
        "node:assert",
        HostedModuleInstall::new(assert::install_assert_module),
        assert::assert_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "assert",
        HostedModuleInstall::new(assert::install_assert_module),
        assert::assert_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:path",
        HostedModuleInstall::new(path::install_path_module),
        path::path_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "path",
        HostedModuleInstall::new(path::install_path_module),
        path::path_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:events",
        HostedModuleInstall::new(events::install_events_module),
        events::events_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "events",
        HostedModuleInstall::new(events::install_events_module),
        events::events_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:os",
        HostedModuleInstall::new(os::install_os_module),
        os::os_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "os",
        HostedModuleInstall::new(os::install_os_module),
        os::os_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:test",
        HostedModuleInstall::new(node_test::install_node_test_module),
        node_test::node_test_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "test",
        HostedModuleInstall::new(node_test::install_node_test_module),
        node_test::node_test_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:stream",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "stream",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:querystring",
        HostedModuleInstall::new(querystring::install_querystring_module),
        querystring::querystring_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "querystring",
        HostedModuleInstall::new(querystring::install_querystring_module),
        querystring::querystring_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:string_decoder",
        HostedModuleInstall::new(string_decoder::install_string_decoder_module),
        string_decoder::string_decoder_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "string_decoder",
        HostedModuleInstall::new(string_decoder::install_string_decoder_module),
        string_decoder::string_decoder_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:util",
        HostedModuleInstall::new(util::install_util_module),
        util::util_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "util",
        HostedModuleInstall::new(util::install_util_module),
        util::util_cjs_value,
    ),
    HostedModule::new("node:net", HostedModuleInstall::new(stubs::install_net)),
    HostedModule::new("net", HostedModuleInstall::new(stubs::install_net)),
    HostedModule::new(
        "node:worker_threads",
        HostedModuleInstall::new(stubs::install_worker_threads),
    ),
    HostedModule::new(
        "worker_threads",
        HostedModuleInstall::new(stubs::install_worker_threads),
    ),
    HostedModule::new_with_cjs_value(
        "node:buffer",
        HostedModuleInstall::new(buffer::install_buffer_module),
        buffer::buffer_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "buffer",
        HostedModuleInstall::new(buffer::install_buffer_module),
        buffer::buffer_cjs_value,
    ),
    HostedModule::new("node:url", HostedModuleInstall::new(stubs::install_url)),
    HostedModule::new("url", HostedModuleInstall::new(stubs::install_url)),
    HostedModule::new(
        "node:child_process",
        HostedModuleInstall::new(stubs::install_child_process),
    ),
    HostedModule::new(
        "child_process",
        HostedModuleInstall::new(stubs::install_child_process),
    ),
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
        self.with_nodejs_modules()
            .global_installer(globals::node_globals_installer())
            .hosted_modules(HOSTED_MODULES.iter().copied())
    }
}

impl NodeApiBuilderExt for OtterBuilder {
    fn with_node_apis(self) -> Self {
        self.with_nodejs_modules()
            .global_installer(globals::node_globals_installer())
            .hosted_modules(HOSTED_MODULES.iter().copied())
    }
}

pub(crate) fn type_error(
    name: &'static str,
    reason: impl Into<String>,
) -> otter_runtime::RuntimeNativeError {
    otter_runtime::runtime_type_error(name, reason)
}

/// A `TypeError` carrying Node's `ERR_INVALID_ARG_TYPE` code. Structured: the
/// code rides through the engine and lands as `error.code` on the instance.
pub(crate) fn invalid_arg_type(message: impl Into<String>) -> otter_runtime::RuntimeNativeError {
    otter_vm::NativeError::Coded {
        kind: otter_vm::ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_TYPE",
        message: message.into(),
    }
}

pub(crate) fn arg_string(
    args: &[otter_runtime::RuntimeValue],
    index: usize,
    _name: &'static str,
    heap: &otter_gc::GcHeap,
) -> Result<String, otter_runtime::RuntimeNativeError> {
    Ok(otter_runtime::runtime_arg_to_string(args, index, heap))
}

pub(crate) fn string_value(
    ctx: &mut otter_runtime::RuntimeNativeCtx<'_>,
    value: &str,
) -> Result<otter_runtime::RuntimeValue, otter_runtime::RuntimeNativeError> {
    otter_runtime::runtime_string_value(ctx, value)
}
