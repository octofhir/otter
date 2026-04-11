use std::collections::BTreeMap;
use std::sync::Arc;

use otter_vm::object::ObjectHandle;
use otter_vm::{RegisterValue, RuntimeState};

/// Native hosted module payload built directly on the VM heap.
#[derive(Debug, Clone, Copy)]
pub enum HostedNativeModule {
    /// ESM-like namespace object.
    Esm(ObjectHandle),
    /// CommonJS-like exported value.
    CommonJs(RegisterValue),
}

/// Stable kind for a native hosted module specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostedNativeModuleKind {
    #[default]
    Esm,
    CommonJs,
}

/// Builder trait for native hosted modules on the new runtime stack.
pub trait HostedNativeModuleLoader: Send + Sync + std::fmt::Debug {
    /// Declares how the module should behave at import/require boundary.
    fn kind(&self) -> HostedNativeModuleKind {
        HostedNativeModuleKind::Esm
    }

    /// Builds the module payload directly in the target runtime instance.
    fn load(&self, runtime: &mut RuntimeState) -> Result<HostedNativeModule, String>;
}

/// Registry of native hosted modules for one host configuration.
#[derive(Debug, Clone, Default)]
pub struct HostedNativeModuleRegistry {
    modules: BTreeMap<String, Arc<dyn HostedNativeModuleLoader>>,
}

impl HostedNativeModuleRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        specifier: impl Into<String>,
        module: Arc<dyn HostedNativeModuleLoader>,
    ) -> Result<(), String> {
        let specifier = specifier.into();
        if self.modules.contains_key(&specifier) {
            return Err(format!(
                "native hosted module '{}' is already registered",
                specifier
            ));
        }
        self.modules.insert(specifier, module);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, specifier: &str) -> Option<&Arc<dyn HostedNativeModuleLoader>> {
        self.modules.get(specifier)
    }

    #[must_use]
    pub fn contains(&self, specifier: &str) -> bool {
        self.modules.contains_key(specifier)
    }

    #[must_use]
    pub fn kind_for(&self, specifier: &str) -> Option<HostedNativeModuleKind> {
        self.get(specifier).map(|module| module.kind())
    }

    #[must_use]
    pub fn specifiers(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    #[must_use]
    pub fn into_entries(self) -> BTreeMap<String, Arc<dyn HostedNativeModuleLoader>> {
        self.modules
    }
}
