//! Node.js built-in module detection

/// List of known Node.js built-in modules
const NODE_BUILTINS: &[&str] = &[
    "assert",
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
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Normalize a Node.js built-in specifier.
///
/// Returns `Some(name)` if the specifier is a known Node.js builtin,
/// `None` otherwise.
///
/// Handles:
/// - `node:fs` -> Some("fs")
/// - `fs` -> Some("fs")
/// - `node:fs/promises` -> Some("fs/promises")
/// - `fs/promises` -> Some("fs/promises")
/// - `unknown` -> None
pub fn normalize_node_builtin(specifier: &str) -> Option<&str> {
    // Strip node: prefix if present
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);

    // Get the base module name (before any subpath)
    let base = name.split('/').next()?;

    // Check if it's a known builtin
    if NODE_BUILTINS.contains(&base) {
        Some(name)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_node_builtin() {
        assert_eq!(normalize_node_builtin("node:fs"), Some("fs"));
        assert_eq!(normalize_node_builtin("fs"), Some("fs"));
        assert_eq!(
            normalize_node_builtin("node:fs/promises"),
            Some("fs/promises")
        );
        assert_eq!(normalize_node_builtin("fs/promises"), Some("fs/promises"));
        assert_eq!(normalize_node_builtin("unknown"), None);
        assert_eq!(normalize_node_builtin("node:unknown"), None);
    }
}
