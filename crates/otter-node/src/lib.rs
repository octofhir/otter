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
pub mod async_hooks;
mod buffer;
pub mod child_process;
pub mod crypto;
pub mod dgram;
pub mod diagnostics_channel;
pub mod dns;
pub mod events;
pub mod fs;
pub mod globals;
pub mod internal_errors_ext;
pub mod internal_test_binding_ext;
mod nodelib;
pub mod misc_modules;
pub mod napi;
pub mod net;
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
    HostedModule::cjs_only("__fsnative", fs::fs_native_cjs_value),
    HostedModule::cjs_only("node:fs/promises", fs::fs_promises_cjs_value),
    HostedModule::cjs_only("fs/promises", fs::fs_promises_cjs_value),
    HostedModule::cjs_only("node:assert", assert::assert_cjs_value),
    HostedModule::cjs_only("assert", assert::assert_cjs_value),
    HostedModule::cjs_only("node:assert/strict", assert::assert_strict_cjs_value),
    HostedModule::cjs_only("assert/strict", assert::assert_strict_cjs_value),
    HostedModule::cjs_only("internal/assert/myers_diff", assert::myers_diff_cjs_value),
    HostedModule::cjs_only("internal/assert/calltracker", assert::calltracker_cjs_value),
    HostedModule::cjs_only("internal/url", misc_modules::internal_url_cjs_value),
    HostedModule::cjs_only(
        "internal/test/binding",
        internal_test_binding_ext::internal_test_binding_cjs_value,
    ),
    HostedModule::cjs_only("node:async_hooks", async_hooks::async_hooks_cjs_value),
    HostedModule::cjs_only("async_hooks", async_hooks::async_hooks_cjs_value),
    HostedModule::cjs_only("node:dgram", dgram::dgram_cjs_value),
    HostedModule::cjs_only("dgram", dgram::dgram_cjs_value),
    HostedModule::cjs_only("node:dns", dns::dns_cjs_value),
    HostedModule::cjs_only("dns", dns::dns_cjs_value),
    HostedModule::cjs_only("node:dns/promises", dns::dns_cjs_value),
    HostedModule::cjs_only("dns/promises", dns::dns_cjs_value),
    HostedModule::cjs_only("node:domain", misc_modules::domain_cjs_value),
    HostedModule::cjs_only("domain", misc_modules::domain_cjs_value),
    HostedModule::cjs_only("node:console", misc_modules::console_cjs_value),
    HostedModule::cjs_only("console", misc_modules::console_cjs_value),
    HostedModule::cjs_only("node:vm", misc_modules::vm_cjs_value),
    HostedModule::cjs_only("vm", misc_modules::vm_cjs_value),
    HostedModule::cjs_only("node:process", misc_modules::process_cjs_value),
    HostedModule::cjs_only("process", misc_modules::process_cjs_value),
    HostedModule::new_with_cjs_value("node:path", path::install_path_module, path::path_cjs_value),
    HostedModule::new_with_cjs_value("path", path::install_path_module, path::path_cjs_value),
    HostedModule::cjs_only("node:events", nodelib::node_events),
    HostedModule::cjs_only("events", nodelib::node_events),
    HostedModule::new_with_cjs_value("node:os", os::install_os_module, os::os_cjs_value),
    HostedModule::new_with_cjs_value("os", os::install_os_module, os::os_cjs_value),
    HostedModule::cjs_only("node:test", node_test::node_test_cjs_value),
    HostedModule::cjs_only("test", node_test::node_test_cjs_value),
    HostedModule::cjs_only("node:stream", nodelib::node_stream),
    HostedModule::cjs_only("node:stream/web", stream::stream_web_cjs_value),
    HostedModule::cjs_only("stream/web", stream::stream_web_cjs_value),
    HostedModule::cjs_only("node:stream/consumers", stream::stream_consumers_cjs_value),
    HostedModule::cjs_only("stream/consumers", stream::stream_consumers_cjs_value),
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
    HostedModule::cjs_only("__cryptonative", crypto::crypto_native_cjs_value),
    HostedModule::cjs_only("node:zlib", zlib::zlib_cjs_value),
    HostedModule::cjs_only("zlib", zlib::zlib_cjs_value),
    HostedModule::cjs_only("__zlibnative", zlib::zlib_native_cjs_value),
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
    HostedModule::cjs_only("stream", nodelib::node_stream),
    HostedModule::cjs_only("node:querystring", querystring::querystring_cjs_value),
    HostedModule::cjs_only("querystring", querystring::querystring_cjs_value),
    HostedModule::cjs_only(
        "node:string_decoder",
        string_decoder::string_decoder_cjs_value,
    ),
    HostedModule::cjs_only("string_decoder", string_decoder::string_decoder_cjs_value),
    HostedModule::cjs_only("node:util", nodelib::node_util),
    HostedModule::cjs_only("util", nodelib::node_util),
    HostedModule::cjs_only("node:util/types", util::util_types_cjs_value),
    HostedModule::cjs_only("util/types", util::util_types_cjs_value),
    HostedModule::cjs_only("node:tty", tty::tty_cjs_value),
    HostedModule::cjs_only("tty", tty::tty_cjs_value),
    HostedModule::cjs_only("node:net", net::net_cjs_value),
    HostedModule::cjs_only("net", net::net_cjs_value),
    HostedModule::cjs_only("node:http", misc_modules::http_cjs_value),
    HostedModule::cjs_only("http", misc_modules::http_cjs_value),
    HostedModule::cjs_only("node:https", misc_modules::https_cjs_value),
    HostedModule::cjs_only("https", misc_modules::https_cjs_value),
    HostedModule::cjs_only("_http_common", misc_modules::http_common_cjs_value),
    HostedModule::cjs_only("_http_agent", misc_modules::http_agent_cjs_value),
    HostedModule::cjs_only("_http_client", misc_modules::http_client_cjs_value),
    HostedModule::cjs_only("_http_incoming", misc_modules::http_incoming_cjs_value),
    HostedModule::cjs_only("_http_outgoing", misc_modules::http_outgoing_cjs_value),
    HostedModule::cjs_only("_http_server", misc_modules::http_server_cjs_value),
    HostedModule::cjs_only("internal/http", misc_modules::internal_http_cjs_value),
    HostedModule::new("node:worker_threads", stubs::install_worker_threads),
    HostedModule::new("worker_threads", stubs::install_worker_threads),
    HostedModule::cjs_only("node:buffer", buffer::buffer_cjs_value),
    HostedModule::cjs_only("buffer", buffer::buffer_cjs_value),
    HostedModule::new_with_cjs_value("node:url", url::install_url_module, url::url_cjs_value),
    HostedModule::new_with_cjs_value("url", url::install_url_module, url::url_cjs_value),
    HostedModule::cjs_only("node:child_process", child_process::child_process_cjs_value),
    HostedModule::cjs_only("child_process", child_process::child_process_cjs_value),
    HostedModule::cjs_only("__cpnative", child_process::child_process_native_cjs_value),
    HostedModule::cjs_only("internal/bootstrap/realm", nodelib::bootstrap_realm),
    HostedModule::cjs_only("internal/util", nodelib::internal_util),
    HostedModule::cjs_only("internal/util/types", nodelib::internal_util_types),
    HostedModule::cjs_only("internal/util/inspect", nodelib::internal_util_inspect),
    HostedModule::cjs_only("internal/util/comparisons", nodelib::internal_util_comparisons),
    HostedModule::cjs_only("internal/source_map/source_map_cache", nodelib::internal_source_map_cache),
    HostedModule::cjs_only("internal/crypto/keys", nodelib::internal_crypto_keys),
    HostedModule::cjs_only("internal/fixed_queue", nodelib::internal_fixed_queue),
    HostedModule::cjs_only("internal/events/symbols", nodelib::internal_events_symbols),
    HostedModule::cjs_only("internal/util/debuglog", nodelib::internal_util_debuglog),
    HostedModule::cjs_only("internal/util/colors", nodelib::internal_util_colors),
    HostedModule::cjs_only("internal/assert", nodelib::internal_assert),
    HostedModule::cjs_only("internal/options", nodelib::internal_options),
    HostedModule::cjs_only("internal/events/abort_listener", nodelib::internal_events_abort_listener),
    HostedModule::cjs_only("internal/abort_controller", nodelib::internal_abort_controller),
    HostedModule::cjs_only("internal/webstreams/adapters", nodelib::internal_webstreams_adapters),
    HostedModule::cjs_only("internal/event_target", nodelib::internal_event_target),
    HostedModule::cjs_only("internal/v8/startup_snapshot", nodelib::internal_v8_startup_snapshot),
    HostedModule::cjs_only("internal/modules/helpers", nodelib::internal_modules_helpers),
    HostedModule::cjs_only("internal/process/task_queues", nodelib::internal_process_task_queues),
    HostedModule::cjs_only("internal/process/execution", nodelib::internal_process_execution),
    HostedModule::cjs_only("internal/process/warning", nodelib::internal_process_warning),
    HostedModule::cjs_only("internal/otter/natives", util::otter_natives_cjs_value),
    HostedModule::cjs_only("internal/buffer", nodelib::internal_buffer),
    HostedModule::cjs_only("internal/encoding", nodelib::internal_encoding),
    HostedModule::cjs_only("internal/blob", nodelib::internal_blob),
    HostedModule::cjs_only("internal/async_hooks", nodelib::internal_async_hooks),
    HostedModule::cjs_only("internal/async_context_frame", nodelib::internal_async_context_frame),
    HostedModule::cjs_only("internal/errors", nodelib::internal_errors),
    HostedModule::cjs_only("internal/validators", nodelib::internal_validators),
    HostedModule::cjs_only("stream/promises", nodelib::stream_promises),
    HostedModule::cjs_only("node:stream/promises", nodelib::stream_promises),
    HostedModule::cjs_only("internal/streams/add-abort-signal", nodelib::streams_add_abort_signal),
    HostedModule::cjs_only("internal/streams/compose", nodelib::streams_compose),
    HostedModule::cjs_only("internal/streams/destroy", nodelib::streams_destroy),
    HostedModule::cjs_only("internal/streams/duplex", nodelib::streams_duplex),
    HostedModule::cjs_only("internal/streams/duplexify", nodelib::streams_duplexify),
    HostedModule::cjs_only("internal/streams/duplexpair", nodelib::streams_duplexpair),
    HostedModule::cjs_only("internal/streams/end-of-stream", nodelib::streams_end_of_stream),
    HostedModule::cjs_only("internal/streams/from", nodelib::streams_from),
    HostedModule::cjs_only("internal/streams/lazy_transform", nodelib::streams_lazy_transform),
    HostedModule::cjs_only("internal/streams/legacy", nodelib::streams_legacy),
    HostedModule::cjs_only("internal/streams/operators", nodelib::streams_operators),
    HostedModule::cjs_only("internal/streams/passthrough", nodelib::streams_passthrough),
    HostedModule::cjs_only("internal/streams/pipeline", nodelib::streams_pipeline),
    HostedModule::cjs_only("internal/streams/readable", nodelib::streams_readable),
    HostedModule::cjs_only("internal/streams/state", nodelib::streams_state),
    HostedModule::cjs_only("internal/streams/transform", nodelib::streams_transform),
    HostedModule::cjs_only("internal/streams/utils", nodelib::streams_utils),
    HostedModule::cjs_only("internal/streams/writable", nodelib::streams_writable),
    HostedModule::cjs_only("internal/streams/iter/types", nodelib::streams_iter_types),
    HostedModule::cjs_only("internal/streams/iter/classic", nodelib::streams_iter_classic),
    HostedModule::cjs_only("internal/streams/iter/from", nodelib::streams_iter_from),
    HostedModule::cjs_only("internal/streams/iter/utils", nodelib::streams_iter_utils),
    HostedModule::cjs_only("internal/streams/iter/broadcast", nodelib::streams_iter_broadcast),
    HostedModule::cjs_only("internal/streams/iter/consumers", nodelib::streams_iter_consumers),
    HostedModule::cjs_only("internal/streams/iter/duplex", nodelib::streams_iter_duplex),
    HostedModule::cjs_only("internal/streams/iter/pull", nodelib::streams_iter_pull),
    HostedModule::cjs_only("internal/streams/iter/push", nodelib::streams_iter_push),
    HostedModule::cjs_only("internal/streams/iter/ringbuffer", nodelib::streams_iter_ringbuffer),
    HostedModule::cjs_only("internal/streams/iter/share", nodelib::streams_iter_share),
    HostedModule::cjs_only("stream/iter", nodelib::stream_iter),
    HostedModule::cjs_only("node:stream/iter", nodelib::stream_iter),
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
