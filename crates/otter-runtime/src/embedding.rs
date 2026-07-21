//! Owned, high-level embedding surface.
//!
//! This module is the preferred import path for applications hosting Otter.
//! It collects the browser/server/game-safe orchestration API without exposing
//! interpreter internals, JavaScript values, objects, GC handles, or native
//! mutation contexts. Declarative extension authors may use the crate's
//! advanced binding surface separately.
//!
//! # Contents
//! - Direct thread-pinned and sendable isolate/runtime entry points.
//! - Opaque realm lifecycle and realm-targeted script/module execution types.
//! - Shared Tokio host, owned task delivery, capabilities, hooks, diagnostics,
//!   and canonical in-memory module loading.
//!
//! # Invariants
//! - Every type crossing a runtime thread boundary is owned and `Send`.
//! - [`RuntimeRealmId`](crate::RuntimeRealmId) is opaque and isolate-bound.
//! - No VM/GC handle is re-exported from this module.
//! - Browser concepts such as DOM, navigation, origins, frames, storage, and
//!   broadcast registries remain embedder-owned extensions.
//!
//! # See also
//! - [Browser embedding](../../../docs/site/src/content/docs/extensions/browser-embedding.md)
//! - [`crate::surface`] for advanced declarative extension implementation.

pub use crate::module_loader::{
    ModuleLoadCancellation, RemoteModuleError, RemoteModuleFuture, RemoteModuleProvider,
    RemoteModuleRequest, RemoteModuleSource,
};
pub use crate::{
    CapabilityRequest, CapabilitySet, ConfigError, ConsoleLevel, ConsoleSink, ConsoleSinkHandle,
    Diagnostic, DiagnosticCategory, DiagnosticCode, DiagnosticKind, ExecutionAttempt,
    ExecutionResult, HostAtomInterner, InterruptHandle, IoErrorKind, Otter, OtterBuilder,
    OtterError, Permission, RealmError, Runtime, RuntimeActivityStats, RuntimeBuilder,
    RuntimeCapability, RuntimeCapabilityHook, RuntimeCompileHook, RuntimeDiagnosticHook,
    RuntimeGlobalInstaller, RuntimeGlobalValue, RuntimeHandle, RuntimeHooks, RuntimeHostAtom,
    RuntimeHostAtomId, RuntimeJobHook, RuntimeLoadHook, RuntimeRealmContext, RuntimeRealmId,
    RuntimeResolveHook, RuntimeTask, RuntimeTaskSpawner, SourceInput, StackFrame, TokioRuntimeHost,
};
