use std::fmt;
use std::sync::Arc;

use otter_vm::RuntimeState;

use super::{HostedNativeModuleLoader, HostedNativeModuleRegistry, RuntimeProfile};

/// One native module registration emitted by an extension.
#[derive(Clone)]
pub struct HostedExtensionModule {
    pub specifier: String,
    pub loader: Arc<dyn HostedNativeModuleLoader>,
}

impl fmt::Debug for HostedExtensionModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostedExtensionModule")
            .field("specifier", &self.specifier)
            .finish_non_exhaustive()
    }
}

/// Extension hook for the new host integration layer.
pub trait HostedExtension: Send + Sync {
    /// Stable extension name used for dependency ordering.
    fn name(&self) -> &str;

    /// Runtime profiles in which this extension is active.
    fn profiles(&self) -> &[RuntimeProfile] {
        &[RuntimeProfile::Core]
    }

    /// Names of extensions that must already be registered.
    fn deps(&self) -> &[&str] {
        &[]
    }

    /// Bootstrap globals / namespaces / runtime hooks directly on this runtime.
    fn install(&self, _runtime: &mut RuntimeState) -> Result<(), String> {
        Ok(())
    }

    /// Native modules exported by this extension.
    fn native_modules(&self) -> Vec<HostedExtensionModule> {
        Vec::new()
    }
}

impl fmt::Debug for dyn HostedExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostedExtension")
            .field("name", &self.name())
            .field("profiles", &self.profiles())
            .field("deps", &self.deps())
            .finish()
    }
}

/// Runtime-local extension registry owned by host configuration.
#[derive(Clone, Default)]
pub struct HostedExtensionRegistry {
    extensions: Vec<Arc<dyn HostedExtension>>,
}

impl fmt::Debug for HostedExtensionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostedExtensionRegistry")
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

impl HostedExtensionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, extension: Arc<dyn HostedExtension>) -> Result<(), String> {
        let name = extension.name().to_string();

        if self.extensions.iter().any(|ext| ext.name() == name) {
            return Err(format!("extension '{}' is already registered", name));
        }

        for dep in extension.deps() {
            if !self.extensions.iter().any(|ext| ext.name() == *dep) {
                return Err(format!(
                    "missing dependency '{}' for extension '{}'",
                    dep, name
                ));
            }
        }

        let mut candidate_registry = self.native_module_registry(RuntimeProfile::Full)?;
        for module in extension.native_modules() {
            candidate_registry.register(module.specifier, module.loader)?;
        }

        self.extensions.push(extension);
        Ok(())
    }

    #[must_use]
    pub fn extensions(&self) -> &[Arc<dyn HostedExtension>] {
        &self.extensions
    }

    #[must_use]
    pub fn extensions_for_profile(
        &self,
        profile: RuntimeProfile,
    ) -> Vec<&Arc<dyn HostedExtension>> {
        self.extensions
            .iter()
            .filter(|extension| {
                extension
                    .profiles()
                    .iter()
                    .copied()
                    .any(|candidate| profile_includes(profile, candidate))
            })
            .collect()
    }

    pub fn bootstrap(
        &self,
        runtime: &mut RuntimeState,
        profile: RuntimeProfile,
    ) -> Result<(), String> {
        for extension in self.extensions_for_profile(profile) {
            extension.install(runtime)?;
        }
        Ok(())
    }

    pub fn native_module_registry(
        &self,
        profile: RuntimeProfile,
    ) -> Result<HostedNativeModuleRegistry, String> {
        let mut registry = HostedNativeModuleRegistry::new();
        for extension in self.extensions_for_profile(profile) {
            for module in extension.native_modules() {
                registry.register(module.specifier, module.loader)?;
            }
        }
        Ok(registry)
    }
}

const fn profile_includes(active: RuntimeProfile, candidate: RuntimeProfile) -> bool {
    matches!(
        (active, candidate),
        (RuntimeProfile::Core, RuntimeProfile::Core)
            | (
                RuntimeProfile::SafeCore,
                RuntimeProfile::Core | RuntimeProfile::SafeCore
            )
            | (
                RuntimeProfile::Full,
                RuntimeProfile::Core | RuntimeProfile::SafeCore | RuntimeProfile::Full
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct CoreExtension;

    impl HostedExtension for CoreExtension {
        fn name(&self) -> &str {
            "core"
        }
    }

    #[derive(Debug)]
    struct DependentExtension;

    impl HostedExtension for DependentExtension {
        fn name(&self) -> &str {
            "dependent"
        }

        fn deps(&self) -> &[&str] {
            &["core"]
        }
    }

    #[derive(Debug)]
    struct FullOnlyExtension;

    impl HostedExtension for FullOnlyExtension {
        fn name(&self) -> &str {
            "full"
        }

        fn profiles(&self) -> &[RuntimeProfile] {
            &[RuntimeProfile::Full]
        }
    }

    #[test]
    fn rejects_missing_dependency() {
        let mut registry = HostedExtensionRegistry::new();
        let error = registry
            .register(Arc::new(DependentExtension))
            .expect_err("missing dep should fail");
        assert!(error.contains("missing dependency"));
    }

    #[test]
    fn filters_by_profile() {
        let mut registry = HostedExtensionRegistry::new();
        registry
            .register(Arc::new(CoreExtension))
            .expect("core should register");
        registry
            .register(Arc::new(FullOnlyExtension))
            .expect("full should register");

        assert_eq!(
            registry.extensions_for_profile(RuntimeProfile::Core).len(),
            1
        );
        assert_eq!(
            registry.extensions_for_profile(RuntimeProfile::Full).len(),
            2
        );
    }
}
