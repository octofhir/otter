//! Runtime hook contracts for host-owned engine boundaries.
//!
//! # Contents
//! - [`RuntimeResolveHook`] and [`RuntimeLoadHook`] — module resolver and
//!   source loading extension points.
//! - [`RuntimeCompileHook`] — compile boundary for already loaded sources.
//! - [`RuntimeJobHook`] — runtime-owned job enqueue boundary.
//! - [`RuntimeDiagnosticHook`] — structured diagnostics sink.
//! - [`RuntimeCapabilityHook`] — capability policy override point.
//! - [`RuntimeHooks`] — cloneable hook set stored on the runtime session.
//!
//! # Invariants
//! - Hooks are `Send + Sync + 'static` so they can be shared by
//!   [`crate::RuntimeHandle`] without exposing isolate-local VM or GC state.
//! - Hook requests carry runtime DTOs and owned/public values only; package
//!   manager internals and raw VM handles do not cross this boundary.
//!
//! # See also
//! - [Engine architecture](../../../docs/book/src/engine/architecture.md)

use std::path::Path;
use std::sync::Arc;

use otter_compiler::CompiledModule;

use crate::module_loader::{ImportKind, ResolvedSource};
use crate::{CapabilitySet, Diagnostic, OtterError, SourceInput};

/// Module-resolution request passed to [`RuntimeResolveHook`].
#[derive(Debug, Clone, Copy)]
pub struct RuntimeResolveRequest<'a> {
    /// Import specifier text from source.
    pub specifier: &'a str,
    /// Canonical referrer URL when resolution is importer-aware.
    pub referrer: Option<&'a str>,
    /// Resolution kind (`import` or future `require`).
    pub kind: ImportKind,
    /// Active condition names, in resolver priority order.
    pub conditions: &'a [&'a str],
}

/// Loaded source request passed to [`RuntimeLoadHook`].
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLoadRequest<'a> {
    /// Import specifier text from source.
    pub specifier: &'a str,
    /// Canonical referrer URL when loading is importer-aware.
    pub referrer: Option<&'a str>,
}

/// Compile request passed to [`RuntimeCompileHook`].
#[derive(Debug, Clone, Copy)]
pub struct RuntimeCompileRequest<'a> {
    /// Source DTO produced by the loader boundary.
    pub source: &'a ResolvedSource,
}

/// Runtime job enqueue request passed to [`RuntimeJobHook`].
#[derive(Debug, Clone, Copy)]
pub struct RuntimeJobRequest<'a> {
    /// Stable job family label.
    pub kind: RuntimeJobKind,
    /// Human-readable origin used in diagnostics.
    pub origin: &'a str,
}

/// Runtime job families that cross the public hook boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeJobKind {
    /// VM microtask.
    Microtask,
    /// Timer callback.
    Timer,
    /// Host operation completion.
    HostOp,
    /// Dynamic module load/evaluation job.
    DynamicImport,
}

/// Capability family passed to [`RuntimeCapabilityHook`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeCapability {
    /// Filesystem read.
    Read,
    /// Filesystem write.
    Write,
    /// Network access.
    Net,
    /// Environment variable read.
    Env,
    /// Subprocess execution.
    Run,
    /// FFI/native library loading.
    Ffi,
}

/// Concrete resource requested by a capability check.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum CapabilityRequest<'a> {
    /// Filesystem path.
    Path(&'a Path),
    /// Network URL plus the canonical module/document URL that initiated it.
    ///
    /// The engine's default policy checks only the configured host allowlist.
    /// Embedders may use the complete URLs for richer generic policy without
    /// teaching the runtime about origins, documents, or navigation.
    Network {
        /// Absolute target URL.
        url: &'a url::Url,
        /// Absolute initiator URL when one exists.
        initiator: Option<&'a url::Url>,
    },
    /// Environment variable name.
    EnvVar(&'a str),
    /// Subprocess command name or path.
    Command(&'a str),
}

/// Runtime module-resolution hook.
pub trait RuntimeResolveHook: Send + Sync + 'static {
    /// Resolve `request` to a canonical target URL.
    fn resolve(&self, request: RuntimeResolveRequest<'_>) -> Result<String, OtterError>;
}

impl<F> RuntimeResolveHook for F
where
    F: for<'a> Fn(RuntimeResolveRequest<'a>) -> Result<String, OtterError> + Send + Sync + 'static,
{
    fn resolve(&self, request: RuntimeResolveRequest<'_>) -> Result<String, OtterError> {
        self(request)
    }
}

/// Runtime source-loading hook.
pub trait RuntimeLoadHook: Send + Sync + 'static {
    /// Load source for `request`.
    fn load(&self, request: RuntimeLoadRequest<'_>) -> Result<ResolvedSource, OtterError>;
}

impl<F> RuntimeLoadHook for F
where
    F: for<'a> Fn(RuntimeLoadRequest<'a>) -> Result<ResolvedSource, OtterError>
        + Send
        + Sync
        + 'static,
{
    fn load(&self, request: RuntimeLoadRequest<'_>) -> Result<ResolvedSource, OtterError> {
        self(request)
    }
}

/// Runtime compile hook.
pub trait RuntimeCompileHook: Send + Sync + 'static {
    /// Compile an already loaded source into bytecode plus metadata.
    fn compile(&self, request: RuntimeCompileRequest<'_>) -> Result<CompiledModule, OtterError>;
}

impl<F> RuntimeCompileHook for F
where
    F: for<'a> Fn(RuntimeCompileRequest<'a>) -> Result<CompiledModule, OtterError>
        + Send
        + Sync
        + 'static,
{
    fn compile(&self, request: RuntimeCompileRequest<'_>) -> Result<CompiledModule, OtterError> {
        self(request)
    }
}

/// Runtime-owned job enqueue hook.
pub trait RuntimeJobHook: Send + Sync + 'static {
    /// Enqueue a runtime job.
    fn enqueue_job(&self, request: RuntimeJobRequest<'_>) -> Result<(), OtterError>;
}

impl<F> RuntimeJobHook for F
where
    F: for<'a> Fn(RuntimeJobRequest<'a>) -> Result<(), OtterError> + Send + Sync + 'static,
{
    fn enqueue_job(&self, request: RuntimeJobRequest<'_>) -> Result<(), OtterError> {
        self(request)
    }
}

/// Runtime diagnostic sink hook.
pub trait RuntimeDiagnosticHook: Send + Sync + 'static {
    /// Emit a structured diagnostic.
    fn emit_diagnostic(&self, diagnostic: &Diagnostic);
}

impl<F> RuntimeDiagnosticHook for F
where
    F: Fn(&Diagnostic) + Send + Sync + 'static,
{
    fn emit_diagnostic(&self, diagnostic: &Diagnostic) {
        self(diagnostic);
    }
}

/// Runtime capability-check hook.
pub trait RuntimeCapabilityHook: Send + Sync + 'static {
    /// Return whether `request` is permitted under `capabilities`.
    fn check_capability(
        &self,
        capabilities: &CapabilitySet,
        capability: RuntimeCapability,
        request: &CapabilityRequest<'_>,
    ) -> bool;
}

impl<F> RuntimeCapabilityHook for F
where
    F: for<'a> Fn(&CapabilitySet, RuntimeCapability, &CapabilityRequest<'a>) -> bool
        + Send
        + Sync
        + 'static,
{
    fn check_capability(
        &self,
        capabilities: &CapabilitySet,
        capability: RuntimeCapability,
        request: &CapabilityRequest<'_>,
    ) -> bool {
        self(capabilities, capability, request)
    }
}

/// Cloneable runtime hook set stored by [`crate::RuntimeBuilder`].
#[derive(Clone, Default)]
pub struct RuntimeHooks {
    resolve: Option<Arc<dyn RuntimeResolveHook>>,
    load: Option<Arc<dyn RuntimeLoadHook>>,
    compile: Option<Arc<dyn RuntimeCompileHook>>,
    enqueue_job: Option<Arc<dyn RuntimeJobHook>>,
    diagnostic: Option<Arc<dyn RuntimeDiagnosticHook>>,
    capability: Option<Arc<dyn RuntimeCapabilityHook>>,
}

impl std::fmt::Debug for RuntimeHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHooks")
            .field("resolve", &self.resolve.is_some())
            .field("load", &self.load.is_some())
            .field("compile", &self.compile.is_some())
            .field("enqueue_job", &self.enqueue_job.is_some())
            .field("diagnostic", &self.diagnostic.is_some())
            .field("capability", &self.capability.is_some())
            .finish()
    }
}

impl RuntimeHooks {
    /// Set the module-resolution hook.
    #[must_use]
    pub fn with_resolve_hook(mut self, hook: impl RuntimeResolveHook) -> Self {
        self.resolve = Some(Arc::new(hook));
        self
    }

    /// Set the source-loading hook.
    #[must_use]
    pub fn with_load_hook(mut self, hook: impl RuntimeLoadHook) -> Self {
        self.load = Some(Arc::new(hook));
        self
    }

    /// Set the compile hook.
    #[must_use]
    pub fn with_compile_hook(mut self, hook: impl RuntimeCompileHook) -> Self {
        self.compile = Some(Arc::new(hook));
        self
    }

    /// Set the runtime job enqueue hook.
    #[must_use]
    pub fn with_job_hook(mut self, hook: impl RuntimeJobHook) -> Self {
        self.enqueue_job = Some(Arc::new(hook));
        self
    }

    /// Set the diagnostic sink hook.
    #[must_use]
    pub fn with_diagnostic_hook(mut self, hook: impl RuntimeDiagnosticHook) -> Self {
        self.diagnostic = Some(Arc::new(hook));
        self
    }

    /// Set the capability-check hook.
    #[must_use]
    pub fn with_capability_hook(mut self, hook: impl RuntimeCapabilityHook) -> Self {
        self.capability = Some(Arc::new(hook));
        self
    }

    /// Return the module-resolution hook, when installed.
    #[must_use]
    pub fn resolve_hook(&self) -> Option<&dyn RuntimeResolveHook> {
        self.resolve.as_deref()
    }

    /// Return the source-loading hook, when installed.
    #[must_use]
    pub fn load_hook(&self) -> Option<&dyn RuntimeLoadHook> {
        self.load.as_deref()
    }

    /// Return the compile hook, when installed.
    #[must_use]
    pub fn compile_hook(&self) -> Option<&dyn RuntimeCompileHook> {
        self.compile.as_deref()
    }

    /// Return the job enqueue hook, when installed.
    #[must_use]
    pub fn job_hook(&self) -> Option<&dyn RuntimeJobHook> {
        self.enqueue_job.as_deref()
    }

    /// Return the diagnostic hook, when installed.
    #[must_use]
    pub fn diagnostic_hook(&self) -> Option<&dyn RuntimeDiagnosticHook> {
        self.diagnostic.as_deref()
    }

    /// Return the capability hook, when installed.
    #[must_use]
    pub fn capability_hook(&self) -> Option<&dyn RuntimeCapabilityHook> {
        self.capability.as_deref()
    }
}

/// Default capability policy used when no custom hook is installed.
#[must_use]
pub fn default_check_capability(
    capabilities: &CapabilitySet,
    capability: RuntimeCapability,
    request: &CapabilityRequest<'_>,
) -> bool {
    match (capability, request) {
        (RuntimeCapability::Read, CapabilityRequest::Path(path)) => {
            capabilities.read.matches_path(path)
        }
        (RuntimeCapability::Write, CapabilityRequest::Path(path)) => {
            capabilities.write.matches_path(path)
        }
        (RuntimeCapability::Net, CapabilityRequest::Network { url, .. }) => {
            network_url_allowed(capabilities, url)
        }
        (RuntimeCapability::Env, CapabilityRequest::EnvVar(name)) => capabilities.env_allows(name),
        (RuntimeCapability::Run, CapabilityRequest::Command(command)) => {
            capabilities.run.matches(command)
        }
        (RuntimeCapability::Ffi, CapabilityRequest::Path(path)) => {
            capabilities.ffi.matches_path(path)
        }
        _ => false,
    }
}

fn network_url_allowed(capabilities: &CapabilitySet, url: &url::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if capabilities.net.matches(host) {
        return true;
    }
    url.port()
        .is_some_and(|port| capabilities.net.matches(&format!("{host}:{port}")))
}

/// Apply the active runtime hook while preserving non-overridable security
/// filters. Capability hooks may replace the configured allow policy, but they
/// cannot expose environment names covered by the built-in secret denylist.
#[must_use]
pub(crate) fn check_capability_with_hooks(
    hooks: &RuntimeHooks,
    capabilities: &CapabilitySet,
    capability: RuntimeCapability,
    request: &CapabilityRequest<'_>,
) -> bool {
    if let (RuntimeCapability::Env, CapabilityRequest::EnvVar(name)) = (capability, request)
        && crate::env_name_is_builtin_denied(name)
    {
        return false;
    }
    hooks.capability_hook().map_or_else(
        || default_check_capability(capabilities, capability, request),
        |hook| hook.check_capability(capabilities, capability, request),
    )
}

/// Compile source through the default script compiler.
///
/// Hook implementations can call this helper to preserve the runtime's
/// standard script compile behavior for [`SourceInput`] values.
pub fn default_compile_source(
    source: &SourceInput,
    specifier: &str,
) -> Result<CompiledModule, OtterError> {
    otter_compiler::compile_script_source_to_module(&source.text, source.kind, specifier)
        .map_err(|err| crate::map_compile_error(err, specifier))
}
