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
pub mod child_process;
pub mod crypto;
pub mod diagnostics_channel;
pub mod events;
pub mod fs;
pub mod globals;
pub mod misc_modules;
pub mod node_test;
pub mod os;
pub mod path;
pub mod querystring;
pub mod readline;
pub mod stream;
pub mod string_decoder;
pub mod stubs;
pub mod timers;
pub mod util;
pub mod zlib;

pub use otter_runtime::otter_gc;
use otter_runtime::{HostedModule, HostedModuleInstall, OtterBuilder, RuntimeBuilder};

/// Active Node-compatible hosted modules in deterministic install order.
pub const HOSTED_MODULES: &[HostedModule] = &[
    HostedModule::new_with_cjs_value(
        "node:fs",
        HostedModuleInstall::new(fs::install_fs_module),
        fs::fs_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "fs",
        HostedModuleInstall::new(fs::install_fs_module),
        fs::fs_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:fs/promises",
        HostedModuleInstall::new(fs::install_fs_module),
        fs::fs_promises_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "fs/promises",
        HostedModuleInstall::new(fs::install_fs_module),
        fs::fs_promises_cjs_value,
    ),
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
        "node:stream/web",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_web_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "stream/web",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_web_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:stream/consumers",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_consumers_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "stream/consumers",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_consumers_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:stream/promises",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_promises_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "stream/promises",
        HostedModuleInstall::new(stream::install_stream_module),
        stream::stream_promises_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:timers",
        HostedModuleInstall::new(timers::install_noop),
        timers::timers_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "timers",
        HostedModuleInstall::new(timers::install_noop),
        timers::timers_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:timers/promises",
        HostedModuleInstall::new(timers::install_noop),
        timers::timers_promises_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "timers/promises",
        HostedModuleInstall::new(timers::install_noop),
        timers::timers_promises_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:readline",
        HostedModuleInstall::new(readline::install_readline_module),
        readline::readline_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "readline",
        HostedModuleInstall::new(readline::install_readline_module),
        readline::readline_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:readline/promises",
        HostedModuleInstall::new(readline::install_readline_module),
        readline::readline_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "readline/promises",
        HostedModuleInstall::new(readline::install_readline_module),
        readline::readline_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:cluster",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::cluster_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "cluster",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::cluster_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:crypto",
        HostedModuleInstall::new(crypto::install_crypto_module),
        crypto::crypto_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "crypto",
        HostedModuleInstall::new(crypto::install_crypto_module),
        crypto::crypto_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:zlib",
        HostedModuleInstall::new(zlib::install_zlib_module),
        zlib::zlib_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "zlib",
        HostedModuleInstall::new(zlib::install_zlib_module),
        zlib::zlib_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:perf_hooks",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::perf_hooks_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "perf_hooks",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::perf_hooks_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:v8",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::v8_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "v8",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::v8_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:module",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::module_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "module",
        HostedModuleInstall::new(misc_modules::install_noop),
        misc_modules::module_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "node:diagnostics_channel",
        HostedModuleInstall::new(diagnostics_channel::install_diagnostics_channel_module),
        diagnostics_channel::diagnostics_channel_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "diagnostics_channel",
        HostedModuleInstall::new(diagnostics_channel::install_diagnostics_channel_module),
        diagnostics_channel::diagnostics_channel_cjs_value,
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
    HostedModule::new_with_cjs_value(
        "node:child_process",
        HostedModuleInstall::new(child_process::install_child_process_module),
        child_process::child_process_cjs_value,
    ),
    HostedModule::new_with_cjs_value(
        "child_process",
        HostedModuleInstall::new(child_process::install_child_process_module),
        child_process::child_process_cjs_value,
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
