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

mod commonjs;
pub use commonjs::run_builtin_cjs_shim;
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
pub mod module_scope;
mod package_graph_resolver;
mod process;
mod process_env;
mod process_events;
mod process_flags;
pub mod promise_registry;
mod runtime_activity;
pub mod structured_clone;
pub mod surface;
pub mod web_fetch_host;
pub mod web_structured_clone;
pub mod worker;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use otter_bytecode::{BytecodeModule, SpanEntry};
use otter_compiler::{
    compile_script_program, compile_script_source, compile_script_source_to_module,
    compile_script_source_with_top_level_await,
};
use otter_gc::GcStats;
use otter_syntax::{SourceKind, SyntaxDiagnostic, SyntaxError, detect_source_kind, with_program};
use otter_vm::{EvalCompileOptions, ExecutionContext, Interpreter, InterruptFlag};
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
pub use otter_vm::CpuProfile;
pub use otter_vm::{
    AccessorSpec, Attr, ConstSpec, ConstValue, ConstructorSpec, JsObject, JsSurfaceError,
    MethodSpec, NativeCall, ObjectBuilder, Value, array, bootstrap, intrinsic_install, object,
    rooting,
};
// Unrenamed re-exports consumed by `#[js_class]`- and `couch!`-generated
// glue, which must resolve the same `::otter_vm::…` paths whether they
// expand inside `otter-vm` itself or in a binding crate that aliases
// this crate as `otter_vm` (the established linking convention).
pub use otter_vm::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
pub use otter_vm::{
    ExecutionContext as RuntimeExecutionContext, PersistentRootId as RuntimePersistentRootId,
};
pub use otter_vm::{
    JitRuntimeStats, RuntimeBudget, RuntimeBudgetExceededAction, RuntimeBudgetStats,
};
pub use otter_vm::{
    NamespaceBuilder, NamespaceSpec, NativeCtx, NativeError, marshal, string, symbol,
};
pub use promise_registry::{HostSettleOutcome, PromiseId};
pub use runtime_activity::{RuntimeKeepAlive, RuntimeTask, RuntimeTaskSpawner};
pub use structured_clone::{
    StructuredCloneError, StructuredCloneMapEntry, StructuredCloneNumber, StructuredCloneOptions,
    StructuredCloneProperty, StructuredCloneTransfer, StructuredCloneTransferId,
    StructuredCloneTransferKind, StructuredCloneTransferList, StructuredCloneTransferListError,
    StructuredCloneValue,
};
pub use surface::{
    RuntimeAccessorSpec, RuntimeAttr, RuntimeClassSpec, RuntimeConstSpec, RuntimeConstValue,
    RuntimeConstructorSpec, RuntimeHostObjectData, RuntimeHostObjectError, RuntimeJsObject,
    RuntimeJsString, RuntimeLocal, RuntimeMethodSpec, RuntimeNamespaceSpec, RuntimeNativeCall,
    RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeFastFn, RuntimeNativeFn, RuntimeNativeScope,
    RuntimeNumberValue, RuntimeObjectBuilder, RuntimePropertySpec, RuntimeSurfaceError,
    RuntimeValue, runtime_accessor, runtime_alloc_object, runtime_arg_to_string,
    runtime_array_from_elements, runtime_class, runtime_constant, runtime_constructor,
    runtime_getter, runtime_method, runtime_method_with_attrs, runtime_namespace,
    runtime_native_dynamic, runtime_native_static, runtime_optional_arg_to_string,
    runtime_property, runtime_set_property, runtime_string_value, runtime_this_object,
    runtime_type_error, runtime_with_host_data, runtime_with_host_data_mut,
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
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
}

impl<'rt> HostedModuleCtx<'rt> {
    fn new(
        interp: &'rt mut Interpreter,
        capabilities: &'rt CapabilitySet,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            builder: RuntimeObjectBuilder::new_in_interpreter(interp)?,
            capabilities,
            runtime_task_spawner,
        })
    }

    /// Return the configured capability set for this runtime.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilitySet {
        self.capabilities
    }

    /// Return the runtime event-loop task spawner, when this runtime has one.
    #[must_use]
    pub fn runtime_task_spawner(&self) -> Option<RuntimeTaskSpawner> {
        self.runtime_task_spawner.clone()
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
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<JsObject, String> {
        let mut ctx = HostedModuleCtx::new(interp, capabilities, runtime_task_spawner)
            .map_err(|err| format!("out of memory: {err}"))?;
        (self.raw)(&mut ctx)?;
        Ok(ctx.build())
    }
}

/// Produces the full CommonJS export value for a hosted module — used when the
/// export must be a callable (for example `assert`, invoked directly as
/// `assert(cond)` as well as via `assert.strictEqual`). The object-namespace
/// [`HostedModuleInstall`] path cannot represent a callable, so a builtin that
/// needs one supplies this instead; `require()` returns its result verbatim.
pub type HostedModuleValueInstall =
    for<'rt> fn(&mut otter_vm::NativeCtx<'rt>, &CapabilitySet) -> Result<otter_vm::Value, String>;

/// Load one CommonJS native addon (`.node`) after module resolution and the
/// filesystem capability check have completed.
///
/// The Node compatibility product installs this hook; the core runtime keeps
/// native code loading opt-in and has no dependency on a particular addon ABI.
/// Implementations must enforce the separate `ffi` capability before opening
/// the library. The optional task spawner is the owned, sendable route for
/// addon worker callbacks to re-enter a handle-backed isolate.
pub type CommonJsAddonLoader = for<'rt> fn(
    &mut otter_vm::NativeCtx<'rt>,
    &Path,
    &CapabilitySet,
    Option<RuntimeTaskSpawner>,
) -> Result<otter_vm::Value, otter_vm::NativeError>;

/// One runtime-hosted module.
#[derive(Debug, Clone, Copy)]
pub struct HostedModule {
    /// Module specifier, for example `otter:kv`.
    specifier: &'static str,
    /// Namespace installer (object export). Also used as the ESM module env.
    install: HostedModuleInstall,
    /// Optional callable/value export used by `require()` in place of the
    /// object namespace. `None` => `require()` returns the namespace object.
    cjs_value: Option<HostedModuleValueInstall>,
}

impl HostedModule {
    /// Create a hosted module spec from an opaque runtime installer.
    #[must_use]
    pub const fn new(specifier: &'static str, install: HostedModuleInstall) -> Self {
        Self {
            specifier,
            install,
            cjs_value: None,
        }
    }

    /// Create a hosted module whose CommonJS export is a value (e.g. a
    /// callable). The `install` namespace is still used for ESM imports.
    #[must_use]
    pub const fn new_with_cjs_value(
        specifier: &'static str,
        install: HostedModuleInstall,
        cjs_value: HostedModuleValueInstall,
    ) -> Self {
        Self {
            specifier,
            install,
            cjs_value: Some(cjs_value),
        }
    }

    /// Module specifier, for example `otter:kv`.
    #[must_use]
    pub const fn specifier(self) -> &'static str {
        self.specifier
    }

    /// The optional CommonJS value-export installer, if any.
    #[must_use]
    pub(crate) const fn cjs_value(self) -> Option<HostedModuleValueInstall> {
        self.cjs_value
    }

    fn install(
        self,
        interp: &mut Interpreter,
        capabilities: &CapabilitySet,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<JsObject, String> {
        self.install
            .install(interp, capabilities, runtime_task_spawner)
    }
}

/// Cloneable callback that installs embedder globals on every runtime isolate.
///
/// Runtime workers clone [`RuntimeConfig`], so installers registered on a
/// parent runtime also run inside child worker isolates before user code
/// starts. Installers must copy owned data into the runtime and must not expose
/// isolate-local VM handles across runtime boundaries.
#[derive(Clone)]
pub struct RuntimeGlobalInstaller {
    install: Arc<dyn Fn(&mut Runtime) -> Result<(), OtterError> + Send + Sync>,
}

impl RuntimeGlobalInstaller {
    /// Build a global installer from a sendable callback.
    #[must_use]
    pub fn new(
        install: impl Fn(&mut Runtime) -> Result<(), OtterError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            install: Arc::new(install),
        }
    }

    fn install(&self, runtime: &mut Runtime) -> Result<(), OtterError> {
        (self.install)(runtime)
    }
}

impl std::fmt::Debug for RuntimeGlobalInstaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeGlobalInstaller")
            .finish_non_exhaustive()
    }
}

/// Runtime-owned class-shaped global surface.
///
/// Product crates expose this opaque handle to embedders instead of exposing VM
/// class specs directly.
#[derive(Debug, Clone, Copy)]
pub struct GlobalClass {
    inner: GlobalClassInner,
}

#[derive(Debug, Clone, Copy)]
enum GlobalClassInner {
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

/// One JS source attached to an [`Extension`], with the global names
/// it defines (`js_defines` in the declaration). The names feed the
/// native lazy-accessor registration; a drift between `defines` and
/// the source's actual definitions is caught by the extension's
/// def-scan test.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionJs {
    /// The JS source, evaluated in global scope on first touch of any
    /// defined name.
    pub source: &'static str,
    /// Global names the source defines.
    pub defines: &'static [&'static str],
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
    /// JS sources with their defined names. All sources of an
    /// extension form one lazy group: first touch of any defined name
    /// evaluates every source, in declaration order.
    pub js: &'static [ExtensionJs],
}

impl Extension {
    /// Every global name the JS half defines, in declaration order.
    pub fn lazy_names(&self) -> impl Iterator<Item = &'static str> + '_ {
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

/// Default heap cap (2 GiB) when none is configured.
pub const DEFAULT_MAX_HEAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
    /// Allow top-level `await` in this source even though it
    /// compiles through the classic-script pipeline. Embedder
    /// snippet APIs (`Otter::run_typescript` and friends) opt in so
    /// REPL-style strings keep module-grade `await`; spec-faithful
    /// script execution (e.g. the test262 runner) leaves this off
    /// and gets the §16.1 Script-goal early error.
    pub allow_top_level_await: bool,
}

impl SourceInput {
    /// Build a JavaScript source bundle from in-memory text.
    #[must_use]
    pub fn from_javascript(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SourceKind::JavaScript,
            path: None,
            allow_top_level_await: false,
        }
    }

    /// Permit top-level `await` in a classic-script source. See
    /// [`Self::allow_top_level_await`].
    #[must_use]
    pub fn with_top_level_await(mut self) -> Self {
        self.allow_top_level_await = true;
        self
    }

    /// Build a TypeScript source bundle from in-memory text.
    #[must_use]
    pub fn from_typescript(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SourceKind::TypeScript,
            path: None,
            allow_top_level_await: false,
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
            allow_top_level_await: false,
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
    /// Diagnostic counter snapshot sampled after the run completed.
    stats: Box<RuntimeExecutionStats>,
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
            stats: Box::default(),
        }
    }

    /// Build from a host-visible runtime exit request.
    #[must_use]
    fn from_exit_code(code: u8, duration: Duration) -> Self {
        Self {
            completion: "undefined".to_string(),
            exit_code: code,
            duration,
            stats: Box::default(),
        }
    }

    fn with_stats(mut self, stats: RuntimeExecutionStats) -> Self {
        self.stats = Box::new(stats);
        self
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

    /// Diagnostic counter snapshot sampled after the run completed.
    #[must_use]
    pub fn stats(&self) -> RuntimeExecutionStats {
        *self.stats
    }
}

/// Machine-readable diagnostic counter snapshot sampled at the end of a run.
///
/// These counters reflect the current `Runtime` state at sampling time. A reused
/// runtime reports cumulative counters unless the caller resets the relevant VM
/// budget counters between runs.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeExecutionStats {
    /// `LoadProperty` IC fast-path hits.
    pub property_load_hits: u64,
    /// `LoadProperty` IC misses or absent entries.
    pub property_load_misses: u64,
    /// `LoadProperty` IC installs.
    pub property_load_installs: u64,
    /// `LoadProperty` IC disables / megamorphic transitions.
    pub property_load_disables: u64,
    /// `StoreProperty` IC fast-path hits.
    pub property_store_hits: u64,
    /// `StoreProperty` IC misses or absent entries.
    pub property_store_misses: u64,
    /// `StoreProperty` IC installs.
    pub property_store_installs: u64,
    /// `StoreProperty` IC disables / megamorphic transitions.
    pub property_store_disables: u64,
    /// `HasProperty` IC fast-path hits.
    pub property_has_hits: u64,
    /// `HasProperty` IC misses or absent entries.
    pub property_has_misses: u64,
    /// `HasProperty` IC installs.
    pub property_has_installs: u64,
    /// `HasProperty` IC disables / megamorphic transitions.
    pub property_has_disables: u64,
    /// Runtime reduction units executed.
    pub reductions_executed: u64,
    /// Bytecode calls observed by runtime budget stats.
    pub bytecode_calls: u64,
    /// Native calls observed by runtime budget stats.
    pub native_calls: u64,
    /// Construct calls observed by runtime budget stats.
    pub construct_calls: u64,
    /// Max frame-stack depth observed.
    pub max_stack_depth_observed: u32,
    /// Bytes allocated in the largest observed root turn.
    pub max_turn_allocated_bytes: u64,
    /// Longest observed root turn, in nanoseconds.
    pub max_turn_nanos: u64,
    /// Compiled `Op::Call` bridge invocations.
    pub jit_runtime_calls: u64,
    /// Compiled-to-compiled fast-call hits.
    pub jit_direct_calls: u64,
    /// Compiled calls that fell back to the Rust callable path.
    pub jit_rust_call_fallbacks: u64,
    /// Optimizing-tier function and OSR entries.
    pub jit_optimized_entries: u64,
    /// Optimizing-tier entries materialized at a hot loop header.
    pub jit_optimized_osr_entries: u64,
    /// Optimizing-tier deopts resumed on reconstructed interpreter frames.
    pub jit_optimized_deopts: u64,
    /// Function-entry compile attempts across native tiers.
    pub jit_compile_attempts: u64,
    /// Loop-OSR threshold attempts.
    pub jit_osr_attempts: u64,
    /// JIT property/method/element/global/upvalue runtime stub calls.
    pub jit_runtime_property_stubs: u64,
    /// JIT method-call runtime stub calls.
    pub jit_runtime_method_stubs: u64,
    /// JIT method-call runtime stubs reached from baseline dynasm.
    pub jit_runtime_method_baseline_stubs: u64,
    /// JIT method-call runtime stubs reached from optimizing dynasm.
    pub jit_runtime_method_optimizing_stubs: u64,
    /// Narrow collection-IC method bridge calls from compiled code.
    pub jit_runtime_collection_method_ic_stubs: u64,
    /// ABI-classified runtime stub transitions from compiled code.
    pub jit_runtime_stub_transitions: u64,
    /// ABI-classified leaf runtime stubs.
    pub jit_leaf_stub_transitions: u64,
    /// ABI-classified allocating runtime stubs.
    pub jit_alloc_stub_transitions: u64,
    /// ABI-classified re-entrant runtime stubs.
    pub jit_reentrant_stub_transitions: u64,
    /// Executed `AllocValueStub` entries that returned `Ok`.
    pub jit_alloc_value_stub_ok: u64,
    /// Executed `AllocValueStub` entries that returned `Miss`.
    pub jit_alloc_value_stub_miss: u64,
    /// Executed `AllocValueStub` entries that returned `OutOfMemory`.
    pub jit_alloc_value_stub_out_of_memory: u64,
    /// Executed `AllocValueStub` entries that returned another non-`Ok` status.
    pub jit_alloc_value_stub_other: u64,
    /// JIT method bridge calls served by a live collection method IC.
    pub jit_method_collection_ic_hits: u64,
    /// JIT method bridge calls served by collection prototype fast paths.
    pub jit_method_fast_collection_hits: u64,
    /// JIT method bridge calls served by array fast paths.
    pub jit_method_array_fast_hits: u64,
    /// JIT method bridge calls served by primitive string fast paths.
    pub jit_method_string_fast_hits: u64,
    /// JIT method bridge calls served by primitive number fast paths.
    pub jit_method_number_fast_hits: u64,
    /// JIT method bridge calls that reached generic callable dispatch.
    pub jit_method_generic_calls: u64,
    /// VM-published collection method IC mirror slots.
    pub jit_collection_method_ic_slots: u64,
    /// Empty collection method IC mirror slots.
    pub jit_collection_method_ic_empty_slots: u64,
    /// Live collection method IC mirror slots.
    pub jit_collection_method_ic_collection_slots: u64,
    /// Live collection method IC slots with leaf/no-allocation stubs.
    pub jit_collection_method_ic_leaf_stub_slots: u64,
    /// Live collection method IC slots with allocating stubs.
    pub jit_collection_method_ic_alloc_stub_slots: u64,
    /// Total GC-cell bytes allocated since heap creation.
    pub gc_alloc_bytes_total: u64,
    /// Live heap objects after the last stats reconciliation.
    pub gc_live_objects: usize,
    /// Live heap bytes after the last stats reconciliation.
    pub gc_live_bytes: usize,
    /// Full GC cycles executed.
    pub gc_cycles: u64,
    /// Most recent full-GC pause in milliseconds.
    pub gc_last_pause_ms: f32,
    /// Cumulative full-GC pause time, in nanoseconds.
    pub gc_full_pause_ns_total: u64,
    /// Minor (young-gen scavenge) cycles executed.
    pub gc_minor_cycles: u64,
    /// Cumulative minor-GC pause time, in nanoseconds.
    pub gc_minor_pause_ns_total: u64,
    /// Cumulative remembered-set entries scanned across all minor GCs.
    pub gc_minor_dirty_cards_scanned: u64,
    /// Cumulative old-space headers strided to re-derive edge owners; zero
    /// once the parents are held directly (proves no per-page header walk).
    pub gc_minor_old_headers_walked: u64,
    /// Cumulative remembered parents re-traced across all minor GCs.
    pub gc_minor_objects_retraced: u64,
    /// Cumulative slots visited re-tracing remembered parents.
    pub gc_minor_slots_scanned: u64,
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
        if env_name_is_builtin_denied(name) {
            return false;
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

fn env_name_is_builtin_denied(name: &str) -> bool {
    ENV_BUILTIN_DENY_PATTERNS
        .iter()
        .any(|pattern| glob_match_string(pattern, name))
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

#[cfg(test)]
mod host_global_tests {
    use super::*;

    #[test]
    fn builder_can_disable_product_host_globals() {
        let mut runtime = Runtime::builder()
            .process_global(false)
            .worker_global(false)
            .build()
            .expect("runtime");
        let completion = runtime
            .run_script(
                SourceInput::from_javascript("typeof process + ':' + typeof Worker;"),
                "<host-global-config>",
            )
            .expect("script")
            .completion_string()
            .to_string();
        assert_eq!(completion, "undefined:undefined");
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

/// Re-export of the VM's step-trace interfaces. Embedders that wire
/// a [`TracerFactory`] through [`RuntimeBuilder`] / [`OtterBuilder`]
/// build their tracers against these types.
pub use otter_vm::inspect;

/// Factory for the per-instruction step tracer.
///
/// The runtime executes the factory once on the isolate runner
/// thread immediately after the [`Interpreter`] is constructed.
/// Returning a tracer installs it through
/// [`Interpreter::set_tracer`]; the dispatch loop then routes every
/// instruction through [`inspect::StepTracer::on_step`]. The factory
/// is `Send + Sync` because the spawned isolate is on a dedicated
/// thread; the tracer itself never needs to cross threads.
#[derive(Clone)]
pub struct TracerFactory(Arc<dyn Fn() -> Box<dyn otter_vm::inspect::StepTracer> + Send + Sync>);

impl TracerFactory {
    /// Wrap a closure that produces a fresh tracer on demand.
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn() -> Box<dyn otter_vm::inspect::StepTracer> + Send + Sync + 'static,
    {
        Self(Arc::new(factory))
    }

    fn build(&self) -> Box<dyn otter_vm::inspect::StepTracer> {
        (self.0)()
    }
}

impl std::fmt::Debug for TracerFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracerFactory").finish_non_exhaustive()
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
    /// Enable Node-style CommonJS module execution (`require` / `module.exports`
    /// / `__dirname`) for script-shaped sources. Off by default; opt in via
    /// [`RuntimeBuilder::with_nodejs_modules`].
    commonjs_enabled: bool,
    commonjs_addon_loader: Option<CommonJsAddonLoader>,
    global_classes: Vec<GlobalClass>,
    extensions: Vec<&'static Extension>,
    global_installers: Vec<RuntimeGlobalInstaller>,
    allow_blocking_atomics_wait: bool,
    install_process_global: bool,
    install_worker_global: bool,
    console_sink: ConsoleSinkHandle,
    hooks: RuntimeHooks,
    process_argv: Vec<String>,
    process_cwd: PathBuf,
    tracer_factory: Option<TracerFactory>,
    jit_selection: JitSelection,
    jit_osr_threshold: Option<u32>,
}

/// Which execution tiers a runtime installs at construction.
///
/// A structured embedder/harness selection: differential tests build one
/// runtime per tier and compare observable behavior against the
/// interpreter-only semantic oracle. The default installs the production
/// compiler; there is no environment toggle for tier selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitSelection {
    /// Interpreter plus the production optimizing and template tiers.
    #[default]
    Baseline,
    /// Explicit production-tier selection; equivalent to the default.
    Template,
    /// Interpreter only — the semantic oracle for differential runs.
    InterpreterOnly,
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

    fn load_program_profiled(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry_path: &Path,
    ) -> Result<
        (
            module_graph::LinkedProgram,
            module_graph::ModulePhaseTimings,
        ),
        module_graph::GraphError,
    > {
        let (linked, timings) = module_graph::load_program_profiled(loader, entry_path)?;
        self.last_entry_url = Some(linked.entry_url.clone());
        self.last_module_count = linked.module.module_inits.len();
        Ok((linked, timings))
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
            commonjs_enabled: false,
            commonjs_addon_loader: None,
            global_classes: Vec::new(),
            extensions: Vec::new(),
            global_installers: Vec::new(),
            allow_blocking_atomics_wait: false,
            install_process_global: true,
            install_worker_global: true,
            console_sink: otter_vm::console::default_console_sink(),
            hooks: RuntimeHooks::default(),
            process_argv: process::default_argv(),
            process_cwd: process::default_cwd(),
            tracer_factory: None,
            jit_selection: JitSelection::default(),
            jit_osr_threshold: None,
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

/// Map a CommonJS loader error into the runtime error type, rendering the
/// carried context once at this outermost boundary (a thrown JS value is
/// preserved intact through nested `require`s and only stringified here).
fn commonjs_native_to_error(err: otter_vm::NativeError) -> OtterError {
    let message = match err {
        otter_vm::NativeError::Thrown { message, .. } => message,
        otter_vm::NativeError::TypeError { reason, .. }
        | otter_vm::NativeError::RangeError { reason, .. }
        | otter_vm::NativeError::SyntaxError { reason, .. }
        | otter_vm::NativeError::ReferenceError { reason, .. }
        | otter_vm::NativeError::URIError { reason, .. } => reason,
        other => other.to_string(),
    };
    OtterError::Internal {
        code: "COMMONJS_LOAD".to_string(),
        message,
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

    /// Enable Node-style CommonJS module execution (`require`,
    /// `module.exports`, `exports`, `__dirname`, `__filename`) for
    /// script-shaped sources (`.cjs`, CommonJS-typed `.js`, and ambiguous
    /// non-module `.js`). Builtin `require('node:fs')` / `require('fs')` resolve
    /// through the registered hosted modules.
    #[must_use]
    pub fn with_nodejs_modules(mut self) -> Self {
        self.config.commonjs_enabled = true;
        self
    }

    /// Register the native-addon loader used for CommonJS `.node` files.
    ///
    /// This does not grant filesystem or FFI access. The runtime checks `read`
    /// during module resolution and the loader must independently check `ffi`.
    #[must_use]
    pub fn commonjs_addon_loader(mut self, loader: CommonJsAddonLoader) -> Self {
        self.config.commonjs_addon_loader = Some(loader);
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

    /// Register a declared extension: its native classes install
    /// eagerly (with their attached JS glue), and its JS half
    /// registers as one native lazy-global group.
    #[must_use]
    pub fn extension(mut self, extension: &'static Extension) -> Self {
        self.config.extensions.push(extension);
        self
    }

    /// Register a callback that installs embedder globals on this runtime and
    /// on worker runtimes spawned from it.
    #[must_use]
    pub fn global_installer(mut self, installer: RuntimeGlobalInstaller) -> Self {
        self.config.global_installers.push(installer);
        self
    }

    /// Allow blocking `Atomics.wait` on the main runtime.
    ///
    /// The default is `false`, so direct embedders cannot accidentally park
    /// their host thread forever. Enable this only when the embedder has a
    /// watchdog or another reliable cancellation path.
    #[must_use]
    pub fn allow_blocking_atomics_wait(mut self, allow: bool) -> Self {
        self.config.allow_blocking_atomics_wait = allow;
        self
    }

    /// Install the product `process` global. Enabled by default for CLI and
    /// embedding compatibility; spec harnesses can disable it to keep the
    /// global object closer to an engine shell.
    #[must_use]
    pub fn process_global(mut self, install: bool) -> Self {
        self.config.install_process_global = install;
        self
    }

    /// Install Worker-related globals. Enabled by default for product runtimes;
    /// spec harnesses may disable it once their `$262.agent` host is decoupled
    /// from the runtime Worker backend.
    #[must_use]
    pub fn worker_global(mut self, install: bool) -> Self {
        self.config.install_worker_global = install;
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

    /// Install a per-instruction step-trace factory. The factory
    /// runs once on the isolate runner thread immediately after the
    /// interpreter is constructed; its produced tracer routes every
    /// dispatched instruction through
    /// [`inspect::StepTracer::on_step`]. Pass `None` (the default)
    /// to skip tracing — the dispatch loop then pays only a single
    /// `Option` check per instruction.
    #[must_use]
    pub fn tracer_factory(mut self, factory: Option<TracerFactory>) -> Self {
        self.config.tracer_factory = factory;
        self
    }

    /// Select which execution tiers the runtime installs. Differential
    /// harnesses build one runtime per [`JitSelection`] and compare
    /// observable behavior; production embedders keep the default.
    #[must_use]
    pub fn jit_selection(mut self, selection: JitSelection) -> Self {
        self.config.jit_selection = selection;
        self
    }

    /// Override the back-edge count at which a hot loop tiers up via OSR.
    /// Differential and conformance harnesses set `1` to force compiled loop
    /// coverage; production embedders keep the default.
    #[must_use]
    pub fn jit_osr_threshold(mut self, threshold: u32) -> Self {
        self.config.jit_osr_threshold = Some(threshold);
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
        // Hosted-module lookups take the first specifier match, so a
        // duplicate registration would silently shadow the later installer.
        // Surface the conflict at build time instead.
        let mut seen = std::collections::HashSet::new();
        for module in &config.hosted_modules {
            if !seen.insert(module.specifier()) {
                return Err(OtterError::HostedModule {
                    specifier: module.specifier().to_string(),
                    message: "registered more than once".to_string(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn from_config(config: RuntimeConfig) -> Result<Self, OtterError> {
        Self::from_config_with_task_spawner(config, None)
    }

    pub(crate) fn from_config_with_task_spawner(
        config: RuntimeConfig,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<Self, OtterError> {
        Self::validate_config(&config)?;
        let module_loader = RuntimeModuleLoaderState::new(config.loader.clone());
        let package_manager =
            RuntimePackageManagerHandle::from_loader_config(config.loader.as_ref());
        // The interpreter owns the per-isolate GC heap (since
        // task 76); both the string heap and the GC heap honour
        // the configured cap.
        let mut interp = Interpreter::with_string_heap_cap(config.max_heap_bytes);
        interp.set_max_stack_depth(config.max_stack_depth);
        interp.set_allow_blocking_atomics_wait(config.allow_blocking_atomics_wait);
        interp.set_console_sink(config.console_sink.clone());
        let layer_a_dynamic_imports = LayerADynamicImportQueue::default();
        // Attached class glue and extension JS are deferred until the
        // runtime is fully assembled: the sources reference globals
        // installed later in this build, and evaluation needs the
        // compile pipeline. Both are evaluated at a top-level frame so
        // every value they build is reachable from a scanned register —
        // a native-nested `eval` (the former lazy-global getter) strands
        // objects allocated mid-evaluation under GC pressure.
        let (pending_class_js, pending_extension_js) =
            interp.with_runtime_roots(
                |interp| -> Result<
                    (Vec<(&'static str, &'static str)>, Vec<(String, String)>),
                    OtterError,
                > {
                    let mut pending_class_js: Vec<(&'static str, &'static str)> = Vec::new();
                    let mut pending_extension_js: Vec<(String, String)> = Vec::new();
                    for spec in &config.global_classes {
                        match spec.inner {
                            GlobalClassInner::Spec(raw) => {
                                interp.install_global_class(raw).map_err(|err| {
                                    OtterError::Internal {
                                        code: DiagnosticCode::GlobalClassBootstrap
                                            .as_str()
                                            .to_string(),
                                        message: err.to_string(),
                                    }
                                })?;
                            }
                            GlobalClassInner::Intrinsic {
                                install,
                                install_well_knowns,
                                js_glue,
                                name,
                            } => {
                                if let Some(source) = js_glue {
                                    pending_class_js.push((name, source));
                                }
                                let global = *interp.global_this();
                                install(interp.gc_heap_mut(), global).map_err(|err| {
                                    OtterError::Internal {
                                        code: DiagnosticCode::GlobalClassBootstrap
                                            .as_str()
                                            .to_string(),
                                        message: err.to_string(),
                                    }
                                })?;
                                // Second phase, mirroring the bootstrap registry walk:
                                // symbol-keyed members (@@toStringTag) resolve against
                                // the realm's already-materialized well-known table.
                                interp
                                    .run_install_well_knowns(install_well_knowns, global)
                                    .map_err(|err| OtterError::Internal {
                                        code: DiagnosticCode::GlobalClassBootstrap
                                            .as_str()
                                            .to_string(),
                                        message: err.to_string(),
                                    })?;
                            }
                        }
                    }
                    // Declared extensions: native classes ride the identical
                    // install path (declaration order, parents before subclasses),
                    // then every JS source of the extension registers as one lazy
                    // group under its declared names.
                    let extensions = config.extensions.clone();
                    for extension in extensions {
                        for spec in extension.classes {
                            match spec.inner {
                                GlobalClassInner::Spec(raw) => {
                                    interp.install_global_class(raw).map_err(|err| {
                                        OtterError::Internal {
                                            code: DiagnosticCode::GlobalClassBootstrap
                                                .as_str()
                                                .to_string(),
                                            message: err.to_string(),
                                        }
                                    })?;
                                }
                                GlobalClassInner::Intrinsic {
                                    install,
                                    install_well_knowns,
                                    js_glue,
                                    name,
                                } => {
                                    if let Some(source) = js_glue {
                                        pending_class_js.push((name, source));
                                    }
                                    let global = *interp.global_this();
                                    install(interp.gc_heap_mut(), global).map_err(|err| {
                                        OtterError::Internal {
                                            code: DiagnosticCode::GlobalClassBootstrap
                                                .as_str()
                                                .to_string(),
                                            message: err.to_string(),
                                        }
                                    })?;
                                    interp
                                        .run_install_well_knowns(install_well_knowns, global)
                                        .map_err(|err| OtterError::Internal {
                                            code: DiagnosticCode::GlobalClassBootstrap
                                                .as_str()
                                                .to_string(),
                                            message: err.to_string(),
                                        })?;
                                }
                            }
                        }
                        if !extension.js.is_empty() {
                            let mut source = String::new();
                            for entry in extension.js {
                                source.push_str(entry.source);
                                // Guard against a source omitting its own
                                // statement terminator.
                                source.push_str("\n;\n");
                            }
                            pending_extension_js.push((extension.name.to_string(), source));
                        }
                    }
                    if config.install_process_global {
                        process::install_global(
                            &mut *interp,
                            &config.process_argv,
                            &config.process_cwd,
                            &config.capabilities,
                            &config.hooks,
                        )?;
                    }
                    // §19.4.1 / §20.2.1.1 — wire the eval hook so `eval(src)` /
                    // `new Function(...)` reach a real parse + compile path.
                    // The closure is reusable across calls; each invocation
                    // builds a fresh `BytecodeModule`.
                    let hook: otter_vm::EvalHook =
                        std::sync::Arc::new(|source: &str, options: EvalCompileOptions| {
                            // §16.1.6 ScriptEvaluation — host-requested script
                            // execution ($262.evalScript) compiles under script
                            // GDI semantics, not eval semantics.
                            if options.script_goal {
                                return otter_compiler::compile_script_source(
                                    source,
                                    SourceKind::JavaScript,
                                    "<evalScript>",
                                )
                                .map_err(|e| format!("compile error: {e:?}"));
                            }
                            // §19.2.1.3 — a direct eval inside a function carries
                            // its caller variable environment binding list.
                            let caller_scope: Option<Vec<otter_compiler::EvalCallerBinding>> =
                                options.caller_scope.map(|bindings| {
                                    bindings
                                        .into_iter()
                                        .map(|binding| otter_compiler::EvalCallerBinding {
                                            name: binding.name,
                                            lexical: binding.lexical,
                                            captured: binding.captured,
                                            is_const: binding.is_const,
                                            fn_self_name: binding.fn_self_name,
                                        })
                                        .collect()
                                });
                            otter_compiler::compile_eval_source(
                                source,
                                SourceKind::JavaScript,
                                "<eval>",
                                options.force_strict,
                                options.forbid_var_arguments,
                                caller_scope.as_deref(),
                                options.new_target_allowed,
                                options.in_class_field_initializer,
                                options.super_property_allowed,
                            )
                            .map_err(|e| format!("compile error: {e:?}"))
                        });
                    interp.set_eval_hook(Some(hook));
                    // The JIT is the default execution path: functions start in the
                    // interpreter and tier up through the configured compiler.
                    // `OTTER_JIT=0` drops to interpreter-only (debugging escape hatch).
                    if let Some(threshold) = config.jit_osr_threshold {
                        interp.set_jit_osr_threshold(threshold);
                    }
                    if std::env::var("OTTER_JIT").map_or(true, |v| v != "0") {
                        match config.jit_selection {
                            JitSelection::Baseline | JitSelection::Template => {
                                interp.set_jit_compiler(Some(std::sync::Arc::new(
                                    otter_jit::BaselineJitCompiler::new(),
                                )));
                            }
                            JitSelection::InterpreterOnly => {}
                        }
                    }
                    if let Some(factory) = &config.tracer_factory {
                        interp.set_tracer(Some(factory.build()));
                    }
                    interp.set_dynamic_import_loader(std::sync::Arc::new(
                        LayerADynamicImportLoader {
                            queue: layer_a_dynamic_imports.clone(),
                        },
                    ));
                    Ok((pending_class_js, pending_extension_js))
                },
            )?;
        let mut runtime = Runtime {
            interp,
            config,
            module_loader,
            module_graph: RuntimeModuleGraphState::default(),
            module_records: module_records::RuntimeModuleRecords::default(),
            source_maps: RuntimeSourceMapTable::default(),
            diagnostics: RuntimeDiagnosticsSink::default(),
            package_manager,
            layer_a_dynamic_imports,
            promise_registry: promise_registry::PromiseRegistry::new(),
            runtime_task_spawner,
            remote_module_fetch: None,
        };
        if runtime.config.install_worker_global {
            worker::install_main_worker_globals(&mut runtime)?;
        }
        let global_installers = runtime.config.global_installers.clone();
        for installer in global_installers {
            installer.install(&mut runtime)?;
        }
        // Extension JS (Web platform shims, etc.) installs its globals
        // eagerly, in declaration order, after the native function
        // installers above so it can reference `self`/`structuredClone`
        // and before the class glue below so that glue sees the real
        // globals. Each source is a self-contained installer IIFE that
        // attaches to `globalThis`; evaluating here — at a top-level
        // frame — keeps every object it allocates rooted through GC.
        for (name, source) in pending_extension_js {
            runtime
                .eval(SourceInput::from_javascript(source))
                .map_err(|err| OtterError::Internal {
                    code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                    message: format!("extension `{name}` globals failed: {err}"),
                })?;
        }
        // Co-located class glue: the JS half of a declared class
        // installs in the same build as its native half, in class
        // declaration order, after every installer above — so the glue
        // can reference any global (including lazy ones) safely.
        for (name, source) in pending_class_js {
            runtime
                .eval(SourceInput::from_javascript(source))
                .map_err(|err| OtterError::Internal {
                    code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                    message: format!("class `{name}` attached JS glue failed: {err}"),
                })?;
        }
        Ok(runtime)
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
    /// Direct-mode (Layer A) dynamic-import requests. The default
    /// loader installed at construction queues `import()` calls whose
    /// target the linked graph cannot resolve; `run_script` /
    /// `run_module` pump the queue through [`Self::begin_dynamic_import`]
    /// between microtask drains. The isolate runner replaces the
    /// loader at spawn, so this queue stays empty under Layer B.
    layer_a_dynamic_imports: LayerADynamicImportQueue,
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
    /// Sender for owned tasks that must run on the isolate event loop.
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    /// Blocking remote-module fetch hook, wired by the isolate runner from the
    /// event loop's HTTP client. Enables static http/https imports (the
    /// Deno-style remote graph); `None` in embedders without remote loading.
    remote_module_fetch: Option<Arc<dyn module_loader::RemoteModuleFetch>>,
}

pub(crate) enum MessageEventDispatchError {
    Materialize(OtterError),
    Handler(OtterError),
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

/// Shared FIFO of `(token, specifier, referrer)` dynamic-import
/// requests awaiting the Layer A pump.
type LayerADynamicImportQueue =
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<(u64, String, String)>>>;

/// Default [`otter_vm::DynamicImportLoader`] for direct-mode runtimes:
/// queues the request for [`Runtime::pump_layer_a_dynamic_imports`].
/// Replaced by the isolate runner's inbox-backed loader under Layer B.
struct LayerADynamicImportLoader {
    queue: LayerADynamicImportQueue,
}

impl otter_vm::DynamicImportLoader for LayerADynamicImportLoader {
    fn schedule(&self, token: u64, specifier: String, referrer: String) {
        self.queue
            .lock()
            .expect("layer-a dynamic import queue poisoned")
            .push_back((token, specifier, referrer));
    }
}

enum DynamicModuleLoad {
    Loaded(otter_vm::Value),
    /// The loaded graph evaluates async (top-level await somewhere in
    /// the target's subtree) — the import must not settle until the
    /// target's per-record evaluation gate does (§16.2.1.9).
    PendingAsyncEvaluation {
        promise: otter_vm::JsPromiseHandle,
        target_url: String,
        context: ExecutionContext,
    },
    FetchHttps {
        target_url: String,
    },
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

    /// Clone the runtime event-loop task spawner, if this runtime has one.
    #[must_use]
    pub fn runtime_task_spawner(&self) -> Option<RuntimeTaskSpawner> {
        self.runtime_task_spawner.clone()
    }

    pub(crate) fn set_allow_blocking_atomics_wait(&mut self, allow: bool) {
        self.interp.set_allow_blocking_atomics_wait(allow);
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

    /// Install the host completion sink backing async native methods.
    /// Wired by the isolate runner alongside the timer scheduler;
    /// direct-mode embedders never call this, and their async native
    /// methods reject with a TypeError unless the future is
    /// immediately ready.
    pub fn install_host_completion_sink(
        &mut self,
        sink: std::sync::Arc<dyn otter_vm::host_completion::HostCompletionSink>,
    ) {
        self.interp.set_host_completion_sink(sink);
    }

    /// Run a host completion job posted by an async native method's
    /// future. Executed on the isolate thread by the runner's inbox.
    pub(crate) fn run_host_completion(
        &mut self,
        job: otter_vm::host_completion::HostCompletionJob,
    ) {
        job.run(&mut self.interp);
    }

    /// Install the host-side dynamic-import scheduler. Wired by
    /// the isolate runner so `Op::ImportNamespaceDynamic` can
    /// reach the loader through the runtime inbox.
    pub fn install_dynamic_import_loader(&mut self, loader: otter_vm::DynamicImportLoaderHandle) {
        self.interp.set_dynamic_import_loader(loader);
    }

    /// Install the blocking remote-module fetch hook so static http/https
    /// imports load through the synchronous module graph (Deno-style). Wired
    /// by the isolate runner from the event loop's HTTP client.
    pub fn install_remote_module_fetch(
        &mut self,
        fetch: Arc<dyn module_loader::RemoteModuleFetch>,
    ) {
        self.remote_module_fetch = Some(fetch);
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
            Ok(DynamicModuleLoad::PendingAsyncEvaluation {
                promise,
                target_url,
                context,
            }) => {
                self.interp
                    .settle_dynamic_import_on_async_inits(
                        &context,
                        token,
                        vec![promise],
                        std::sync::Arc::from(target_url.as_str()),
                    )
                    .map_err(|err| {
                        map_vm_error(otter_vm::RunError {
                            error: err,
                            frames: Vec::new(),
                            detail: None,
                        })
                    })?;
                // Drive the parked top-level-await frames to
                // completion; their settlement reactions settle the
                // import token, and the same drain delivers the
                // import promise's own reactions.
                if let Err(err) = self.interp.drain_microtasks_with_default(Some(context)) {
                    return Err(enrich_runtime_diagnostic_with_cause(
                        &mut self.interp,
                        map_vm_error(err),
                    ));
                }
                Ok(DynamicImportBegin::Settled)
            }
            Ok(DynamicModuleLoad::FetchHttps { target_url }) => {
                Ok(DynamicImportBegin::FetchHttps { target_url })
            }
            Err(DynLoadError::Diagnostic { kind, message }) => self
                .alloc_dynamic_import_error(kind, message)
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
            .map_err(DynLoadError::type_error)
            .and_then(|source| self.evaluate_dynamic_module_https_source(target_url, source))
        {
            Ok(namespace) => Ok(namespace),
            Err(DynLoadError::Diagnostic { kind, message }) => {
                Err(self.alloc_dynamic_import_error(kind, message)?)
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
        kind: otter_vm::ErrorKind,
        message: String,
    ) -> Result<otter_vm::Value, OtterError> {
        let proto = self.interp.error_classes_for_trace().prototype(kind);
        let proto_root = otter_vm::Value::object(proto);
        let mut obj = self
            .interp
            .alloc_host_object_with_roots(&[&proto_root], &[])?;
        otter_vm::object::set_prototype(obj, self.interp.gc_heap_mut(), Some(proto));
        let message_str = otter_vm::JsString::from_str(&message, self.interp.gc_heap_mut())
            .map_err(|err| OtterError::Internal {
                code: DiagnosticCode::StringAlloc.as_str().to_string(),
                message: err.to_string(),
            })?;
        otter_vm::object::set(
            &mut obj,
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
                DynLoadError::type_error(format!(
                    "dynamic import: referrer is not a file:// URL: \"{url}\""
                ))
            })?,
            None => std::env::current_dir().map_err(|e| {
                DynLoadError::type_error(format!("dynamic import: cwd lookup failed: {e}"))
            })?,
        };
        let loader = self.module_loader_for_entry(&entry_for_loader);
        let target_url = loader.resolve(specifier, referrer_opt).map_err(|e| {
            DynLoadError::type_error(format!(
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
            DynLoadError::type_error(format!(
                "dynamic import: target is not a file:// URL: \"{target_url}\""
            ))
        })?;
        let linked = self
            .module_graph
            .load_program(&loader, &target_path)
            .map_err(|e| {
                DynLoadError::from_graph_error(
                    &e,
                    format!("dynamic import: load failed for \"{target_url}\": {e:?}"),
                )
            })?;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        self.register_resolved_exports(&linked.metadata);
        self.register_module_sources(&linked.module_sources);
        let context = self.interp.link_module(linked.module);
        for init in context.module_inits() {
            if self.interp.module_env(&init.url).is_some() {
                continue;
            }
            let env = self
                .interp
                .alloc_host_object_with_roots(&[], &[])
                .map_err(|e| {
                    DynLoadError::type_error(format!("dynamic import: alloc env failed: {e}"))
                })?;
            self.interp
                .register_module_env(std::sync::Arc::from(init.url.as_str()), env);
        }
        // §13.3.10 step 7 — Evaluate(target): the records-backed
        // InnerModuleEvaluation walks the target's eager dependency
        // closure, parking on top-level await instead of blocking.
        match self.interp.evaluate_module(&context, &target_url) {
            Ok(Some(promise)) => {
                return Ok(DynamicModuleLoad::PendingAsyncEvaluation {
                    promise,
                    target_url,
                    context,
                });
            }
            Ok(None) => {}
            Err(err) => {
                // §16.2.1.7 step 7.b.i — an evaluation throw maps
                // to a promise rejection. Prefer the original
                // thrown Value (preserved on
                // `pending_uncaught_throw` whenever the throw
                // walked the empty stack inside the dispatch
                // sub-loop) so `.catch` observes the spec-correct
                // payload, not a stringified `VmError::Uncaught`
                // rendering.
                if matches!(err, otter_vm::VmError::Uncaught)
                    && let Some(thrown) = self.interp.take_pending_uncaught_throw()
                {
                    return Err(DynLoadError::Thrown(thrown));
                }
                return Err(DynLoadError::type_error(format!(
                    "dynamic import: evaluation failed for \"{target_url}\": {err}"
                )));
            }
        }
        let namespace = self.interp.module_env(&target_url).ok_or_else(|| {
            DynLoadError::type_error(format!(
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
            DynLoadError::syntax_error(format!(
                "dynamic import: parse failed for \"{target_url}\": {e:?}"
            ))
        })?
        .map_err(|e| {
            DynLoadError::from_compile_error(
                &e,
                format!("dynamic import: compile failed for \"{target_url}\": {e:?}"),
            )
        })?;
        if fragment_has_import_namespace_ops(&fragment) {
            return Err(DynLoadError::type_error(format!(
                "dynamic import: HTTPS module \"{target_url}\" has own static imports — not yet supported"
            )));
        }
        let context = self.interp.link_module(fragment);
        let env = self
            .interp
            .alloc_host_object_with_roots(&[], &[])
            .map_err(|e| {
                DynLoadError::type_error(format!("dynamic import: alloc env failed: {e}"))
            })?;
        self.interp
            .register_module_env(std::sync::Arc::from(target_url), env);
        otter_vm_init_marker_install(&mut self.interp, env);
        let import_meta = alloc_dynamic_import_meta(&mut self.interp, env, target_url)?;
        let callee = otter_vm::Value::function_id(context.main().id);
        let args: smallvec::SmallVec<[otter_vm::Value; 8]> = smallvec::smallvec![
            otter_vm::Value::object(env),
            otter_vm::Value::object(import_meta),
        ];
        if let Err(err) =
            self.interp
                .run_callable_sync(&context, &callee, otter_vm::Value::undefined(), args)
        {
            if matches!(err, otter_vm::VmError::Uncaught)
                && let Some(thrown) = self.interp.take_pending_uncaught_throw()
            {
                return Err(DynLoadError::Thrown(thrown));
            }
            return Err(DynLoadError::type_error(format!(
                "dynamic import: HTTPS evaluation failed for \"{target_url}\": {err}"
            )));
        }
        // `run_callable_sync` may have moved `env` (the local handle is not in
        // any root set). Re-read the relocated handle from the GC-traced module
        // env registry rather than returning the stale local.
        let env = self.interp.module_env(target_url).unwrap_or(env);
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

    /// Define `value[Symbol.toStringTag]` as a non-enumerable host tag.
    ///
    /// Host integrations use this for Node/Web globals whose observable brand
    /// differs from a plain object while still being ordinary host-owned
    /// objects in the VM.
    pub fn define_to_string_tag(
        &mut self,
        value: otter_vm::Value,
        tag: &str,
    ) -> Result<(), OtterError> {
        let Some(obj) = value.as_object() else {
            return Ok(());
        };
        let tag_value = otter_vm::JsString::from_str(tag, self.interp.gc_heap_mut())
            .map(otter_vm::Value::string)
            .map_err(string_oom_to_error)?;
        let tag_sym = self
            .interp
            .well_known_symbols()
            .get(otter_vm::symbol::WellKnown::ToStringTag);
        otter_vm::object::define_own_symbol_property_partial(
            obj,
            self.interp.gc_heap_mut(),
            tag_sym,
            otter_vm::object::PartialPropertyDescriptor {
                value: Some(tag_value),
                writable: Some(false),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
        Ok(())
    }

    pub(crate) fn global_this_value(&self) -> otter_vm::Value {
        otter_vm::Value::object(*self.interp.global_this())
    }

    /// The `globalThis` object as a value. Used by hosts that expose an alias
    /// global (e.g. Node's `global`).
    #[must_use]
    pub fn global_this(&self) -> otter_vm::Value {
        self.global_this_value()
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

    /// Install a host-defined native function or closure as a global binding.
    ///
    /// This is the captured-state counterpart to [`Self::install_native_global`].
    /// Product crates use it when a global native needs immutable runtime-owned
    /// captures such as a capability snapshot. Captured JS values must still be
    /// supplied through the native-function capture list; this helper only
    /// exposes the call target shape, not arbitrary untraced VM handles.
    ///
    /// # Errors
    /// Returns [`OtterError::OutOfMemory`] when the heap cap blocks the native
    /// function allocation.
    pub fn install_native_global_call(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeCall,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_function_from_call_host_rooted(name, length, call, &[], &[])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.interp.set_global(name, value);
        Ok(())
    }

    pub(crate) fn install_native_constructor_global_call(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeCall,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_constructor_from_call_host_rooted(name, length, call, &[], &[])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.interp.set_global(name, value);
        Ok(())
    }

    pub(crate) fn dispatch_worker_message_event<F>(
        &mut self,
        context: &ExecutionContext,
        materialize_data: F,
    ) -> Result<(), MessageEventDispatchError>
    where
        F: FnOnce(&mut otter_vm::NativeCtx<'_>) -> Result<otter_vm::Value, otter_vm::NativeError>,
    {
        let global = *self.interp.global_this();
        let global_value = otter_vm::Value::object(global);
        otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global_value),
            Some(context),
            |ctx| {
                let global = ctx
                    .this_value()
                    .as_object()
                    .expect("worker event receiver is globalThis");
                let handler = otter_vm::object::get(global, ctx.heap(), "onmessage")
                    .unwrap_or_else(otter_vm::Value::undefined);
                if !handler.is_callable() {
                    return Ok(());
                }

                // The handler must survive payload materialization, which can
                // move the young generation before the call starts.
                let handler_root = ctx.persistent_root_insert(handler);
                let data = match materialize_data(ctx) {
                    Ok(data) => data,
                    Err(err) => {
                        ctx.persistent_root_remove(handler_root);
                        return Err(MessageEventDispatchError::Materialize(map_native_error(
                            err,
                        )));
                    }
                };
                let data_root = ctx.persistent_root_insert(data);
                let handler = ctx
                    .persistent_root_get(handler_root)
                    .expect("fresh worker handler root");
                let data = ctx
                    .persistent_root_get(data_root)
                    .expect("fresh worker message data root");

                let dispatch = ctx.scope(|mut scope| {
                    let global = scope.this();
                    let handler = scope.value(handler);
                    let data = scope.value(data);
                    let event = scope
                        .object()
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    let ty = scope
                        .string("message")
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    scope
                        .set(event, "type", ty)
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    scope
                        .set(event, "data", data)
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    scope
                        .call(handler, global, &[event])
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Handler)?;
                    Ok(())
                });

                ctx.persistent_root_remove(data_root);
                ctx.persistent_root_remove(handler_root);
                dispatch
            },
        )?;
        self.interp.drain_microtasks(context).map_err(|err| {
            MessageEventDispatchError::Handler(enrich_runtime_diagnostic_with_cause(
                &mut self.interp,
                map_vm_error(err),
            ))
        })
    }

    /// Run one host event on the isolate thread through a fresh native context.
    ///
    /// The closure must materialize any JS values from owned host data while it
    /// is executing. VM values must not be stored in the task that calls this
    /// method; use persistent root ids for long-lived references.
    ///
    /// # Errors
    /// Returns mapped runtime errors for native failures or microtask drain
    /// failures.
    pub fn run_native_event<F>(
        &mut self,
        context: &ExecutionContext,
        run: F,
    ) -> Result<(), OtterError>
    where
        F: FnOnce(&mut otter_vm::NativeCtx<'_>) -> Result<otter_vm::Value, otter_vm::NativeError>,
    {
        let global = *self.interp.global_this();
        let global_value = otter_vm::Value::object(global);
        let result_root = otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global_value),
            Some(context),
            |ctx| {
                run(ctx)
                    .map(|value| ctx.persistent_root_insert(value))
                    .map_err(map_native_error)
            },
        )?;
        let drain = self.interp.drain_microtasks(context).map_err(|err| {
            enrich_runtime_diagnostic_with_cause(&mut self.interp, map_vm_error(err))
        });
        self.interp.persistent_root_remove(result_root);
        drain?;
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
                    detail: None,
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

    /// Snapshot the compact end-of-run diagnostics used by `OTTER_STATS=1`.
    pub fn execution_stats(&mut self) -> RuntimeExecutionStats {
        let ic = self.interp.property_ic_stats();
        let budget = self.interp.runtime_budget_stats();
        let jit = self.interp.jit_runtime_stats();
        let collection_method_ics = self.interp.jit_collection_method_ic_stats();
        let gc = self.interp.gc_heap_mut().gc_stats().clone();
        RuntimeExecutionStats {
            property_load_hits: ic.load_hits,
            property_load_misses: ic.load_misses,
            property_load_installs: ic.load_installs,
            property_load_disables: ic.load_disables,
            property_store_hits: ic.store_hits,
            property_store_misses: ic.store_misses,
            property_store_installs: ic.store_installs,
            property_store_disables: ic.store_disables,
            property_has_hits: ic.has_hits,
            property_has_misses: ic.has_misses,
            property_has_installs: ic.has_installs,
            property_has_disables: ic.has_disables,
            reductions_executed: budget.reductions_executed,
            bytecode_calls: budget.bytecode_calls,
            native_calls: budget.native_calls,
            construct_calls: budget.construct_calls,
            max_stack_depth_observed: budget.max_stack_depth_observed,
            max_turn_allocated_bytes: budget.max_turn_allocated_bytes,
            max_turn_nanos: budget.max_turn_nanos,
            jit_runtime_calls: jit.runtime_calls,
            jit_direct_calls: jit.direct_calls,
            jit_rust_call_fallbacks: jit.rust_call_fallbacks,
            jit_optimized_entries: jit.optimized_entries,
            jit_optimized_osr_entries: jit.optimized_osr_entries,
            jit_optimized_deopts: jit.optimized_deopts,
            jit_compile_attempts: jit.compile_attempts,
            jit_osr_attempts: jit.osr_attempts,
            jit_runtime_property_stubs: jit.runtime_property_stubs,
            jit_runtime_method_stubs: jit.runtime_method_stubs,
            jit_runtime_method_baseline_stubs: jit.runtime_method_baseline_stubs,
            jit_runtime_method_optimizing_stubs: jit.runtime_method_optimizing_stubs,
            jit_runtime_collection_method_ic_stubs: jit.runtime_collection_method_ic_stubs,
            jit_runtime_stub_transitions: jit.runtime_stub_transitions,
            jit_leaf_stub_transitions: jit.leaf_stub_transitions,
            jit_alloc_stub_transitions: jit.alloc_stub_transitions,
            jit_reentrant_stub_transitions: jit.reentrant_stub_transitions,
            jit_alloc_value_stub_ok: jit.alloc_value_stub_ok,
            jit_alloc_value_stub_miss: jit.alloc_value_stub_miss,
            jit_alloc_value_stub_out_of_memory: jit.alloc_value_stub_out_of_memory,
            jit_alloc_value_stub_other: jit.alloc_value_stub_other,
            jit_method_collection_ic_hits: jit.method_collection_ic_hits,
            jit_method_fast_collection_hits: jit.method_fast_collection_hits,
            jit_method_array_fast_hits: jit.method_array_fast_hits,
            jit_method_string_fast_hits: jit.method_string_fast_hits,
            jit_method_number_fast_hits: jit.method_number_fast_hits,
            jit_method_generic_calls: jit.method_generic_calls,
            jit_collection_method_ic_slots: collection_method_ics.slots,
            jit_collection_method_ic_empty_slots: collection_method_ics.empty_slots,
            jit_collection_method_ic_collection_slots: collection_method_ics.collection_slots,
            jit_collection_method_ic_leaf_stub_slots: collection_method_ics.leaf_stub_slots,
            jit_collection_method_ic_alloc_stub_slots: collection_method_ics.alloc_stub_slots,
            gc_alloc_bytes_total: gc.alloc_bytes_total,
            gc_live_objects: gc.live_objects,
            gc_live_bytes: gc.live_bytes,
            gc_cycles: gc.gc_cycles,
            gc_last_pause_ms: gc.last_gc_pause_ms,
            gc_full_pause_ns_total: gc.full_pause_ns_total,
            gc_minor_cycles: gc.minor_gc_cycles,
            gc_minor_pause_ns_total: gc.minor_pause_ns_total,
            gc_minor_dirty_cards_scanned: gc.minor_dirty_cards_scanned,
            gc_minor_old_headers_walked: gc.minor_old_headers_walked,
            gc_minor_objects_retraced: gc.minor_objects_retraced,
            gc_minor_slots_scanned: gc.minor_slots_scanned,
        }
    }

    /// Snapshot executable code retained by this runtime's JIT caches.
    ///
    /// This is an explicit diagnostics/benchmark query and performs no
    /// accounting on ordinary execution paths.
    #[must_use]
    pub fn jit_code_residency(&self) -> otter_vm::JitCodeResidency {
        self.interp.jit_code_residency()
    }

    fn attach_execution_stats(&mut self, result: ExecutionResult) -> ExecutionResult {
        let stats = self.execution_stats();
        result.with_stats(stats)
    }

    /// Reset VM runtime budget/resource counters.
    pub fn reset_runtime_budget_stats(&mut self) {
        self.interp.reset_runtime_budget_stats();
    }

    /// Snapshot every property inline-cache site. See
    /// [`inspect::IcSiteSnapshot`].
    #[must_use]
    pub fn ic_snapshot(&self) -> Vec<inspect::IcSiteSnapshot> {
        self.interp.ic_snapshot()
    }

    /// Snapshot the active hidden-class transition tree. See
    /// [`inspect::ShapeTransitionSnapshot`].
    #[must_use]
    pub fn shape_transition_snapshot(&self) -> inspect::ShapeTransitionSnapshot {
        self.interp.shape_transition_snapshot()
    }

    /// Install (or clear) the shape-transition observer used by
    /// shape-transition breakpoints. See
    /// [`inspect::ShapeTransitionObserver`].
    pub fn set_shape_transition_observer(
        &mut self,
        observer: Option<Box<dyn inspect::ShapeTransitionObserver>>,
    ) {
        self.interp.set_shape_transition_observer(observer);
    }

    /// Install (or clear) the per-instruction step tracer. See
    /// [`inspect::StepTracer`].
    pub fn set_tracer(&mut self, tracer: Option<Box<dyn inspect::StepTracer>>) {
        self.interp.set_tracer(tracer);
    }

    /// Enable VM stack sampling for CPU-profile artifacts.
    pub fn enable_cpu_profiler(&mut self, interval: u64) {
        self.interp.enable_cpu_profiler(interval);
    }

    /// Disable VM stack sampling without returning collected samples.
    pub fn disable_cpu_profiler(&mut self) {
        self.interp.disable_cpu_profiler();
    }

    /// Take and clear the current VM stack CPU profile.
    #[must_use]
    pub fn take_cpu_profile(&mut self) -> Option<otter_vm::CpuProfile> {
        self.interp.take_cpu_profile()
    }

    /// Type-count summary of every live GC body. See
    /// [`inspect::HeapSnapshotSummary`].
    #[must_use]
    pub fn heap_snapshot_summary(&self) -> inspect::HeapSnapshotSummary {
        self.interp.heap_snapshot_summary()
    }

    /// Write a Chrome DevTools `.heapsnapshot` for the current heap
    /// state. The output is JSON; the DevTools "Memory" panel
    /// accepts it as-is.
    ///
    /// # Errors
    /// Propagates I/O errors from `writer`.
    pub fn write_chrome_heap_snapshot<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.interp.write_chrome_heap_snapshot(writer)
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
    pub fn force_gc(&mut self) -> Result<(), OtterError> {
        self.interp.force_gc().map_err(Into::into)
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
        hooks::check_capability_with_hooks(
            &self.config.hooks,
            &self.config.capabilities,
            capability,
            request,
        )
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
        self.run_script_with_context(source, specifier)
            .map(|(result, _)| result)
    }

    fn run_script_with_context(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let start = std::time::Instant::now();
        let compiled = self.compile_source(&source, specifier)?;
        self.run_compiled_script_with_context_since(compiled.bytecode, start)
    }

    fn run_compiled_script_with_context_since(
        &mut self,
        module: BytecodeModule,
        start: std::time::Instant,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let context = self.interp.link_module(module);
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
                let result = ExecutionResult::from_exit_code(code, start.elapsed());
                return Ok((self.attach_execution_stats(result), context));
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
        self.pump_layer_a_dynamic_imports(&context)?;
        let result =
            ExecutionResult::from_vm_value(value, start.elapsed(), self.interp.gc_heap_mut())
                .with_exit_code(process::exit_code(&self.interp));
        let result = self.attach_execution_stats(result);
        Ok((result, context))
    }

    /// Drive direct-mode dynamic imports to completion: pop every
    /// queued `import()` request, load + evaluate it through
    /// [`Self::begin_dynamic_import`], then drain microtasks — whose
    /// reactions may queue further imports — until the queue is dry.
    /// HTTPS targets need the isolate runner's fetcher and reject
    /// with a `TypeError` here.
    fn pump_layer_a_dynamic_imports(
        &mut self,
        context: &ExecutionContext,
    ) -> Result<(), OtterError> {
        loop {
            let batch: Vec<(u64, String, String)> = {
                let mut queue = self
                    .layer_a_dynamic_imports
                    .lock()
                    .expect("layer-a dynamic import queue poisoned");
                queue.drain(..).collect()
            };
            if batch.is_empty() {
                return Ok(());
            }
            for (token, specifier, referrer) in batch {
                match self.begin_dynamic_import(token, &specifier, &referrer)? {
                    DynamicImportBegin::Settled => {}
                    DynamicImportBegin::FetchHttps { target_url } => {
                        let reason = self.alloc_dynamic_import_error(
                            otter_vm::ErrorKind::TypeError,
                            format!(
                                "dynamic import: remote module \"{target_url}\" requires the isolate runner"
                            ),
                        )?;
                        self.settle_dynamic_import_result(token, Err(reason))?;
                    }
                }
            }
            if let Err(err) = self
                .interp
                .drain_microtasks_with_default(Some(context.clone()))
            {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ));
            }
        }
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
        } else if source.allow_top_level_await {
            // Embedder snippet APIs opt into module-grade top-level
            // `await` while staying on the classic-script pipeline.
            let bytecode =
                compile_script_source_with_top_level_await(&source.text, source.kind, specifier)
                    .map_err(|err| map_compile_error(err, specifier))?;
            CompiledModule::from_bytecode(bytecode)
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
        self.run_module_with_context(entry_path)
            .map(|(result, _)| result)
    }

    /// Load, link, and execute a module graph with opt-in phase timings.
    ///
    /// Ordinary [`Self::run_module`] calls do not read the clock at phase
    /// boundaries or accumulate telemetry. This surface is intended for
    /// benchmark evidence and explicit diagnostics.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_module_profiled(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<(ExecutionResult, module_graph::ModulePhaseTimings), OtterError> {
        let mut timings = module_graph::ModulePhaseTimings::default();
        let (result, _) =
            self.run_module_with_context_inner(entry_path.as_ref(), Some(&mut timings))?;
        Ok((result, timings))
    }

    /// Register every linked module's §16.2.1.6 ResolveExport table with
    /// the interpreter so the Module Namespace Exotic Object reads and
    /// `Op::LoadImportBinding` resolve re-exported / star-exported names
    /// to the defining module's live binding. Called once per graph load,
    /// before evaluation begins.
    fn register_resolved_exports(&mut self, metadata: &[CompiledModuleMetadata]) {
        for module in metadata {
            if module.source_url.is_empty() || module.resolved_exports.is_empty() {
                continue;
            }
            let table = module
                .resolved_exports
                .iter()
                .map(|(name, resolved)| {
                    (
                        name.clone(),
                        (
                            std::sync::Arc::from(resolved.defining_module.as_str()),
                            resolved.binding.clone(),
                        ),
                    )
                })
                .collect();
            self.interp.register_module_resolved_exports(
                std::sync::Arc::from(module.source_url.as_str()),
                table,
            );
        }
    }

    /// Forward every linked module's verbatim source to the interpreter
    /// so `Error.prototype.stack` and `util.getCallSites` can resolve a
    /// frame's byte span to a `(line, column)` position. Called once per
    /// graph load, before evaluation begins.
    fn register_module_sources(&mut self, sources: &std::collections::BTreeMap<String, String>) {
        for (url, text) in sources {
            self.interp
                .register_module_source(url.clone(), std::sync::Arc::from(text.as_str()));
        }
    }

    pub(crate) fn run_module_with_context(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        self.run_module_with_context_inner(entry_path.as_ref(), None)
    }

    fn run_module_with_context_inner(
        &mut self,
        entry_path: &Path,
        mut timings: Option<&mut module_graph::ModulePhaseTimings>,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let start = std::time::Instant::now();
        let loader = self.module_loader_for_entry(entry_path);
        let linked = if timings.is_some() {
            let (linked, graph_timings) = self
                .module_graph
                .load_program_profiled(&loader, entry_path)
                .map_err(map_graph_error)?;
            *timings.as_deref_mut().expect("checked timing sink") = graph_timings;
            linked
        } else {
            self.module_graph
                .load_program(&loader, entry_path)
                .map_err(map_graph_error)?
        };

        let runtime_link_started = timings.is_some().then(std::time::Instant::now);
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
            self.runtime_task_spawner.clone(),
        )?;
        // After `allocate_for_module_inits` (which resets per-run module
        // state); registering earlier would be wiped by that reset.
        self.register_resolved_exports(&linked.metadata);
        self.register_module_sources(&linked.module_sources);
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
                        deferred: false,
                        dynamic: false,
                        synthetic: true,
                    });
                module
                    .module_resolutions
                    .push(otter_bytecode::ModuleResolution {
                        referrer: String::new(),
                        specifier: url.to_string(),
                        target: url.to_string(),
                        deferred: false,
                        dynamic: false,
                        synthetic: true,
                    });
            });

        self.module_records.mark_evaluating();
        if let (Some(timings), Some(started)) = (timings.as_deref_mut(), runtime_link_started) {
            timings.link_time_ns = timings
                .link_time_ns
                .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        let codeblock_started = timings.is_some().then(std::time::Instant::now);
        let context = self.interp.link_module(module);
        if let (Some(timings), Some(started)) = (timings.as_deref_mut(), codeblock_started) {
            timings.compile_time_ns = timings
                .compile_time_ns
                .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        let execute_started = timings.is_some().then(std::time::Instant::now);
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
                if let (Some(timings), Some(started)) = (timings.as_deref_mut(), execute_started) {
                    timings.execute_time_ns =
                        started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                }
                let result = ExecutionResult::from_exit_code(code, start.elapsed());
                return Ok((self.attach_execution_stats(result), context));
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
        self.pump_layer_a_dynamic_imports(&context)?;
        if let (Some(timings), Some(started)) = (timings, execute_started) {
            timings.execute_time_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        }
        self.module_records.mark_evaluated();
        let result =
            ExecutionResult::from_vm_value(value, start.elapsed(), self.interp.gc_heap_mut())
                .with_exit_code(process::exit_code(&self.interp));
        let result = self.attach_execution_stats(result);
        Ok((result, context))
    }

    fn module_loader_for_entry(&self, entry_path: &Path) -> module_loader::ModuleLoader {
        let loader = self.module_loader.for_entry(
            entry_path,
            &self.config.hosted_modules,
            &self.package_manager,
            &self.config.capabilities,
        );
        match &self.remote_module_fetch {
            Some(fetch) => loader.with_remote_fetch(fetch.clone()),
            None => loader,
        }
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
        self.register_resolved_exports(&linked.metadata);
        Ok(())
    }

    /// Run a file from disk, detecting script vs module shape.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_file(&mut self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.run_file_with_context(path).map(|(result, _)| result)
    }

    pub(crate) fn run_file_with_context(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_path_has_module_extension(path) {
            return self.run_module_with_context(path);
        }
        let package_type = {
            let loader = self.module_loader_for_entry(path);
            source_path_package_type(path, &loader)
        };
        if package_type == Some(module_loader::LoaderPackageType::Module) {
            return self.run_module_with_context(path);
        }
        let specifier = path.to_string_lossy().to_string();
        // The CommonJS wrapper compiles its body as JavaScript (through the
        // eval/`new Function` path), so only route JavaScript-kind sources here.
        // TypeScript CommonJS (`.cts`) needs type-stripping in the wrapper and
        // is handled by the existing path until that lands.
        let commonjs_kind = matches!(
            source.kind,
            SourceKind::JavaScript | SourceKind::JavaScriptJsx
        );
        if self.config.commonjs_enabled && commonjs_kind {
            // CommonJS handles every script-shaped source. Only explicit ESM
            // (module extension / package type, handled above) or an ambiguous
            // source that actually parses as a module takes the ESM path.
            if package_type == Some(module_loader::LoaderPackageType::CommonJs)
                || source_path_has_script_extension(path)
            {
                return self.run_commonjs_file(path, source);
            }
            let looks_module = with_program(&source.text, source.kind, |program| {
                Ok::<bool, OtterError>(program_looks_like_module(program))
            })
            .map_err(|err| map_syntax_error(err, &specifier))??;
            if looks_module {
                return self.run_module_with_context(path);
            }
            return self.run_commonjs_file(path, source);
        }
        if package_type == Some(module_loader::LoaderPackageType::CommonJs) {
            return self.run_script_with_context(source, &specifier);
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
                return self.run_compiled_script_with_context_since(module, start);
            }
            return self.run_module_with_context(path);
        }
        let specifier = path.to_string_lossy().to_string();
        self.run_script_with_context(source, &specifier)
    }

    /// Execute a file as a CommonJS module: wrap it in
    /// `(function (exports, require, module, __filename, __dirname) { ... })`,
    /// invoke it with a per-module `require`, and run any microtasks it queued.
    ///
    /// Enabled by [`RuntimeBuilder::with_nodejs_modules`].
    ///
    /// # Errors
    /// See [`OtterError`] variants — compile failures, capability denials, and
    /// errors thrown while loading the module or its dependencies.
    pub(crate) fn run_commonjs_file(
        &mut self,
        path: &Path,
        source: SourceInput,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let start = std::time::Instant::now();
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let cfg = std::sync::Arc::new(commonjs::CjsConfig {
            capabilities: self.config.capabilities.clone(),
            hosted: self.config.hosted_modules.clone(),
            runtime_task_spawner: self.runtime_task_spawner.clone(),
            addon_loader: self.config.commonjs_addon_loader,
        });
        // Entry execution context, linked into the interpreter code space so the
        // wrapper closures resolve from any frame.
        let empty = compile_script_source("", SourceKind::JavaScript, "<commonjs-root>")
            .map_err(|err| map_compile_error(err, "<commonjs-root>"))?;
        let context = self.interp.link_module(empty);
        let load = otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::default_call(),
            Some(&context),
            |ctx| -> Result<Option<u8>, OtterError> {
                let cache = ctx.alloc_object().map_err(gc_oom_to_error)?;
                match commonjs::cjs_instantiate_file(ctx, &cfg, cache, &abs, &source.text) {
                    Ok(_) => Ok(None),
                    Err(otter_vm::NativeError::Exit { code }) => Ok(Some(code)),
                    Err(err) => Err(commonjs_native_to_error(err)),
                }
            },
        );
        match load {
            Ok(Some(code)) => {
                // `process.exit(code)` during module evaluation (e.g. `common.skip`)
                // is a clean process termination, not a load failure — surface the
                // exit code instead of wrapping it as a COMMONJS_LOAD error.
                let result = ExecutionResult::from_exit_code(code, start.elapsed());
                return Ok((self.attach_execution_stats(result), context));
            }
            Err(err) => return Err(err),
            Ok(None) => {}
        }
        // Drain microtasks queued during module execution.
        self.interp.drain_microtasks(&context).map_err(|err| {
            enrich_runtime_diagnostic_with_cause(&mut self.interp, map_vm_error(err))
        })?;
        let result = ExecutionResult::from_vm_value(
            otter_vm::Value::undefined(),
            start.elapsed(),
            self.interp.gc_heap_mut(),
        )
        .with_exit_code(process::exit_code(&self.interp));
        let result = self.attach_execution_stats(result);
        Ok((result, context))
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
            .run_script(
                SourceInput::from_javascript(source).with_top_level_await(),
                "<script>",
            )
            .await
    }

    /// Run a string of TypeScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_typescript(source).with_top_level_await(),
                "<script>",
            )
            .await
    }

    /// Run a source bundle with an explicit diagnostic specifier.
    ///
    /// This is the async handle equivalent of [`Runtime::run_script`], for
    /// embedders that need to execute multiple script sources in the same
    /// isolate while preserving the full hosted CLI/runtime wiring.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_script_source(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.handle.run_script(source, specifier).await
    }

    /// Evaluate a snippet.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .eval(SourceInput::from_javascript(source).with_top_level_await())
            .await
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
                .run_script(
                    SourceInput::from_javascript(source).with_top_level_await(),
                    "<script>",
                )
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
                .run_script(
                    SourceInput::from_typescript(source).with_top_level_await(),
                    "<script>",
                )
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
        self.handle.block_on(async move {
            handle
                .eval(SourceInput::from_javascript(source).with_top_level_await())
                .await
        })
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

    /// Override capability decisions while retaining runtime-level mandatory
    /// filters such as the environment secret denylist.
    #[must_use]
    pub fn capability_hook(mut self, hook: impl RuntimeCapabilityHook) -> Self {
        self.runtime = self.runtime.capability_hook(hook);
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

    /// Register a declared extension (native classes + lazy JS half).
    #[must_use]
    pub fn extension(mut self, extension: &'static Extension) -> Self {
        self.runtime = self.runtime.extension(extension);
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

    /// Enable Node-style CommonJS module execution. See
    /// [`RuntimeBuilder::with_nodejs_modules`].
    #[must_use]
    pub fn with_nodejs_modules(mut self) -> Self {
        self.runtime = self.runtime.with_nodejs_modules();
        self
    }

    /// Register the CommonJS native-addon loader. See
    /// [`RuntimeBuilder::commonjs_addon_loader`].
    #[must_use]
    pub fn commonjs_addon_loader(mut self, loader: CommonJsAddonLoader) -> Self {
        self.runtime = self.runtime.commonjs_addon_loader(loader);
        self
    }

    /// Register a global installer that runs on every isolate. See
    /// [`RuntimeBuilder::global_installer`].
    #[must_use]
    pub fn global_installer(mut self, installer: RuntimeGlobalInstaller) -> Self {
        self.runtime = self.runtime.global_installer(installer);
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

    /// Install a per-instruction step-trace factory. See
    /// [`RuntimeBuilder::tracer_factory`].
    #[must_use]
    pub fn tracer_factory(mut self, factory: Option<TracerFactory>) -> Self {
        self.runtime = self.runtime.tracer_factory(factory);
        self
    }

    /// [`RuntimeBuilder::jit_selection`].
    #[must_use]
    pub fn jit_selection(mut self, selection: JitSelection) -> Self {
        self.runtime = self.runtime.jit_selection(selection);
        self
    }

    /// [`RuntimeBuilder::jit_osr_threshold`].
    #[must_use]
    pub fn jit_osr_threshold(mut self, threshold: u32) -> Self {
        self.runtime = self.runtime.jit_osr_threshold(threshold);
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
        module_graph::GraphError::Resolution { url, message } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::syntax(message)
                    .with_source_url(url)
                    .with_help("export the imported binding from the target module"),
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
///   fresh JS error of the carried kind from the message.
/// - [`DynLoadError::Thrown`] — the dynamically-loaded module's
///   `<module-init>` threw a JS value. The settler uses that
///   value directly as the promise's rejection reason per
///   §16.2.1.7 step 7.b.i + §27.2.1.7.
enum DynLoadError {
    Diagnostic {
        kind: otter_vm::ErrorKind,
        message: String,
    },
    Thrown(otter_vm::Value),
}

impl DynLoadError {
    fn diagnostic(kind: otter_vm::ErrorKind, message: impl Into<String>) -> Self {
        Self::Diagnostic {
            kind,
            message: message.into(),
        }
    }

    fn type_error(message: impl Into<String>) -> Self {
        Self::diagnostic(otter_vm::ErrorKind::TypeError, message)
    }

    fn syntax_error(message: impl Into<String>) -> Self {
        Self::diagnostic(otter_vm::ErrorKind::SyntaxError, message)
    }

    fn from_compile_error(err: &otter_compiler::CompileError, message: impl Into<String>) -> Self {
        let kind = match err {
            otter_compiler::CompileError::Syntax { .. } => otter_vm::ErrorKind::SyntaxError,
            _ => otter_vm::ErrorKind::TypeError,
        };
        Self::diagnostic(kind, message)
    }

    fn from_graph_error(err: &module_graph::GraphError, message: impl Into<String>) -> Self {
        let kind = match err {
            module_graph::GraphError::Parse { .. } => otter_vm::ErrorKind::SyntaxError,
            module_graph::GraphError::Compile { error, .. } => match error {
                otter_compiler::CompileError::Syntax { .. } => otter_vm::ErrorKind::SyntaxError,
                _ => otter_vm::ErrorKind::TypeError,
            },
            module_graph::GraphError::Cycle { .. } => otter_vm::ErrorKind::RangeError,
            module_graph::GraphError::Resolution { .. } => otter_vm::ErrorKind::SyntaxError,
            module_graph::GraphError::Loader(_) => otter_vm::ErrorKind::TypeError,
        };
        Self::diagnostic(kind, message)
    }
}

/// Sentinel property used to flag a `module_env` as already
/// having had its `<module-init>` body executed. The file-backed
/// dynamic-import path dedupes through the interpreter's module
/// records; only the HTTPS single-module path still installs the
/// marker for its separately linked fragments.
const DYNAMIC_INIT_MARKER: &str = "__otter_module_inited__";

fn otter_vm_init_marker_install(interp: &mut otter_vm::Interpreter, mut env: otter_vm::JsObject) {
    otter_vm::object::set(
        &mut env,
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
    let mut import_meta = interp
        .alloc_host_object_with_roots(&[&env_root], &[])
        .map_err(|e| {
            DynLoadError::type_error(format!("dynamic import: alloc import_meta failed: {e}"))
        })?;
    let url_string = otter_vm::JsString::from_str(url, interp.gc_heap_mut()).map_err(|err| {
        DynLoadError::type_error(format!(
            "dynamic import: alloc import_meta.url failed: {err}"
        ))
    })?;
    otter_vm::object::set(
        &mut import_meta,
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
    use otter_vm::{ErrorDetail, VmError};
    // Render the message before destructuring moves `detail` out of `run_err`.
    let detail_message = run_err.message();
    let otter_vm::RunError {
        error,
        frames,
        detail,
    } = run_err;
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
            diagnostic: Box::new(Diagnostic {
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
            }),
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
        VmError::BudgetExceeded => runtime_diagnostic(
            DiagnosticKind::Timeout,
            DiagnosticCode::BudgetExceeded,
            detail_message,
        ),
        VmError::TypeMismatch => {
            runtime_diagnostic(DiagnosticKind::Type, DiagnosticCode::TypeMismatch, display)
        }
        VmError::TypeError | VmError::TypeMismatchAt => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::TypeError,
            detail_message,
        ),
        VmError::SyntaxError => runtime_diagnostic(
            DiagnosticKind::Syntax,
            DiagnosticCode::SyntaxError,
            detail_message,
        ),
        VmError::UnknownIntrinsic => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::UnknownMethod,
            detail_message,
        ),
        VmError::TemporalDeadZone { local_index } => runtime_diagnostic(
            DiagnosticKind::Reference,
            DiagnosticCode::Tdz,
            format!("cannot access local {local_index} before initialization"),
        ),
        VmError::ThisUninitialized => runtime_diagnostic(
            DiagnosticKind::Reference,
            DiagnosticCode::Tdz,
            detail_message,
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
        VmError::Uncaught => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::Uncaught,
            detail_message,
        ),
        VmError::JsonError => {
            // `code` is `&'static str` from the VM JSON path (every
            // value is one of the `JSON_*` codes in the closed
            // [`DiagnosticCode`] set). Parse it back through
            // `DiagnosticCode::parse` so the diagnostic still
            // carries a typed code in the closed set.
            let (code, message) = match &detail {
                Some(ErrorDetail::Json(p)) => (p.code, p.message.clone()),
                _ => ("JSON_BAD_ARG", detail_message),
            };
            let typed = DiagnosticCode::parse(code).unwrap_or(DiagnosticCode::JsonBadArg);
            runtime_diagnostic(DiagnosticKind::Type, typed, message)
        }
        VmError::InvalidRegExp => runtime_diagnostic(
            DiagnosticKind::Syntax,
            DiagnosticCode::InvalidRegexp,
            detail_message,
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

fn map_native_error(err: otter_vm::NativeError) -> OtterError {
    match err {
        otter_vm::NativeError::Interrupted => OtterError::Interrupted,
        otter_vm::NativeError::Exit { code } => OtterError::Runtime {
            diagnostic: Box::new(Diagnostic {
                kind: DiagnosticKind::Type,
                code: DiagnosticCode::Uncaught.as_str().to_string(),
                message: format!("native function requested process exit with code {code}"),
                source_url: None,
                range: None,
                span: None,
                help: None,
                frames: Vec::new(),
                cause: None,
                aggregated_errors: Vec::new(),
            }),
        },
        other => OtterError::Runtime {
            diagnostic: Box::new(Diagnostic {
                kind: DiagnosticKind::Type,
                code: DiagnosticCode::TypeError.as_str().to_string(),
                message: other.to_string(),
                source_url: None,
                range: None,
                span: None,
                help: None,
                frames: Vec::new(),
                cause: None,
                aggregated_errors: Vec::new(),
            }),
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
        struct AllowExampleNetAndEnv;

        impl RuntimeCapabilityHook for AllowExampleNetAndEnv {
            fn check_capability(
                &self,
                _capabilities: &CapabilitySet,
                capability: RuntimeCapability,
                request: &CapabilityRequest<'_>,
            ) -> bool {
                (capability == RuntimeCapability::Net
                    && matches!(request, CapabilityRequest::Host("example.com")))
                    || capability == RuntimeCapability::Env
            }
        }

        let runtime = Runtime::builder()
            .capabilities(CapabilitySet::sandbox())
            .capability_hook(AllowExampleNetAndEnv)
            .build()
            .unwrap();

        assert!(runtime.check_capability(
            RuntimeCapability::Net,
            &CapabilityRequest::Host("example.com")
        ));
        assert!(
            runtime.check_capability(RuntimeCapability::Env, &CapabilityRequest::EnvVar("HOME"))
        );
        assert!(!runtime.check_capability(
            RuntimeCapability::Env,
            &CapabilityRequest::EnvVar("OPENAI_API_KEY")
        ));
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
    async fn runtime_keep_alive_liveness_is_idempotent() {
        let otter = Otter::new();
        let handle = otter.handle().clone();

        let resource = handle.retain_keep_alive(RuntimeLiveness::Ref);
        assert_eq!(handle.activity_stats().pending_ref_host_ops, 1);
        assert!(!resource.is_closed());

        resource.unref();
        let stats = handle.activity_stats();
        assert_eq!(stats.pending_ref_host_ops, 0);
        assert_eq!(stats.pending_unref_host_ops, 1);
        assert_eq!(stats.completed_host_ops, 0);
        assert_eq!(stats.cancelled_host_ops, 0);

        resource.ref_();
        resource.ref_();
        let stats = handle.activity_stats();
        assert_eq!(stats.pending_ref_host_ops, 1);
        assert_eq!(stats.pending_unref_host_ops, 0);
        assert_eq!(stats.completed_host_ops, 0);
        assert_eq!(stats.cancelled_host_ops, 0);

        resource.close();
        resource.close();
        assert!(resource.is_closed());
        let stats = handle.activity_stats();
        assert_eq!(stats.pending_ref_host_ops, 0);
        assert_eq!(stats.pending_unref_host_ops, 0);
        assert_eq!(stats.completed_host_ops, 1);
        assert_eq!(stats.cancelled_host_ops, 0);

        let unref = handle.retain_keep_alive(RuntimeLiveness::Unref);
        assert_eq!(handle.activity_stats().pending_unref_host_ops, 1);
        drop(unref);
        let stats = handle.activity_stats();
        assert_eq!(stats.pending_unref_host_ops, 0);
        assert_eq!(stats.cancelled_host_ops, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_task_runs_on_isolate_loop() {
        struct NotifyTask(std::sync::mpsc::Sender<()>);

        impl RuntimeTask for NotifyTask {
            fn run(self: Box<Self>, _runtime: &mut Runtime) -> Result<(), OtterError> {
                self.0
                    .send(())
                    .expect("runtime task receiver should be alive");
                Ok(())
            }
        }

        let otter = Otter::new();
        let handle = otter.handle().clone();
        let (tx, rx) = std::sync::mpsc::channel();

        handle
            .enqueue_runtime_task(NotifyTask(tx), RuntimeLiveness::Ref)
            .expect("runtime task should enqueue");

        rx.recv_timeout(Duration::from_secs(5))
            .expect("runtime task should run");
        let mut stats = handle.activity_stats();
        for _ in 0..50 {
            if stats.pending_ref_host_ops == 0 && stats.completed_host_ops == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            stats = handle.activity_stats();
        }
        assert_eq!(stats.pending_ref_host_ops, 0);
        assert_eq!(stats.completed_host_ops, 1);
    }

    #[test]
    fn async_function_adopts_returned_native_promise() {
        let mut runtime = Runtime::builder().build().unwrap();
        runtime
            .eval(SourceInput::from_javascript(
                r#"
                let out = "pending";
                async function inner() {
                  return Promise.resolve("adopted");
                }
                inner().then((value) => {
                  out = value;
                });
                "#,
            ))
            .unwrap();
        let result = runtime.eval(SourceInput::from_javascript("out")).unwrap();
        assert_eq!(result.completion_string(), "adopted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "hangs intermittently after active runtime changes; keep targeted while Node compat work continues"]
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
    #[ignore = "hangs intermittently after active runtime changes; keep targeted while Node compat work continues"]
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
    fn module_program_dynamic_import_invalid_target_rejects_syntax_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bad.js"),
            "var smoosh; function smoosh() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("entry.mjs"),
            "function fail() { return undefined.x; }\n\
             import(\"./bad.js\")\n\
               .catch((error) => { if (error.name !== \"SyntaxError\") fail(); })\n\
               .then(() => {}, fail);\n",
        )
        .unwrap();

        let mut runtime = Runtime::builder().build().unwrap();
        runtime.run_module(dir.path().join("entry.mjs")).unwrap();
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
    fn json_cyclic_throws_type_error() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("const a = {}; a.self = a; JSON.stringify(a);")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "UNCAUGHT");
                assert!(
                    diagnostic.message.contains("TypeError")
                        && diagnostic
                            .message
                            .contains("JSON.stringify cannot serialize cyclic structures."),
                    "unexpected diagnostic: {}",
                    diagnostic.message,
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn json_parse_error_throws_syntax_error_with_byte_position() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("JSON.parse(\"[1, 2,]\");")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "UNCAUGHT");
                assert!(
                    diagnostic.message.contains("SyntaxError")
                        && diagnostic.message.contains("trailing comma")
                        && diagnostic.message.contains("at byte 6"),
                    "unexpected diagnostic: {}",
                    diagnostic.message,
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn string_unicode_case_mapping() {
        let otter = Otter::new();
        otter
            .blocking_run_typescript(
                r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + JSON.stringify(a)); }
                eq("Éab".toLowerCase(), "éab", "latin-1");
                eq("ß".toUpperCase(), "SS", "sharp-s-upper");
                eq("İ".toLowerCase(), "i̇", "dotted-I");
                eq("𐐀".toLowerCase(), "𐐨", "supplementary-deseret");
                // Final Sigma: cased before, end-of-word -> final form.
                eq("AΣ".toLowerCase(), "aς", "final-sigma");
                eq("ΣA".toLowerCase(), "σa", "non-final-sigma");
                // toLocale* default to the locale-insensitive mapping.
                eq("ABC".toLocaleLowerCase(), "abc", "locale-lower");
                "#,
            )
            .expect("Unicode case mapping");
    }

    #[test]
    fn eval_directive_prologue_completion_value() {
        let otter = Otter::new();
        otter
            .blocking_run_typescript(
                r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + JSON.stringify(a)); }
                // A directive-prologue string is an expression statement;
                // its value is the eval completion value.
                eq(eval('"hello"'), "hello", "bare-directive");
                eq(eval('"use strict"'), "use strict", "use-strict-value");
                eq(eval('"a"; 1'), 1, "directive-then-stmt");
                "#,
            )
            .expect("eval directive completion value");
    }

    #[test]
    fn regexp_subclass_overrides_and_super_computed_call() {
        let otter = Otter::new();
        otter
            .blocking_run_typescript(
                r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                // A RegExp subclass's own method shadows the base
                // prototype via the instance's real [[Prototype]].
                class RE extends RegExp {
                    exec() { return "OWN"; }
                    [Symbol.replace]() { return "SYM"; }
                }
                const r = new RE("b", "g");
                eq(r.exec("xb"), "OWN", "subclass-exec-override");
                eq(r[Symbol.replace]("ab", "z"), "SYM", "subclass-symbol-override");
                // Plain RegExp keeps intrinsic behaviour.
                eq(/b/.exec("ab")[0], "b", "plain-exec");
                eq(/a/gi.flags, "gi", "plain-flags");

                // super[computed](...) resolves the parent method with
                // `this` bound to the receiver (spread + non-spread).
                let called = 0;
                class RE2 extends RegExp {
                    [Symbol.replace](...args) {
                        const out = super[Symbol.replace](...args);
                        called += 1;
                        return out;
                    }
                }
                eq("a b a".replaceAll(new RE2(" ", "g"), "_"), "a_b_a", "super-computed-spread");
                eq(called, 1, "super-computed-called-once");
                "#,
            )
            .expect("RegExp subclass overrides + super[computed] call");
    }

    #[test]
    fn string_replace_split_match_spec_dispatch() {
        let otter = Otter::new();
        otter
            .blocking_run_typescript(
                r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                // searchValue coerces via ToString; function replacer fires.
                eq("gnulluna".replace(null, (m, p) => p + ""), "g1una", "replace-null-fn");
                // $-substitution patterns.
                eq("abc".replace("b", "[$&]"), "a[b]c", "replace-dollar-amp");
                eq("abc".replace("b", "$`|$'"), "aa|cc", "replace-dollar-ctx");
                eq("a$b".replace("$", "$$"), "a$b", "replace-dollar-dollar");
                // replaceAll over all occurrences.
                eq("a.b.c".replaceAll(".", "-"), "a-b-c", "replaceAll");

                // split delegates to RegExp @@split (captures included).
                eq("a1b2c".split(/(\d)/).join(","), "a,1,b,2,c", "split-regexp");
                eq("a,b,c".split(",").length, 3, "split-string");

                // match / search delegate to RegExp @@match / @@search.
                eq("a1b2".match(/\d/g).join(""), "12", "match-global");
                eq("xxabc".search(/abc/), 2, "search");
                eq([..."a1b2".matchAll(/\d/g)].length, 2, "matchAll");

                // Primitive searchValue never probes @@replace.
                Object.defineProperty(String.prototype, Symbol.replace, {
                    configurable: true,
                    get() { throw new Error("@@replace probed on primitive"); },
                });
                eq("a,b,c".replace(",", "X"), "aXb,c", "replace-primitive-no-symbol");
                delete String.prototype[Symbol.replace];
                "#,
            )
            .expect("String replace/split/match spec dispatch");
    }

    #[test]
    fn json_parse_reviver_raw_json_and_source() {
        let otter = Otter::new();
        otter
            .blocking_run_typescript(
                r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                // Reviver transforms values bottom-up; parsed objects
                // inherit Object.prototype.
                var parsed = JSON.parse('{"a":1,"b":2}', (k, v) =>
                    typeof v === "number" ? v + 1 : v);
                eq(parsed.a, 2, "reviver-a");
                eq(parsed.b, 3, "reviver-b");
                eq(Object.getPrototypeOf(JSON.parse("{}")), Object.prototype, "parse-proto");
                eq(JSON.parse('{"__proto__":1}').__proto__, 1, "parse-proto-own");
                eq(Object.is(JSON.parse("-0"), -0), true, "parse-neg-zero");

                // rawJSON / isRawJSON round-trip.
                var raw = JSON.rawJSON("1.50");
                eq(JSON.isRawJSON(raw), true, "isRawJSON");
                eq(JSON.isRawJSON({}), false, "isRawJSON-plain");
                eq(raw.rawJSON, "1.50", "raw-text");
                eq(Object.isFrozen(raw), true, "raw-frozen");
                eq(JSON.stringify({ x: raw }), '{"x":1.50}', "stringify-raw");

                // Reviver context.source carries the verbatim leaf text.
                var src = JSON.parse("1.50", (k, v, ctx) => ctx.source);
                eq(src, "1.50", "context-source");
                "#,
            )
            .expect("JSON.parse reviver / rawJSON / context.source");
    }

    #[test]
    fn json_stringify_runs_spec_observable_hooks() {
        // toJSON, replacer function, array-replacer PropertyList,
        // wrapper-object ToNumber/ToString unwrap, and verbatim
        // propagation of a user exception thrown from a getter.
        let otter = Otter::new();
        otter
            .blocking_run_typescript(
                r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                eq(JSON.stringify({ d: { toJSON() { return "X"; } } }),
                   '{"d":"X"}', "toJSON");

                eq(JSON.stringify({ a: 1, b: 2 }, (k, v) => typeof v === "number" ? v * 10 : v),
                   '{"a":10,"b":20}', "replacer-fn");

                eq(JSON.stringify({ a: 1, b: 2, c: 3 }, ["b", "a", "b"]),
                   '{"b":2,"a":1}', "replacer-array");

                eq(JSON.stringify(new Number(8.5)), "8.5", "number-wrapper");
                eq(JSON.stringify(new String("s")), '"s"', "string-wrapper");

                var sw = new String("raw");
                sw.toString = function () { return "cooked"; };
                eq(JSON.stringify(sw), '"cooked"', "string-wrapper-tostring");
                "#,
            )
            .expect("spec-observable JSON.stringify hooks");

        // A user exception thrown from a getter propagates verbatim,
        // not as a wrapped TypeError — the caught value is the exact
        // object that was thrown.
        let err = otter.blocking_run_typescript(
            "var sentinel = { marker: 42 }; \
             try { JSON.stringify({ get k() { throw sentinel; } }); \
             throw new Error('no throw'); } \
             catch (e) { if (e !== sentinel) throw new Error('not verbatim'); }",
        );
        assert!(
            err.is_ok(),
            "user exception should propagate verbatim: {err:?}"
        );
    }

    #[test]
    fn json_bigint_throws_type_error() {
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("JSON.stringify({ n: 1n });")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "UNCAUGHT");
                assert!(
                    diagnostic.message.contains("TypeError")
                        && diagnostic
                            .message
                            .contains("JSON.stringify cannot serialize BigInt values."),
                    "unexpected diagnostic: {}",
                    diagnostic.message,
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn otter_rejects_unsupported_js_feature() {
        // `with` in strict mode is a SyntaxError (§14.13) — a stable
        // canary for the structured Compile diagnostic shape.
        let otter = Otter::new();
        let err = otter
            .blocking_run_typescript("\"use strict\"; with (o) { x; }")
            .unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].code, "STRICT_MODE_WITH");
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
