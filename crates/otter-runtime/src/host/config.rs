use std::sync::Arc;

use super::{
    Capabilities, HostedExtensionRegistry, HostedNativeModuleRegistry, IsolatedEnvStore,
    ModuleLoaderConfig,
};

/// Runtime host feature profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeProfile {
    /// Minimal runtime-facing host surface.
    #[default]
    Core,
    /// Embedded-safe host surface.
    SafeCore,
    /// Full host surface.
    Full,
}

/// Host configuration owned by one [`crate::OtterRuntime`] instance.
#[derive(Debug, Clone)]
pub struct HostConfig {
    capabilities: Capabilities,
    env_store: Arc<IsolatedEnvStore>,
    profile: RuntimeProfile,
    loader: ModuleLoaderConfig,
    native_modules: HostedNativeModuleRegistry,
    extensions: HostedExtensionRegistry,
}

impl HostConfig {
    /// Creates a host configuration with secure defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the configured capabilities.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Replaces the capabilities configuration.
    pub fn set_capabilities(&mut self, capabilities: Capabilities) {
        self.capabilities = capabilities;
    }

    /// Returns the isolated environment store.
    pub fn env_store(&self) -> &Arc<IsolatedEnvStore> {
        &self.env_store
    }

    /// Replaces the environment store.
    pub fn set_env_store(&mut self, env_store: Arc<IsolatedEnvStore>) {
        self.env_store = env_store;
    }

    /// Returns the configured host profile.
    pub fn profile(&self) -> RuntimeProfile {
        self.profile
    }

    /// Replaces the host profile.
    pub fn set_profile(&mut self, profile: RuntimeProfile) {
        self.profile = profile;
    }

    /// Returns the hosted module loader configuration.
    pub fn loader(&self) -> &ModuleLoaderConfig {
        &self.loader
    }

    /// Replaces the hosted module loader configuration.
    pub fn set_loader(&mut self, loader: ModuleLoaderConfig) {
        self.loader = loader;
    }

    /// Returns the native hosted module registry.
    pub fn native_modules(&self) -> &HostedNativeModuleRegistry {
        &self.native_modules
    }

    /// Replaces the native hosted module registry.
    pub fn set_native_modules(&mut self, native_modules: HostedNativeModuleRegistry) {
        self.native_modules = native_modules;
    }

    /// Returns the hosted extension registry.
    pub fn extensions(&self) -> &HostedExtensionRegistry {
        &self.extensions
    }

    /// Replaces the hosted extension registry.
    pub fn set_extensions(&mut self, extensions: HostedExtensionRegistry) {
        self.extensions = extensions;
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            capabilities: Capabilities::none(),
            env_store: Arc::new(IsolatedEnvStore::default()),
            profile: RuntimeProfile::Core,
            loader: ModuleLoaderConfig::default(),
            native_modules: HostedNativeModuleRegistry::default(),
            extensions: HostedExtensionRegistry::default(),
        }
    }
}
