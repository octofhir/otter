//! Node.js module provider for Otter VM.
//!
//! Implements [`ModuleProvider`] to handle `node:` protocol imports
//! and bare Node.js module specifiers (e.g., `fs`, `path`).

use crate::{NodeApiProfile, is_builtin_for_profile};
use otter_vm_runtime::module_provider::ModuleType;
use otter_vm_runtime::{ModuleProvider, ModuleResolution};

/// Provider for Node.js built-in modules.
///
/// Handles both prefixed (`node:fs`) and bare (`fs`) specifiers.
/// All modules are now native — no JS source loading.
pub struct NodeModuleProvider {
    profile: NodeApiProfile,
}

impl NodeModuleProvider {
    fn new(profile: NodeApiProfile) -> Self {
        Self { profile }
    }
}

impl ModuleProvider for NodeModuleProvider {
    fn protocol(&self) -> &str {
        "node:"
    }

    fn resolve(&self, specifier: &str, _referrer: &str) -> Option<ModuleResolution> {
        let name = specifier.strip_prefix("node:").unwrap_or(specifier);

        if is_builtin_for_profile(name, self.profile) {
            return Some(ModuleResolution {
                url: format!("builtin://node:{}", name),
                module_type: ModuleType::ESM,
            });
        }

        None
    }

    fn load(&self, _url: &str) -> Option<otter_vm_runtime::ModuleSource> {
        // All Node.js modules are now native extensions — no JS source to load.
        None
    }
}

/// Create a Node.js module provider.
pub fn create_nodejs_provider() -> std::sync::Arc<dyn ModuleProvider> {
    create_nodejs_provider_for_profile(NodeApiProfile::Full)
}

/// Create a Node.js module provider for the embedded-safe profile.
pub fn create_nodejs_safe_provider() -> std::sync::Arc<dyn ModuleProvider> {
    create_nodejs_provider_for_profile(NodeApiProfile::SafeCore)
}

/// Create a Node.js module provider for a specific profile.
pub fn create_nodejs_provider_for_profile(
    profile: NodeApiProfile,
) -> std::sync::Arc<dyn ModuleProvider> {
    std::sync::Arc::new(NodeModuleProvider::new(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_prefixed() {
        let provider = NodeModuleProvider::new(NodeApiProfile::Full);
        let res = provider.resolve("node:fs", "");
        assert!(res.is_some());
        assert_eq!(res.unwrap().url, "builtin://node:fs");
    }

    #[test]
    fn test_resolve_bare() {
        let provider = NodeModuleProvider::new(NodeApiProfile::Full);
        let res = provider.resolve("path", "");
        assert!(res.is_some());
        assert_eq!(res.unwrap().url, "builtin://node:path");
    }

    #[test]
    fn test_resolve_unknown() {
        let provider = NodeModuleProvider::new(NodeApiProfile::Full);

        let res = provider.resolve("./local_file.js", "");
        assert!(res.is_none());

        let res = provider.resolve("some-npm-package", "");
        assert!(res.is_none());
    }

    #[test]
    fn test_load_returns_none_for_native_modules() {
        let provider = NodeModuleProvider::new(NodeApiProfile::Full);
        // All modules are native now — load always returns None
        assert!(provider.load("builtin://node:path").is_none());
        assert!(provider.load("builtin://node:fs").is_none());
    }

    #[test]
    fn test_safe_profile_blocks_process() {
        let provider = NodeModuleProvider::new(NodeApiProfile::SafeCore);
        let res = provider.resolve("node:process", "");
        assert!(res.is_none());
    }
}
