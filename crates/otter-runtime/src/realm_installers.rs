//! Hosted modules, extensions and global installers shared by an isolate's realms.
//!
//! # Contents
//! - Hosted ESM and CommonJS installers, native global classes and extension bundles.
//!
//! # Invariants
//! - Each callback installs its surface using the supplied realm and native scope.
//!
//! # See also
//! - [`crate::realm`] provides the installation contexts.

use crate::*;

/// Runtime-hosted namespace installer.
///
/// Installers allocate and publish one namespace value inside the caller's
/// existing native handle scope, so the ESM loader never opens a second root
/// boundary or exposes raw VM handles to product crates.
pub type HostedModuleInstall =
    for<'scope, 'rt> fn(
        &mut RuntimeNativeScope<'scope, 'rt>,
        &CapabilitySet,
        Option<RuntimeTaskSpawner>,
    ) -> Result<RuntimeLocal<'scope>, RuntimeNativeError>;

/// Runtime-hosted CommonJS value installer.
///
/// `module` is the live record already published in `require.cache`, and
/// `require` is the canonical resolver bound to the importing module. Passing
/// both as rooted locals lets embedded shims participate in circular loading,
/// replace `module.exports`, and resolve installer dependencies through the
/// same cache without ambient runtime state.
pub type HostedCommonJsInstall =
    for<'scope, 'rt> fn(
        &mut RuntimeNativeScope<'scope, 'rt>,
        &CapabilitySet,
        Option<RuntimeTaskSpawner>,
        RuntimeLocal<'scope>,
        RuntimeLocal<'scope>,
    ) -> Result<RuntimeLocal<'scope>, RuntimeNativeError>;

/// Load one CommonJS native addon (`.node`) after module resolution and the
/// filesystem capability check have completed.
///
/// The Node compatibility product installs this hook; the core runtime keeps
/// native code loading opt-in and has no dependency on a particular addon ABI.
/// Implementations must enforce the separate `ffi` capability before opening
/// the library. The optional task spawner is the owned, sendable route for
/// addon worker callbacks to re-enter a handle-backed isolate.
pub type CommonJsAddonLoader =
    for<'scope, 'rt> fn(
        &mut RuntimeNativeScope<'scope, 'rt>,
        &Path,
        &CapabilitySet,
        Option<RuntimeTaskSpawner>,
    ) -> Result<RuntimeLocal<'scope>, RuntimeNativeError>;

/// One runtime-hosted module.
#[derive(Debug, Clone, Copy)]
pub struct HostedModule {
    /// Module specifier, for example `otter:kv`.
    specifier: &'static str,
    /// Namespace installer (object export), when the module supports ESM.
    install: Option<HostedModuleInstall>,
    /// Optional callable/value export used by `require()` in place of the
    /// object namespace. `None` => `require()` returns the namespace object.
    cjs_value: Option<HostedCommonJsInstall>,
}

impl HostedModule {
    /// Create a hosted module with one namespace shared by ESM and CommonJS.
    #[must_use]
    pub const fn new(specifier: &'static str, install: HostedModuleInstall) -> Self {
        Self {
            specifier,
            install: Some(install),
            cjs_value: None,
        }
    }

    /// Create a hosted module whose CommonJS export is a value (e.g. a
    /// callable). The `install` namespace is still used for ESM imports.
    #[must_use]
    pub const fn new_with_cjs_value(
        specifier: &'static str,
        install: HostedModuleInstall,
        cjs_value: HostedCommonJsInstall,
    ) -> Self {
        Self {
            specifier,
            install: Some(install),
            cjs_value: Some(cjs_value),
        }
    }

    /// Create a hosted module whose implementation is a CommonJS value.
    ///
    /// An `import` of such a module gets a namespace whose `default` is that
    /// value and whose named exports are the value's own enumerable string
    /// keys — the shape Node gives a builtin backed by CommonJS.
    #[must_use]
    pub const fn cjs_only(specifier: &'static str, cjs_value: HostedCommonJsInstall) -> Self {
        Self {
            specifier,
            install: None,
            cjs_value: Some(cjs_value),
        }
    }

    /// Module specifier, for example `otter:kv`.
    #[must_use]
    pub const fn specifier(self) -> &'static str {
        self.specifier
    }

    /// Whether the ESM loader can serve this module. A module that publishes
    /// only a CommonJS value is importable too: its namespace is synthesized
    /// from that value, which is what Node does for every builtin.
    #[must_use]
    pub const fn supports_esm(self) -> bool {
        self.install.is_some() || self.cjs_value.is_some()
    }

    /// Namespace installer used by the ESM loader.
    #[must_use]
    pub(crate) const fn namespace_install(self) -> Option<HostedModuleInstall> {
        self.install
    }

    /// Dedicated CommonJS value installer, when one is registered.
    #[must_use]
    pub(crate) const fn commonjs_value_install(self) -> Option<HostedCommonJsInstall> {
        self.cjs_value
    }
}

/// Cloneable callback that installs embedder globals in every configured realm.
///
/// Runtime workers clone [`RuntimeConfig`], so installers registered on a
/// parent runtime also run inside child worker isolates before user code starts.
/// Additional realms replay the same installers through
/// [`RuntimeRealmContext`], which exposes only high-level global installation,
/// owned capabilities/task delivery and trusted bootstrap source. Raw VM/GC
/// handles never cross this boundary.
#[derive(Clone)]
pub struct RuntimeGlobalInstaller {
    install:
        Arc<dyn for<'a> Fn(&mut RuntimeRealmContext<'a>) -> Result<(), OtterError> + Send + Sync>,
}

impl RuntimeGlobalInstaller {
    /// Build a global installer from a sendable callback.
    #[must_use]
    pub fn new(
        install: impl for<'a> Fn(&mut RuntimeRealmContext<'a>) -> Result<(), OtterError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            install: Arc::new(install),
        }
    }

    fn install(&self, realm: &mut RuntimeRealmContext<'_>) -> Result<(), OtterError> {
        (self.install)(realm)
    }
}

impl std::fmt::Debug for RuntimeGlobalInstaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeGlobalInstaller")
            .finish_non_exhaustive()
    }
}

/// Advanced native installer for engine extension crates.
///
/// Unlike [`RuntimeGlobalInstaller`], this callback receives
/// [`RuntimeExtensionContext`] and can install VM-shaped native call targets.
/// Application embedders should use the high-level installer and import from
/// [`embedding`], which deliberately does not re-export this type.
#[derive(Clone)]
pub struct RuntimeExtensionInstaller {
    install: Arc<ExtensionInstallFn>,
    run_services: Option<Arc<ExtensionInstallFn>>,
}

type ExtensionInstallFn =
    dyn for<'a> Fn(&mut RuntimeExtensionContext<'a>) -> Result<(), OtterError> + Send + Sync;

impl RuntimeExtensionInstaller {
    /// Build an advanced installer from a sendable callback.
    #[must_use]
    pub fn new(
        install: impl for<'a> Fn(&mut RuntimeExtensionContext<'a>) -> Result<(), OtterError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            install: Arc::new(install),
            run_services: None,
        }
    }

    /// Also install this run's host services with `services`: the
    /// per-isolate state the extension's static natives read
    /// ([`RuntimeExtensionContext::insert_host_service`]). It runs before the
    /// installer on a build, and alone on a runtime restored from a startup
    /// image, whose heap already holds everything the installer made.
    #[must_use]
    pub fn with_run_services(
        mut self,
        services: impl for<'a> Fn(&mut RuntimeExtensionContext<'a>) -> Result<(), OtterError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.run_services = Some(Arc::new(services));
        self
    }

    fn install(&self, realm: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
        self.install_run_services(realm)?;
        (self.install)(realm)
    }

    pub(super) fn install_run_services(
        &self,
        realm: &mut RuntimeExtensionContext<'_>,
    ) -> Result<(), OtterError> {
        match &self.run_services {
            Some(services) => services(realm),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for RuntimeExtensionInstaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeExtensionInstaller")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(super) enum ConfiguredRealmInstaller {
    Global(RuntimeGlobalInstaller),
    Extension(RuntimeExtensionInstaller),
}

impl ConfiguredRealmInstaller {
    pub(super) fn install(
        &self,
        interp: &mut otter_vm::Interpreter,
        capabilities: &CapabilitySet,
        hooks: &RuntimeHooks,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<(), OtterError> {
        match self {
            Self::Global(installer) => {
                let mut realm =
                    RuntimeRealmContext::new(interp, capabilities, hooks, runtime_task_spawner);
                installer.install(&mut realm)
            }
            Self::Extension(installer) => {
                let mut realm =
                    RuntimeExtensionContext::new(interp, capabilities, hooks, runtime_task_spawner);
                installer.install(&mut realm)
            }
        }
    }

    /// Only this run's host services, for a realm a startup image restored.
    pub(super) fn install_run_services(
        &self,
        interp: &mut otter_vm::Interpreter,
        capabilities: &CapabilitySet,
        hooks: &RuntimeHooks,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<(), OtterError> {
        match self {
            Self::Global(_) => Ok(()),
            Self::Extension(installer) => {
                let mut realm =
                    RuntimeExtensionContext::new(interp, capabilities, hooks, runtime_task_spawner);
                installer.install_run_services(&mut realm)
            }
        }
    }
}

/// Runtime-owned class-shaped global surface.
///
/// Product crates expose this opaque handle to embedders instead of exposing VM
/// class specs directly.
#[derive(Debug, Clone, Copy)]
pub struct GlobalClass {
    pub(super) inner: GlobalClassInner,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum GlobalClassInner {
    /// Legacy path — runtime builder installs via `Interpreter::install_global_class`.
    Spec(&'static RuntimeClassSpec),
    /// Bootstrap-style path — runtime builder calls the
    /// `BuiltinIntrinsic::install` fn pointer directly, same as the
    /// `BOOTSTRAP_ENTRIES` registry. Used by `couch!`-generated Web
    /// API classes.
    Intrinsic {
        install: fn(
            &mut otter_gc::GcHeap,
            otter_vm::JsObject,
        ) -> Result<(), otter_vm::js_surface::JsSurfaceError>,
        /// Post-install hook for members keyed by per-realm well-known
        /// symbols (`@@toStringTag`, `@@iterator`) — the same second
        /// phase the bootstrap registry walk runs for every entry.
        install_well_knowns: fn(
            &mut otter_gc::GcHeap,
            otter_vm::JsObject,
            &otter_vm::WellKnownSymbols,
        ) -> Result<(), otter_vm::js_surface::JsSurfaceError>,
        /// Co-located JS glue attached to the class declaration
        /// (`BuiltinIntrinsic::JS_GLUE`), evaluated after every global
        /// installer has run so the glue sees the full global surface.
        js_glue: Option<&'static str>,
        name: &'static str,
    },
}

/// A declared extension: native classes plus the JS half, installed
/// as one unit. Built by `romp!`; consumed by
/// [`RuntimeBuilder::extension`].
#[derive(Debug, Clone, Copy)]
pub struct Extension {
    /// Extension name (diagnostics).
    pub name: &'static str,
    /// Native class intrinsics, installed eagerly in declaration
    /// order (a subclass resolves its parent off the global, so
    /// parents precede children).
    pub classes: &'static [GlobalClass],
    /// One build-produced classic script, executed eagerly after native installers.
    /// Constituent files share its top-level scope; separate extensions retain
    /// declaration order. `None` declares no JS half.
    pub js: Option<ExtensionJs>,
}

impl Extension {
    /// Every global name the JS half defines, in declaration order.
    pub fn defined_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.js
            .iter()
            .flat_map(|entry| entry.defines.iter().copied())
    }
}

impl GlobalClass {
    /// Build a runtime global class handle from a runtime-owned static class
    /// spec.
    #[must_use]
    pub const fn from_runtime(raw: &'static RuntimeClassSpec) -> Self {
        Self {
            inner: GlobalClassInner::Spec(raw),
        }
    }

    /// Build a runtime global class handle from a `couch!`-generated
    /// `BuiltinIntrinsic`. Equivalent install shape to bootstrap
    /// registry entries — the runtime calls `I::install(heap, global)`
    /// at startup instead of routing through `RuntimeClassSpec`.
    #[must_use]
    pub const fn from_intrinsic<I: otter_vm::intrinsic_install::BuiltinIntrinsic>() -> Self {
        Self {
            inner: GlobalClassInner::Intrinsic {
                install: I::install,
                install_well_knowns: I::install_well_knowns,
                js_glue: I::JS_GLUE,
                name: I::NAME,
            },
        }
    }

    /// Constructor/global name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self.inner {
            GlobalClassInner::Spec(raw) => raw.constructor.name,
            GlobalClassInner::Intrinsic { name, .. } => name,
        }
    }
}
