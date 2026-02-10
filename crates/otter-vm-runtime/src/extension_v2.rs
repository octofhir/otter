//! Unified native extension system.
//!
//! `OtterExtension` is the single trait for ALL extensions — ECMAScript intrinsics,
//! `node:*` modules, and `otter:*` namespaces. Extensions register native functions
//! directly via `RegistrationContext`, eliminating JS shims and JSON serde overhead.
//!
//! # Architecture
//!
//! ```text
//! OtterExtension trait
//!   ├── allocate()      — Stage 1: allocate objects (for circular refs)
//!   ├── install()       — Stage 2: wire methods, properties, globals
//!   ├── init_state()    — Initialize ExtensionState
//!   ├── load_module()   — Build native module namespace on first import
//!   └── module_specifiers() — Import specifiers this extension handles
//! ```

use std::fmt;

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::JsObject;

use crate::extension_state::ExtensionState;
use crate::registration::RegistrationContext;

/// Extension profile — controls which extensions are loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Minimal core (always loaded).
    Core,
    /// Safe subset for embedded/sandbox use.
    SafeCore,
    /// Full runtime with all modules.
    Full,
}

/// Trait implemented by all native extensions.
///
/// Each extension has a unique name, declares its dependencies, and registers
/// its native functions during the `install()` phase.
pub trait OtterExtension: Send + Sync {
    /// Unique extension name (e.g., "node:path", "otter:kv").
    fn name(&self) -> &str;

    /// Profiles in which this extension is available.
    fn profiles(&self) -> &[Profile] {
        &[Profile::Core]
    }

    /// Names of extensions that must be installed before this one.
    fn deps(&self) -> &[&str] {
        &[]
    }

    /// Stage 1: Allocate objects needed for two-stage init (circular refs).
    ///
    /// Called before `install()`. Most extensions can leave this empty.
    fn allocate(&self, _ctx: &mut RegistrationContext) {}

    /// Stage 2: Install methods, properties, and globals.
    ///
    /// This is where the extension registers its native functions on prototypes,
    /// constructors, namespaces, or the global object.
    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError>;

    /// Import specifiers this extension handles (e.g., `["node:path", "path"]`).
    ///
    /// When any of these specifiers is imported, `load_module()` is called.
    fn module_specifiers(&self) -> &[&str] {
        &[]
    }

    /// Build a native module namespace object for the given specifier.
    ///
    /// Called on first `import` of a matching specifier. Returns a plain object
    /// with all exported functions/values as properties.
    fn load_module(
        &self,
        _specifier: &str,
        _ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        None
    }

    /// Initialize per-extension state in `ExtensionState`.
    ///
    /// Called before `install()`. Use this to `state.put()` configuration or
    /// caches that native functions will `borrow()` at runtime.
    fn init_state(&self, _state: &mut ExtensionState) {}
}

impl fmt::Debug for dyn OtterExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtterExtension")
            .field("name", &self.name())
            .field("profiles", &self.profiles())
            .field("deps", &self.deps())
            .field("specifiers", &self.module_specifiers())
            .finish()
    }
}

/// Registry for v2 extensions.
///
/// Manages registration, dependency ordering, and lookup by module specifier.
pub struct NativeExtensionRegistry {
    /// Extensions in registration (topological) order.
    extensions: Vec<Box<dyn OtterExtension>>,
    /// Map from module specifier to extension index.
    specifier_map: std::collections::HashMap<String, usize>,
}

impl NativeExtensionRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            specifier_map: std::collections::HashMap::new(),
        }
    }

    /// Register an extension.
    ///
    /// Extensions must be registered in dependency order — all deps must already
    /// be registered. Returns an error if a dependency is missing or if the
    /// extension name or any specifier conflicts with an existing one.
    pub fn register(&mut self, ext: Box<dyn OtterExtension>) -> Result<(), String> {
        let name = ext.name().to_string();

        // Check for duplicate name
        if self.extensions.iter().any(|e| e.name() == name) {
            return Err(format!("Extension already registered: {}", name));
        }

        // Check dependencies
        for dep in ext.deps() {
            if !self.extensions.iter().any(|e| e.name() == *dep) {
                return Err(format!(
                    "Missing dependency '{}' for extension '{}'",
                    dep, name
                ));
            }
        }

        // Check specifier conflicts
        for spec in ext.module_specifiers() {
            if self.specifier_map.contains_key(*spec) {
                return Err(format!(
                    "Module specifier '{}' already registered (extension '{}')",
                    spec, name
                ));
            }
        }

        // Register specifiers
        let idx = self.extensions.len();
        for spec in ext.module_specifiers() {
            self.specifier_map.insert(spec.to_string(), idx);
        }

        self.extensions.push(ext);
        Ok(())
    }

    /// Find extension by module specifier (e.g., "node:path").
    pub fn find_by_specifier(&self, specifier: &str) -> Option<&dyn OtterExtension> {
        self.specifier_map
            .get(specifier)
            .map(|&idx| self.extensions[idx].as_ref())
    }

    /// Check if a specifier is handled by a registered extension.
    pub fn has_specifier(&self, specifier: &str) -> bool {
        self.specifier_map.contains_key(specifier)
    }

    /// Get all extensions in registration order.
    pub fn extensions(&self) -> &[Box<dyn OtterExtension>] {
        &self.extensions
    }

    /// Get extensions filtered by profile.
    pub fn extensions_for_profile(&self, profile: Profile) -> Vec<&dyn OtterExtension> {
        self.extensions
            .iter()
            .filter(|ext| ext.profiles().contains(&profile))
            .map(|ext| ext.as_ref())
            .collect()
    }

    /// Check if an extension with the given name is registered.
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|e| e.name() == name)
    }

    /// Get extension count.
    pub fn extension_count(&self) -> usize {
        self.extensions.len()
    }

    /// Get specifier count.
    pub fn specifier_count(&self) -> usize {
        self.specifier_map.len()
    }

    /// Run the full bootstrap sequence for all registered extensions.
    ///
    /// 1. `init_state()` for all extensions
    /// 2. `allocate()` for all extensions (stage 1)
    /// 3. `install()` for all extensions (stage 2)
    pub fn bootstrap(
        &self,
        ctx: &mut RegistrationContext,
        profile: Profile,
    ) -> Result<(), VmError> {
        // Phase 1: init_state
        for ext in &self.extensions {
            if ext.profiles().contains(&profile) {
                ext.init_state(ctx.state_mut());
            }
        }

        // Phase 2: allocate (stage 1)
        for ext in &self.extensions {
            if ext.profiles().contains(&profile) {
                ext.allocate(ctx);
            }
        }

        // Phase 3: install (stage 2)
        for ext in &self.extensions {
            if ext.profiles().contains(&profile) {
                ext.install(ctx)?;
            }
        }

        Ok(())
    }

    /// Load a native module by specifier.
    ///
    /// Returns `Some(namespace_object)` if the specifier is handled by a
    /// registered extension, `None` otherwise.
    pub fn load_module(
        &self,
        specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let idx = *self.specifier_map.get(specifier)?;
        self.extensions[idx].load_module(specifier, ctx)
    }
}

impl Default for NativeExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestExtension {
        name: &'static str,
        profiles: Vec<Profile>,
        deps: Vec<&'static str>,
        specifiers: Vec<&'static str>,
    }

    impl OtterExtension for TestExtension {
        fn name(&self) -> &str {
            self.name
        }

        fn profiles(&self) -> &[Profile] {
            &self.profiles
        }

        fn deps(&self) -> &[&str] {
            &self.deps
        }

        fn module_specifiers(&self) -> &[&str] {
            &self.specifiers
        }

        fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
            Ok(())
        }
    }

    #[test]
    fn test_register_and_lookup() {
        let mut registry = NativeExtensionRegistry::new();

        registry
            .register(Box::new(TestExtension {
                name: "node:path",
                profiles: vec![Profile::SafeCore, Profile::Full],
                deps: vec![],
                specifiers: vec!["node:path", "path"],
            }))
            .unwrap();

        assert!(registry.has_extension("node:path"));
        assert!(registry.has_specifier("node:path"));
        assert!(registry.has_specifier("path"));
        assert!(!registry.has_specifier("node:fs"));

        let ext = registry.find_by_specifier("node:path").unwrap();
        assert_eq!(ext.name(), "node:path");
    }

    #[test]
    fn test_duplicate_name() {
        let mut registry = NativeExtensionRegistry::new();

        registry
            .register(Box::new(TestExtension {
                name: "dup",
                profiles: vec![Profile::Core],
                deps: vec![],
                specifiers: vec![],
            }))
            .unwrap();

        let result = registry.register(Box::new(TestExtension {
            name: "dup",
            profiles: vec![Profile::Core],
            deps: vec![],
            specifiers: vec![],
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_dep() {
        let mut registry = NativeExtensionRegistry::new();

        let result = registry.register(Box::new(TestExtension {
            name: "child",
            profiles: vec![Profile::Core],
            deps: vec!["parent"],
            specifiers: vec![],
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing dependency"));
    }

    #[test]
    fn test_specifier_conflict() {
        let mut registry = NativeExtensionRegistry::new();

        registry
            .register(Box::new(TestExtension {
                name: "ext1",
                profiles: vec![Profile::Core],
                deps: vec![],
                specifiers: vec!["shared:spec"],
            }))
            .unwrap();

        let result = registry.register(Box::new(TestExtension {
            name: "ext2",
            profiles: vec![Profile::Core],
            deps: vec![],
            specifiers: vec!["shared:spec"],
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn test_profile_filter() {
        let mut registry = NativeExtensionRegistry::new();

        registry
            .register(Box::new(TestExtension {
                name: "core_only",
                profiles: vec![Profile::Core],
                deps: vec![],
                specifiers: vec![],
            }))
            .unwrap();

        registry
            .register(Box::new(TestExtension {
                name: "full_only",
                profiles: vec![Profile::Full],
                deps: vec![],
                specifiers: vec![],
            }))
            .unwrap();

        let core = registry.extensions_for_profile(Profile::Core);
        assert_eq!(core.len(), 1);
        assert_eq!(core[0].name(), "core_only");

        let full = registry.extensions_for_profile(Profile::Full);
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].name(), "full_only");
    }
}
