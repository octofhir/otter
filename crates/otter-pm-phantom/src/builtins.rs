//! Recognition of runtime builtin module names.
//!
//! A builtin is never a dependency, so it must never be reported as an
//! undeclared one. The list is matched with and without the `node:` prefix,
//! and subpaths of prefix-only builtins (`node:test/reporters`) resolve to
//! their parent.
//!
//! # Contents
//! - [`is_builtin`] — whether a specifier names a builtin module.
//!
//! # Invariants
//! - A `node:`-prefixed specifier is a builtin whenever its bare form is; the
//!   prefix is the unambiguous spelling of the same module.
//! - Unknown `node:` specifiers are still treated as builtins: the prefix is
//!   reserved, so a name this list has not caught up with is a builtin Otter
//!   does not implement yet, never a package on a registry.
//!
//! # See also
//! - [`crate::specifier`] for the bare-versus-relative classification that
//!   runs before this one.

/// Builtin module names, without the `node:` prefix.
const BUILTINS: &[&str] = &[
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
    "sqlite",
    "stream",
    "string_decoder",
    "sys",
    "test",
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

/// `true` when `specifier` names a builtin module.
#[must_use]
pub fn is_builtin(specifier: &str) -> bool {
    if let Some(rest) = specifier.strip_prefix("node:") {
        // The `node:` prefix is reserved, so anything wearing it is a builtin
        // whether or not this list knows the name yet.
        return !rest.is_empty();
    }
    let head = specifier.split('/').next().unwrap_or(specifier);
    BUILTINS.contains(&head) && (head == specifier || matches!(head, "fs" | "stream" | "dns"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_prefixed_builtins_are_both_recognized() {
        assert!(is_builtin("fs"));
        assert!(is_builtin("node:fs"));
        assert!(is_builtin("node:test/reporters"));
    }

    #[test]
    fn builtin_subpaths_are_recognized_only_where_they_exist() {
        assert!(is_builtin("fs/promises"));
        assert!(is_builtin("stream/web"));
        assert!(is_builtin("dns/promises"));
        assert!(!is_builtin("path/posix-helper"));
    }

    #[test]
    fn packages_are_not_builtins() {
        assert!(!is_builtin("left-pad"));
        assert!(!is_builtin("@scope/fs"));
        assert!(!is_builtin("node"));
    }

    #[test]
    fn an_unknown_prefixed_name_is_still_a_builtin() {
        assert!(is_builtin("node:some-future-module"));
        assert!(!is_builtin("node:"));
    }
}
