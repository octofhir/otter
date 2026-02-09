//! Node.js module provider for Otter VM.
//!
//! Implements [`ModuleProvider`] to handle `node:` protocol imports
//! and bare Node.js module specifiers (e.g., `fs`, `path`).

use crate::{get_builtin_source, is_builtin};
use otter_vm_runtime::module_provider::ModuleType;
use otter_vm_runtime::{MediaType, ModuleProvider, ModuleResolution, ModuleSource};

/// Provider for Node.js built-in modules.
///
/// Handles both prefixed (`node:fs`) and bare (`fs`) specifiers.
pub struct NodeModuleProvider;

impl ModuleProvider for NodeModuleProvider {
    fn protocol(&self) -> &str {
        "node:"
    }

    fn resolve(&self, specifier: &str, _referrer: &str) -> Option<ModuleResolution> {
        // Handle "node:fs" -> "fs"
        let name = specifier.strip_prefix("node:").unwrap_or(specifier);

        // Check if it's a known Node.js builtin
        if is_builtin(name) {
            return Some(ModuleResolution {
                url: format!("builtin://node:{}", name),
                module_type: ModuleType::ESM,
            });
        }

        None // Not a Node builtin, delegate to next provider
    }

    fn load(&self, url: &str) -> Option<ModuleSource> {
        // Handle "builtin://node:fs" -> "fs"
        let name = url.strip_prefix("builtin://node:")?;

        // Get the source code for this builtin
        let code = get_builtin_source(name)?;

        Some(ModuleSource {
            code: code.to_string(),
            media_type: MediaType::JavaScript,
        })
    }
}

/// Create a Node.js module provider.
///
/// Use this to register Node.js built-in module support with the module loader:
///
/// ```ignore
/// let provider = create_nodejs_provider();
/// module_loader.register_provider(provider);
/// ```
pub fn create_nodejs_provider() -> std::sync::Arc<dyn ModuleProvider> {
    std::sync::Arc::new(NodeModuleProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_prefixed() {
        let provider = NodeModuleProvider;

        // Should resolve "node:fs"
        let res = provider.resolve("node:fs", "");
        assert!(res.is_some());
        assert_eq!(res.unwrap().url, "builtin://node:fs");
    }

    #[test]
    fn test_resolve_bare() {
        let provider = NodeModuleProvider;

        // Should resolve "path" (bare specifier)
        let res = provider.resolve("path", "");
        assert!(res.is_some());
        assert_eq!(res.unwrap().url, "builtin://node:path");
    }

    #[test]
    fn test_resolve_unknown() {
        let provider = NodeModuleProvider;

        // Should NOT resolve unknown modules
        let res = provider.resolve("./local_file.js", "");
        assert!(res.is_none());

        let res = provider.resolve("some-npm-package", "");
        assert!(res.is_none());
    }

    #[test]
    fn test_load() {
        let provider = NodeModuleProvider;

        // Should load "builtin://node:path"
        let source = provider.load("builtin://node:path");
        assert!(source.is_some());
        assert!(source.unwrap().code.contains("export"));
    }
}
