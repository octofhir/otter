//! Supported Node.js built-in module specifiers.
//!
//! This list is the source of truth for:
//! - `otter-engine` resolver validation (`node:*` strict allowlist)
//! - runtime bootstrap validation and error messages
//!
//! Names are stored **without** the `node:` prefix.

/// List of supported Node.js built-in modules (without the `node:` prefix).
pub const NODE_BUILTINS: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "test",
    "test/reporters",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Check whether a Node.js built-in module name is supported.
///
/// Accepts a name without the `node:` prefix (e.g. `"fs"`, `"fs/promises"`).
pub fn is_supported_node_builtin(name: &str) -> bool {
    NODE_BUILTINS.contains(&name)
}

/// Normalize a potential Node.js built-in specifier to a name without the `node:` prefix.
///
/// Returns `None` when the specifier is not a supported Node.js built-in.
pub fn normalize_node_builtin(specifier: &str) -> Option<&str> {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    if is_supported_node_builtin(name) {
        Some(name)
    } else {
        None
    }
}
