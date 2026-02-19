//! OtterBuilder - Builder API for creating Otter with configuration
//!
//! Provides a fluent API for configuring the runtime with:
//! - Extensions (fetch is always included, HTTP server is optional)
//! - Environment store for secure env var access
//! - Capabilities for permission checking

use std::sync::Arc;

use otter_vm_core::isolate::IsolateConfig;

use crate::capabilities::Capabilities;
use crate::env_store::{EnvStoreBuilder, IsolatedEnvStore};

use crate::extension::Extension;
use crate::otter_runtime::Otter;

/// Builder for embedded runtime
///
/// # Example
///
/// ```ignore
/// use otter_vm_runtime::OtterBuilder;
///
/// // Basic runtime
/// let mut runtime = OtterBuilder::new().build();
///
/// // With HTTP server support
/// let mut runtime = OtterBuilder::new()
///     .with_http()
///     .build();
///
/// // With environment variables
/// let mut runtime = OtterBuilder::new()
///     .env(|b| b
///         .explicit("NODE_ENV", "production")
///         .passthrough(&["HOME", "USER"])
///     )
///     .build();
///
/// // With capabilities
/// let mut runtime = OtterBuilder::new()
///     .capabilities(Capabilities::all())
///     .build();
/// ```
pub struct OtterBuilder {
    extensions: Vec<Extension>,
    native_extensions: Vec<Box<dyn crate::extension_v2::OtterExtension>>,
    with_http: bool,
    env_store: Option<Arc<IsolatedEnvStore>>,
    capabilities: Option<Capabilities>,
    isolate_config: Option<IsolateConfig>,
}

impl OtterBuilder {
    /// Create a new builder with secure defaults
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            native_extensions: Vec::new(),
            with_http: false,
            env_store: None,
            capabilities: None,
            isolate_config: None,
        }
    }

    /// Enable `Otter.serve()` HTTP server API
    ///
    /// Note: `fetch()` is always available as part of builtins.
    /// This flag only enables the HTTP server functionality.
    pub fn with_http(mut self) -> Self {
        self.with_http = true;
        self
    }

    /// Set isolate configuration (stack depth, heap size, strict mode).
    ///
    /// If not set, uses `IsolateConfig::default()`.
    pub fn isolate_config(mut self, config: IsolateConfig) -> Self {
        self.isolate_config = Some(config);
        self
    }

    /// Set environment store for `process.env` access
    pub fn env_store(mut self, store: IsolatedEnvStore) -> Self {
        self.env_store = Some(Arc::new(store));
        self
    }

    /// Configure environment store with builder
    ///
    /// # Example
    ///
    /// ```ignore
    /// let runtime = OtterBuilder::new()
    ///     .env(|b| b
    ///         .explicit("NODE_ENV", "production")
    ///         .explicit("PORT", "3000")
    ///         .passthrough(&["HOME", "USER"])
    ///     )
    ///     .build();
    /// ```
    pub fn env(mut self, f: impl FnOnce(EnvStoreBuilder) -> EnvStoreBuilder) -> Self {
        self.env_store = Some(Arc::new(f(EnvStoreBuilder::new()).build()));
        self
    }

    /// Set capabilities (permissions)
    ///
    /// By default, capabilities are `Capabilities::none()` (deny all).
    ///
    /// # Example
    ///
    /// ```ignore
    /// use otter_vm_runtime::{Capabilities, CapabilitiesBuilder};
    ///
    /// let caps = CapabilitiesBuilder::new()
    ///     .allow_net(vec!["api.example.com".into()])
    ///     .allow_env(vec!["NODE_ENV".into()])
    ///     .build();
    ///
    /// let runtime = OtterBuilder::new()
    ///     .capabilities(caps)
    ///     .build();
    /// ```
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = Some(caps);
        self
    }

    /// Add a custom extension (v1 — JSON ops + JS shims).
    ///
    /// Extensions allow adding native Rust functionality callable from JavaScript.
    pub fn extension(mut self, ext: Extension) -> Self {
        self.extensions.push(ext);
        self
    }

    /// Add a native extension.
    pub fn native_extension(mut self, ext: Box<dyn crate::extension_v2::OtterExtension>) -> Self {
        self.native_extensions.push(ext);
        self
    }

    /// Build the runtime
    pub fn build(self) -> Otter {
        let mut runtime = Otter::with_isolate_config(
            self.isolate_config.unwrap_or_default(),
        );

        // Set env store (defaults to empty/secure)
        let env_store = self
            .env_store
            .unwrap_or_else(|| Arc::new(IsolatedEnvStore::default()));
        runtime.set_env_store(env_store);

        // Set capabilities (defaults to minimal)
        let caps = self.capabilities.unwrap_or_default();
        runtime.set_capabilities(caps);

        // Register core timer globals (setTimeout/setInterval/setImmediate/queueMicrotask).
        runtime
            .register_extension(crate::timers_ext::create_timers_extension(&runtime))
            .expect("Failed to register runtime timers extension");

        // Note: default builtins are NOT registered here to avoid circular dependencies.
        // Use `otter_engine::EngineBuilder` for automatic registration,
        // or register your own extensions manually.

        // Register custom extensions (v1)
        for ext in self.extensions {
            runtime
                .register_extension(ext)
                .expect("Failed to register extension");
        }

        // Register v2 native extensions
        for ext in self.native_extensions {
            runtime
                .register_native_extension(ext)
                .expect("Failed to register v2 extension");
        }

        runtime
    }

    /// Check if HTTP server is enabled
    pub fn has_http(&self) -> bool {
        self.with_http
    }
}

impl Default for OtterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_default() {
        let runtime = OtterBuilder::new().build();
        assert!(runtime.capabilities().fs_read.is_none());
        assert!(runtime.capabilities().net.is_none());
    }

    #[test]
    fn test_builder_with_env() {
        let runtime = OtterBuilder::new()
            .env(|b| b.explicit("TEST_VAR", "test_value"))
            .build();

        assert_eq!(
            runtime.env_store().get("TEST_VAR"),
            Some("test_value".to_string())
        );
    }

    #[test]
    fn test_builder_with_capabilities() {
        use crate::capabilities::CapabilitiesBuilder;

        let caps = CapabilitiesBuilder::new()
            .allow_net_all()
            .allow_env_all()
            .build();

        let runtime = OtterBuilder::new().capabilities(caps).build();

        assert!(runtime.capabilities().can_net("any.host.com"));
        assert!(runtime.capabilities().can_env("ANY_VAR"));
    }

    #[test]
    fn test_builder_with_http() {
        let builder = OtterBuilder::new().with_http();
        assert!(builder.has_http());
    }

    #[test]
    fn test_builder_chaining() {
        let runtime = OtterBuilder::new()
            .with_http()
            .env(|b| b.explicit("NODE_ENV", "production"))
            .capabilities(Capabilities::all())
            .build();

        assert_eq!(
            runtime.env_store().get("NODE_ENV"),
            Some("production".to_string())
        );
        assert!(runtime.capabilities().can_net("any.host.com"));
    }
}
