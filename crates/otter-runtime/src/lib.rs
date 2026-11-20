//! Public embedding API for the Otter foundation engine.
//!
//! Two-layer surface per
//! [the public runtime architecture](../../../docs/book/src/engine/architecture.md):
//!
//! - **Direct — [`Runtime`] / [`RuntimeBuilder`]**: thread-pinned runtime for
//!   embedders that own the event loop.
//! - **Sendable — [`RuntimeHandle`] / [`Otter`]**: production isolate runner
//!   backed by a shared [`TokioRuntimeHost`].
//!
//! Applications should import orchestration types from [`embedding`]. That
//! curated module is the mandatory high-level boundary and deliberately omits
//! raw VM values, objects, GC handles, and native mutation contexts.
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
//! - [`ExecutionAttempt`] — diagnostic-aware success/error envelope.
//! - [`OtterError`], [`ConfigError`], [`IoErrorKind`] — error model.
//! - [`ResourceAccount`], [`ResourceLimits`], [`ResourceSnapshot`] — shared
//!   resource admission and diagnostics.
//! - [`RuntimeSnapshot`] — in-process isolate image tied to its donor runtime
//!   configuration and resource account.
//! - [`InterruptHandle`] — cooperative cancellation.
//! - [`embedding`] — preferred owned orchestration API.
//! - Owned default-off collection-service records through [`Runtime::start_gc_pause_capture`].
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
//! - No package-manager data enters this crate: modules resolve through
//!   the `node_modules` tree on disk, as Node resolves them.
//! - Loader, module-graph, source-map, and diagnostics
//!   state are owned by the `Runtime` session; no hidden global resolver state
//!   is used by the active runtime path.
//! - Fresh runtime construction completes trusted realm and extension
//!   installers before installing its configured JIT compiler. Restored
//!   runtimes retain their existing compiler installation boundary, and later
//!   realm creation uses the ready runtime's compiler.
//! - JIT diagnostics cross the async boundary as owned report data, including
//!   partial reports from abrupt completion; they contain no VM/GC handles.
//! - Every live runtime retains one atomic role-specific lease set from its
//!   config's shared [`ResourceAccount`]. Direct runtimes charge an isolate;
//!   handle and worker runtimes additionally charge their native stack, and
//!   workers charge their worker slot. Construction reserves the complete set
//!   before effects and every error path rolls it back.
//!
//! # See also
//! - [Engine architecture](../../../docs/book/src/engine/architecture.md)
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

mod admission;
mod atomic_wait;
mod capabilities;
mod checkpoint;
mod code_cache;
mod commonjs;
mod compile_cache;
mod compile_hooks;
pub mod compiled_program;
mod completion_admission;
pub mod data_modules;
pub mod diagnostics;
mod dynamic_import_delivery;
mod dynamic_import_error;
mod dynamic_module_evaluation;
mod dynamic_module_loading;
pub mod embedding;
mod entry_file;
pub mod error;
mod error_mapping;
mod eval_execution;
mod event_loop;
mod execution_result;
mod gc_observation;
pub mod handle;
mod heap_config;
pub mod hooks;
mod host_calls;
mod hosted_completion;
pub mod import_map;
pub mod ipc;
mod module_execution;
pub mod module_graph;
pub mod module_loader;
mod module_records;
mod module_session;
mod module_syntax;
mod otter;
mod process;
mod process_control;
mod process_env;
mod process_events;
mod process_execve;
mod process_flags;
mod process_info;
mod process_ipc;
mod process_lifecycle;
mod realm;
mod realm_installers;
mod runtime_activity;
mod runtime_bootstrap;
mod runtime_builder;
mod runtime_inspection;
mod runtime_snapshot;
mod script_execution;
mod script_source;
mod session_control;
mod static_bootstrap;
pub mod structured_clone;
pub mod surface;
#[cfg(unix)]
pub mod terminal;
mod timer_delivery;
mod timer_dispatch;
mod uncaught;
pub mod web_fetch_host;
pub mod web_modules;
pub mod web_structured_clone;
pub mod worker;

use capabilities::env_name_is_builtin_denied;
pub use capabilities::{CapabilitySet, ENV_BUILTIN_DENY_PATTERNS, GRANT_VAR, Permission};
pub use code_cache::PreparedScript;
pub use commonjs::{
    SCHEME_ONLY_BUILTINS, create_require, require_commonjs_dependency, run_builtin_cjs_shim,
};
pub(crate) use compile_cache::LAZY_FUNCTIONS;
pub(crate) use compile_hooks::lazy_compile_hook;
use compile_hooks::standard_eval_hook;
pub(crate) use compile_hooks::{CommonJsRouting, compile_classic_script};
pub(crate) use dynamic_module_evaluation::url_to_path;
use dynamic_module_evaluation::{
    DynLoadError, alloc_dynamic_import_error_on, dynamic_import_origin,
    evaluate_and_settle_dynamic_linked_module_on, settle_dynamic_import_result_on,
    unresolved_import,
};
pub(crate) use error_mapping::map_compile_error;
pub(crate) use error_mapping::map_graph_error;
use error_mapping::{
    enrich_runtime_diagnostic_with_cause, map_native_error, map_syntax_error, map_vm_error,
};
pub use execution_result::{ExecutionAttempt, ExecutionResult, RuntimeExecutionStats};
use module_session::{
    RuntimeDiagnosticsSink, RuntimeModuleGraphState, RuntimeModuleLoaderState,
    RuntimeSourceMapTable, commonjs_config, surface_fingerprint,
};
pub(crate) use module_syntax::program_looks_like_module;
#[cfg(test)]
use module_syntax::source_text_looks_like_module;
use module_syntax::{
    source_path_has_module_extension, source_path_has_script_extension, source_path_package_type,
};
pub use otter::{Otter, OtterBuilder};
pub use realm_installers::{
    CommonJsAddonLoader, Extension, GlobalClass, HostedCommonJsInstall, HostedModule,
    HostedModuleInstall, RuntimeExtensionInstaller, RuntimeGlobalInstaller,
};
use realm_installers::{ConfiguredRealmInstaller, GlobalClassInner};
pub use static_bootstrap::ExtensionJs;

/// Implementation details referenced by code generated by `otter-macros`.
///
/// Declarative extension users should invoke the macros and use [`surface`]
/// for handwritten bindings. This module is not a stable embedding API.
#[doc(hidden)]
pub mod __macro_support {
    pub use crate::{
        CapabilitySet, Extension, ExtensionJs, GlobalClass, HostedModule, RuntimeAttr,
        RuntimeLocal, RuntimeNativeError, RuntimeNativeScope, RuntimeTaskSpawner,
    };
    pub use otter_vm::__macro_support::*;
}

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use otter_bytecode::{BytecodeModule, SpanEntry};
use otter_compiler::{
    compile_script_program, compile_script_source, compile_script_source_with_top_level_await,
};
use otter_gc::GcStats;
use otter_syntax::{
    SourceKind, SyntaxDiagnostic, SyntaxError, detect_file_source_kind, with_program,
};
use otter_vm::{EvalCompileOptions, ExecutionContext, Interpreter, InterruptFlag, NativeCallInfo};
use serde::{Deserialize, Serialize};

use admission::{AdmittedRuntimeConfig, RuntimeAdmissionKind};

pub use admission::RUNTIME_THREAD_STACK_BYTES;
pub use compiled_program::CompiledProgram;
pub use diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticCode, DiagnosticKind, StackFrame};
pub use error::{ConfigError, IoErrorKind, OtterError, RealmError};
pub use event_loop::{RuntimeLiveness, TokioRuntimeHost};
pub use gc_observation::{
    GcPauseCapture, GcPauseKind, GcPauseOutcome, GcPauseRecord, GcPauseTrigger, HeapOccupancy,
};
pub use handle::{RuntimeActivityStats, RuntimeHandle};
pub use heap_config::{
    MANAGED_HEAP_PAGE_BYTES, configured_managed_heap_bytes, initialize_managed_heap,
};
pub use hooks::{
    CapabilityRequest, RuntimeCapability, RuntimeCapabilityEvaluator, RuntimeCapabilityHook,
    RuntimeCodeCacheHook, RuntimeCompileHook, RuntimeCompileRequest, RuntimeDiagnosticHook,
    RuntimeHooks, RuntimeJobHook, RuntimeJobKind, RuntimeJobRequest, RuntimeLoadHook,
    RuntimeLoadRequest, RuntimeResolveHook, RuntimeResolveRequest, default_check_capability,
    default_compile_source,
};
pub use ipc::{CarriedHandles, IpcChannel, IpcEvent, IpcSendError};
pub use otter_compiler::{
    CompiledExport, CompiledFunctionSpans, CompiledImport, CompiledImportKind, CompiledModule,
    CompiledModuleMetadata, LiveBindingSlot,
};
pub use otter_resource::{
    ResourceAccount, ResourceClass, ResourceError, ResourceLease, ResourceLeaseSet, ResourceLimits,
    ResourceLimitsBuilder, ResourceReservation, ResourceSnapshot, ResourceSnapshotEntry,
    SharedSource, SharedSourceBuilder, SharedSourceError,
};
/// Canonical intrinsic Error class selector for scoped native allocation.
pub use otter_vm::ErrorKind as RuntimeErrorKind;
pub use otter_vm::{CodeEvictionStats, CpuProfile};
pub use otter_vm::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle, StdConsoleSink};
pub use otter_vm::{EmbeddedCommonJs, NativeCtx, NativeError, Value, ValueWindow, marshal};
pub use otter_vm::{
    ExecutionContext as RuntimeExecutionContext, PersistentRootId as RuntimePersistentRootId,
};
pub use otter_vm::{
    JIT_ARTIFACT_BUNDLE_LIMIT, JIT_ARTIFACT_BYTE_LIMIT, JIT_DEBUG_EVENT_LIMIT, JitArtifactBatch,
    JitArtifactBundle, JitArtifactFile, JitArtifactFileName, JitArtifactManifest,
    JitDebugBakeYieldPoint, JitDebugCompileOutcome, JitDebugEvent, JitDebugPreHookExit,
    JitDebugReport, JitDebugRequest, JitDebugTarget, JitDebugTier, JitDirectCallKind,
    JitDirectCallLoweringOutcome, JitDirectCallLoweringRejectionReason, JitDirectCallPlanOutcome,
    JitInlineRejectionReason, JitInstallDeclineReason, array, object,
};
pub use otter_vm::{
    JitRuntimeStats, WorkBudget, WorkBudgetExceededAction, WorkBudgetStats, WorkBudgetTelemetry,
};
pub use process::{define_stdio_natives, node_platform};
pub use realm::{RuntimeExtensionContext, RuntimeGlobalValue, RuntimeRealmContext, RuntimeRealmId};
pub use runtime_snapshot::{RuntimeSnapshot, RuntimeSnapshotDiagnostics, SnapshotRuntimeOptions};
// Embedder-driven event loop. `Runtime::install_timer_scheduler`,
// `install_host_completion_sink`, and `install_dynamic_import_loader`
// are public, so the types naming their arguments must be reachable
// without a direct `otter-vm` dependency.
pub use otter_vm::host_completion::{
    HostCompletionAdmission, HostCompletionJob, HostCompletionOutcome, HostCompletionSink,
};
pub use otter_vm::promise_rejection::{PromiseRejectionHook, PromiseRejectionHookHandle};
pub use otter_vm::{
    DynamicImportAdmission, DynamicImportLoader, DynamicImportLoaderHandle, TimerAdmission,
    TimerEntry, TimerScheduler, TimerSchedulerHandle,
};
pub use runtime_activity::{RuntimeKeepAlive, RuntimeTask, RuntimeTaskSpawner};
pub use structured_clone::{
    StructuredCloneError, StructuredCloneMapEntry, StructuredCloneNumber, StructuredCloneOptions,
    StructuredCloneProperty, StructuredCloneTransfer, StructuredCloneTransferId,
    StructuredCloneTransferKind, StructuredCloneTransferList, StructuredCloneTransferListError,
    StructuredCloneValue,
};
pub use surface::{
    HostAtomInterner, RuntimeAccessorSpec, RuntimeAttr, RuntimeClassSpec, RuntimeConstSpec,
    RuntimeConstValue, RuntimeConstructorSpec, RuntimeHostAtom, RuntimeHostAtomId,
    RuntimeHostCodeLivenessTracer, RuntimeHostDataTracer, RuntimeHostObjectData,
    RuntimeHostObjectError, RuntimeHostValueSlot, RuntimeJsObject, RuntimeJsString, RuntimeLocal,
    RuntimeMethodSpec, RuntimeNamespaceSpec, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeNativeFastFn, RuntimeNativeFn, RuntimeNativeScope,
    RuntimeNumberValue, RuntimeObjectLayout, RuntimePendingValue, RuntimePendingValues,
    RuntimePropertySpec, RuntimeSurfaceError, RuntimeTracedHostObjectData, RuntimeValue,
    runtime_accessor, runtime_alloc_object, runtime_arg_to_string, runtime_array_from_elements,
    runtime_class, runtime_constant, runtime_constructor, runtime_getter, runtime_method,
    runtime_method_with_attrs, runtime_namespace, runtime_native_dynamic, runtime_native_static,
    runtime_optional_arg_to_string, runtime_property, runtime_string_value, runtime_this_object,
    runtime_type_error, runtime_with_host_data, runtime_with_host_data_mut,
};
pub use worker::{
    OtterPool, OtterPoolBuilder, Worker, WorkerBuilder, WorkerId, WorkerShutdownReport,
};

/// Default heap cap (2 GiB) when none is configured.
pub const DEFAULT_MAX_HEAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default per-`run_*` timeout: none. A program runs until it is done,
/// the way every other JavaScript runtime lets it; a caller that wants a
/// wall clock asks for one with [`RuntimeBuilder::timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::ZERO;

/// Default JS call-stack limit.
pub const DEFAULT_MAX_STACK_DEPTH: u32 = 1024;

/// Default number of threads an isolate compiles optimized code on, at
/// most: they start as compilations queue up.
pub const DEFAULT_JIT_COMPILE_THREADS: usize = 4;

/// Pause records a traced runtime holds between two runs' results.
const GC_TRACE_RECORDS: usize = 65_536;

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
        let kind = detect_file_source_kind(path).ok_or_else(|| OtterError::SourceKind {
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

/// A JavaScript function an embedder keeps to [`Runtime::call`] it without
/// compiling source: returned by [`Runtime::global_function`], rooted until
/// [`Runtime::release_function`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunction {
    root: otter_vm::PersistentRootId,
}

/// One argument of a [`Runtime::call`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CallArgument<'a> {
    /// `undefined`.
    Undefined,
    /// `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(&'a str),
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
pub use otter_vm::asm_stdlib;
pub use otter_vm::binary::byte_storage;
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

/// One reading of an isolate's complete budget.
///
/// The runtime meters two halves of the same policy: the shared resource
/// ledger charges memory, retained bytes, queued work, and worker slots, while
/// the VM meters bytecode, generated backedges, native calls, GC, RegExp, and
/// microtasks in one work currency, alongside allocation/host/time pressure. This carries both, plus
/// the configured limits that produced them, so a caller reads one consistent
/// picture instead of correlating two surfaces by hand.
#[derive(Debug, Clone)]
pub struct WorkBudgetReport {
    /// Per-slice execution policy in force for this isolate.
    pub limits: WorkBudget,
    /// Counters published at the isolate's most recent slice boundary.
    pub execution: WorkBudgetStats,
    /// Current, peak, rejection, and limit counters of the shared ledger.
    pub resources: ResourceSnapshot,
}

/// Runtime configuration.
/// Warning-channel switches carried from the CLI (`--no-warnings`,
/// `--no-deprecation`, `--trace-warnings`, `--throw-deprecation`,
/// `--pending-deprecation`, `--disable-warning=<code-or-type>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WarningOptions {
    /// Suppress the default stderr printer entirely.
    pub no_warnings: bool,
    /// Silence DeprecationWarning emission.
    pub no_deprecation: bool,
    /// Turn DeprecationWarning into a throw at delivery.
    pub throw_deprecation: bool,
    /// Print the warning's stack instead of the one-line form.
    pub trace_warnings: bool,
    /// Emit pending deprecations.
    pub pending_deprecation: bool,
    /// Warning codes or type names whose printing is disabled.
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    /// Shared ledger inherited by config clones and child isolates.
    pub(crate) resource_account: ResourceAccount,
    /// Finite per-isolate credits for terminal work and its live origins.
    pub(crate) completion_capacities: completion_admission::CompletionCapacities,
    /// Work-slice budget installed before the isolate's first turn.
    /// Child isolates inherit it through the cloned config, so a worker
    /// cannot execute under a looser CPU policy than its parent.
    pub(crate) work_budget: WorkBudget,
    max_heap_bytes: u64,
    timeout: Duration,
    max_stack_depth: u32,
    runtime_host: Option<TokioRuntimeHost>,
    capabilities: CapabilitySet,
    loader: Option<module_loader::LoaderConfig>,
    remote_module_provider: Option<Arc<dyn module_loader::RemoteModuleProvider>>,
    hosted_modules: Vec<HostedModule>,
    /// Enable Node-style CommonJS module execution (`require` / `module.exports`
    /// / `__dirname`) for script-shaped sources. Off by default; opt in via
    /// [`RuntimeBuilder::with_nodejs_modules`].
    commonjs_enabled: bool,
    commonjs_addon_loader: Option<CommonJsAddonLoader>,
    global_classes: Vec<GlobalClass>,
    extensions: Vec<&'static Extension>,
    realm_installers: Vec<ConfiguredRealmInstaller>,
    allow_blocking_atomics_wait: bool,
    install_process_global: bool,
    install_worker_global: bool,
    /// Shared hard-limit family every JavaScript `Worker` spawned from this
    /// runtime (and its nested workers) charges. `None` until the root
    /// runtime installs its `Worker` global; child configs inherit the
    /// parent's family so nesting cannot mint fresh limits.
    pub(crate) worker_family: Option<Arc<worker::WorkerFamily>>,
    console_sink: ConsoleSinkHandle,
    promise_rejection_hook: Option<PromiseRejectionHookHandle>,
    hooks: RuntimeHooks,
    /// Explicit process arguments; the installer materializes its default from
    /// the one process snapshot only when this input is absent.
    process_argv: Option<Vec<String>>,
    process_exec_argv: Vec<String>,
    process_cwd: PathBuf,
    process_env_overlay: std::collections::BTreeMap<String, String>,
    warning_options: WarningOptions,
    process_title: Option<String>,
    expose_gc: bool,
    /// `--expose-internals`: user code may load the `internal/*` builtins.
    expose_internals: bool,
    tracer_factory: Option<TracerFactory>,
    /// Dispatch-tick interval for VM stack sampling; `None` leaves the
    /// profiler uninstalled.
    cpu_profile_interval: Option<u64>,
    jit_selection: JitSelection,
    jit_compile_threads: usize,
    jit_debug: JitDebugRequest,
    gc_trace: bool,
    /// Whether each run's result carries the runtime's execution statistics.
    execution_stats: bool,
    /// Restore from the executable's startup image in place of the
    /// bootstrap, when it carries one written for this same API surface.
    startup_image: bool,
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
    ProductionTiered,
    /// Interpreter plus the template tier, without optimizing compilation.
    Template,
    /// Interpreter only — the semantic oracle for differential runs.
    InterpreterOnly,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            resource_account: ResourceAccount::default(),
            completion_capacities: completion_admission::CompletionCapacities {
                guaranteed: completion_admission::DEFAULT_GUARANTEED_COMPLETION_CAPACITY,
                host_operations: completion_admission::DEFAULT_HOST_OPERATION_CAPACITY,
                timers: completion_admission::DEFAULT_TIMER_CAPACITY,
            },
            work_budget: WorkBudget::default(),
            max_heap_bytes: DEFAULT_MAX_HEAP_BYTES,
            timeout: DEFAULT_TIMEOUT,
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            runtime_host: None,
            capabilities: CapabilitySet::default(),
            loader: None,
            remote_module_provider: None,
            hosted_modules: Vec::new(),
            commonjs_enabled: false,
            commonjs_addon_loader: None,
            global_classes: Vec::new(),
            extensions: Vec::new(),
            realm_installers: Vec::new(),
            allow_blocking_atomics_wait: false,
            install_process_global: true,
            install_worker_global: true,
            worker_family: None,
            console_sink: otter_vm::console::default_console_sink(),
            promise_rejection_hook: None,
            hooks: RuntimeHooks::default(),
            process_argv: None,
            process_exec_argv: Vec::new(),
            process_cwd: process::default_cwd(),
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            startup_image: false,
            process_env_overlay: std::collections::BTreeMap::new(),
            tracer_factory: None,
            cpu_profile_interval: None,
            jit_selection: JitSelection::default(),
            jit_compile_threads: DEFAULT_JIT_COMPILE_THREADS,
            jit_debug: JitDebugRequest::default(),
            gc_trace: false,
            execution_stats: true,
        }
    }
}

impl RuntimeConfig {
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn runtime_host(&self) -> Option<TokioRuntimeHost> {
        self.runtime_host.clone()
    }

    pub(crate) const fn work_budget(&self) -> WorkBudget {
        self.work_budget
    }

    pub(crate) const fn completion_capacities(&self) -> completion_admission::CompletionCapacities {
        self.completion_capacities
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

/// Layer B isolate.
#[derive(Debug)]
pub struct Runtime {
    interp: Interpreter,
    /// Where the embedder keeps compiled `eval` text, once it says
    /// ([`Self::set_code_cache_hook`]).
    code_cache_hook: hooks::CodeCacheSlot,
    /// Script sources compiled by this runtime's classic-script path.
    scripts_compiled: std::cell::Cell<u64>,
    /// Whether this runtime came from a snapshot restore rather than a
    /// bootstrap — observable for tests and startup diagnostics.
    restored_from_snapshot: bool,
    /// Process-unique scalar used to reject realm ids from another isolate.
    realm_owner_id: u64,
    config: RuntimeConfig,
    /// Direct runtimes own a watchdog; Layer B enforces deadlines around its
    /// command future and interrupt channel instead.
    enforce_direct_timeout: bool,
    module_loader: RuntimeModuleLoaderState,
    module_graph: RuntimeModuleGraphState,
    module_records: module_records::RuntimeModuleRecords,
    source_maps: RuntimeSourceMapTable,
    diagnostics: RuntimeDiagnosticsSink,
    /// Direct-mode (Layer A) dynamic-import requests. The default
    /// loader installed at construction queues `import()` calls whose
    /// target the linked graph cannot resolve; `run_script` /
    /// `run_module` pump the queue through [`Self::begin_dynamic_import`]
    /// between microtask drains. The isolate runner replaces the
    /// loader at spawn, so this queue stays empty under Layer B.
    layer_a_dynamic_imports: LayerADynamicImportQueue,
    /// Dynamic imports in a web-module runtime whose graph the embedder has
    /// not delivered yet, in the order they were made.
    web_dynamic_imports: Vec<ParkedWebImport>,
    /// Sender for owned tasks that must run on the isolate event loop.
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    /// The CommonJS configuration every `require` of this runtime shares.
    commonjs: Arc<commonjs::CjsConfig>,
    /// Exit requested by JavaScript from a host-driven callback — a timer
    /// fire, a native event delivery, or an `uncaughtException` handler —
    /// after the entry evaluation already completed. An exit is not an
    /// exception: the isolate runner consumes this and completes the
    /// in-flight run with the code, exactly as an exit during entry
    /// evaluation does.
    pending_exit_code: Option<u8>,
    /// A worker entry completed successfully. FIFO child tasks consult this
    /// readiness state; each later callback resolves its own function/source
    /// owner instead of retaining the entry's executable chunk.
    pub(crate) worker_child_ready: bool,
    /// Exact role charge. This field is last so every VM and host-owned
    /// resource is destroyed before the shared ledger releases the isolate,
    /// worker slot, and native stack bytes together.
    /// Construction keeps it `None` only behind a private admission wrapper;
    /// every publicly reachable runtime owns `Some`.
    admission_leases: Option<otter_resource::ResourceLeaseSet>,
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

/// Result of [`Runtime::fire_timer`].
///
/// `Missing` means the token named no live entry — the timer was
/// cancelled, or a one-shot entry already fired. A host scheduler must
/// not decrement liveness for a `Missing` token, or a cancelled timer
/// double-counts as a completed task.
///
/// `Fired { repeat }` reports whether the entry was repeating
/// (`setInterval`). Repeating entries stay in the isolate timer table
/// and the host scheduler re-arms them; one-shot entries are already
/// removed by the time this returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerFireOutcome {
    /// The token named no live entry: cancelled, or already fired.
    Missing,
    /// The callback ran and its microtasks were drained.
    Fired {
        /// `true` for a repeating (`setInterval`) entry, which stays in
        /// the timer table for the host scheduler to re-arm.
        repeat: bool,
    },
}

pub(crate) enum DynamicImportBegin {
    Settled,
    FetchHttps { target_url: String },
}

/// One direct-mode dynamic import and the unique bounded completion carrier
/// acquired before its pending Promise was created.
#[derive(Debug)]
struct LayerADynamicImportRequest {
    admission: completion_admission::CompletionAdmission,
    token: u64,
    specifier: String,
    referrer: String,
    attr_type: Option<String>,
}

/// A web-module dynamic import waiting for the embedder to deliver its graph.
/// The admission stays held, so the import's completion credit is not
/// released while the promise is pending.
#[derive(Debug)]
struct ParkedWebImport {
    admission: completion_admission::CompletionAdmission,
    token: u64,
    target_url: String,
}

/// Shared physically bounded FIFO of dynamic-import requests awaiting the
/// Layer A pump.
type LayerADynamicImportQueue =
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<LayerADynamicImportRequest>>>;

/// Default [`otter_vm::DynamicImportLoader`] for direct-mode runtimes:
/// queues the request for [`Runtime::pump_layer_a_dynamic_imports`].
/// Replaced by the isolate runner's inbox-backed loader under Layer B.
struct LayerADynamicImportLoader {
    queue: LayerADynamicImportQueue,
    completion_pool: completion_admission::CompletionAdmissionPool,
}

impl otter_vm::DynamicImportLoader for LayerADynamicImportLoader {
    fn admit(&self) -> Result<otter_vm::DynamicImportAdmission, String> {
        let admission = self
            .completion_pool
            .admit(completion_admission::CompletionOrigin::HostOperation)
            .map_err(|error| error.to_string())?;
        Ok(otter_vm::DynamicImportAdmission::new(Box::new(admission)))
    }

    fn schedule(
        &self,
        admission: otter_vm::DynamicImportAdmission,
        token: u64,
        specifier: String,
        referrer: String,
        attr_type: Option<String>,
    ) -> Result<(), String> {
        let admission = admission
            .try_into_inner::<completion_admission::CompletionAdmission>()
            .map_err(|_| "foreign Layer A dynamic-import admission carrier".to_string())?;
        if !admission.belongs_to(&self.completion_pool) {
            return Err("Layer A dynamic-import admission belongs to another runtime".to_string());
        }
        self.queue
            .lock()
            .map_err(|_| "Layer A dynamic-import queue is poisoned".to_string())?
            .push_back(LayerADynamicImportRequest {
                admission: *admission,
                token,
                specifier,
                referrer,
                attr_type,
            });
        Ok(())
    }
}

enum DynamicModuleLoad {
    Loaded(otter_vm::Value),
    /// The record is already async-evaluating (top-level await in its
    /// subtree): this import gates on the record's existing evaluation
    /// promise, so it settles after every import that attached earlier.
    GateOnEvaluation {
        promise: otter_vm::JsPromiseHandle,
        target_url: String,
    },
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

struct DirectTimeoutGuard {
    timeout: Duration,
    timed_out: Arc<AtomicBool>,
    interrupt: InterruptFlag,
    cancel: Option<std::sync::mpsc::SyncSender<()>>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl DirectTimeoutGuard {
    fn start(timeout: Duration, interrupt: InterruptFlag) -> Result<Self, OtterError> {
        let (cancel, cancelled) = std::sync::mpsc::sync_channel(1);
        let timed_out = Arc::new(AtomicBool::new(false));
        let watchdog_timed_out = timed_out.clone();
        let watchdog_interrupt = interrupt.clone();
        let watchdog = std::thread::Builder::new()
            .name("otter-direct-watchdog".to_string())
            .spawn(move || {
                if matches!(
                    cancelled.recv_timeout(timeout),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    watchdog_timed_out.store(true, Ordering::Release);
                    watchdog_interrupt.interrupt();
                }
            })
            .map_err(|error| OtterError::Internal {
                code: "DIRECT_TIMEOUT_WATCHDOG".to_string(),
                message: error.to_string(),
            })?;
        Ok(Self {
            timeout,
            timed_out,
            interrupt,
            cancel: Some(cancel),
            watchdog: Some(watchdog),
        })
    }

    fn finish<T>(mut self, result: Result<T, OtterError>) -> Result<T, OtterError> {
        self.cancel.take();
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        if self.timed_out.load(Ordering::Acquire) {
            self.interrupt.reset();
            return Err(OtterError::timeout_after(self.timeout));
        }
        result
    }
}

impl Drop for DirectTimeoutGuard {
    fn drop(&mut self) {
        self.cancel.take();
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        if self.timed_out.load(Ordering::Acquire) {
            self.interrupt.reset();
        }
    }
}

fn install_expose_gc_global(interp: &mut Interpreter) -> Result<(), OtterError> {
    let global_object = *interp.global_this();
    let result: Result<(), otter_vm::NativeError> =
        NativeCtx::with_host_context(interp, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|mut scope| {
                let global_object = scope.value(otter_vm::Value::object(global_object));
                let gc =
                    scope.native_call("gc", 0, otter_vm::NativeCall::Static(force_gc_native))?;
                scope.define(
                    global_object,
                    "gc",
                    gc,
                    otter_vm::Attr {
                        writable: true,
                        enumerable: false,
                        configurable: true,
                    }
                    .to_flags(),
                )
            })
        });
    result.map_err(|error| OtterError::Internal {
        code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
        message: format!("--expose-gc bootstrap failed: {error}"),
    })
}

fn force_gc_native(
    ctx: &mut otter_vm::NativeCtx<'_>,
    _args: otter_vm::ValueWindow<'_>,
) -> Result<otter_vm::Value, otter_vm::NativeError> {
    let _ = ctx.interp_mut().force_gc();
    Ok(otter_vm::Value::undefined())
}

#[cfg(test)]
mod tests;
