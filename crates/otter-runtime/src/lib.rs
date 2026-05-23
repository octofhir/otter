//! Public embedding API for the Otter foundation engine.
//!
//! Two-layer surface per
//! [the public runtime architecture](../../../docs/book/src/engine/architecture.md):
//!
//! - **Layer A — [`Otter`]**: zero-config entry point. The simple
//!   case for embedders ("just run a script").
//! - **Layer B — [`Runtime`] / [`RuntimeBuilder`]**: opt-in
//!   advanced layer for capabilities, custom module loading, trace
//!   sinks, profiling.
//!
//! Every fallible call returns [`Result<_, OtterError>`] —
//! a single, structured, `serde::Serialize` enum. No
//! `Box<dyn Error>` anywhere on the public surface.
//!
//! # Contents
//! - [`Otter`] — Layer A wrapper.
//! - [`Runtime`], [`RuntimeBuilder`] — Layer B.
//! - [`SourceInput`] — JS / TS source bundles.
//! - [`ExecutionResult`] — successful run output.
//! - [`OtterError`], [`ConfigError`], [`IoErrorKind`] — error model.
//! - [`InterruptHandle`] — cooperative cancellation.
//!
//! # Runtime Session
//! [`Runtime`] is the active runtime session owner. [`RuntimeBuilder`] captures
//! session configuration — capabilities, module-loader DTOs, hosted modules,
//! global surfaces, console sink, heap cap, timeout, and stack limit — and
//! materializes either a single-threaded [`Runtime`] or a sendable
//! [`RuntimeHandle`].
//!
//! The session owns the VM isolate (`Interpreter`) and all runtime boundary
//! decisions around loading, compiling, capability checks, hosted-module
//! installation, diagnostics mapping, and microtask draining. Module execution
//! enters through [`Runtime::run_module`] / [`Runtime::check_file`]: those
//! methods ask the runtime-owned loader state for an entry-aware
//! [`module_loader::ModuleLoader`], build through the runtime-owned module graph
//! state, record source spans into the session source-map table, then run or
//! validate the linked bytecode without exposing package-manager internals to
//! `otter-vm`.
//!
//! # Invariants
//! - `otter-vm` receives bytecode, runtime surface builders, and capability
//!   decisions only through `otter-runtime`; it never sees package-manager
//!   lockfiles, registries, or graph mutation APIs.
//! - Package-manager data enters this crate only as the read-only
//!   [`module_loader::LoaderPackageGraph`] DTO.
//! - Loader, module-graph, source-map, diagnostics, and package-manager DTO
//!   state are owned by the `Runtime` session; no hidden global resolver state
//!   is used by the active runtime path.
//!
//! # See also
//! - [Engine architecture](../../../docs/book/src/engine/architecture.md)
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

pub mod compiled_program;
pub mod diagnostics;
pub mod error;
mod event_loop;
pub mod handle;
pub mod hooks;
mod host_services;
pub mod module_graph;
pub mod module_loader;
mod module_records;
mod package_graph_resolver;
mod process;
pub mod promise_registry;
pub mod structured_clone;
pub mod surface;
pub mod worker;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use otter_bytecode::{BytecodeModule, SpanEntry};
use otter_compiler::{
    compile_script_program, compile_script_source, compile_script_source_to_module,
};
use otter_gc::GcStats;
use otter_syntax::{SourceKind, SyntaxDiagnostic, SyntaxError, detect_source_kind, with_program};
use otter_vm::{EvalCompileOptions, ExecutionContext, Interpreter, InterruptFlag, JsObject};
use serde::{Deserialize, Serialize};

pub use compiled_program::CompiledProgram;
pub use diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticCode, DiagnosticKind, StackFrame};
pub use error::{ConfigError, IoErrorKind, OtterError, error_schema_version};
pub use event_loop::RuntimeLiveness;
pub use handle::{RuntimeActivityStats, RuntimeHandle};
pub use hooks::{
    CapabilityRequest, RuntimeCapability, RuntimeCapabilityHook, RuntimeCompileHook,
    RuntimeCompileRequest, RuntimeDiagnosticHook, RuntimeHooks, RuntimeJobHook, RuntimeJobKind,
    RuntimeJobRequest, RuntimeLoadHook, RuntimeLoadRequest, RuntimeResolveHook,
    RuntimeResolveRequest, default_check_capability, default_compile_source,
};
pub use otter_compiler::{
    CompiledExport, CompiledImport, CompiledImportKind, CompiledModule, CompiledModuleMetadata,
    CompiledSourceSpan, LiveBindingSlot,
};
pub use otter_gc;
pub use otter_vm::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
pub use otter_vm::{RuntimeBudget, RuntimeBudgetExceededAction, RuntimeBudgetStats};
pub use promise_registry::{HostSettleOutcome, PromiseId};
pub use structured_clone::{
    StructuredCloneError, StructuredCloneMapEntry, StructuredCloneNumber, StructuredCloneOptions,
    StructuredCloneProperty, StructuredCloneTransfer, StructuredCloneTransferId,
    StructuredCloneTransferKind, StructuredCloneTransferList, StructuredCloneTransferListError,
    StructuredCloneValue,
};
pub use surface::{
    RuntimeAccessorSpec, RuntimeAttr, RuntimeClassSpec, RuntimeConstSpec, RuntimeConstValue,
    RuntimeConstructorSpec, RuntimeHostObjectData, RuntimeHostObjectError, RuntimeJsObject,
    RuntimeJsString, RuntimeMethodSpec, RuntimeNamespaceSpec, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeNativeFastFn, RuntimeNativeFn, RuntimeNumberValue,
    RuntimeObjectBuilder, RuntimePropertySpec, RuntimeSurfaceError, RuntimeValue, runtime_accessor,
    runtime_alloc_object, runtime_arg_to_string, runtime_array_from_elements, runtime_class,
    runtime_constant, runtime_constructor, runtime_getter, runtime_method,
    runtime_method_with_attrs, runtime_namespace, runtime_native_dynamic, runtime_native_static,
    runtime_optional_arg_to_string, runtime_property, runtime_set_property, runtime_string_value,
    runtime_this_object, runtime_type_error, runtime_with_host_data, runtime_with_host_data_mut,
};
pub use worker::{
    OtterPool, OtterPoolBuilder, Worker, WorkerBuilder, WorkerId, WorkerShutdownReport,
};

/// Runtime-owned hosted module installation context.
///
/// Installers use this context to populate the hosted module namespace during a
/// single runtime mutator turn. The context owns the namespace object builder
/// and exposes configured capabilities for boundary checks.
pub struct HostedModuleCtx<'rt> {
    builder: RuntimeObjectBuilder<'rt>,
    capabilities: &'rt CapabilitySet,
}

impl<'rt> HostedModuleCtx<'rt> {
    fn new(
        interp: &'rt mut Interpreter,
        capabilities: &'rt CapabilitySet,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            builder: RuntimeObjectBuilder::new_in_interpreter(interp)?,
            capabilities,
        })
    }

    /// Return the configured capability set for this runtime.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilitySet {
        self.capabilities
    }

    /// Define a native method on the module namespace object.
    ///
    /// This is a temporary low-level method while the stable hosted module
    /// builder API is being expanded. It routes through the same static VM
    /// builder backend as other JS-visible surfaces.
    pub fn method(
        &mut self,
        name: &'static str,
        length: u8,
        call: HostedNativeCall,
    ) -> Result<&mut Self, String> {
        self.builder
            .method(
                name,
                length,
                call.into_runtime(),
                RuntimeAttr::builtin_function(),
            )
            .map_err(|err| err.to_string())?;
        Ok(self)
    }

    /// Define a static builtin method on the module namespace object.
    pub fn builtin_method(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeFastFn,
    ) -> Result<&mut Self, String> {
        self.method(name, length, HostedNativeCall::static_fn(call))
    }

    /// Define an ordinary data property on the module namespace object.
    pub fn property(
        &mut self,
        name: &'static str,
        value: RuntimeValue,
    ) -> Result<&mut Self, String> {
        self.builder
            .data_property(name, value)
            .map_err(|err| err.to_string())?;
        Ok(self)
    }

    /// Define a read-only data property on the module namespace object.
    pub fn readonly_property(
        &mut self,
        name: &'static str,
        value: RuntimeValue,
    ) -> Result<&mut Self, String> {
        self.builder
            .readonly_property(name, value)
            .map_err(|err| err.to_string())?;
        Ok(self)
    }

    fn build(self) -> JsObject {
        self.builder.build()
    }
}

/// Runtime-owned hosted module installer callback.
pub type HostedModuleBuilderInstall = for<'rt> fn(&mut HostedModuleCtx<'rt>) -> Result<(), String>;

/// Opaque native call target for hosted module builders.
///
/// This is the runtime-facing call handle used by [`HostedModuleCtx`]. The
/// current implementation still adapts to the native call backend, but the
/// hosted module builder API no longer exposes that VM enum in its method
/// signatures.
#[derive(Debug, Clone)]
pub struct HostedNativeCall {
    raw: RuntimeNativeCall,
}

impl HostedNativeCall {
    /// Build a hosted native call from a static function pointer.
    #[must_use]
    pub const fn static_fn(raw: RuntimeNativeFastFn) -> Self {
        Self {
            raw: RuntimeNativeCall::Static(raw),
        }
    }

    /// Build a hosted native call from a captured-state dynamic native
    /// function. Use this only where the hosted module needs immutable
    /// runtime-owned captures such as capability snapshots.
    #[must_use]
    pub fn dynamic(raw: Arc<RuntimeNativeFn>) -> Self {
        Self {
            raw: RuntimeNativeCall::Dynamic(raw),
        }
    }

    fn into_runtime(self) -> RuntimeNativeCall {
        self.raw
    }
}

/// Runtime-hosted module installer.
///
/// This is an opaque runtime-owned handle. Product crates should construct it
/// through runtime APIs instead of exposing VM installer details as part of
/// their public surface.
#[derive(Debug, Clone, Copy)]
pub struct HostedModuleInstall {
    raw: HostedModuleBuilderInstall,
}

impl HostedModuleInstall {
    /// Build an installer from a runtime-owned hosted module context callback.
    #[must_use]
    pub const fn new(raw: HostedModuleBuilderInstall) -> Self {
        Self { raw }
    }

    fn install(
        self,
        interp: &mut Interpreter,
        capabilities: &CapabilitySet,
    ) -> Result<JsObject, String> {
        let mut ctx = HostedModuleCtx::new(interp, capabilities)
            .map_err(|err| format!("out of memory: {err}"))?;
        (self.raw)(&mut ctx)?;
        Ok(ctx.build())
    }
}

/// One runtime-hosted module.
#[derive(Debug, Clone, Copy)]
pub struct HostedModule {
    /// Module specifier, for example `otter:kv`.
    specifier: &'static str,
    /// Namespace installer.
    install: HostedModuleInstall,
}

impl HostedModule {
    /// Create a hosted module spec from an opaque runtime installer.
    #[must_use]
    pub const fn new(specifier: &'static str, install: HostedModuleInstall) -> Self {
        Self { specifier, install }
    }

    /// Module specifier, for example `otter:kv`.
    #[must_use]
    pub const fn specifier(self) -> &'static str {
        self.specifier
    }

    fn install(
        self,
        interp: &mut Interpreter,
        capabilities: &CapabilitySet,
    ) -> Result<JsObject, String> {
        self.install.install(interp, capabilities)
    }
}

/// Runtime-owned class-shaped global surface.
///
/// Product crates expose this opaque handle to embedders instead of exposing VM
/// class specs directly.
#[derive(Debug, Clone, Copy)]
pub struct GlobalClass {
    raw: &'static RuntimeClassSpec,
}

impl GlobalClass {
    /// Build a runtime global class handle from a runtime-owned static class
    /// spec.
    #[must_use]
    pub const fn from_runtime(raw: &'static RuntimeClassSpec) -> Self {
        Self { raw }
    }

    /// Constructor/global name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.raw.constructor.name
    }

    fn raw(self) -> &'static RuntimeClassSpec {
        self.raw
    }
}

/// Default heap cap (256 MiB) when none is configured.
pub const DEFAULT_MAX_HEAP_BYTES: u64 = 256 * 1024 * 1024;

/// Default per-`run_*` timeout (30 s) when none is configured.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default JS call-stack limit.
pub const DEFAULT_MAX_STACK_DEPTH: u32 = 1024;

/// Source bundle accepted by the runtime.
#[derive(Debug, Clone)]
pub struct SourceInput {
    /// Source text.
    pub text: String,
    /// Source kind.
    pub kind: SourceKind,
    /// Optional originating path (for diagnostics).
    pub path: Option<PathBuf>,
}

impl SourceInput {
    /// Build a JavaScript source bundle from in-memory text.
    #[must_use]
    pub fn from_javascript(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SourceKind::JavaScript,
            path: None,
        }
    }

    /// Build a TypeScript source bundle from in-memory text.
    #[must_use]
    pub fn from_typescript(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SourceKind::TypeScript,
            path: None,
        }
    }

    /// Read a source bundle from disk, detecting kind by extension.
    ///
    /// # Errors
    /// - [`OtterError::SourceKind`] when the extension is not a
    ///   foundation extension.
    /// - [`OtterError::Io`] when the file cannot be read.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, OtterError> {
        let path = path.as_ref();
        let kind = detect_source_kind(path).ok_or_else(|| OtterError::SourceKind {
            path: path.to_path_buf(),
            extension: path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string(),
        })?;
        let text = std::fs::read_to_string(path).map_err(|e| OtterError::Io {
            path: path.to_path_buf(),
            kind: IoErrorKind::from_std(e.kind()),
            message: e.to_string(),
        })?;
        Ok(Self {
            text,
            kind,
            path: Some(path.to_path_buf()),
        })
    }
}

/// Successful run output.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Completion value rendered at the isolate boundary.
    ///
    /// Public async handles must not expose internal VM values or GC
    /// handles. The local [`Runtime`] therefore renders the completion
    /// before sending it through [`RuntimeHandle`].
    completion: String,
    /// Process-style exit status requested by JS-visible runtime APIs.
    exit_code: u8,
    /// Wall-clock duration.
    pub duration: Duration,
}

impl ExecutionResult {
    /// Build from an interpreter completion value.
    #[must_use]
    fn from_vm_value(
        completion: otter_vm::Value,
        duration: Duration,
        heap: &otter_gc::GcHeap,
    ) -> Self {
        Self {
            completion: completion.display_string(heap),
            exit_code: 0,
            duration,
        }
    }

    /// Build from a host-visible runtime exit request.
    #[must_use]
    fn from_exit_code(code: u8, duration: Duration) -> Self {
        Self {
            completion: "undefined".to_string(),
            exit_code: code,
            duration,
        }
    }

    #[must_use]
    fn with_exit_code(mut self, code: u8) -> Self {
        self.exit_code = code;
        self
    }

    /// Render the completion value for CLI preview output.
    #[must_use]
    pub fn completion_string(&self) -> &str {
        &self.completion
    }

    /// Process-style exit status requested by runtime APIs.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

/// Deno-inspired, UX-first capability set.
///
/// Each capability is a [`Permission<T>`] with three states:
///
/// - `Deny` — operations of this kind are rejected;
/// - `AllowAll` — operations are permitted unconditionally;
/// - `Scoped { allow_list, deny_list }` — power-user form: the
///   allow / deny patterns narrow the set further. Most users
///   never need this state.
///
/// [`CapabilitySet::default`] is deny-by-default. Embedders must opt in to
/// capabilities explicitly, or use [`CapabilitySet::allow_all`] for trusted
/// development scenarios.
///
/// | Capability | Default | Why |
/// | --- | --- | --- |
/// | `read` | `Deny` | Filesystem access is a host resource and must be explicit. |
/// | `write` | `Deny` | Filesystem mutation is rare in scripts and dangerous by default. |
/// | `net` | `Deny` | Network access is opt-in. |
/// | `env` | `Deny` | Environment variables may contain secrets. |
/// | `run` | `Deny` | Subprocess execution is opt-in. |
/// | `ffi` | `Deny` | Native library loading is opt-in. |
///
/// Two convenience presets:
///
/// - [`CapabilitySet::sandbox`] — deny everything. Equivalent to
///   [`CapabilitySet::default`] and the CLI's `--sandbox`.
/// - [`CapabilitySet::allow_all`] — allow everything unconditionally.
///   Equivalent to the CLI's `--allow-all`.
///
/// Power users can still pass scoped pattern lists
/// (`--allow-net=api.example.com`) but the CLI flags also accept the
/// **boolean form** (`--allow-net`) which upgrades the capability to
/// `AllowAll`.
///
/// Runtime and product crates must enforce these checks at the Rust boundary
/// before opening host resources or starting host work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySet {
    /// Filesystem read permission.
    pub read: Permission<PathBuf>,
    /// Filesystem write permission.
    pub write: Permission<PathBuf>,
    /// Network permission. Patterns are `host[:port]`.
    pub net: Permission<String>,
    /// Environment variable permission. Patterns are variable names.
    pub env: Permission<String>,
    /// Subprocess permission. Patterns are command names / paths.
    pub run: Permission<String>,
    /// FFI loading permission. Patterns are library names / paths.
    pub ffi: Permission<PathBuf>,
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self::sandbox()
    }
}

/// Built-in secret-name deny patterns enforced on top of any
/// configured `env` permission.
///
/// These patterns are **always** denied, even under `--allow-all` or
/// `--allow-env=*`, so a stray `--allow-env` does not exfiltrate
/// secrets. Embedders can extend the list at the runtime boundary in
/// later slices.
pub const ENV_BUILTIN_DENY_PATTERNS: &[&str] = &[
    "*_SECRET",
    "*_TOKEN",
    "*_PASSWORD",
    "*_API_KEY",
    "AWS_*",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];

impl CapabilitySet {
    /// Test whether reading the named environment variable is
    /// permitted, taking [`ENV_BUILTIN_DENY_PATTERNS`] into account.
    ///
    /// The built-in deny list **always** wins, even under
    /// `Permission::AllowAll`.
    #[must_use]
    pub fn env_allows(&self, name: &str) -> bool {
        for pattern in ENV_BUILTIN_DENY_PATTERNS {
            if glob_match_string(pattern, name) {
                return false;
            }
        }
        self.env.matches(name)
    }

    /// Sandbox: deny every capability. Use when running untrusted
    /// code. The CLI maps `--sandbox` to this.
    #[must_use]
    pub fn sandbox() -> Self {
        Self {
            read: Permission::Deny,
            write: Permission::Deny,
            net: Permission::Deny,
            env: Permission::Deny,
            run: Permission::Deny,
            ffi: Permission::Deny,
        }
    }

    /// Grant everything unconditionally. The CLI maps `--allow-all`
    /// to this. Development / testing only.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            read: Permission::AllowAll,
            write: Permission::AllowAll,
            net: Permission::AllowAll,
            env: Permission::AllowAll,
            run: Permission::AllowAll,
            ffi: Permission::AllowAll,
        }
    }
}

/// Per-resource permission state.
///
/// Generic over the **pattern** type (e.g., `PathBuf` for fs,
/// `String` for hosts / env names). Mirrors Deno's per-permission
/// `--allow-X[=patterns]` / `--deny-X[=patterns]` model.
///
/// Patterns of type `String` (used by `net`, `env`, `run`) support
/// **glob-style wildcards**:
///
/// - `*` (alone) matches anything;
/// - `PREFIX_*` matches values starting with `PREFIX_` — useful for
///   environment variable scoping (`VITE_APP_*`, `NEXT_PUBLIC_*`,
///   `OTEL_*`);
/// - `*_SUFFIX` matches values ending with `_SUFFIX`;
/// - `PREFIX_*_SUFFIX` matches values that have both;
/// - exact strings (no `*`) match exactly.
///
/// Path patterns (`PathBuf`) are matched as **path prefixes** — an
/// allow pattern of `/var/data` matches `/var/data/x.json` and any
/// descendant. Wildcards are not supported in path patterns at this
/// slice; if needed, a future amendment adds them.
///
/// Use [`Permission::matches`] to test whether a concrete value is
/// permitted under the current rule set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Permission<T> {
    /// No operations of this kind are permitted.
    #[default]
    Deny,
    /// All operations of this kind are permitted unconditionally.
    AllowAll,
    /// Allow operations matching `allow_list`, except those matching
    /// `deny_list`. An empty `allow_list` is the same as `Deny`;
    /// `deny_list` always wins on conflict.
    Scoped {
        /// Allow patterns. Empty = deny.
        allow_list: Vec<T>,
        /// Deny patterns. Override `allow_list` on conflict.
        #[serde(default = "Vec::new")]
        deny_list: Vec<T>,
    },
}

impl<T> Permission<T> {
    /// Convenience: allow only the patterns listed (`Scoped` with no
    /// deny list).
    #[must_use]
    pub fn allow<I: IntoIterator<Item = T>>(items: I) -> Self {
        Self::Scoped {
            allow_list: items.into_iter().collect(),
            deny_list: Vec::new(),
        }
    }

    /// Convenience: allow `allow_list` but explicitly deny anything
    /// in `deny_list`.
    #[must_use]
    pub fn allow_except<A, D>(allow: A, deny: D) -> Self
    where
        A: IntoIterator<Item = T>,
        D: IntoIterator<Item = T>,
    {
        Self::Scoped {
            allow_list: allow.into_iter().collect(),
            deny_list: deny.into_iter().collect(),
        }
    }

    /// `true` when this permission is unconditional (`AllowAll`).
    #[must_use]
    pub const fn is_allow_all(&self) -> bool {
        matches!(self, Self::AllowAll)
    }

    /// `true` when this permission rejects every operation.
    #[must_use]
    pub fn is_deny(&self) -> bool {
        match self {
            Self::Deny => true,
            Self::Scoped { allow_list, .. } => allow_list.is_empty(),
            Self::AllowAll => false,
        }
    }
}

impl Permission<String> {
    /// Test whether `value` is permitted under this string permission.
    ///
    /// Patterns support wildcards (`*`, `PREFIX_*`, `*_SUFFIX`,
    /// `PREFIX_*_SUFFIX`); see the type-level documentation.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Deny => false,
            Self::AllowAll => true,
            Self::Scoped {
                allow_list,
                deny_list,
            } => {
                if deny_list
                    .iter()
                    .any(|p| glob_match_string(p.as_str(), value))
                {
                    return false;
                }
                allow_list
                    .iter()
                    .any(|p| glob_match_string(p.as_str(), value))
            }
        }
    }
}

impl Permission<PathBuf> {
    /// Test whether `value` is permitted under this path permission.
    ///
    /// Path patterns are **prefix matches**: an allow pattern of
    /// `/var/data` matches `/var/data` and any descendant.
    #[must_use]
    pub fn matches_path(&self, value: &Path) -> bool {
        match self {
            Self::Deny => false,
            Self::AllowAll => true,
            Self::Scoped {
                allow_list,
                deny_list,
            } => {
                if deny_list.iter().any(|p| value.starts_with(p)) {
                    return false;
                }
                allow_list.iter().any(|p| value.starts_with(p))
            }
        }
    }
}

/// Glob-style match for permission patterns.
///
/// Supports `*` (any-prefix / any-suffix / both) and exact strings.
/// `*` in the middle of a pattern is treated as a single wildcard
/// segment so `A_*_B` matches values that start with `A_` and end
/// with `_B`. Multiple `*` in one pattern are not supported.
fn glob_match_string(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let star_count = pattern.matches('*').count();
    if star_count == 0 {
        return pattern == value;
    }
    if star_count > 1 {
        // Foundation-phase: only one `*` per pattern is recognised.
        // Treat pathological patterns conservatively as no match.
        let (prefix, rest) = match pattern.split_once('*') {
            Some(p) => p,
            None => return false,
        };
        let suffix = match rest.rsplit_once('*') {
            Some((_, s)) => s,
            None => rest,
        };
        return value.starts_with(prefix) && value.ends_with(suffix);
    }
    let (prefix, suffix) = match pattern.split_once('*') {
        Some(p) => p,
        None => return false,
    };
    value.starts_with(prefix) && value.ends_with(suffix)
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn env_prefix_wildcard_matches() {
        let perm = Permission::<String>::allow(["VITE_APP_*".to_string()]);
        assert!(perm.matches("VITE_APP_API_URL"));
        assert!(perm.matches("VITE_APP_"));
        assert!(!perm.matches("OTHER_VAR"));
    }

    #[test]
    fn env_suffix_wildcard_matches() {
        let perm = Permission::<String>::allow(["*_SECRET".to_string()]);
        assert!(perm.matches("API_SECRET"));
        assert!(!perm.matches("API_KEY"));
    }

    #[test]
    fn env_deny_overrides_allow() {
        let perm = Permission::<String>::allow_except(
            ["VITE_*".to_string()],
            ["VITE_INTERNAL_*".to_string()],
        );
        assert!(perm.matches("VITE_PUBLIC"));
        assert!(!perm.matches("VITE_INTERNAL_TOKEN"));
    }

    #[test]
    fn allow_all_matches_anything() {
        let perm = Permission::<String>::AllowAll;
        assert!(perm.matches("anything"));
    }

    #[test]
    fn deny_matches_nothing() {
        let perm = Permission::<String>::Deny;
        assert!(!perm.matches("anything"));
    }

    #[test]
    fn capability_default_is_deny_by_default() {
        let caps = CapabilitySet::default();
        assert!(caps.read.is_deny());
        assert!(caps.write.is_deny());
        assert!(caps.net.is_deny());
        assert!(caps.env.is_deny());
        assert!(caps.run.is_deny());
        assert!(caps.ffi.is_deny());
    }

    #[test]
    fn builtin_secret_patterns_always_denied() {
        // Even with allow-all, secret-named env vars stay denied.
        let caps = CapabilitySet::allow_all();
        assert!(!caps.env_allows("AWS_SECRET_ACCESS_KEY"));
        assert!(!caps.env_allows("SOMETHING_TOKEN"));
        assert!(!caps.env_allows("MY_API_KEY"));
        assert!(!caps.env_allows("OPENAI_API_KEY"));
        assert!(caps.env_allows("VITE_PUBLIC_URL"));
    }

    #[test]
    fn vite_app_prefix_allowed_via_scoped_pattern() {
        let caps = CapabilitySet {
            env: Permission::<String>::allow(["VITE_APP_*".to_string()]),
            ..CapabilitySet::sandbox()
        };
        assert!(caps.env_allows("VITE_APP_API_URL"));
        assert!(!caps.env_allows("HOME"));
        // Built-in secret-deny still wins:
        assert!(!caps.env_allows("VITE_APP_API_KEY"));
    }
}

/// Cloneable cooperative-cancellation handle.
#[derive(Debug, Clone)]
pub struct InterruptHandle(InterruptFlag);

impl InterruptHandle {
    /// Trip the underlying flag from any thread.
    pub fn interrupt(&self) {
        self.0.interrupt();
    }

    /// Check the flag without resetting it.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.0.is_set()
    }

    /// Clear a previous interrupt.
    pub fn reset(&self) {
        self.0.reset();
    }

    pub(crate) fn raw_flag(&self) -> InterruptFlag {
        self.0.clone()
    }
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    max_heap_bytes: u64,
    timeout: Duration,
    max_stack_depth: u32,
    capabilities: CapabilitySet,
    loader: Option<module_loader::LoaderConfig>,
    hosted_modules: Vec<HostedModule>,
    global_classes: Vec<GlobalClass>,
    console_sink: ConsoleSinkHandle,
    hooks: RuntimeHooks,
    process_argv: Vec<String>,
    process_cwd: PathBuf,
}

#[derive(Debug, Clone)]
struct RuntimeModuleLoaderState {
    configured: Option<module_loader::LoaderConfig>,
}

impl RuntimeModuleLoaderState {
    fn new(configured: Option<module_loader::LoaderConfig>) -> Self {
        Self { configured }
    }

    fn for_entry(
        &self,
        entry_path: &Path,
        hosted_modules: &[HostedModule],
        package_manager: &RuntimePackageManagerHandle,
        capabilities: &CapabilitySet,
    ) -> module_loader::ModuleLoader {
        let mut cfg = match &self.configured {
            Some(cfg) => cfg.clone(),
            None => {
                let base_dir = entry_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                module_loader::LoaderConfig::new(base_dir)
            }
        };
        cfg.hosted_specifiers
            .extend(hosted_modules.iter().map(|m| m.specifier().to_string()));
        package_manager.apply_to_loader_config(&mut cfg);
        // Propagate the runtime's capability state so the loader
        // can reject `http:` / `https:` specifiers (and future
        // privileged shapes) without consulting any extra hook.
        cfg.capabilities = capabilities.clone();
        module_loader::ModuleLoader::with_config(cfg)
    }
}

#[derive(Debug, Default)]
struct RuntimeModuleGraphState {
    last_entry_url: Option<String>,
    last_module_count: usize,
}

impl RuntimeModuleGraphState {
    fn load_program(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry_path: &Path,
    ) -> Result<module_graph::LinkedProgram, module_graph::GraphError> {
        let linked = module_graph::load_program(loader, entry_path)?;
        self.last_entry_url = Some(linked.entry_url.clone());
        self.last_module_count = linked.module.module_inits.len();
        Ok(linked)
    }
}

/// Per-runtime source-map table.
///
/// Built incrementally by [`Self::record_compiled_metadata`] every
/// time the runtime compiles a module / script — each
/// [`CompiledSourceSpan`] is keyed by `(module_url, function_id)`
/// because PC namespaces are per-function (two functions in the
/// same source file both start at `pc = 0`).
///
/// The PC vectors are kept sorted by ascending `pc` so
/// [`Self::resolve_frame_span`] can binary-search for the
/// predecessor entry matching a live frame's PC. The compiler
/// already emits spans in PC order, so the push path keeps the
/// invariant for free.
///
/// # See also
/// - [`otter_vm::snapshot_frames`] — the in-VM counterpart that
///   reads [`otter_bytecode::Function::spans`] directly.
#[derive(Debug, Default)]
struct RuntimeSourceMapTable {
    by_module_url: RefCell<BTreeMap<String, BTreeMap<u32, Vec<SpanEntry>>>>,
}

impl RuntimeSourceMapTable {
    fn record_compiled_metadata(&self, metadata: &CompiledModuleMetadata) {
        let mut by_module_url = self.by_module_url.borrow_mut();
        for span in &metadata.spans {
            by_module_url
                .entry(span.module_url.clone())
                .or_default()
                .entry(span.function_id)
                .or_default()
                .push(SpanEntry {
                    pc: span.pc,
                    span: span.span,
                });
        }
    }

    /// Map a `(module_url, function_id, pc)` triple back to the
    /// original source byte range.
    ///
    /// Returns the span for the **predecessor entry** — the
    /// largest `entry.pc <= pc` — to mirror
    /// [`otter_vm::snapshot_frames`]'s lookup when the table has
    /// no exact PC match. Returns `None` when the module URL
    /// has no compiled metadata, when the function id is not in
    /// the module, or when the function's span table is empty.
    fn resolve_frame_span(
        &self,
        module_url: &str,
        function_id: u32,
        pc: u32,
    ) -> Option<(u32, u32)> {
        let by_module_url = self.by_module_url.borrow();
        let by_function = by_module_url.get(module_url)?;
        let spans = by_function.get(&function_id)?;
        if spans.is_empty() {
            return None;
        }
        let idx = spans.partition_point(|s| s.pc <= pc);
        if idx == 0 {
            spans.first().map(|s| s.span)
        } else {
            Some(spans[idx - 1].span)
        }
    }

    #[cfg(test)]
    fn contains_module(&self, module_url: &str) -> bool {
        self.by_module_url.borrow().contains_key(module_url)
    }
}

#[derive(Debug, Default)]
struct RuntimeDiagnosticsSink {
    emitted: RefCell<Vec<Diagnostic>>,
}

impl RuntimeDiagnosticsSink {
    fn emit(&self, hooks: &RuntimeHooks, diagnostic: &Diagnostic) {
        self.emitted.borrow_mut().push(diagnostic.clone());
        if let Some(hook) = hooks.diagnostic_hook() {
            hook.emit_diagnostic(diagnostic);
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RuntimePackageManagerHandle {
    graph: Option<module_loader::LoaderPackageGraph>,
}

impl RuntimePackageManagerHandle {
    fn from_loader_config(config: Option<&module_loader::LoaderConfig>) -> Self {
        Self {
            graph: config.and_then(|cfg| cfg.package_graph.clone()),
        }
    }

    fn apply_to_loader_config(&self, config: &mut module_loader::LoaderConfig) {
        if config.package_graph.is_none() {
            config.package_graph = self.graph.clone();
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_heap_bytes: DEFAULT_MAX_HEAP_BYTES,
            timeout: DEFAULT_TIMEOUT,
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            capabilities: CapabilitySet::default(),
            loader: None,
            hosted_modules: Vec::new(),
            global_classes: Vec::new(),
            console_sink: otter_vm::console::default_console_sink(),
            hooks: RuntimeHooks::default(),
            process_argv: process::default_argv(),
            process_cwd: process::default_cwd(),
        }
    }
}

impl RuntimeConfig {
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

fn gc_oom_to_error(oom: otter_gc::OutOfMemory) -> OtterError {
    OtterError::OutOfMemory {
        requested_bytes: oom.requested_bytes(),
        heap_limit_bytes: oom.heap_limit_bytes(),
    }
}

fn string_oom_to_error(err: otter_gc::OutOfMemory) -> OtterError {
    OtterError::OutOfMemory {
        requested_bytes: err.requested_bytes(),
        heap_limit_bytes: err.heap_limit_bytes(),
    }
}

/// Layer B configuration entry point.
#[derive(Debug, Clone, Default)]
pub struct RuntimeBuilder {
    config: RuntimeConfig,
}

impl RuntimeBuilder {
    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.config.capabilities = caps;
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.config.max_heap_bytes = bytes;
        self
    }

    /// Per-`run_*` timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.config.max_stack_depth = depth;
        self
    }

    /// Override the module-loader configuration. The default
    /// (used when this is left untouched) infers `base_dir` from
    /// the entry file's parent directory at `run_module` time
    /// and uses the foundation extension list / ESM+CJS
    /// condition names with `node_modules` walk-up enabled.
    ///
    /// Spec mapping: <https://nodejs.org/api/esm.html#resolution-and-loading-algorithm>
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.config.loader = Some(loader);
        self
    }

    /// Register one runtime-hosted module such as `otter:kv`.
    #[must_use]
    pub fn hosted_module(mut self, module: HostedModule) -> Self {
        self.config.hosted_modules.push(module);
        self
    }

    /// Register multiple runtime-hosted modules.
    #[must_use]
    pub fn hosted_modules(mut self, modules: impl IntoIterator<Item = HostedModule>) -> Self {
        self.config.hosted_modules.extend(modules);
        self
    }

    /// Register one class-shaped global described by a static spec.
    #[must_use]
    pub fn global_class(mut self, spec: GlobalClass) -> Self {
        self.config.global_classes.push(spec);
        self
    }

    /// Register multiple class-shaped globals.
    #[must_use]
    pub fn global_classes(mut self, specs: impl IntoIterator<Item = GlobalClass>) -> Self {
        self.config.global_classes.extend(specs);
        self
    }

    /// Override the implementation behind `console.*`.
    ///
    /// The default sink writes `log` / `info` / `debug` through
    /// `println!` and `warn` / `error` / `trace` / failed `assert`
    /// through `eprintln!`. Embedders can replace it with a sink
    /// backed by `tracing`, structured logs, or test capture.
    #[must_use]
    pub fn console_sink(mut self, sink: ConsoleSinkHandle) -> Self {
        self.config.console_sink = sink;
        self
    }

    /// Replace the runtime hook set.
    #[must_use]
    pub fn hooks(mut self, hooks: RuntimeHooks) -> Self {
        self.config.hooks = hooks;
        self
    }

    /// Set the module-resolution hook.
    #[must_use]
    pub fn resolve_hook(mut self, hook: impl RuntimeResolveHook) -> Self {
        self.config.hooks = self.config.hooks.with_resolve_hook(hook);
        self
    }

    /// Set the source-loading hook.
    #[must_use]
    pub fn load_hook(mut self, hook: impl RuntimeLoadHook) -> Self {
        self.config.hooks = self.config.hooks.with_load_hook(hook);
        self
    }

    /// Set the compile hook.
    #[must_use]
    pub fn compile_hook(mut self, hook: impl RuntimeCompileHook) -> Self {
        self.config.hooks = self.config.hooks.with_compile_hook(hook);
        self
    }

    /// Set the runtime job enqueue hook.
    #[must_use]
    pub fn job_hook(mut self, hook: impl RuntimeJobHook) -> Self {
        self.config.hooks = self.config.hooks.with_job_hook(hook);
        self
    }

    /// Set the diagnostic sink hook.
    #[must_use]
    pub fn diagnostic_hook(mut self, hook: impl RuntimeDiagnosticHook) -> Self {
        self.config.hooks = self.config.hooks.with_diagnostic_hook(hook);
        self
    }

    /// Set the capability-check hook.
    #[must_use]
    pub fn capability_hook(mut self, hook: impl RuntimeCapabilityHook) -> Self {
        self.config.hooks = self.config.hooks.with_capability_hook(hook);
        self
    }

    /// Set the `process.argv` snapshot installed into the runtime.
    #[must_use]
    pub fn process_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.config.process_argv = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Set the `process.cwd()` snapshot installed into the runtime.
    #[must_use]
    pub fn process_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.config.process_cwd = cwd.into();
        self
    }

    /// Construct the runtime.
    ///
    /// # Errors
    /// Returns [`OtterError::Config`] when the configuration is
    /// inconsistent.
    pub fn build(self) -> Result<Runtime, OtterError> {
        Runtime::from_config(self.config)
    }

    /// Construct a sendable runtime handle.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the configuration is invalid or the
    /// isolate runner cannot be started.
    pub fn build_handle(self) -> Result<RuntimeHandle, OtterError> {
        RuntimeHandle::spawn(self.config)
    }
}

impl Runtime {
    pub(crate) fn validate_config(config: &RuntimeConfig) -> Result<(), OtterError> {
        if config.max_stack_depth == 0 {
            return Err(OtterError::Config {
                reason: ConfigError::InvalidStackDepth {
                    message: "max_stack_depth must be > 0".to_string(),
                },
            });
        }
        Ok(())
    }

    pub(crate) fn from_config(config: RuntimeConfig) -> Result<Self, OtterError> {
        Self::validate_config(&config)?;
        let module_loader = RuntimeModuleLoaderState::new(config.loader.clone());
        let package_manager =
            RuntimePackageManagerHandle::from_loader_config(config.loader.as_ref());
        // The interpreter owns the per-isolate GC heap (since
        // task 76); both the string heap and the GC heap honour
        // the configured cap.
        let mut interp = Interpreter::with_string_heap_cap(config.max_heap_bytes);
        interp.set_max_stack_depth(config.max_stack_depth);
        interp.set_console_sink(config.console_sink.clone());
        for spec in &config.global_classes {
            interp
                .install_global_class(spec.raw())
                .map_err(|err| OtterError::Internal {
                    code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                    message: err.to_string(),
                })?;
        }
        process::install_global(
            &mut interp,
            &config.process_argv,
            &config.process_cwd,
            &config.capabilities,
        )?;
        // §19.4.1 / §20.2.1.1 — wire the eval hook so `eval(src)` /
        // `new Function(...)` reach a real parse + compile path.
        // The closure is reusable across calls; each invocation
        // builds a fresh `BytecodeModule`.
        let hook: otter_vm::EvalHook =
            std::rc::Rc::new(|source: &str, options: EvalCompileOptions| {
                otter_compiler::compile_script_source_with_forced_strict(
                    source,
                    SourceKind::JavaScript,
                    "<eval>",
                    options.force_strict,
                )
                .map_err(|e| format!("compile error: {e:?}"))
            });
        interp.set_eval_hook(Some(hook));
        Ok(Runtime {
            interp,
            config,
            module_loader,
            module_graph: RuntimeModuleGraphState::default(),
            module_records: module_records::RuntimeModuleRecords::default(),
            source_maps: RuntimeSourceMapTable::default(),
            diagnostics: RuntimeDiagnosticsSink::default(),
            package_manager,
            promise_registry: promise_registry::PromiseRegistry::new(),
        })
    }
}

/// Layer B isolate.
#[derive(Debug)]
pub struct Runtime {
    interp: Interpreter,
    config: RuntimeConfig,
    module_loader: RuntimeModuleLoaderState,
    module_graph: RuntimeModuleGraphState,
    module_records: module_records::RuntimeModuleRecords,
    source_maps: RuntimeSourceMapTable,
    diagnostics: RuntimeDiagnosticsSink,
    package_manager: RuntimePackageManagerHandle,
    /// Per-isolate map from runtime-issued `PromiseId` to the
    /// pending [`otter_vm::JsPromiseHandle`] a host async op is
    /// expected to settle. Embedders register a fresh promise
    /// inside a native function (VM thread), then post the
    /// matching settle outcome through
    /// [`crate::RuntimeHandle::settle_promise`] (host thread).
    /// The isolate runner pops the entry on the inbox hop and
    /// resolves / rejects it through the standard promise
    /// dispatch path so reactions land on the microtask queue.
    promise_registry: promise_registry::PromiseRegistry,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MicrotaskStats {
    pub(crate) pending: bool,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerFireOutcome {
    Missing,
    Fired { repeat: bool },
}

pub(crate) enum DynamicImportBegin {
    Settled,
    FetchHttps { target_url: String },
}

enum DynamicModuleLoad {
    Loaded(otter_vm::Value),
    FetchHttps { target_url: String },
}

impl Runtime {
    /// Start configuring a new runtime.
    #[must_use]
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// Cooperative cancellation handle.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle(self.interp.interrupt_handle())
    }

    /// Configured per-`run_*` timeout (currently informational; the
    /// foundation slice does not yet enforce timeouts).
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.config.timeout
    }

    pub(crate) fn microtask_stats(&self) -> MicrotaskStats {
        let queue = self.interp.microtasks();
        MicrotaskStats {
            pending: queue.has_any_pending(),
            generation: queue.generation(),
        }
    }

    /// Install the host-side timer scheduler. The runtime calls
    /// this once during isolate-runner construction. Direct-mode
    /// embedders (Layer A `RuntimeBuilder::build`) never call this;
    /// scripts then receive a TypeError on `setTimeout` /
    /// `setInterval` instead of silently dropping the callback.
    pub fn install_timer_scheduler(&mut self, scheduler: otter_vm::TimerSchedulerHandle) {
        self.interp.set_timer_scheduler(scheduler);
    }

    /// Install the host-side dynamic-import scheduler. Wired by
    /// the isolate runner so `Op::ImportNamespaceDynamic` can
    /// reach the loader through the runtime inbox.
    pub fn install_dynamic_import_loader(&mut self, loader: otter_vm::DynamicImportLoaderHandle) {
        self.interp.set_dynamic_import_loader(loader);
    }

    /// Begin loading a dynamic import on the isolate thread.
    ///
    /// File-backed modules are loaded, compiled, evaluated, and
    /// settled before this returns. HTTPS modules are only resolved
    /// and capability-checked here; network I/O is deferred to the
    /// host service and returns later as owned source text.
    pub(crate) fn begin_dynamic_import(
        &mut self,
        token: u64,
        specifier: &str,
        referrer: &str,
    ) -> Result<DynamicImportBegin, OtterError> {
        match self.load_dynamic_module(specifier, referrer) {
            Ok(DynamicModuleLoad::Loaded(namespace)) => self
                .settle_dynamic_import_result(token, Ok(namespace))
                .map(|_| DynamicImportBegin::Settled),
            Ok(DynamicModuleLoad::FetchHttps { target_url }) => {
                Ok(DynamicImportBegin::FetchHttps { target_url })
            }
            Err(DynLoadError::Diagnostic(message)) => self
                .alloc_dynamic_import_error(message)
                .and_then(|value| self.settle_dynamic_import_result(token, Err(value)))
                .map(|_| DynamicImportBegin::Settled),
            Err(DynLoadError::Thrown(value)) => self
                .settle_dynamic_import_result(token, Err(value))
                .map(|_| DynamicImportBegin::Settled),
        }
    }

    /// Finish an HTTPS dynamic import after the host fetcher has
    /// returned owned UTF-8 source text.
    pub(crate) fn complete_dynamic_import_https(
        &mut self,
        token: u64,
        target_url: &str,
        source: Result<String, String>,
    ) -> Result<bool, OtterError> {
        let reaction_outcome: Result<otter_vm::Value, otter_vm::Value> = match source
            .map_err(DynLoadError::Diagnostic)
            .and_then(|source| self.evaluate_dynamic_module_https_source(target_url, source))
        {
            Ok(namespace) => Ok(namespace),
            Err(DynLoadError::Diagnostic(message)) => {
                Err(self.alloc_dynamic_import_error(message)?)
            }
            Err(DynLoadError::Thrown(value)) => Err(value),
        };
        self.settle_dynamic_import_result(token, reaction_outcome)
    }

    fn settle_dynamic_import_result(
        &mut self,
        token: u64,
        reaction_outcome: Result<otter_vm::Value, otter_vm::Value>,
    ) -> Result<bool, OtterError> {
        let settled_context = self.interp.settle_dynamic_import(token, reaction_outcome);
        if let Some(context) = settled_context {
            if let Err(err) = self.interp.drain_microtasks_with_default(Some(context)) {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ));
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn alloc_dynamic_import_error(
        &mut self,
        message: String,
    ) -> Result<otter_vm::Value, OtterError> {
        let proto = self
            .interp
            .error_classes_for_trace()
            .prototype(otter_vm::ErrorKind::TypeError);
        let proto_root = otter_vm::Value::object(proto);
        let obj = self
            .interp
            .alloc_host_object_with_roots(&[&proto_root], &[])?;
        otter_vm::object::set_prototype(obj, self.interp.gc_heap_mut(), Some(proto));
        let message_str = otter_vm::JsString::from_str(&message, self.interp.gc_heap_mut())
            .map_err(|err| OtterError::Internal {
                code: DiagnosticCode::StringAlloc.as_str().to_string(),
                message: err.to_string(),
            })?;
        otter_vm::object::set(
            obj,
            self.interp.gc_heap_mut(),
            "message",
            otter_vm::Value::string(message_str),
        );
        Ok(otter_vm::Value::object(obj))
    }

    fn load_dynamic_module(
        &mut self,
        specifier: &str,
        referrer: &str,
    ) -> Result<DynamicModuleLoad, DynLoadError> {
        use std::path::PathBuf;
        let referrer_opt = if referrer.is_empty() {
            None
        } else {
            Some(referrer)
        };
        let entry_for_loader: PathBuf = match referrer_opt {
            Some(url) => url_to_path(url).ok_or_else(|| {
                DynLoadError::Diagnostic(format!(
                    "dynamic import: referrer is not a file:// URL: \"{url}\""
                ))
            })?,
            None => std::env::current_dir().map_err(|e| {
                DynLoadError::Diagnostic(format!("dynamic import: cwd lookup failed: {e}"))
            })?,
        };
        let loader = self.module_loader_for_entry(&entry_for_loader);
        let target_url = loader.resolve(specifier, referrer_opt).map_err(|e| {
            DynLoadError::Diagnostic(format!(
                "dynamic import: cannot resolve \"{specifier}\": {e:?}"
            ))
        })?;
        if let Some(env) = self.interp.module_env(&target_url) {
            return Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(env)));
        }
        // HTTPS / HTTP targets take a separate fetch path because
        // `module_graph::load_program` only walks file:// URLs.
        // The capability check has already passed inside
        // `loader.resolve` so the target host is on the allowlist.
        if target_url.starts_with("http://") || target_url.starts_with("https://") {
            return Ok(DynamicModuleLoad::FetchHttps { target_url });
        }
        let target_path: PathBuf = url_to_path(&target_url).ok_or_else(|| {
            DynLoadError::Diagnostic(format!(
                "dynamic import: target is not a file:// URL: \"{target_url}\""
            ))
        })?;
        let linked = self
            .module_graph
            .load_program(&loader, &target_path)
            .map_err(|e| {
                DynLoadError::Diagnostic(format!(
                    "dynamic import: load failed for \"{target_url}\": {e:?}"
                ))
            })?;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        let context = ExecutionContext::from_module(linked.module);
        for init in context.module_inits() {
            if self.interp.module_env(&init.url).is_some() {
                continue;
            }
            let env = self
                .interp
                .alloc_host_object_with_roots(&[], &[])
                .map_err(|e| {
                    DynLoadError::Diagnostic(format!("dynamic import: alloc env failed: {e}"))
                })?;
            self.interp
                .register_module_env(std::rc::Rc::from(init.url.as_str()), env);
        }
        let inits: Vec<(String, u32, otter_vm::JsObject)> = context
            .module_inits()
            .iter()
            .filter_map(|init| {
                self.interp
                    .module_env(&init.url)
                    .map(|env| (init.url.clone(), init.function_id, env))
            })
            .collect();
        for (url, function_id, env) in inits {
            if otter_vm_init_marker_set(&self.interp, env) {
                let _ = url;
                continue;
            }
            otter_vm_init_marker_install(&mut self.interp, env);
            let import_meta = alloc_dynamic_import_meta(&mut self.interp, env, &url)?;
            let callee = otter_vm::Value::function_id(function_id);
            let args: smallvec::SmallVec<[otter_vm::Value; 8]> = smallvec::smallvec![
                otter_vm::Value::object(env),
                otter_vm::Value::object(import_meta),
            ];
            if let Err(err) =
                self.interp
                    .run_callable_sync(&context, &callee, otter_vm::Value::undefined(), args)
            {
                // §16.2.1.7 step 7.b.i — an evaluation throw maps
                // to a promise rejection. Prefer the original
                // thrown Value (preserved on
                // `pending_uncaught_throw` whenever the throw
                // walked the empty stack inside the dispatch
                // sub-loop) so `.catch` observes the spec-correct
                // payload, not a stringified `VmError::Uncaught`
                // rendering.
                if matches!(err, otter_vm::VmError::Uncaught { .. })
                    && let Some(thrown) = self.interp.take_pending_uncaught_throw()
                {
                    return Err(DynLoadError::Thrown(thrown));
                }
                return Err(DynLoadError::Diagnostic(format!(
                    "dynamic import: evaluation failed for \"{url}\": {err}"
                )));
            }
        }
        let namespace = self.interp.module_env(&target_url).ok_or_else(|| {
            DynLoadError::Diagnostic(format!(
                "dynamic import: namespace missing after load: \"{target_url}\""
            ))
        })?;
        Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
            namespace,
        )))
    }

    /// Compile + evaluate an already-fetched HTTPS module
    /// dynamically.
    ///
    /// # Algorithm
    /// 1. Parse the response body as UTF-8 source text once. Compile
    ///    it as one ES-module fragment via
    ///    `otter_compiler::compile_module_program`. Any own
    ///    static imports are rejected for this slice — the
    ///    HTTPS fetcher does not yet recurse into dependencies.
    ///    Local file imports from an HTTPS module are also
    ///    rejected for the same reason.
    /// 2. Wrap the fragment in a one-module `BytecodeModule`
    ///    suitable for `Interpreter::run_callable_sync`, allocate
    ///    an env, mark it inited, then dispatch `<module-init>`.
    /// 3. Settle with the populated env as the namespace.
    fn evaluate_dynamic_module_https_source(
        &mut self,
        target_url: &str,
        response_text: String,
    ) -> Result<otter_vm::Value, DynLoadError> {
        let host = otter_compiler::ModuleHostInfo {
            module_url: target_url.to_string(),
            resolved_imports: std::collections::HashMap::new(),
        };
        let fragment = otter_syntax::with_program(
            response_text,
            otter_syntax::SourceKind::JavaScript,
            |program| {
                otter_compiler::compile_module_program(
                    program,
                    otter_syntax::SourceKind::JavaScript,
                    &host,
                )
            },
        )
        .map_err(|e| {
            DynLoadError::Diagnostic(format!(
                "dynamic import: parse failed for \"{target_url}\": {e:?}"
            ))
        })?
        .map_err(|e| {
            DynLoadError::Diagnostic(format!(
                "dynamic import: compile failed for \"{target_url}\": {e:?}"
            ))
        })?;
        if fragment_has_import_namespace_ops(&fragment) {
            return Err(DynLoadError::Diagnostic(format!(
                "dynamic import: HTTPS module \"{target_url}\" has own static imports — not yet supported"
            )));
        }
        let context = ExecutionContext::from_module(fragment);
        let env = self
            .interp
            .alloc_host_object_with_roots(&[], &[])
            .map_err(|e| {
                DynLoadError::Diagnostic(format!("dynamic import: alloc env failed: {e}"))
            })?;
        self.interp
            .register_module_env(std::rc::Rc::from(target_url), env);
        otter_vm_init_marker_install(&mut self.interp, env);
        let import_meta = alloc_dynamic_import_meta(&mut self.interp, env, target_url)?;
        let callee = otter_vm::Value::function_id(0);
        let args: smallvec::SmallVec<[otter_vm::Value; 8]> = smallvec::smallvec![
            otter_vm::Value::object(env),
            otter_vm::Value::object(import_meta),
        ];
        if let Err(err) =
            self.interp
                .run_callable_sync(&context, &callee, otter_vm::Value::undefined(), args)
        {
            if matches!(err, otter_vm::VmError::Uncaught { .. })
                && let Some(thrown) = self.interp.take_pending_uncaught_throw()
            {
                return Err(DynLoadError::Thrown(thrown));
            }
            return Err(DynLoadError::Diagnostic(format!(
                "dynamic import: HTTPS evaluation failed for \"{target_url}\": {err}"
            )));
        }
        Ok(otter_vm::Value::object(env))
    }

    /// Install `value` as a `globalThis.<name>` data property.
    /// Standard descriptor attributes (`{ writable: true,
    /// enumerable: false, configurable: true }` per §17 + §19) are
    /// applied so the binding behaves like every other default
    /// global. Embedders that need to expose a host-bound JS value
    /// (e.g. a [`otter_vm::Value::Promise`] returned by
    /// [`Self::register_pending_promise`]) call this from the
    /// runner thread before re-entering script execution.
    pub fn set_global(&mut self, name: &str, value: otter_vm::Value) {
        self.interp.set_global(name, value);
    }

    /// Install a host-defined native function as a global binding.
    ///
    /// The function is allocated on the runtime's GC heap as a
    /// `Value::native_function` with the given `name` and arity, then
    /// stored on `globalThis` via [`Self::set_global`]. Standard
    /// descriptor attributes (`{ writable: true, enumerable: false,
    /// configurable: true }`) are applied.
    ///
    /// Used by `crates/otter-test262/src/agent.rs` to plug the
    /// `__otter_agent_*` family of `$262.agent` host bindings into
    /// every fresh per-test runtime without modifying the otter-vm
    /// bootstrap.
    ///
    /// # Errors
    /// Returns [`OtterError::OutOfMemory`] when the heap cap blocks
    /// the native function allocation.
    pub fn install_native_global(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeFastFn,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_function_static_host_rooted(name, length, call, &[], &[])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.interp.set_global(name, value);
        Ok(())
    }

    /// `true` when the per-isolate `TimerCallbacks` table has any
    /// outstanding entries. The isolate runner uses this to decide
    /// whether the script's run loop needs to keep ticking the
    /// inbox after the synchronous body returns.
    #[must_use]
    pub fn has_pending_timer_callbacks(&self) -> bool {
        !self.interp.timer_callbacks().is_empty()
    }

    /// Register a fresh pending JS promise and return the
    /// `(PromiseId, Value::Promise)` pair. The caller — typically
    /// a native function exposed to JS — returns the
    /// [`otter_vm::Value`] to the script and ships the
    /// [`promise_registry::PromiseId`] over to a host async op.
    /// Settlement happens later through
    /// [`Self::settle_pending_promise`] (runner-side) or
    /// [`crate::RuntimeHandle::settle_promise`] (host-side, posts
    /// the inbox message).
    ///
    /// # Errors
    /// Returns [`OtterError::OutOfMemory`] when the GC heap cap
    /// blocks the fresh pure-promise allocation.
    pub fn register_pending_promise(
        &mut self,
    ) -> Result<(promise_registry::PromiseId, otter_vm::Value), OtterError> {
        let handle = otter_vm::promise_dispatch::pending_runtime_rooted(&mut self.interp, &[], &[])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        let id = self.promise_registry.register(handle);
        Ok((id, otter_vm::Value::promise(handle)))
    }

    /// Settle the promise registered under `id` with `outcome` and
    /// drain any reactions the settlement enqueued onto the
    /// per-isolate microtask queue. A late or duplicate settle
    /// (entry already taken) is a silent no-op so the host can
    /// race-cancel without observable damage.
    ///
    /// # Errors
    /// Returns the wrapped [`otter_vm::VmError`] when the reaction
    /// drain reports an unhandled error.
    pub fn settle_pending_promise(
        &mut self,
        id: promise_registry::PromiseId,
        outcome: promise_registry::HostSettleOutcome,
    ) -> Result<bool, OtterError> {
        let handle = match self.promise_registry.take(id) {
            Some(handle) => handle,
            None => return Ok(false),
        };
        // Convert the owned host payload into a `Value` on the
        // runner thread. String allocations land against the
        // per-runtime string heap so the result honours the heap
        // cap.
        use promise_registry::HostSettleOutcome;
        let (jobs, _was_resolve) = match outcome {
            HostSettleOutcome::ResolveUndefined => (
                otter_vm::JsPromise::fulfill(
                    &handle,
                    self.interp.gc_heap_mut(),
                    otter_vm::Value::undefined(),
                ),
                true,
            ),
            HostSettleOutcome::ResolveNull => (
                otter_vm::JsPromise::fulfill(
                    &handle,
                    self.interp.gc_heap_mut(),
                    otter_vm::Value::null(),
                ),
                true,
            ),
            HostSettleOutcome::ResolveBoolean(b) => (
                otter_vm::JsPromise::fulfill(
                    &handle,
                    self.interp.gc_heap_mut(),
                    otter_vm::Value::boolean(b),
                ),
                true,
            ),
            HostSettleOutcome::ResolveNumber(n) => (
                otter_vm::JsPromise::fulfill(
                    &handle,
                    self.interp.gc_heap_mut(),
                    otter_vm::Value::number(otter_vm::NumberValue::from_f64(n)),
                ),
                true,
            ),
            HostSettleOutcome::ResolveString(s) => {
                let str_val = self.alloc_string(&s)?;
                (
                    otter_vm::JsPromise::fulfill(
                        &handle,
                        self.interp.gc_heap_mut(),
                        otter_vm::Value::string(str_val),
                    ),
                    true,
                )
            }
            HostSettleOutcome::RejectString(s) => {
                let str_val = self.alloc_string(&s)?;
                (
                    otter_vm::JsPromise::reject(
                        &handle,
                        self.interp.gc_heap_mut(),
                        otter_vm::Value::string(str_val),
                    ),
                    false,
                )
            }
        };
        for job in jobs.jobs {
            self.interp.microtasks_mut().enqueue(job);
        }
        if let Err(err) = self.interp.drain_microtasks_with_default(None) {
            return Err(enrich_runtime_diagnostic_with_cause(
                &mut self.interp,
                map_vm_error(err),
            ));
        }
        Ok(true)
    }

    fn alloc_string(&mut self, s: &str) -> Result<otter_vm::JsString, OtterError> {
        otter_vm::JsString::from_str(s, self.interp.gc_heap_mut()).map_err(|err| {
            OtterError::Internal {
                code: DiagnosticCode::StringAlloc.as_str().to_string(),
                message: err.to_string(),
            }
        })
    }

    /// Fire the timer identified by `token`. Routes through
    /// the per-isolate `TimerCallbacks` table to recover the JS
    /// callable + extra arguments + execution context, invokes the
    /// callback as a top-level call against that context, and drains
    /// any microtasks the callback queued. Returns [`TimerFireOutcome`]
    /// so the isolate runner can update timer liveness without
    /// treating a cancelled timer as a fired task. Returns an
    /// [`OtterError`] when the callback raises an unhandled throw.
    ///
    /// One-shot (`setTimeout`) entries are removed from the table
    /// before invocation so a re-entrant `clearTimeout(token)`
    /// from inside the callback observes the entry as gone.
    /// Repeating (`setInterval`) entries stay in the table; the
    /// host scheduler re-arms them on its own.
    pub(crate) fn fire_timer(&mut self, token: u64) -> Result<TimerFireOutcome, OtterError> {
        let entry = match self.interp.timer_callbacks().get(token).cloned() {
            Some(entry) => entry,
            None => return Ok(TimerFireOutcome::Missing),
        };
        let repeat = entry.repeat_ms.is_some();
        if !repeat {
            self.interp.timer_callbacks_mut().remove(token);
        }
        let context = entry.context.clone();
        let mut args: smallvec::SmallVec<[otter_vm::Value; 8]> =
            smallvec::SmallVec::with_capacity(entry.extra_args.len());
        args.extend(entry.extra_args);
        self.interp
            .run_callable_sync(
                &context,
                &entry.callback,
                otter_vm::Value::undefined(),
                args,
            )
            .map_err(|error| {
                map_vm_error(otter_vm::RunError {
                    error,
                    frames: Vec::new(),
                })
            })?;
        let outcome = self.interp.drain_microtasks(&context);
        match outcome {
            Ok(()) => Ok(TimerFireOutcome::Fired { repeat }),
            Err(err) => Err(enrich_runtime_diagnostic_with_cause(
                &mut self.interp,
                map_vm_error(err),
            )),
        }
    }

    /// Configured heap cap in bytes (`0` = disabled).
    ///
    /// The cap is **load-bearing** as of task 73:
    /// allocations against the [`otter_gc::GcHeap`] and string
    /// allocations against the interpreter's string heap that
    /// would overshoot the cap surface as
    /// [`OtterError::OutOfMemory`]. Per-type GC migrations
    /// (tasks 76–83) progressively widen the set of script
    /// allocations subject to the cap.
    #[must_use]
    pub fn max_heap_bytes(&self) -> u64 {
        self.config.max_heap_bytes
    }

    /// Per-heap GC counters: live objects / live bytes / per-
    /// `type_tag` rows / last-GC pause / cycle counter.
    ///
    /// Takes `&mut self` because the aggregate `live_objects` /
    /// `live_bytes` fields are derived lazily from the per-tag
    /// rows (the alloc fast path only updates the per-tag
    /// counters, see [`otter_gc::GcHeap::gc_stats`]). Per-type
    /// GC migrations (tasks 76–83) widen the surface populated
    /// under `by_type` — Phase 1 only sees host-side
    /// allocations through [`Self::gc_heap_mut`].
    pub fn heap_stats(&mut self) -> &GcStats {
        self.interp.gc_heap_mut().gc_stats()
    }

    /// Return the VM's observational runtime budget policy.
    #[must_use]
    pub fn runtime_budget(&self) -> RuntimeBudget {
        self.interp.runtime_budget()
    }

    /// Set the VM's observational runtime budget policy.
    ///
    /// This does not yet enforce preemption; it records limit exceedance in
    /// [`RuntimeBudgetStats`].
    pub fn set_runtime_budget(&mut self, budget: RuntimeBudget) {
        self.interp.set_runtime_budget(budget);
    }

    /// Snapshot VM runtime budget/resource counters.
    #[must_use]
    pub fn runtime_budget_stats(&self) -> RuntimeBudgetStats {
        self.interp.runtime_budget_stats()
    }

    /// Reset VM runtime budget/resource counters.
    pub fn reset_runtime_budget_stats(&mut self) {
        self.interp.reset_runtime_budget_stats();
    }

    /// Force a full GC cycle (scavenge + old-gen mark-sweep).
    ///
    /// **Debug / test only.** Production code must never call
    /// this — the GC's own triggers are tuned to allocation
    /// pressure, and a forced cycle perturbs those metrics.
    /// Tests use this to assert "after dropping these handles
    /// and forcing a GC, live counts return to baseline".
    ///
    /// The walker delegates to
    /// [`otter_vm::runtime_state::RuntimeState::trace_roots`]
    /// (task 75) via [`otter_vm::Interpreter::force_gc`], which
    /// owns the heap and does the split-borrow internally.
    pub fn force_gc(&mut self) {
        self.interp.force_gc();
    }

    /// Configured stack-depth cap.
    #[must_use]
    pub fn max_stack_depth(&self) -> u32 {
        self.config.max_stack_depth
    }

    /// Borrow the capability set.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.config.capabilities
    }

    /// Check a runtime capability request through the configured hook policy.
    #[must_use]
    pub fn check_capability(
        &self,
        capability: RuntimeCapability,
        request: &CapabilityRequest<'_>,
    ) -> bool {
        if let Some(hook) = self.config.hooks.capability_hook() {
            return hook.check_capability(&self.config.capabilities, capability, request);
        }
        default_check_capability(&self.config.capabilities, capability, request)
    }

    /// Emit a structured diagnostic through the configured runtime hook.
    pub fn emit_diagnostic(&self, diagnostic: &Diagnostic) {
        self.diagnostics.emit(&self.config.hooks, diagnostic);
    }

    /// Map a `(module_url, function_id, pc)` triple back to the
    /// original source byte range using the runtime-owned source
    /// map table.
    ///
    /// Populated incrementally as the runtime compiles modules /
    /// scripts. Returns the predecessor entry's span when the
    /// table has no exact PC match — matches the VM's frame
    /// snapshot policy in
    /// [`otter_vm::snapshot_frames`].
    ///
    /// Returns `None` when the module URL has no compiled
    /// metadata yet, when the function id is unknown for that
    /// module, or when the function's span table is empty.
    #[must_use]
    pub fn resolve_frame_span(
        &self,
        module_url: &str,
        function_id: u32,
        pc: u32,
    ) -> Option<(u32, u32)> {
        self.source_maps
            .resolve_frame_span(module_url, function_id, pc)
    }

    /// Compile and execute `source` as a script.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_script(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> Result<ExecutionResult, OtterError> {
        let start = std::time::Instant::now();
        let compiled = self.compile_source(&source, specifier)?;
        self.run_compiled_script_since(compiled.bytecode, start)
    }

    fn run_compiled_script_since(
        &mut self,
        module: BytecodeModule,
        start: std::time::Instant,
    ) -> Result<ExecutionResult, OtterError> {
        let context = ExecutionContext::from_module(module);
        // Run the script first; the script error wins if both the
        // script and the drain fail. On script success we still
        // drain so any `queueMicrotask` registered during script
        // execution gets a chance to run before we report success.
        let script_outcome = self.interp.run(&context);
        let drain_outcome = self.interp.drain_microtasks(&context);
        let value = match (script_outcome, drain_outcome) {
            (
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
                _,
            )
            | (
                Ok(_),
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
            ) => {
                return Ok(ExecutionResult::from_exit_code(code, start.elapsed()));
            }
            (Err(script_err), _) => {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(script_err),
                ));
            }
            (Ok(_), Err(drain_err)) => {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(drain_err),
                ));
            }
            (Ok(v), Ok(())) => v,
        };
        Ok(
            ExecutionResult::from_vm_value(value, start.elapsed(), self.interp.gc_heap_mut())
                .with_exit_code(process::exit_code(&self.interp)),
        )
    }

    /// Drain the microtask queue manually. Embedders that want to
    /// step the queue between script runs (or in response to
    /// host-side events) call this directly. Foundation slices
    /// run `run_script` / `eval` always drain after the script,
    /// so manual draining is rarely needed today.
    ///
    /// # Errors
    /// Any `VmError` raised by a microtask propagates as
    /// `OtterError::Runtime`.
    pub fn run_microtasks(&mut self) -> Result<(), OtterError> {
        if !self.interp.microtasks().has_any_pending() {
            return Ok(());
        }
        self.interp
            .drain_microtasks_with_default(None)
            .map_err(map_vm_error)
    }

    /// Convenience: run an expression for tooling.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn eval(&mut self, source: SourceInput) -> Result<ExecutionResult, OtterError> {
        self.run_script(source, "<eval>")
    }

    /// Compile-only: parse + erase + lower without executing.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn check(&self, source: SourceInput, specifier: &str) -> Result<(), OtterError> {
        self.compile_source(&source, specifier).map(|_| ())
    }

    /// Compile-and-dump: produce the bytecode module for inspection.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn dump(&self, source: SourceInput, specifier: &str) -> Result<CompiledModule, OtterError> {
        self.compile_source(&source, specifier)
    }

    fn compile_source(
        &self,
        source: &SourceInput,
        specifier: &str,
    ) -> Result<CompiledModule, OtterError> {
        let compiled = if let Some(hook) = self.config.hooks.compile_hook() {
            let resolved = module_loader::ResolvedSource {
                url: specifier.to_string(),
                kind: source.kind,
                jsx: None,
                text: source.text.clone(),
            };
            hook.compile(RuntimeCompileRequest { source: &resolved })?
        } else {
            compile_script_source_to_module(&source.text, source.kind, specifier)
                .map_err(|err| map_compile_error(err, specifier))?
        };
        self.source_maps
            .record_compiled_metadata(&compiled.metadata);
        Ok(compiled)
    }

    /// Load + link + execute the module-graph rooted at `entry_path`.
    ///
    /// # Algorithm
    /// 1. Build a [`module_loader::ModuleLoader`] rooted at the
    ///    entry's parent directory.
    /// 2. Walk the dependency graph
    ///    ([`module_graph::load_program`]), resolving every static
    ///    import + literal-string `import("./x")` into a unified
    ///    linked [`BytecodeModule`].
    /// 3. Pre-allocate one `module_env` JsObject per module URL,
    ///    register each in the interpreter, and append self-loop
    ///    `(referrer = entry_url, specifier = url, target = url)`
    ///    rows to `module_resolutions` so the synthesised
    ///    `<entry>` driver's `Op::ImportNamespace` lookups succeed.
    /// 4. Run `<entry>` through the existing dispatch loop. Drain
    ///    microtasks afterwards.
    ///
    /// # Errors
    /// See [`OtterError`] variants. Loader / link errors surface
    /// as [`OtterError::Compile`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-modules>
    pub fn run_module(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<ExecutionResult, OtterError> {
        let start = std::time::Instant::now();
        let entry_path = entry_path.as_ref();
        let loader = self.module_loader_for_entry(entry_path);
        let linked = self
            .module_graph
            .load_program(&loader, entry_path)
            .map_err(map_graph_error)?;

        let mut module = linked.module;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        let entry_url = linked.entry_url.clone();
        self.module_records.allocate_for_module_inits(
            &mut self.interp,
            &module.module_inits,
            &self.config.hosted_modules,
            &self.config.capabilities,
        )?;
        self.module_records
            .for_each_record(|url, _function_id, _env| {
                // Self-loop edge: <entry>'s referrer is the entry's URL
                // (the synthesized <entry> function carries empty
                // module_url, so the dispatcher uses an empty string;
                // we add edges keyed on both shapes).
                module
                    .module_resolutions
                    .push(otter_bytecode::ModuleResolution {
                        referrer: entry_url.clone(),
                        specifier: url.to_string(),
                        target: url.to_string(),
                    });
                module
                    .module_resolutions
                    .push(otter_bytecode::ModuleResolution {
                        referrer: String::new(),
                        specifier: url.to_string(),
                        target: url.to_string(),
                    });
            });

        self.module_records.mark_evaluating();
        let context = ExecutionContext::from_module(module);
        let script_outcome = self.interp.run(&context);
        let drain_outcome = self.interp.drain_microtasks(&context);
        let value = match (script_outcome, drain_outcome) {
            (
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
                _,
            )
            | (
                Ok(_),
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
            ) => {
                self.module_records.mark_evaluated();
                return Ok(ExecutionResult::from_exit_code(code, start.elapsed()));
            }
            (Err(script_err), _) => {
                self.module_records.mark_errored();
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(script_err),
                ));
            }
            (Ok(_), Err(drain_err)) => {
                self.module_records.mark_errored();
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(drain_err),
                ));
            }
            (Ok(v), Ok(())) => v,
        };
        self.module_records.mark_evaluated();
        Ok(
            ExecutionResult::from_vm_value(value, start.elapsed(), self.interp.gc_heap_mut())
                .with_exit_code(process::exit_code(&self.interp)),
        )
    }

    fn module_loader_for_entry(&self, entry_path: &Path) -> module_loader::ModuleLoader {
        self.module_loader.for_entry(
            entry_path,
            &self.config.hosted_modules,
            &self.package_manager,
            &self.config.capabilities,
        )
    }

    /// Parse and compile a file without executing it.
    ///
    /// Module-shaped inputs use the same [`module_loader`] and module graph
    /// pipeline as [`Self::run_file`]. Script-shaped inputs use the same
    /// script compiler path, stopping before interpreter dispatch.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn check_file(&mut self, path: impl AsRef<Path>) -> Result<(), OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_path_has_module_extension(path) {
            return self.check_module(path);
        }
        let package_type = {
            let loader = self.module_loader_for_entry(path);
            source_path_package_type(path, &loader)
        };
        if package_type == Some(module_loader::LoaderPackageType::Module) {
            return self.check_module(path);
        }
        let specifier = path.to_string_lossy().to_string();
        if package_type == Some(module_loader::LoaderPackageType::CommonJs) {
            return compile_script_source(&source.text, source.kind, &specifier)
                .map(|_| ())
                .map_err(|err| map_compile_error(err, &specifier));
        }
        if !source_path_has_script_extension(path) {
            let module = with_program(&source.text, source.kind, |program| {
                if program_looks_like_module(program) {
                    return Ok(None);
                }
                compile_script_program(program, source.kind, &specifier)
                    .map(Some)
                    .map_err(|err| map_compile_error(err, &specifier))
            })
            .map_err(|err| map_syntax_error(err, &specifier))??;
            if module.is_some() {
                return Ok(());
            }
            return self.check_module(path);
        }
        compile_script_source(&source.text, source.kind, &specifier)
            .map(|_| ())
            .map_err(|err| map_compile_error(err, &specifier))
    }

    fn check_module(&mut self, entry_path: &Path) -> Result<(), OtterError> {
        let loader = self.module_loader_for_entry(entry_path);
        let linked = self
            .module_graph
            .load_program(&loader, entry_path)
            .map_err(map_graph_error)?;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        Ok(())
    }

    /// Run a file from disk, detecting script vs module shape.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_file(&mut self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_path_has_module_extension(path) {
            return self.run_module(path);
        }
        let package_type = {
            let loader = self.module_loader_for_entry(path);
            source_path_package_type(path, &loader)
        };
        if package_type == Some(module_loader::LoaderPackageType::Module) {
            return self.run_module(path);
        }
        let specifier = path.to_string_lossy().to_string();
        if package_type == Some(module_loader::LoaderPackageType::CommonJs) {
            return self.run_script(source, &specifier);
        }
        if !source_path_has_script_extension(path) {
            let start = std::time::Instant::now();
            let module = with_program(&source.text, source.kind, |program| {
                if program_looks_like_module(program) {
                    return Ok(None);
                }
                compile_script_program(program, source.kind, &specifier)
                    .map(Some)
                    .map_err(|err| map_compile_error(err, &specifier))
            })
            .map_err(|err| map_syntax_error(err, &specifier))??;
            if let Some(module) = module {
                return self.run_compiled_script_since(module, start);
            }
            return self.run_module(path);
        }
        let specifier = path.to_string_lossy().to_string();
        self.run_script(source, &specifier)
    }
}

/// Layer A entry point: zero-config Otter.
///
/// Wraps a [`RuntimeHandle`] with sensible defaults. The simple case
/// for embedders is async-first and safe to clone into Tokio worker
/// tasks.
#[derive(Clone, Debug)]
pub struct Otter {
    handle: RuntimeHandle,
}

impl Otter {
    /// Construct with defaults: deny-all capabilities,
    /// 256 MiB heap cap, 30 s timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::builder()
            .build()
            .expect("default OtterBuilder must build")
    }

    /// Start configuring the public handle facade.
    #[must_use]
    pub fn builder() -> OtterBuilder {
        OtterBuilder::default()
    }

    /// Run a file from disk, detecting kind by extension and
    /// routing module-shaped files through the module-graph
    /// pipeline.
    ///
    /// # Algorithm
    /// 1. Read the file's source text.
    /// 2. Use module-only extensions (`.mjs` / `.mts`) to route directly to
    ///    the module graph and script-only extensions (`.cjs` / `.cts`) to
    ///    route directly to script execution.
    /// 3. For ambiguous `.js` / `.ts`, parse once with OXC, inspect the AST
    ///    for module syntax, and reuse that parsed program for script
    ///    compilation when no module syntax is present.
    /// 4. Module sources go through [`Runtime::run_module`]; plain scripts go
    ///    through the script bytecode path.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_file(path.as_ref().to_path_buf()).await
    }

    /// Parse and compile a file without executing it.
    ///
    /// Module-shaped files use the same loader and package-graph resolution as
    /// [`Self::run_file`], so CLI `check` and `run` report resolver failures
    /// from the same path.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn check_file(&self, path: impl AsRef<Path>) -> Result<(), OtterError> {
        self.handle.check_file(path.as_ref().to_path_buf()).await
    }

    /// Run an ES module entry file from disk.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_module(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_module(path.as_ref().to_path_buf()).await
    }

    /// Run a string of JavaScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(SourceInput::from_javascript(source), "<script>")
            .await
    }

    /// Run a string of TypeScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(SourceInput::from_typescript(source), "<script>")
            .await
    }

    /// Evaluate a snippet.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle.eval(SourceInput::from_javascript(source)).await
    }

    /// Blocking file execution wrapper for sync embedders.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref().to_path_buf();
        self.handle.block_on(self.run_file(path))
    }

    /// Blocking ES module execution wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_module(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref().to_path_buf();
        self.handle.block_on(self.run_module(path))
    }

    /// Blocking JavaScript execution wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        let source = source.to_string();
        let handle = self.handle.clone();
        self.handle.block_on(async move {
            handle
                .run_script(SourceInput::from_javascript(source), "<script>")
                .await
        })
    }

    /// Blocking TypeScript execution wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        let source = source.to_string();
        let handle = self.handle.clone();
        self.handle.block_on(async move {
            handle
                .run_script(SourceInput::from_typescript(source), "<script>")
                .await
        })
    }

    /// Blocking eval wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        let source = source.to_string();
        let handle = self.handle.clone();
        self.handle
            .block_on(async move { handle.eval(SourceInput::from_javascript(source)).await })
    }

    /// Cooperative cancellation.
    pub fn interrupt(&self) {
        self.handle.interrupt();
    }

    /// Snapshot activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        self.handle.activity_stats()
    }

    /// Drop down to Layer B.
    #[must_use]
    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }
}

/// Builder for the public [`Otter`] facade.
#[derive(Debug, Clone, Default)]
pub struct OtterBuilder {
    runtime: RuntimeBuilder,
}

impl OtterBuilder {
    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Register one class-shaped global described by a static spec.
    #[must_use]
    pub fn global_class(mut self, spec: GlobalClass) -> Self {
        self.runtime = self.runtime.global_class(spec);
        self
    }

    /// Register multiple class-shaped globals.
    #[must_use]
    pub fn global_classes(mut self, specs: impl IntoIterator<Item = GlobalClass>) -> Self {
        self.runtime = self.runtime.global_classes(specs);
        self
    }

    /// Register one runtime-hosted module such as `node:fs` or `otter:kv`.
    #[must_use]
    pub fn hosted_module(mut self, module: HostedModule) -> Self {
        self.runtime = self.runtime.hosted_module(module);
        self
    }

    /// Register multiple runtime-hosted modules.
    #[must_use]
    pub fn hosted_modules(mut self, modules: impl IntoIterator<Item = HostedModule>) -> Self {
        self.runtime = self.runtime.hosted_modules(modules);
        self
    }

    /// Override the implementation behind `console.*`.
    #[must_use]
    pub fn console_sink(mut self, sink: ConsoleSinkHandle) -> Self {
        self.runtime = self.runtime.console_sink(sink);
        self
    }

    /// Set the `process.argv` snapshot installed into the runtime.
    #[must_use]
    pub fn process_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.runtime = self.runtime.process_argv(argv);
        self
    }

    /// Set the `process.cwd()` snapshot installed into the runtime.
    #[must_use]
    pub fn process_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.runtime = self.runtime.process_cwd(cwd);
        self
    }

    /// Construct the public async facade.
    ///
    /// # Errors
    /// Returns [`OtterError`] if configuration validation or isolate
    /// startup fails.
    pub fn build(self) -> Result<Otter, OtterError> {
        Ok(Otter {
            handle: self.runtime.build_handle()?,
        })
    }
}

impl Default for Otter {
    fn default() -> Self {
        Self::new()
    }
}

/// AST-based "is this a module?" detection.
///
/// Parses the source through `otter-syntax` (the same OXC frontend
/// the compiler uses) and walks the program's AST looking for any
/// of:
/// - `ImportDeclaration` / `ExportNamedDeclaration` /
///   `ExportDefaultDeclaration` / `ExportAllDeclaration` (these
///   can only appear at the module top level — finding one is
///   conclusive proof);
/// - any `ImportExpression` (`import(specifier)` — only legal in
///   modules);
/// - any `MetaProperty` whose base is `import` (`import.meta` —
///   only legal in modules).
///
/// The walk uses [`oxc_ast_visit::Visit`] so we don't hand-roll a
/// match arm per AST node kind. The frontend policy forbids regex /
/// string parsing of JS/TS source.
///
/// Parse failures default to `false`: the caller routes to the
/// script path, which will re-parse and surface the same syntax
/// error through its diagnostic pipeline.
#[cfg(test)]
fn source_text_looks_like_module(text: &str, kind: SourceKind) -> bool {
    with_program(text, kind, program_looks_like_module).unwrap_or(false)
}

fn program_looks_like_module(program: &oxc_ast::ast::Program<'_>) -> bool {
    use oxc_ast::ast::{Expression, Statement};
    use oxc_ast_visit::Visit;

    #[derive(Default)]
    struct ModuleSyntaxFinder {
        found: bool,
    }

    impl<'a> Visit<'a> for ModuleSyntaxFinder {
        fn visit_import_declaration(&mut self, _: &oxc_ast::ast::ImportDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_named_declaration(&mut self, _: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_default_declaration(
            &mut self,
            _: &oxc_ast::ast::ExportDefaultDeclaration<'a>,
        ) {
            self.found = true;
        }
        fn visit_export_all_declaration(&mut self, _: &oxc_ast::ast::ExportAllDeclaration<'a>) {
            self.found = true;
        }
        fn visit_import_expression(&mut self, _: &oxc_ast::ast::ImportExpression<'a>) {
            self.found = true;
        }
        fn visit_meta_property(&mut self, meta: &oxc_ast::ast::MetaProperty<'a>) {
            if meta.meta.name.as_str() == "import" && meta.property.name.as_str() == "meta" {
                self.found = true;
            }
        }
    }

    let mut finder = ModuleSyntaxFinder::default();
    // Visit each statement; the visitor short-circuits via the
    // `found` flag (we still walk everything, but the boolean
    // result is correct).
    for stmt in &program.body {
        finder.visit_statement(stmt);
        if finder.found {
            return true;
        }
    }
    // Also check program-level expressions inside expression
    // statements — covered by visit_statement above.
    let _ = (Statement::EmptyStatement, Expression::NullLiteral);
    finder.found
}

fn source_path_has_module_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("mjs" | "mts")
    )
}

fn source_path_has_script_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("cjs" | "cts")
    )
}

fn source_path_package_type(
    path: &Path,
    loader: &module_loader::ModuleLoader,
) -> Option<module_loader::LoaderPackageType> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "ts" | "jsx" | "tsx") => loader.package_type_for_path(path),
        _ => None,
    }
}

pub(crate) fn map_graph_error(err: module_graph::GraphError) -> OtterError {
    match err {
        // Capability gating per ENGINE_REFACTOR_EXECUTION_PLAN
        // §P2.1: surface capability denials with their own error
        // code so embedders / CLI users can distinguish a missing
        // permission from a genuinely unresolvable specifier.
        module_graph::GraphError::Loader(module_loader::LoaderError::CapabilityDenied {
            specifier,
            capability,
            resource,
        }) => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::permission(format!(
                    "import of `{specifier}` requires capability `{capability}` for `{resource}`"
                ))
                .with_code_enum(DiagnosticCode::ModuleCapabilityDenied)
                .with_help(format!(
                    "grant the matching capability (e.g. --allow-{capability}={resource}) or remove the import"
                )),
            ],
        },
        module_graph::GraphError::Loader(err) => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::syntax(err.to_string())
                    .with_code_enum(DiagnosticCode::ModuleResolutionError)
                    .with_help(
                        "check the import specifier and package/module loader configuration",
                    ),
            ],
        },
        module_graph::GraphError::Parse { url, error } => map_syntax_error(error, &url),
        module_graph::GraphError::Compile { url, error } => map_compile_error(error, &url),
        module_graph::GraphError::Cycle { url } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::syntax(format!(
                    "module graph cycle or depth limit reached at `{url}`"
                ))
                .with_code_enum(DiagnosticCode::ModuleGraphCycle)
                .with_source_url(url)
                .with_help("break the import cycle or reduce module graph depth"),
            ],
        },
    }
}

fn map_syntax_error(err: SyntaxError, source_url: &str) -> OtterError {
    let diagnostics = if err.diagnostics.is_empty() {
        vec![
            Diagnostic::syntax(err.messages.join("; "))
                .with_source_url(source_url)
                .with_help("fix the syntax error in the source file"),
        ]
    } else {
        err.diagnostics
            .iter()
            .map(|diagnostic| map_syntax_diagnostic(diagnostic, source_url))
            .collect()
    };
    OtterError::Compile { diagnostics }
}

fn map_syntax_diagnostic(diagnostic: &SyntaxDiagnostic, source_url: &str) -> Diagnostic {
    let mut mapped = Diagnostic::syntax(diagnostic.message.clone())
        .with_code(diagnostic.code.clone())
        .with_source_url(source_url);
    if let Some(range) = diagnostic.range {
        mapped = mapped.with_range(range);
    }
    mapped.with_help(
        diagnostic
            .help
            .clone()
            .unwrap_or_else(|| "fix the syntax error in the source file".to_string()),
    )
}

pub(crate) fn map_compile_error(err: otter_compiler::CompileError, source_url: &str) -> OtterError {
    use otter_compiler::CompileError;
    match err {
        CompileError::Syntax {
            messages,
            diagnostics,
        } => {
            if diagnostics.is_empty() {
                OtterError::Compile {
                    diagnostics: vec![
                        Diagnostic::syntax(messages.join("; "))
                            .with_source_url(source_url)
                            .with_help("fix the syntax error in the source file"),
                    ],
                }
            } else {
                OtterError::Compile {
                    diagnostics: diagnostics
                        .iter()
                        .map(|diagnostic| map_syntax_diagnostic(diagnostic, source_url))
                        .collect(),
                }
            }
        }
        CompileError::Unsupported { node, span } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::unsupported(format!("unsupported AST node: {node}"), span)
                    .with_source_url(source_url),
            ],
        },
        CompileError::TypeScriptUnsupported { node, span } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::ts_unsupported(
                    format!("typescript {node} is not supported in foundation"),
                    span,
                )
                .with_source_url(source_url),
            ],
        },
        _ => OtterError::Internal {
            code: DiagnosticCode::CompileUnknown.as_str().to_string(),
            message: "unknown compiler error variant".to_string(),
        },
    }
}

/// Convert a `file://` URL back into a filesystem path. Returns
/// `None` for any other scheme (e.g. `https://`) — dynamic
/// imports targeting non-`file://` URLs use a separate fetch
/// path inside [`Runtime::load_dynamic_module`].
fn url_to_path(url: &str) -> Option<std::path::PathBuf> {
    let trimmed = url.strip_prefix("file://")?;
    Some(std::path::PathBuf::from(trimmed))
}

/// Internal error type for [`Runtime::load_dynamic_module`]. Split
/// so the surrounding settle path can distinguish:
///
/// - [`DynLoadError::Diagnostic`] — host-side resolve / load /
///   compile / link / alloc failure. The settler synthesises a
///   fresh `TypeError` from the message.
/// - [`DynLoadError::Thrown`] — the dynamically-loaded module's
///   `<module-init>` threw a JS value. The settler uses that
///   value directly as the promise's rejection reason per
///   §16.2.1.7 step 7.b.i + §27.2.1.7.
enum DynLoadError {
    Diagnostic(String),
    Thrown(otter_vm::Value),
}

/// Sentinel property used to flag a `module_env` as already
/// having had its `<module-init>` body executed. Dynamic imports
/// load + invoke each new init exactly once; this avoids
/// re-running an init when the same module shows up in two
/// dynamically-loaded sub-graphs (the linker generates separate
/// `<module-init>` instances per LinkedProgram so the runtime
/// has to dedupe).
const DYNAMIC_INIT_MARKER: &str = "__otter_module_inited__";

fn otter_vm_init_marker_set(interp: &otter_vm::Interpreter, env: otter_vm::JsObject) -> bool {
    otter_vm::object::get(env, interp.gc_heap(), DYNAMIC_INIT_MARKER)
        .is_some_and(|v| v.as_boolean() == Some(true))
}

fn otter_vm_init_marker_install(interp: &mut otter_vm::Interpreter, env: otter_vm::JsObject) {
    otter_vm::object::set(
        env,
        interp.gc_heap_mut(),
        DYNAMIC_INIT_MARKER,
        otter_vm::Value::boolean(true),
    );
}

fn alloc_dynamic_import_meta(
    interp: &mut otter_vm::Interpreter,
    env: otter_vm::JsObject,
    url: &str,
) -> Result<otter_vm::JsObject, DynLoadError> {
    let env_root = otter_vm::Value::object(env);
    let import_meta = interp
        .alloc_host_object_with_roots(&[&env_root], &[])
        .map_err(|e| {
            DynLoadError::Diagnostic(format!("dynamic import: alloc import_meta failed: {e}"))
        })?;
    let url_string = otter_vm::JsString::from_str(url, interp.gc_heap_mut()).map_err(|err| {
        DynLoadError::Diagnostic(format!(
            "dynamic import: alloc import_meta.url failed: {err}"
        ))
    })?;
    otter_vm::object::set(
        import_meta,
        interp.gc_heap_mut(),
        "url",
        otter_vm::Value::string(url_string),
    );
    Ok(import_meta)
}

/// `true` when the bytecode fragment contains any
/// `Op::ImportNamespace` / `Op::ImportNamespaceDynamic`
/// instruction. The HTTPS fetcher only handles modules without
/// own imports for now — modules with imports need a recursive
/// HTTPS-aware loader pipeline (next slice).
fn fragment_has_import_namespace_ops(module: &otter_bytecode::BytecodeModule) -> bool {
    module.functions.iter().any(|f| {
        f.code.iter().any(|instr| {
            matches!(
                instr.op,
                otter_bytecode::Op::ImportNamespace | otter_bytecode::Op::ImportNamespaceDynamic
            )
        })
    })
}

/// Maximum depth the diagnostic cause-chain walker descends into
/// nested `Error.cause` properties before bailing out. Protects
/// against pathological self-referential chains.
const MAX_CAUSE_CHAIN_DEPTH: usize = 32;

/// Walk a thrown JS value into a [`Diagnostic`] tree.
///
/// Reads `name` / `message` / `cause` / `errors` from any Error
/// instance (§20.5 / §20.5.7). Recurses on `cause` up to
/// [`MAX_CAUSE_CHAIN_DEPTH`]. Non-Error throws (strings, numbers,
/// …) become plain `TypeError`-categorised diagnostics with the
/// stringified value as `message`.
fn diagnostic_from_thrown_value(
    interp: &otter_vm::Interpreter,
    value: &otter_vm::Value,
    depth: usize,
) -> Diagnostic {
    if depth > MAX_CAUSE_CHAIN_DEPTH {
        return Diagnostic::new(
            DiagnosticKind::Internal,
            DiagnosticCode::VmBytecodeInvariant,
            "diagnostic cause chain exceeded depth limit".to_string(),
        );
    }
    let heap = interp.gc_heap();
    let Some(obj) = value.as_object() else {
        return Diagnostic::new(
            DiagnosticKind::Type,
            DiagnosticCode::Uncaught,
            value.display_string(heap),
        );
    };

    let name = otter_vm::object::get(obj, heap, "name")
        .map(|v| v.display_string(heap))
        .unwrap_or_else(|| "Error".to_string());
    let message = otter_vm::object::get(obj, heap, "message")
        .map(|v| v.display_string(heap))
        .unwrap_or_default();
    let full = if message.is_empty() {
        name.clone()
    } else {
        format!("{name}: {message}")
    };
    let (kind, code) = vm_error_kind_and_code_from_name(&name);
    let mut diag = Diagnostic::new(kind, code, full);

    // `cause`: recurse.
    if let Some(cause) = otter_vm::object::get(obj, heap, "cause") {
        diag = diag.with_cause(diagnostic_from_thrown_value(interp, &cause, depth + 1));
    }

    // `errors`: AggregateError. Each entry becomes an aggregated
    // diagnostic; recurse one level so nested causes inside each
    // error survive.
    if let Some(errors) = otter_vm::object::get(obj, heap, "errors").and_then(|v| v.as_array()) {
        let entries: Vec<Diagnostic> = otter_vm::array::with_elements(errors, heap, |slice| {
            slice
                .iter()
                .map(|v| diagnostic_from_thrown_value(interp, v, depth + 1))
                .collect()
        });
        if !entries.is_empty() {
            diag = diag.with_aggregated_errors(entries);
        }
    }

    diag
}

/// Map a thrown Error's `.name` to the matching
/// [`DiagnosticKind`] / [`DiagnosticCode`] pair.
fn vm_error_kind_and_code_from_name(name: &str) -> (DiagnosticKind, DiagnosticCode) {
    match name {
        "TypeError" => (DiagnosticKind::Type, DiagnosticCode::TypeError),
        "RangeError" => (DiagnosticKind::Range, DiagnosticCode::StackOverflow),
        "SyntaxError" => (DiagnosticKind::Syntax, DiagnosticCode::SyntaxError),
        "ReferenceError" => (DiagnosticKind::Reference, DiagnosticCode::Tdz),
        _ => (DiagnosticKind::Type, DiagnosticCode::Uncaught),
    }
}

/// Enrich a runtime diagnostic with the `cause` chain and
/// `errors` aggregated entries from the original thrown JS value,
/// when available.
fn enrich_runtime_diagnostic_with_cause(
    interp: &mut otter_vm::Interpreter,
    err: OtterError,
) -> OtterError {
    let OtterError::Runtime { mut diagnostic } = err else {
        return err;
    };
    let Some(thrown) = interp.take_pending_uncaught_throw() else {
        return OtterError::Runtime { diagnostic };
    };
    // Walk the throw value once and attach both `cause` and
    // `aggregated_errors` directly onto the outer diagnostic.
    let chain = diagnostic_from_thrown_value(interp, &thrown, 0);
    diagnostic.cause = chain.cause;
    diagnostic.aggregated_errors = chain.aggregated_errors;
    OtterError::Runtime { diagnostic }
}

fn map_vm_error(run_err: otter_vm::RunError) -> OtterError {
    use otter_vm::VmError;
    let otter_vm::RunError { error, frames } = run_err;
    let stack_frames: Vec<StackFrame> = frames
        .into_iter()
        .map(|f| StackFrame {
            function: f.function_name,
            module: f.module,
            span: Some(f.span),
        })
        .collect();
    let top_span = stack_frames.first().and_then(|f| f.span);
    let display = error.to_string();
    let runtime_diagnostic =
        |kind: DiagnosticKind, code: DiagnosticCode, message: String| OtterError::Runtime {
            diagnostic: Diagnostic {
                kind,
                code: code.as_str().to_string(),
                message,
                source_url: stack_frames.first().map(|frame| frame.module.clone()),
                range: top_span,
                span: top_span,
                help: None,
                frames: stack_frames.clone(),
                cause: None,
                aggregated_errors: Vec::new(),
            },
        };
    match error {
        VmError::Interrupted => OtterError::Interrupted,
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => OtterError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        VmError::BudgetExceeded { message } => runtime_diagnostic(
            DiagnosticKind::Timeout,
            DiagnosticCode::BudgetExceeded,
            message,
        ),
        VmError::TypeMismatch => {
            runtime_diagnostic(DiagnosticKind::Type, DiagnosticCode::TypeMismatch, display)
        }
        VmError::TypeError { message } => {
            runtime_diagnostic(DiagnosticKind::Type, DiagnosticCode::TypeError, message)
        }
        VmError::SyntaxError { message } => {
            runtime_diagnostic(DiagnosticKind::Syntax, DiagnosticCode::SyntaxError, message)
        }
        VmError::UnknownIntrinsic { name } => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::UnknownMethod,
            format!("unknown method `{name}`"),
        ),
        VmError::TemporalDeadZone { local_index } => runtime_diagnostic(
            DiagnosticKind::Reference,
            DiagnosticCode::Tdz,
            format!("cannot access local {local_index} before initialization"),
        ),
        VmError::StackOverflow { limit } => runtime_diagnostic(
            DiagnosticKind::Range,
            DiagnosticCode::StackOverflow,
            format!("maximum call stack size exceeded (limit {limit})"),
        ),
        VmError::NotCallable => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::NotCallable,
            "value is not a function".to_string(),
        ),
        VmError::Uncaught { value } => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::Uncaught,
            format!("uncaught exception: {value}"),
        ),
        VmError::JsonError { code, message } => {
            // `code` is `&'static str` from the VM JSON path (every
            // value is one of the `JSON_*` codes in the closed
            // [`DiagnosticCode`] set). Parse it back through
            // `DiagnosticCode::parse` so the diagnostic still
            // carries a typed code in the closed set.
            let typed = DiagnosticCode::parse(code).unwrap_or(DiagnosticCode::JsonBadArg);
            runtime_diagnostic(DiagnosticKind::Type, typed, message)
        }
        VmError::InvalidRegExp { message } => runtime_diagnostic(
            DiagnosticKind::Syntax,
            DiagnosticCode::InvalidRegexp,
            message,
        ),
        VmError::MissingReturn | VmError::InvalidOperand => OtterError::Internal {
            code: DiagnosticCode::VmBytecodeInvariant.as_str().to_string(),
            message: display,
        },
        _ => OtterError::Internal {
            code: DiagnosticCode::VmUnknown.as_str().to_string(),
            message: display,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_loop::{TimerRequest, TimerToken};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn public_handle_types_are_send_sync() {
        assert_send_sync::<Otter>();
        assert_send_sync::<RuntimeHandle>();
        assert_send_sync::<RuntimeHooks>();
    }

    #[test]
    fn runtime_diagnostic_hook_receives_emit() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let runtime = Runtime::builder()
            .diagnostic_hook(move |diagnostic: &Diagnostic| {
                if diagnostic.code == "SYNTAX_ERROR" {
                    seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })
            .build()
            .unwrap();

        runtime.emit_diagnostic(&Diagnostic::syntax("resolver failed"));

        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn runtime_capability_hook_can_override_default_policy() {
        struct AllowExampleNet;

        impl RuntimeCapabilityHook for AllowExampleNet {
            fn check_capability(
                &self,
                _capabilities: &CapabilitySet,
                capability: RuntimeCapability,
                request: &CapabilityRequest<'_>,
            ) -> bool {
                capability == RuntimeCapability::Net
                    && matches!(request, CapabilityRequest::Host("example.com"))
            }
        }

        let runtime = Runtime::builder()
            .capabilities(CapabilitySet::sandbox())
            .capability_hook(AllowExampleNet)
            .build()
            .unwrap();

        assert!(runtime.check_capability(
            RuntimeCapability::Net,
            &CapabilityRequest::Host("example.com")
        ));
        assert!(
            !runtime.check_capability(RuntimeCapability::Env, &CapabilityRequest::EnvVar("HOME"))
        );
    }

    #[test]
    fn runtime_session_owns_module_graph_and_source_map_state() {
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep.ts");
        let entry = dir.path().join("entry.ts");
        std::fs::write(&dep, "export const value = 1;\n").unwrap();
        std::fs::write(
            &entry,
            "import { value } from './dep.ts';\nexport const result = value + 1;\n",
        )
        .unwrap();

        let mut runtime = Runtime::builder().build().unwrap();
        runtime.check_file(&entry).unwrap();

        let entry_url = format!(
            "file://{}",
            std::fs::canonicalize(&entry).unwrap().display()
        );
        let dep_url = format!("file://{}", std::fs::canonicalize(&dep).unwrap().display());
        assert_eq!(
            runtime.module_graph.last_entry_url.as_deref(),
            Some(entry_url.as_str())
        );
        assert_eq!(runtime.module_graph.last_module_count, 2);
        assert!(runtime.source_maps.contains_module(&entry_url));
        assert!(runtime.source_maps.contains_module(&dep_url));
    }

    #[test]
    fn dump_file_uses_module_graph_and_preserves_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep.ts");
        let entry = dir.path().join("entry.ts");
        std::fs::write(&dep, "export const value = 1;\n").unwrap();
        std::fs::write(
            &entry,
            "import { value } from './dep.ts';\nexport const result = value + 1;\n",
        )
        .unwrap();

        let mut runtime = Runtime::builder().build().unwrap();
        let compiled = runtime.dump_file(&entry).unwrap();

        let entry_url = format!(
            "file://{}",
            std::fs::canonicalize(&entry).unwrap().display()
        );
        let dep_url = format!("file://{}", std::fs::canonicalize(&dep).unwrap().display());
        assert_eq!(compiled.entry_url.as_deref(), Some(entry_url.as_str()));
        assert_eq!(compiled.metadata.len(), 2);
        assert!(compiled.metadata.iter().any(|metadata| {
            metadata.source_url == entry_url
                && metadata.imports.iter().any(|import| {
                    import.specifier == "./dep.ts"
                        && import.target.as_deref() == Some(dep_url.as_str())
                })
                && metadata
                    .exports
                    .iter()
                    .any(|export| export.name == "result")
        }));
        assert!(runtime.source_maps.contains_module(&entry_url));
        assert!(runtime.source_maps.contains_module(&dep_url));
    }

    #[test]
    fn run_module_allocates_records_before_evaluation() {
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep.ts");
        let entry = dir.path().join("entry.ts");
        std::fs::write(&dep, "export const value = 1;\n").unwrap();
        std::fs::write(
            &entry,
            "import { value } from './dep.ts';\nif (value !== 1) undefined.x;\n",
        )
        .unwrap();

        let mut runtime = Runtime::builder().build().unwrap();
        runtime.run_module(&entry).unwrap();

        let entry_url = format!(
            "file://{}",
            std::fs::canonicalize(&entry).unwrap().display()
        );
        let dep_url = format!("file://{}", std::fs::canonicalize(&dep).unwrap().display());
        assert_eq!(runtime.module_records.len(), 2);
        assert_eq!(
            runtime.module_records.state(&entry_url),
            Some(module_records::RuntimeModuleRecordState::Evaluated)
        );
        assert_eq!(
            runtime.module_records.state(&dep_url),
            Some(module_records::RuntimeModuleRecordState::Evaluated)
        );
        assert!(runtime.module_records.env(&entry_url).is_some());
        assert!(runtime.interp.module_env(&entry_url).is_some());
        assert!(runtime.module_records.env(&dep_url).is_some());
        assert!(runtime.interp.module_env(&dep_url).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn otter_clone_runs_from_tokio_worker_tasks() {
        let otter = Otter::new();
        let mut joins = Vec::new();
        for i in 0..8 {
            let otter = otter.clone();
            joins.push(tokio::spawn(async move {
                let source = format!("{i};");
                let result = otter.run_script(&source).await.unwrap();
                result.completion_string().to_string()
            }));
        }

        let mut completions = Vec::new();
        for join in joins {
            completions.push(join.await.unwrap());
        }
        completions.sort();
        assert_eq!(completions, ["0", "1", "2", "3", "4", "5", "6", "7"]);

        let stats = otter.activity_stats();
        assert_eq!(stats.submitted_commands, 8);
        assert_eq!(stats.completed_commands, 8);
        assert_eq!(stats.failed_commands, 0);
        assert_eq!(stats.queued_commands, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn otter_async_run_module_uses_handle_runner() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dep.ts"), "export const value = 41;\n").unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import { value } from \"./dep.ts\";\nfunction fail() { return undefined.x; }\nif (value !== 41) fail();\n",
        )
        .unwrap();

        let otter = Otter::new();
        let result = otter.run_module(dir.path().join("entry.ts")).await.unwrap();

        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn runtime_installs_console_global() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_typescript(
                r#"
                function fail(msg) { throw new Error(msg); }
                if (typeof console !== "object") fail("missing console");
                if (typeof console.log !== "function") fail("missing console.log");
                if (typeof console.error !== "function") fail("missing console.error");
                console.log("runtime-console", 7);
                console.warn(new Error("warn-path"));
                console.assert(true, "should not print");
                console.assert(false, "assert-path");
                "#,
            )
            .unwrap();

        assert_eq!(result.completion_string(), "undefined");
    }

    type ConsoleEvents =
        std::sync::Arc<std::sync::Mutex<Vec<(otter_vm::ConsoleLevel, Vec<String>)>>>;

    #[derive(Debug)]
    struct CapturingConsole {
        events: ConsoleEvents,
    }

    impl otter_vm::ConsoleSink for CapturingConsole {
        fn write(&self, level: otter_vm::ConsoleLevel, fields: &[String]) {
            self.events.lock().unwrap().push((level, fields.to_vec()));
        }
    }

    #[test]
    fn runtime_console_sink_is_embedder_overridable() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::new(CapturingConsole {
            events: events.clone(),
        });
        let otter = Otter::builder().console_sink(sink).build().unwrap();
        let result = otter
            .blocking_run_typescript(
                r#"
                console.log("hello", 7);
                console.warn("careful");
                console.assert(false, "nope");
                "#,
            )
            .unwrap();

        assert_eq!(result.completion_string(), "undefined");
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].0, otter_vm::ConsoleLevel::Log);
        assert_eq!(captured[0].1, vec!["hello".to_string(), "7".to_string()]);
        assert_eq!(captured[1].0, otter_vm::ConsoleLevel::Warn);
        assert_eq!(captured[1].1, vec!["careful".to_string()]);
        assert_eq!(captured[2].0, otter_vm::ConsoleLevel::Assert);
        assert_eq!(
            captured[2].1,
            vec!["Assertion failed".to_string(), "nope".to_string()]
        );
    }

    #[test]
    fn runtime_installs_math_namespace_from_static_surface_spec() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_typescript(
                r#"
                function fail(msg) { throw new Error(msg); }
                const pi = Object.getOwnPropertyDescriptor(Math, "PI");
                if (pi.writable !== false) fail("PI writable");
                if (pi.enumerable !== false) fail("PI enumerable");
                if (pi.configurable !== false) fail("PI configurable");
                const abs = Math.abs;
                if (typeof abs !== "function") fail("missing Math.abs");
                if (abs.length !== 1) fail("bad Math.abs length");
                if (abs(-7) !== 7) fail("bad extracted Math.abs");
                "#,
            )
            .unwrap();

        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn runtime_installs_json_and_atomics_namespaces_from_static_surface_specs() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_typescript(
                r#"
                function fail(msg) { throw new Error(msg); }

                const parse = JSON.parse;
                if (typeof parse !== "function") fail("missing JSON.parse");
                if (parse.length !== 2) fail("bad JSON.parse length");
                if (parse("{\"x\":3}").x !== 3) fail("bad extracted JSON.parse");

                const stringify = JSON.stringify;
                if (stringify.length !== 3) fail("bad JSON.stringify length");
                if (stringify({ x: 4 }) !== "{\"x\":4}") fail("bad extracted JSON.stringify");

                const isLockFree = Atomics.isLockFree;
                if (typeof isLockFree !== "function") fail("missing Atomics.isLockFree");
                if (isLockFree.length !== 1) fail("bad Atomics.isLockFree length");
                if (isLockFree(4) !== true) fail("bad extracted Atomics.isLockFree");
                "#,
            )
            .unwrap();

        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn top_level_await_rejection_renders_error_to_string() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript(
                r#"
                await Promise.resolve();
                throw new Error("boom");
                "#,
            )
            .unwrap_err();

        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "UNCAUGHT");
                assert_eq!(diagnostic.message, "uncaught exception: Error: boom");
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_handle_timers_and_diagnostics_update_stats() {
        let otter = Otter::new();
        let handle = otter.handle().clone();

        let token = handle.schedule_timer(TimerRequest {
            delay: Duration::from_millis(10),
            repeat: None,
        });
        assert!(!handle.cancel_timer(TimerToken(token.0 + 1)));
        handle.complete_dynamic_module_job_for_tests();
        handle.wake_runtime("runtime-test");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.activity_stats();

        assert_eq!(stats.pending_ref_timers, 0);
        assert_eq!(stats.fired_timers, 1);
        assert_eq!(stats.pending_dynamic_module_jobs, 0);
        assert_eq!(stats.completed_dynamic_module_jobs, 1);
        assert_eq!(stats.diagnostics, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_handle_timer_cancellation_and_timeout_are_observable() {
        let otter = Otter::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let handle = otter.handle().clone();
        let token = handle.schedule_timer(TimerRequest {
            delay: Duration::from_secs(60),
            repeat: None,
        });
        assert!(handle.cancel_timer(token));

        let err = otter.run_script("while (true) {}").await.unwrap_err();
        assert!(matches!(err, OtterError::Timeout { .. }));

        let result = otter.run_script("1 + 1;").await.unwrap();
        assert_eq!(result.completion_string(), "2");

        let stats = otter.activity_stats();
        assert_eq!(stats.cancelled_timers, 1);
        assert_eq!(stats.timed_out_commands, 1);
        assert!(stats.failed_commands >= 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_handle_bounded_queue_reports_backpressure() {
        let config = RuntimeConfig {
            timeout: Duration::from_millis(100),
            ..RuntimeConfig::default()
        };
        let handle = RuntimeHandle::spawn_with_capacity(config, 1).unwrap();

        let first = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .run_script(SourceInput::from_javascript("while (true) {}"), "<busy>")
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;

        let second = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .run_script(SourceInput::from_javascript("1;"), "<queued>")
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;

        let err = handle
            .run_script(SourceInput::from_javascript("2;"), "<overflow>")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OtterError::Internal { code, .. } if code == "RUNTIME_BACKPRESSURE"
        ));

        assert!(matches!(
            first.await.unwrap(),
            Err(OtterError::Timeout { .. })
        ));
        assert_eq!(second.await.unwrap().unwrap().completion_string(), "1");
        assert_eq!(handle.activity_stats().backpressure_rejections, 1);
    }

    #[test]
    fn looks_like_module_detects_import_at_top_level() {
        let text = "import { x } from \"./y.ts\";\n";
        assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
    }

    #[test]
    fn looks_like_module_detects_export_function() {
        let text = "export function f() {}\n";
        assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
    }

    #[test]
    fn looks_like_module_with_leading_block_comment() {
        let text = "/* hi */\nimport { x } from \"./y.ts\";\n";
        assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
    }

    #[test]
    fn module_program_runs_two_file_static_import() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.ts"), "export let value = 7;\n").unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import { value } from \"./other.ts\";\nfunction fail() { return undefined.x; }\nif (value !== 7) fail();\n",
        )
        .unwrap();
        let otter = Otter::new();
        otter
            .blocking_run_file(dir.path().join("entry.ts"))
            .unwrap();
    }

    #[test]
    fn module_program_imports_json_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.json"), r#"{"answer":42}"#).unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import data from \"./data.json\";\nfunction fail() { return undefined.x; }\nif (data.answer !== 42) fail();\n",
        )
        .unwrap();

        Otter::new()
            .blocking_run_file(dir.path().join("entry.ts"))
            .unwrap();
    }

    #[test]
    fn module_program_imports_json_with_type_attribute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.json"), r#"{"answer":42}"#).unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import data from \"./data.json\" with { type: \"json\" };\nfunction fail() { return undefined.x; }\nif (data.answer !== 42) fail();\n",
        )
        .unwrap();

        Otter::new()
            .blocking_run_file(dir.path().join("entry.ts"))
            .unwrap();
    }

    #[test]
    fn module_program_imports_package_from_loader_graph() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let dep = dir.path().join("store/dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("index.js"), "export let value = 11;\n").unwrap();
        std::fs::write(
            app.join("entry.ts"),
            "import { value } from \"dep\";\nfunction fail() { return undefined.x; }\nif (value !== 11) fail();\n",
        )
        .unwrap();

        let mut graph = module_loader::LoaderPackageGraph::new();
        graph.insert_package(module_loader::LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_package(module_loader::LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep,
            main: Some("index.js".into()),
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_dependency("app@workspace:.", "dep", "dep@npm:^1.0.0");

        let mut loader = module_loader::LoaderConfig::new(app.clone());
        loader.enable_node_modules = false;
        loader.package_graph = Some(graph);
        let otter = Otter::builder().module_loader(loader).build().unwrap();
        otter.blocking_run_file(app.join("entry.ts")).unwrap();
    }

    #[test]
    fn module_program_imports_package_import_alias_from_loader_graph() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(app.join("src/alias.ts"), "export let value = 19;\n").unwrap();
        std::fs::write(
            app.join("entry.ts"),
            "import { value } from \"#alias\";\nfunction fail() { return undefined.x; }\nif (value !== 19) fail();\n",
        )
        .unwrap();

        let mut graph = module_loader::LoaderPackageGraph::new();
        graph.insert_package(module_loader::LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(serde_json::json!({ "#alias": "./src/alias.ts" })),
            package_type: None,
        });

        let mut loader = module_loader::LoaderConfig::new(app.clone());
        loader.enable_node_modules = false;
        loader.package_graph = Some(graph);
        let otter = Otter::builder().module_loader(loader).build().unwrap();
        otter.blocking_run_file(app.join("entry.ts")).unwrap();
    }

    #[tokio::test]
    async fn commonjs_package_type_forces_script_parse_for_ambiguous_js() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("dep.js"), "export let value = 1;\n").unwrap();
        std::fs::write(
            app.join("entry.js"),
            "import { value } from \"./dep.js\";\nvalue;\n",
        )
        .unwrap();

        let mut graph = module_loader::LoaderPackageGraph::new();
        graph.insert_package(module_loader::LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: Some(module_loader::LoaderPackageType::CommonJs),
        });

        let mut loader = module_loader::LoaderConfig::new(app.clone());
        loader.enable_node_modules = false;
        loader.package_graph = Some(graph);
        let otter = Otter::builder().module_loader(loader).build().unwrap();

        let err = otter
            .check_file(app.join("entry.js"))
            .await
            .expect_err("commonjs package type should parse ambiguous .js as script");
        assert!(matches!(err, OtterError::Compile { .. }));
    }

    #[tokio::test]
    async fn check_file_imports_package_from_loader_graph_without_running_entry() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let dep = dir.path().join("store/dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("index.js"), "export let value = 13;\n").unwrap();
        std::fs::write(
            app.join("entry.ts"),
            "import { value } from \"dep\";\nfunction fail() { return undefined.x; }\nfail();\nvalue;\n",
        )
        .unwrap();

        let mut graph = module_loader::LoaderPackageGraph::new();
        graph.insert_package(module_loader::LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_package(module_loader::LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep,
            main: Some("index.js".into()),
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_dependency("app@workspace:.", "dep", "dep@npm:^1.0.0");

        let mut loader = module_loader::LoaderConfig::new(app.clone());
        loader.enable_node_modules = false;
        loader.package_graph = Some(graph);
        let otter = Otter::builder().module_loader(loader).build().unwrap();

        otter.check_file(app.join("entry.ts")).await.unwrap();
    }

    #[test]
    fn module_program_propagates_live_binding_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("counter.ts"),
            "export let count = 0;\nexport function inc() { count = count + 1; }\nexport function get() { return count; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import { inc, get } from \"./counter.ts\";\nfunction fail() { return undefined.x; }\ninc(); inc();\nif (get() !== 2) fail();\n",
        )
        .unwrap();
        let otter = Otter::new();
        otter
            .blocking_run_file(dir.path().join("entry.ts"))
            .unwrap();
    }

    /// ECMA-262 §16.2.1 Cyclic Module Records — the loader must
    /// short-circuit cyclic edges and rely on live-binding
    /// indirection through `module_env`. Both modules must
    /// compile and evaluate; spec correctness checks live in
    /// `tests/module_cycle_and_lifecycle.rs`.
    #[test]
    fn module_program_accepts_cycle_per_spec() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.ts"),
            "import { b } from \"./b.ts\";\nexport let a = 1;\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.ts"),
            "import { a } from \"./a.ts\";\nexport let b = 2;\n",
        )
        .unwrap();
        let otter = Otter::new();
        otter
            .blocking_run_file(dir.path().join("a.ts"))
            .expect("two-file cycle must run per §16.2.1");
    }

    #[test]
    fn module_program_dynamic_import_literal_resolves_to_namespace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("util.ts"), "export let answer = 42;\n").unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "function fail() { return undefined.x; }\nimport(\"./util.ts\").then((m) => { if (m.answer !== 42) fail(); });\n",
        )
        .unwrap();
        let otter = Otter::new();
        otter
            .blocking_run_file(dir.path().join("entry.ts"))
            .unwrap();
    }

    #[test]
    fn module_program_import_meta_url_matches_canonical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "function fail() { return undefined.x; }\nlet u = import.meta.url;\nif (u.indexOf(\"file://\") !== 0) fail();\nif (u.indexOf(\"entry.ts\") < 0) fail();\n",
        )
        .unwrap();
        let otter = Otter::new();
        otter
            .blocking_run_file(dir.path().join("entry.ts"))
            .unwrap();
    }

    #[test]
    fn otter_runs_empty_typescript() {
        let otter = Otter::new();
        let result = otter.blocking_run_typescript("").unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn otter_runs_undefined_literal() {
        let otter = Otter::new();
        let result = otter.blocking_run_typescript("undefined;").unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn json_cyclic_surfaces_jsc_style_diagnostic() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("const a = {}; a.self = a; JSON.stringify(a);")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "JSON_CYCLIC");
                assert_eq!(
                    diagnostic.message,
                    "JSON.stringify cannot serialize cyclic structures."
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn json_parse_error_carries_byte_position() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("JSON.parse(\"[1, 2,]\");")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "JSON_PARSE");
                assert!(
                    diagnostic
                        .message
                        .starts_with("JSON Parse error: trailing comma")
                );
                assert!(diagnostic.message.contains("at byte 6"));
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn json_bigint_emits_jsc_style_message() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("JSON.stringify({ n: 1n });")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "JSON_BIGINT");
                assert_eq!(
                    diagnostic.message,
                    "JSON.stringify cannot serialize BigInt values."
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn otter_rejects_unsupported_js_feature() {
        // `with` is permanently outside the foundation subset, so
        // it makes a stable canary for the FEATURE_NOT_IN_SLICE
        // diagnostic shape. (`try`/`catch` shipped in task 24.)
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("with (o) { x; }")
            .unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].code, "FEATURE_NOT_IN_SLICE");
            }
            other => panic!("expected Compile, got {other:?}"),
        }
    }

    #[test]
    fn otter_rejects_typescript_enum() {
        // `enum` is intentionally rejected by the frontend policy.
        let otter = Otter::new();
        let err = otter.blocking_run_typescript("enum E { A }").unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].code, "TS_UNSUPPORTED");
            }
            other => panic!("expected Compile, got {other:?}"),
        }
    }

    #[test]
    fn otter_erases_interface() {
        let otter = Otter::new();
        let result = otter
            .blocking_run_typescript("interface I { x: number; } undefined;")
            .unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn run_file_rejects_unknown_extension() {
        let otter = Otter::new();
        let err = otter.blocking_run_file("/nonexistent.foo").unwrap_err();
        assert!(matches!(err, OtterError::SourceKind { .. }));
    }

    #[test]
    fn json_error_carries_schema_version() {
        let err = OtterError::SourceKind {
            path: PathBuf::from("nope.foo"),
            extension: "foo".to_string(),
        };
        let json = err.to_json().unwrap();
        assert!(json.contains("\"error_schema_version\":1"));
        assert!(json.contains("\"kind\":\"source_kind\""));
    }
}
