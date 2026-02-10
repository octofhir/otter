//! Central registry for Node.js built-in module definitions.
//!
//! This keeps module availability and implementation strategy in one place,
//! which makes it easier to add new native modules without split-brain
//! behavior across loader/profile checks.

use crate::NodeApiProfile;

/// Single built-in module descriptor.
#[derive(Debug, Clone, Copy)]
pub struct NodeModuleEntry {
    /// Canonical module name without `node:` prefix (e.g. `fs`, `path`).
    pub name: &'static str,
    /// Whether this module is allowed under `SafeCore`.
    pub safe_core: bool,
}

const FULL_BUILTIN_MODULES: &[&str] = &[
    "node:buffer",
    "node:events",
    "node:fs",
    "node:fs/promises",
    "node:path",
    "node:process",
    "node:util",
    "node:stream",
    "node:assert",
    "node:assert/strict",
    "node:os",
];

const SAFE_BUILTIN_MODULES: &[&str] = &[
    "node:buffer",
    "node:events",
    "node:path",
    "node:util",
    "node:stream",
    "node:assert",
    "node:assert/strict",
];

static NODE_MODULES: &[NodeModuleEntry] = &[
    NodeModuleEntry { name: "buffer", safe_core: true },
    NodeModuleEntry { name: "events", safe_core: true },
    NodeModuleEntry { name: "fs", safe_core: false },
    NodeModuleEntry { name: "fs/promises", safe_core: false },
    NodeModuleEntry { name: "path", safe_core: true },
    NodeModuleEntry { name: "process", safe_core: false },
    NodeModuleEntry { name: "util", safe_core: true },
    NodeModuleEntry { name: "stream", safe_core: true },
    NodeModuleEntry { name: "assert", safe_core: true },
    NodeModuleEntry { name: "assert/strict", safe_core: true },
    NodeModuleEntry { name: "os", safe_core: false },
];

fn normalize_builtin_name(specifier: &str) -> &str {
    specifier.strip_prefix("node:").unwrap_or(specifier)
}

fn profile_allows(entry: &NodeModuleEntry, profile: NodeApiProfile) -> bool {
    match profile {
        NodeApiProfile::None => false,
        NodeApiProfile::SafeCore => entry.safe_core,
        NodeApiProfile::Full => true,
    }
}

pub fn builtin_modules() -> &'static [&'static str] {
    FULL_BUILTIN_MODULES
}

pub fn safe_builtin_modules() -> &'static [&'static str] {
    SAFE_BUILTIN_MODULES
}

pub fn module_entry_for_profile(
    name: &str,
    profile: NodeApiProfile,
) -> Option<&'static NodeModuleEntry> {
    let normalized = normalize_builtin_name(name);
    let entry = NODE_MODULES.iter().find(|entry| entry.name == normalized)?;
    profile_allows(entry, profile).then_some(entry)
}

pub fn is_builtin_for_profile(specifier: &str, profile: NodeApiProfile) -> bool {
    module_entry_for_profile(specifier, profile).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_core_filters_process() {
        let process = module_entry_for_profile("node:process", NodeApiProfile::SafeCore);
        assert!(process.is_none());

        let path = module_entry_for_profile("node:path", NodeApiProfile::SafeCore);
        assert!(path.is_some());
    }

    #[test]
    fn full_profile_has_all_modules() {
        let fs = module_entry_for_profile("fs", NodeApiProfile::Full).unwrap();
        assert_eq!(fs.name, "fs");
    }
}
