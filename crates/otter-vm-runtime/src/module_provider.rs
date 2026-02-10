//! Module provider system for custom import protocols.
//!
//! This module provides the [`ModuleProvider`] trait that allows extensions
//! to register custom module resolution and loading logic for specific protocols
//! (e.g., `node:`, `otter:`, `https:`).
//!
//! Inspired by Deno's `ModuleLoader` trait and Bun's `onResolve/onLoad` plugin hooks.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Module type for resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModuleType {
    /// ECMAScript module (import/export)
    ESM,
    /// CommonJS module (require/module.exports)
    CommonJS,
    /// JSON module
    JSON,
}

/// Media type for source code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    /// JavaScript source
    JavaScript,
    /// TypeScript source (needs transpilation)
    TypeScript,
    /// JSON data
    Json,
}

/// Resolution result from a module provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleResolution {
    /// Canonical URL for the module (e.g., "builtin://node:fs")
    pub url: String,
    /// Module type (ESM, CommonJS, JSON)
    pub module_type: ModuleType,
}

/// Module source returned by a provider
#[derive(Debug, Clone)]
pub struct ModuleSource {
    /// Source code content
    pub code: String,
    /// Media type of the source
    pub media_type: MediaType,
}

/// Provider trait for custom import protocols.
///
/// Implementations handle resolution and loading for specific URL schemes
/// like `node:`, `otter:`, `https:`, etc.
///
/// # Example
///
/// ```ignore
/// struct MyProvider;
///
/// impl ModuleProvider for MyProvider {
///     fn protocol(&self) -> &str { "my:" }
///     
///     fn resolve(&self, specifier: &str, _referrer: &str) -> Option<ModuleResolution> {
///         if specifier.starts_with("my:") {
///             Some(ModuleResolution {
///                 url: format!("builtin://{}", specifier),
///                 module_type: ModuleType::ESM,
///             })
///         } else { None }
///     }
///     
///     fn load(&self, url: &str) -> Option<ModuleSource> {
///         // Load source code for the URL
///         None
///     }
/// }
/// ```
pub trait ModuleProvider: Send + Sync {
    /// Protocol prefix this provider handles (e.g., "node:", "otter:")
    fn protocol(&self) -> &str;

    /// Resolve a module specifier to a canonical URL.
    ///
    /// Called during import resolution phase. The provider should check if
    /// the specifier matches its protocol and return a `ModuleResolution`
    /// with the canonical URL.
    ///
    /// Returns `None` to delegate to the next provider in the chain.
    ///
    /// # Arguments
    /// * `specifier` - The import specifier (e.g., "node:fs", "fs", "./file.js")
    /// * `referrer` - The URL of the module that contains the import
    fn resolve(&self, specifier: &str, referrer: &str) -> Option<ModuleResolution>;

    /// Load module source code by resolved URL.
    ///
    /// Called during module loading phase after resolution. The provider
    /// should return the source code for modules it can load.
    ///
    /// Returns `None` to delegate to the next provider in the chain.
    ///
    /// # Arguments
    /// * `url` - The resolved canonical URL (e.g., "builtin://node:fs")
    fn load(&self, url: &str) -> Option<ModuleSource>;
}

/// Registry of module providers
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn ModuleProvider>>,
}

impl ProviderRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Register a new module provider
    pub fn register(&mut self, provider: Arc<dyn ModuleProvider>) {
        self.providers.push(provider);
    }

    /// Resolve a specifier using registered providers
    ///
    /// Providers are checked in order of registration. Returns the first
    /// successful resolution, or `None` if no provider handles the specifier.
    pub fn resolve(&self, specifier: &str, referrer: &str) -> Option<ModuleResolution> {
        for provider in &self.providers {
            if let Some(resolution) = provider.resolve(specifier, referrer) {
                return Some(resolution);
            }
        }
        None
    }

    /// Load a module using registered providers
    ///
    /// Providers are checked in order of registration. Returns the first
    /// successful load, or `None` if no provider can load the URL.
    pub fn load(&self, url: &str) -> Option<ModuleSource> {
        for provider in &self.providers {
            if let Some(source) = provider.load(url) {
                return Some(source);
            }
        }
        None
    }

    /// Get the number of registered providers
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProvider {
        prefix: String,
    }

    impl ModuleProvider for TestProvider {
        fn protocol(&self) -> &str {
            &self.prefix
        }

        fn resolve(&self, specifier: &str, _referrer: &str) -> Option<ModuleResolution> {
            if specifier.starts_with(&self.prefix) {
                Some(ModuleResolution {
                    url: format!("test://{}", specifier),
                    module_type: ModuleType::ESM,
                })
            } else {
                None
            }
        }

        fn load(&self, url: &str) -> Option<ModuleSource> {
            if url.starts_with("test://") {
                Some(ModuleSource {
                    code: "export default 42;".to_string(),
                    media_type: MediaType::JavaScript,
                })
            } else {
                None
            }
        }
    }

    #[test]
    fn test_provider_registry() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(TestProvider {
            prefix: "test:".to_string(),
        }));

        // Should resolve test: specifier
        let resolution = registry.resolve("test:foo", "");
        assert!(resolution.is_some());
        assert_eq!(resolution.unwrap().url, "test://test:foo");

        // Should not resolve unknown specifier
        let resolution = registry.resolve("unknown:bar", "");
        assert!(resolution.is_none());
    }
}
