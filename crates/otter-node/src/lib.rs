//! Opt-in Node.js-compatible hosted modules.
//!
//! This crate owns Node-specific module surfaces such as `node:fs` and `fs`.
//! It is intentionally separate from `otter-runtime`: embedders only receive
//! Node compatibility when they depend on this crate and call
//! [`NodeApiBuilderExt::with_node_apis`].
//!
//! # Contents
//! - [`fs`] - permission-gated `node:fs` / `fs` helpers.
//! - [`napi`] - stable Node-API ABI and `.node` dynamic-library loader.
//! - [`HOSTED_MODULES`] - static Node hosted-module specs.
//! - [`NodeApiBuilderExt`] - convenience helper for runtime builders.
//!
//! # Invariants
//! - Node modules are opt-in and are not installed by `otter-runtime` itself.
//! - Permission checks happen at the Rust boundary before host resources open.
//! - Native addons require both read and FFI capabilities and expose VM values
//!   through persistent-root-backed ABI handles.
//! - Host state is owned Rust data; no VM values, handles, or contexts are
//!   stored in futures or long-lived module state.
//!
//! # See also
//! - [`otter_runtime::CommonJsAddonLoader`]

pub mod assert;
pub mod buffer;
pub mod child_process;
pub mod crypto;
pub mod diagnostics_channel;
pub mod events;
pub mod fs;
pub mod globals;
pub mod internal_errors_ext;
pub mod internal_test_binding_ext;
pub mod misc_modules;
pub mod napi;
pub mod node_test;
pub mod os;
pub mod path;
pub mod querystring;
pub mod readline;
pub mod stream;
pub mod string_decoder;
pub mod stubs;
pub mod timers;
pub mod tty;
pub mod url;
pub mod util;
pub mod zlib;

pub use otter_runtime::otter_gc;
use otter_runtime::{HostedModule, OtterBuilder, RuntimeBuilder};

/// Active Node-compatible hosted modules in deterministic install order.
pub const HOSTED_MODULES: &[HostedModule] = &[
    HostedModule::new_with_cjs_value("node:fs", fs::install_fs_module, fs::fs_cjs_value),
    HostedModule::new_with_cjs_value("fs", fs::install_fs_module, fs::fs_cjs_value),
    HostedModule::cjs_only("node:fs/promises", fs::fs_promises_cjs_value),
    HostedModule::cjs_only("fs/promises", fs::fs_promises_cjs_value),
    HostedModule::cjs_only("node:assert", assert::assert_cjs_value),
    HostedModule::cjs_only("assert", assert::assert_cjs_value),
    HostedModule::cjs_only("node:assert/strict", assert::assert_strict_cjs_value),
    HostedModule::cjs_only("assert/strict", assert::assert_strict_cjs_value),
    HostedModule::cjs_only("internal/assert/myers_diff", assert::myers_diff_cjs_value),
    HostedModule::cjs_only("internal/util", misc_modules::internal_util_cjs_value),
    HostedModule::cjs_only(
        "internal/event_target",
        misc_modules::internal_event_target_cjs_value,
    ),
    HostedModule::cjs_only("internal/url", misc_modules::internal_url_cjs_value),
    HostedModule::cjs_only(
        "internal/errors",
        internal_errors_ext::internal_errors_cjs_value,
    ),
    HostedModule::cjs_only(
        "internal/test/binding",
        internal_test_binding_ext::internal_test_binding_cjs_value,
    ),
    HostedModule::cjs_only("node:vm", misc_modules::vm_cjs_value),
    HostedModule::cjs_only("vm", misc_modules::vm_cjs_value),
    HostedModule::cjs_only("node:process", misc_modules::process_cjs_value),
    HostedModule::cjs_only("process", misc_modules::process_cjs_value),
    HostedModule::new_with_cjs_value("node:path", path::install_path_module, path::path_cjs_value),
    HostedModule::new_with_cjs_value("path", path::install_path_module, path::path_cjs_value),
    HostedModule::cjs_only("node:events", events::events_cjs_value),
    HostedModule::cjs_only("events", events::events_cjs_value),
    HostedModule::new_with_cjs_value("node:os", os::install_os_module, os::os_cjs_value),
    HostedModule::new_with_cjs_value("os", os::install_os_module, os::os_cjs_value),
    HostedModule::cjs_only("node:test", node_test::node_test_cjs_value),
    HostedModule::cjs_only("test", node_test::node_test_cjs_value),
    HostedModule::cjs_only("node:stream", stream::stream_cjs_value),
    HostedModule::cjs_only("node:stream/web", stream::stream_web_cjs_value),
    HostedModule::cjs_only("stream/web", stream::stream_web_cjs_value),
    HostedModule::cjs_only("node:stream/consumers", stream::stream_consumers_cjs_value),
    HostedModule::cjs_only("stream/consumers", stream::stream_consumers_cjs_value),
    HostedModule::cjs_only("node:stream/promises", stream::stream_promises_cjs_value),
    HostedModule::cjs_only("stream/promises", stream::stream_promises_cjs_value),
    HostedModule::cjs_only("node:timers", timers::timers_cjs_value),
    HostedModule::cjs_only("timers", timers::timers_cjs_value),
    HostedModule::cjs_only("node:timers/promises", timers::timers_promises_cjs_value),
    HostedModule::cjs_only("timers/promises", timers::timers_promises_cjs_value),
    HostedModule::cjs_only("node:readline", readline::readline_cjs_value),
    HostedModule::cjs_only("readline", readline::readline_cjs_value),
    HostedModule::cjs_only("node:readline/promises", readline::readline_cjs_value),
    HostedModule::cjs_only("readline/promises", readline::readline_cjs_value),
    HostedModule::cjs_only("node:cluster", misc_modules::cluster_cjs_value),
    HostedModule::cjs_only("cluster", misc_modules::cluster_cjs_value),
    HostedModule::cjs_only("node:crypto", crypto::crypto_cjs_value),
    HostedModule::cjs_only("crypto", crypto::crypto_cjs_value),
    HostedModule::cjs_only("node:zlib", zlib::zlib_cjs_value),
    HostedModule::cjs_only("zlib", zlib::zlib_cjs_value),
    HostedModule::cjs_only("node:perf_hooks", misc_modules::perf_hooks_cjs_value),
    HostedModule::cjs_only("perf_hooks", misc_modules::perf_hooks_cjs_value),
    HostedModule::cjs_only("node:v8", misc_modules::v8_cjs_value),
    HostedModule::cjs_only("v8", misc_modules::v8_cjs_value),
    HostedModule::cjs_only("node:module", misc_modules::module_cjs_value),
    HostedModule::cjs_only("module", misc_modules::module_cjs_value),
    HostedModule::cjs_only(
        "node:diagnostics_channel",
        diagnostics_channel::diagnostics_channel_cjs_value,
    ),
    HostedModule::cjs_only(
        "diagnostics_channel",
        diagnostics_channel::diagnostics_channel_cjs_value,
    ),
    HostedModule::cjs_only("stream", stream::stream_cjs_value),
    HostedModule::cjs_only("node:querystring", querystring::querystring_cjs_value),
    HostedModule::cjs_only("querystring", querystring::querystring_cjs_value),
    HostedModule::cjs_only(
        "node:string_decoder",
        string_decoder::string_decoder_cjs_value,
    ),
    HostedModule::cjs_only("string_decoder", string_decoder::string_decoder_cjs_value),
    HostedModule::cjs_only("node:util", util::util_cjs_value),
    HostedModule::cjs_only("util", util::util_cjs_value),
    HostedModule::cjs_only("node:util/types", util::util_types_cjs_value),
    HostedModule::cjs_only("util/types", util::util_types_cjs_value),
    HostedModule::cjs_only("node:tty", tty::tty_cjs_value),
    HostedModule::cjs_only("tty", tty::tty_cjs_value),
    HostedModule::new("node:net", stubs::install_net),
    HostedModule::new("net", stubs::install_net),
    HostedModule::new("node:worker_threads", stubs::install_worker_threads),
    HostedModule::new("worker_threads", stubs::install_worker_threads),
    HostedModule::cjs_only("node:buffer", buffer::buffer_cjs_value),
    HostedModule::cjs_only("buffer", buffer::buffer_cjs_value),
    HostedModule::new_with_cjs_value("node:url", url::install_url_module, url::url_cjs_value),
    HostedModule::new_with_cjs_value("url", url::install_url_module, url::url_cjs_value),
    HostedModule::cjs_only("node:child_process", child_process::child_process_cjs_value),
    HostedModule::cjs_only("child_process", child_process::child_process_cjs_value),
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
            .commonjs_addon_loader(napi::load_addon)
            .global_installer(globals::node_globals_installer())
            .hosted_modules(HOSTED_MODULES.iter().copied())
    }
}

impl NodeApiBuilderExt for OtterBuilder {
    fn with_node_apis(self) -> Self {
        self.with_nodejs_modules()
            .commonjs_addon_loader(napi::load_addon)
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
