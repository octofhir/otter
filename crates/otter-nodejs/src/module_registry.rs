/// Single built-in module descriptor.
#[derive(Debug, Clone, Copy)]
pub struct NodeModuleEntry {
    /// Canonical module name without `node:` prefix.
    pub name: &'static str,
}

const BUILTIN_MODULES: &[&str] = &[
    "node:process",
    "process",
    "node:assert",
    "assert",
    "node:assert/strict",
    "assert/strict",
    "node:util",
    "util",
    "node:worker_threads",
    "worker_threads",
    "node:net",
    "net",
    "node:path",
    "path",
    "node:url",
    "url",
    "node:test",
    "node:child_process",
    "child_process",
    "node:vm",
    "vm",
    "node:fs",
    "fs",
];

#[must_use]
pub fn builtin_modules() -> &'static [&'static str] {
    BUILTIN_MODULES
}
