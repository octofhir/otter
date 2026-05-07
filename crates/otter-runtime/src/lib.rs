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
//! # See also
//! - [Engine architecture](../../../docs/book/src/engine/architecture.md)
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

pub mod error;
pub mod event_loop;
pub mod handle;
pub mod module_graph;
pub mod module_loader;
pub mod structured_clone;
pub mod surface;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use otter_bytecode::BytecodeModule;
use otter_compiler::{
    compile_parsed_program as compile_script_program, compile_source as compile_script_source,
};
use otter_gc::GcStats;
use otter_syntax::{SourceKind, detect_source_kind, with_program};
use otter_vm::{Interpreter, InterruptFlag, JsObject};
use serde::{Deserialize, Serialize};

pub use error::{ConfigError, IoErrorKind, OtterError, error_schema_version};
pub use event_loop::{
    EventLoop, HostFuture, HostJoinHandle, HostOpCompletion, RuntimeLiveness, RuntimeWake,
    TimerRequest, TimerToken, TokioEventLoop,
};
pub use handle::{RuntimeActivityStats, RuntimeHandle};
pub use otter_vm::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
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
    /// Wall-clock duration.
    pub duration: Duration,
}

impl ExecutionResult {
    /// Build from an interpreter completion value.
    #[must_use]
    fn from_vm_value(completion: otter_vm::Value, duration: Duration) -> Self {
        Self {
            completion: completion.display_string(),
            duration,
        }
    }

    /// Render the completion value for CLI preview output.
    #[must_use]
    pub fn completion_string(&self) -> &str {
        &self.completion
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
/// | `hrtime` | `Deny` | High-resolution time can be a side-channel and should be explicit. |
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
    /// High-resolution time permission (boolean toggle).
    pub hrtime: BooleanPermission,
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
            hrtime: BooleanPermission::Deny,
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
            hrtime: BooleanPermission::Allow,
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
        assert!(!caps.hrtime.is_allowed());
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

/// Boolean permission (no patterns; on / off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BooleanPermission {
    /// Operation is denied.
    #[default]
    Deny,
    /// Operation is allowed.
    Allow,
}

impl BooleanPermission {
    /// Convert to [`bool`].
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
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
        }
    }
}

impl RuntimeConfig {
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
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
                    code: "GLOBAL_CLASS_BOOTSTRAP".to_string(),
                    message: err.to_string(),
                })?;
        }
        // §19.4.1 / §20.2.1.1 — wire the eval hook so `eval(src)` /
        // `new Function(...)` reach a real parse + compile path.
        // The closure is reusable across calls; each invocation
        // builds a fresh `BytecodeModule`.
        let hook: otter_vm::EvalHook = std::rc::Rc::new(|source: &str| {
            compile_script_source(source, SourceKind::JavaScript, "<eval>")
                .map_err(|e| format!("compile error: {e:?}"))
        });
        interp.set_eval_hook(Some(hook));
        Ok(Runtime { interp, config })
    }
}

/// Layer B isolate.
#[derive(Debug)]
pub struct Runtime {
    interp: Interpreter,
    config: RuntimeConfig,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MicrotaskStats {
    pub(crate) pending: bool,
    pub(crate) generation: u64,
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
        let module = self.compile_source(&source, specifier)?;
        self.run_compiled_script_since(module, start)
    }

    fn run_compiled_script_since(
        &mut self,
        module: BytecodeModule,
        start: std::time::Instant,
    ) -> Result<ExecutionResult, OtterError> {
        // Run the script first; the script error wins if both the
        // script and the drain fail. On script success we still
        // drain so any `queueMicrotask` registered during script
        // execution gets a chance to run before we report success.
        let script_outcome = self.interp.run(&module);
        let drain_outcome = self.interp.drain_microtasks(&module);
        let value = match (script_outcome, drain_outcome) {
            (Err(script_err), _) => return Err(map_vm_error(script_err)),
            (Ok(_), Err(drain_err)) => return Err(map_vm_error(drain_err)),
            (Ok(v), Ok(())) => v,
        };
        Ok(ExecutionResult::from_vm_value(value, start.elapsed()))
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
        // Empty module so the drain has a `BytecodeModule` to look
        // up function bodies in. Microtasks always reference
        // functions defined in the original module they were
        // queued from — this entry point is for embedders who
        // already have that module on hand; for now we surface
        // it as a no-op when the queue is empty.
        if !self.interp.microtasks().has_any_pending() {
            return Ok(());
        }
        // Without a module we cannot resolve function ids; the
        // foundation contract is that callers use the auto-drain
        // path. Document this loudly.
        Err(OtterError::Internal {
            code: "MICROTASK_DRAIN_NEEDS_MODULE".to_string(),
            message: "manual microtask draining requires the originating module; use run_script which auto-drains".to_string(),
        })
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
    pub fn dump(&self, source: SourceInput, specifier: &str) -> Result<BytecodeModule, OtterError> {
        self.compile_source(&source, specifier)
    }

    fn compile_source(
        &self,
        source: &SourceInput,
        specifier: &str,
    ) -> Result<BytecodeModule, OtterError> {
        compile_script_source(&source.text, source.kind, specifier).map_err(map_compile_error)
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
        let loader = match &self.config.loader {
            Some(cfg) => {
                let mut cfg = cfg.clone();
                cfg.hosted_specifiers.extend(
                    self.config
                        .hosted_modules
                        .iter()
                        .map(|m| m.specifier().to_string()),
                );
                module_loader::ModuleLoader::with_config(cfg)
            }
            None => {
                let base_dir = entry_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let mut cfg = module_loader::LoaderConfig::new(base_dir);
                cfg.hosted_specifiers = self
                    .config
                    .hosted_modules
                    .iter()
                    .map(|m| m.specifier().to_string())
                    .collect();
                module_loader::ModuleLoader::with_config(cfg)
            }
        };
        let linked =
            module_graph::load_program(&loader, entry_path).map_err(|e| OtterError::Compile {
                diagnostics: vec![Diagnostic::syntax(e.to_string())],
            })?;

        // Pre-populate the per-run module-env registry. Every
        // module URL gets a fresh JsObject; <entry> resolves via
        // self-loop ImportNamespace edges and stores into the
        // env as the body runs.
        self.interp.reset_module_state();
        let mut module = linked.module;
        let entry_url = linked.entry_url.clone();
        for init in &module.module_inits {
            let env = if let Some(hosted) = self
                .config
                .hosted_modules
                .iter()
                .find(|hosted| hosted.specifier() == init.url)
            {
                hosted
                    .install(&mut self.interp, &self.config.capabilities)
                    .map_err(|message| OtterError::Config {
                        reason: ConfigError::ConflictingCapabilities { message },
                    })?
            } else {
                otter_vm::object::alloc_object(self.interp.gc_heap_mut())?
            };
            self.interp
                .register_module_env(std::rc::Rc::from(init.url.as_str()), env);
            // Self-loop edge: <entry>'s referrer is the entry's URL
            // (the synthesized <entry> function carries empty
            // module_url, so the dispatcher uses an empty string;
            // we add edges keyed on both shapes).
            module
                .module_resolutions
                .push(otter_bytecode::ModuleResolution {
                    referrer: entry_url.clone(),
                    specifier: init.url.clone(),
                    target: init.url.clone(),
                });
            module
                .module_resolutions
                .push(otter_bytecode::ModuleResolution {
                    referrer: String::new(),
                    specifier: init.url.clone(),
                    target: init.url.clone(),
                });
        }

        let script_outcome = self.interp.run(&module);
        let drain_outcome = self.interp.drain_microtasks(&module);
        let value = match (script_outcome, drain_outcome) {
            (Err(script_err), _) => return Err(map_vm_error(script_err)),
            (Ok(_), Err(drain_err)) => return Err(map_vm_error(drain_err)),
            (Ok(v), Ok(())) => v,
        };
        Ok(ExecutionResult::from_vm_value(value, start.elapsed()))
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
        let specifier = path.to_string_lossy().to_string();
        if !source_path_has_script_extension(path) {
            let start = std::time::Instant::now();
            let module = with_program(&source.text, source.kind, |program| {
                if program_looks_like_module(program) {
                    return Ok(None);
                }
                compile_script_program(program, source.kind, &specifier)
                    .map(Some)
                    .map_err(map_compile_error)
            })
            .map_err(|e| {
                map_compile_error(otter_compiler::CompileError::Syntax {
                    messages: e.messages,
                })
            })??;
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

    /// Override the implementation behind `console.*`.
    #[must_use]
    pub fn console_sink(mut self, sink: ConsoleSinkHandle) -> Self {
        self.runtime = self.runtime.console_sink(sink);
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

/// Stable diagnostic shape (foundation subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Machine-readable kind.
    pub kind: DiagnosticKind,
    /// Stable code (`TS_UNSUPPORTED`, `OOM_HEAP_LIMIT`, …).
    pub code: String,
    /// Human-readable summary.
    pub message: String,
    /// Optional source span.
    pub span: Option<(u32, u32)>,
    /// Stack frames when relevant.
    #[serde(default)]
    pub frames: Vec<StackFrame>,
    /// Optional cause chain.
    #[serde(default)]
    pub cause: Option<Box<Diagnostic>>,
}

impl Diagnostic {
    /// Construct a syntax-class diagnostic.
    #[must_use]
    pub fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "SYNTAX_ERROR".to_string(),
            message: message.into(),
            span: None,
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Construct a TS-unsupported diagnostic.
    #[must_use]
    pub fn ts_unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "TS_UNSUPPORTED".to_string(),
            message: message.into(),
            span: Some(span),
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Construct a generic "feature not in this slice" diagnostic.
    #[must_use]
    pub fn unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "FEATURE_NOT_IN_SLICE".to_string(),
            message: message.into(),
            span: Some(span),
            frames: Vec::new(),
            cause: None,
        }
    }
}

/// Diagnostic category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// Syntax / TypeScript erasure / compile-time.
    Syntax,
    /// `TypeError`.
    Type,
    /// `ReferenceError`.
    Reference,
    /// `RangeError`.
    Range,
    /// Heap cap hit.
    OutOfMemory,
    /// Timeout fired.
    Timeout,
    /// Capability denied.
    Capability,
    /// Internal bug.
    Internal,
}

/// Single stack frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StackFrame {
    /// Function name; `"<main>"` for the script entry.
    pub function: String,
    /// Module specifier.
    pub module: String,
    /// Source span within `module`.
    pub span: Option<(u32, u32)>,
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

fn map_compile_error(err: otter_compiler::CompileError) -> OtterError {
    use otter_compiler::CompileError;
    match err {
        CompileError::Syntax { messages } => OtterError::Compile {
            diagnostics: vec![Diagnostic::syntax(messages.join("; "))],
        },
        CompileError::Unsupported { node, span } => OtterError::Compile {
            diagnostics: vec![Diagnostic::unsupported(
                format!("unsupported AST node: {node}"),
                span,
            )],
        },
        CompileError::TypeScriptUnsupported { node, span } => OtterError::Compile {
            diagnostics: vec![Diagnostic::ts_unsupported(
                format!("typescript {node} is not supported in foundation"),
                span,
            )],
        },
        _ => OtterError::Internal {
            code: "COMPILE_UNKNOWN".to_string(),
            message: "unknown compiler error variant".to_string(),
        },
    }
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
        |kind: DiagnosticKind, code: &str, message: String| OtterError::Runtime {
            diagnostic: Diagnostic {
                kind,
                code: code.to_string(),
                message,
                span: top_span,
                frames: stack_frames.clone(),
                cause: None,
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
        VmError::TypeMismatch => runtime_diagnostic(DiagnosticKind::Type, "TYPE_MISMATCH", display),
        VmError::TypeError { message } => {
            runtime_diagnostic(DiagnosticKind::Type, "TYPE_ERROR", message)
        }
        VmError::UnknownIntrinsic { name } => runtime_diagnostic(
            DiagnosticKind::Type,
            "UNKNOWN_METHOD",
            format!("unknown method `{name}`"),
        ),
        VmError::TemporalDeadZone { local_index } => runtime_diagnostic(
            DiagnosticKind::Reference,
            "TDZ",
            format!("cannot access local {local_index} before initialization"),
        ),
        VmError::StackOverflow { limit } => runtime_diagnostic(
            DiagnosticKind::Range,
            "STACK_OVERFLOW",
            format!("maximum call stack size exceeded (limit {limit})"),
        ),
        VmError::NotCallable => runtime_diagnostic(
            DiagnosticKind::Type,
            "NOT_CALLABLE",
            "value is not a function".to_string(),
        ),
        VmError::Uncaught { value } => runtime_diagnostic(
            DiagnosticKind::Type,
            "UNCAUGHT",
            format!("uncaught exception: {value}"),
        ),
        VmError::JsonError { code, message } => {
            // `code` is `&'static str` so we can pass it straight
            // through; it stays stable for telemetry/log filters.
            runtime_diagnostic(DiagnosticKind::Type, code, message)
        }
        VmError::InvalidRegExp { message } => {
            runtime_diagnostic(DiagnosticKind::Syntax, "INVALID_REGEXP", message)
        }
        VmError::MissingReturn | VmError::InvalidOperand => OtterError::Internal {
            code: "VM_BYTECODE_INVARIANT".to_string(),
            message: display,
        },
        _ => OtterError::Internal {
            code: "VM_UNKNOWN".to_string(),
            message: display,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn public_handle_types_are_send_sync() {
        assert_send_sync::<Otter>();
        assert_send_sync::<RuntimeHandle>();
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
    async fn runtime_handle_host_ops_timers_and_diagnostics_update_stats() {
        let otter = Otter::new();
        let handle = otter.handle().clone();

        handle.spawn_host_op(
            RuntimeLiveness::Ref,
            Box::pin(async {
                HostOpCompletion {
                    id: 0,
                    kind: "test-op".to_string(),
                    result: Ok("done".to_string()),
                }
            }),
        );
        let token = handle.schedule_timer(TimerRequest {
            delay: Duration::from_millis(10),
            repeat: None,
            liveness: RuntimeLiveness::Ref,
            origin: "runtime-test".to_string(),
        });
        assert!(!handle.cancel_timer(TimerToken(token.0 + 1)));
        handle.complete_dynamic_module_job_for_tests();
        handle.wake_runtime("runtime-test");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.activity_stats();

        assert_eq!(stats.pending_ref_host_ops, 0);
        assert_eq!(stats.completed_host_ops, 1);
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
            liveness: RuntimeLiveness::Ref,
            origin: "runtime-test-cancel".to_string(),
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

    #[test]
    fn module_program_detects_cycle() {
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
        let err = otter
            .blocking_run_file(dir.path().join("a.ts"))
            .unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert!(
                    diagnostics
                        .iter()
                        .any(|d| d.message.contains("cycle") || d.message.contains("RangeError")),
                    "expected cycle diagnostic, got {diagnostics:?}"
                );
            }
            other => panic!("expected Compile (cycle), got {other:?}"),
        }
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
