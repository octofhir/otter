//! Runtime-owned ES module records.
//!
//! The bytecode linker describes module initializers; this module owns the
//! per-URL host record that tracks each module across its
//! ECMA-262 §16.2 lifecycle. The state machine mirrors the spec phases so
//! diagnostics, cycle handling, and incremental driver hooks have one
//! authoritative source of truth.
//!
//! # Contents
//! - [`RuntimeModuleRecords`] — per-realm module-record tables owned by one
//!   runtime isolate.
//! - [`RuntimeModuleRecord`] — one allocated record.
//! - [`RuntimeModuleRecordState`] — spec-aligned lifecycle states.
//!
//! # Invariants
//! - Each realm owns an independent module map. Repeated entry graphs in one
//!   realm reuse evaluated records and environments by canonical URL.
//! - Each record advances monotonically through the phase order
//!   `Unresolved → Resolved → Compiled → Instantiated → Evaluating →
//!   Evaluated|Errored`. The transition methods enforce that ordering;
//!   skipping a phase or going backwards is a programmer bug.
//! - The VM module-env registry is the sole owner/root of allocated
//!   environments. Runtime records retain the owning [`ExecutionContext`] for
//!   their initializer id, but never retain raw VM handles.
//! - Cycle support: a module that the loader has already started
//!   instantiating is in [`RuntimeModuleRecordState::Instantiated`] (or
//!   later) by the time a back-edge revisits it. The host treats the
//!   existing record as authoritative; live-binding indirection through
//!   the env object handles late-bound exports.
//!
//! # See also
//! - [`crate::module_graph`]
//! - <https://tc39.es/ecma262/#sec-source-text-module-records>
//! - <https://tc39.es/ecma262/#sec-cyclic-module-records>
//! - <https://tc39.es/ecma262/#sec-InnerModuleEvaluation>

use otter_bytecode::ModuleInit;
use otter_vm::{ExecutionContext, Interpreter, NativeCallInfo, NativeCtx, NativeError};
use std::collections::BTreeMap;

use crate::{CapabilitySet, HostedModule, OtterError, RuntimeTaskSpawner};

/// Lifecycle phases per ECMA-262 §16.2 Cyclic Module Records.
///
/// The variants match the spec phases that have observable
/// behavior at the host boundary. The runtime advances each
/// record through the phases in order as load/compile/instantiate/
/// evaluate hooks fire.
//
// `Unresolved`, `Resolved`, and `Compiled` are part of the spec
// lifecycle but currently the load pipeline batches them under a
// single hand-off to `allocate_for_module_inits` (the linker has
// already done resolve + compile + link by then). The variants
// are kept on the enum so the per-phase loader hooks, when they
// land, route through the same authoritative state machine.
#[allow(dead_code, reason = "phases reserved for per-loader-hook transitions")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeModuleRecordState {
    /// URL identified, source has not yet been read by the loader.
    Unresolved,
    /// Source text loaded into memory.
    Resolved,
    /// Bytecode fragment compiled from source.
    Compiled,
    /// Module env (and namespace exotic object) allocated and
    /// registered in the VM; imports linked. Body has not run.
    /// Spec §16.2.1.6 InitializeEnvironment / §16.2.1.10
    /// InnerModuleLinking exit state.
    Instantiated,
    /// Module body is currently executing (its `<module-init>`
    /// frame is on the VM stack).
    Evaluating,
    /// Module body completed successfully.
    Evaluated,
    /// Module body raised an uncaught error during evaluation.
    Errored,
}

/// One runtime-owned module record.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeModuleRecord {
    /// Function id of this module's `<module-init>` inside linked bytecode.
    pub(crate) function_id: u32,
    /// Current lifecycle state.
    pub(crate) state: RuntimeModuleRecordState,
    /// Owning linked chunk for `function_id` once the bytecode is admitted.
    context: Option<ExecutionContext>,
}

/// Per-realm tables of allocated module records owned by one runtime.
#[derive(Debug, Default)]
pub(crate) struct RuntimeModuleRecords {
    realms: BTreeMap<u32, BTreeMap<String, RuntimeModuleRecord>>,
}

impl RuntimeModuleRecords {
    /// Allocate records and module-env objects for linked module init records.
    ///
    /// Walks each linked `<module-init>` URL and emits the
    /// `Unresolved → Resolved → Compiled → Instantiated`
    /// transitions, mirroring the phases each fragment already
    /// went through during the graph load + linker pipeline.
    /// Future slices may split the earlier transitions out into
    /// per-phase hooks; the per-record state machine stays
    /// authoritative either way.
    ///
    /// Existing canonical URLs in the active realm are retained. This is the
    /// browser/module-map rule: a dependency imported by a later entry module
    /// observes the same environment and is not evaluated twice. Every new
    /// allocation, hosted installer, cache publication, and registry
    /// publication runs in one native handle scope; after registration the VM
    /// registry is the environment's sole root.
    pub(crate) fn allocate_for_module_inits(
        &mut self,
        interp: &mut Interpreter,
        module_inits: &[ModuleInit],
        hosted_modules: &[HostedModule],
        capabilities: &CapabilitySet,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<(), OtterError> {
        let realm_id = interp.active_host_realm_id();
        // Synthesizing a namespace from a CommonJS builtin runs JavaScript (the
        // module's own wrapper and `require` graph), which needs a linked
        // context to dispatch into. Build one only when a module in this batch
        // actually needs it.
        let needs_javascript = module_inits.iter().any(|init| {
            hosted_modules.iter().any(|hosted| {
                hosted.specifier() == init.url && hosted.commonjs_value_install().is_some()
            })
        });
        let host_context = if needs_javascript {
            let empty = otter_compiler::compile_script_source(
                "",
                otter_syntax::SourceKind::JavaScript,
                "<hosted-namespace-root>",
            )
            .map_err(|error| OtterError::HostedModule {
                specifier: "<hosted-namespace-root>".to_string(),
                message: format!("{error:?}"),
            })?;
            Some(interp.link_module(empty)?)
        } else {
            None
        };
        let records = self.realms.entry(realm_id).or_default();
        NativeCtx::with_host_context(
            interp,
            NativeCallInfo::default_call(),
            host_context.as_ref(),
            |ctx| {
                ctx.scope(|mut scope| {
                    for init in module_inits {
                        if records.contains_key(&init.url) {
                            continue;
                        }
                        let env = if let Some(hosted) = hosted_modules
                            .iter()
                            .copied()
                            .find(|hosted| hosted.specifier() == init.url)
                        {
                            // One namespace per specifier per isolate: the
                            // installer's side effects must run once, and a
                            // namespace-only CommonJS load shares this object.
                            match scope.cached_host_module_env(init.url.as_str()) {
                                Some(env) => env,
                                None => {
                                    // A module that publishes a CommonJS value is
                                    // served from that value, so `default` is the
                                    // object `require` returns. Only a module
                                    // without one falls back to its namespace
                                    // installer, which then becomes its own
                                    // `default`.
                                    let env = if hosted.commonjs_value_install().is_some() {
                                        synthesize_commonjs_namespace(
                                            &mut scope,
                                            &init.url,
                                            hosted_modules,
                                            capabilities,
                                            runtime_task_spawner.clone(),
                                        )?
                                    } else {
                                        let install =
                                            hosted.namespace_install().ok_or_else(|| {
                                                OtterError::HostedModule {
                                                    specifier: init.url.clone(),
                                                    message: "module exposes neither a namespace \
                                                          nor a CommonJS value"
                                                        .to_string(),
                                                }
                                            })?;
                                        let namespace = install(
                                            &mut scope,
                                            capabilities,
                                            runtime_task_spawner.clone(),
                                        )
                                        .map_err(|error| OtterError::HostedModule {
                                            specifier: init.url.clone(),
                                            message: error.to_string(),
                                        })?;
                                        scope.set(namespace, "default", namespace).map_err(
                                            |error| OtterError::HostedModule {
                                                specifier: init.url.clone(),
                                                message: error.to_string(),
                                            },
                                        )?;
                                        namespace
                                    };
                                    scope
                                        .cache_host_module_env(init.url.as_str(), env)
                                        .map_err(|error| OtterError::HostedModule {
                                            specifier: init.url.clone(),
                                            message: error.to_string(),
                                        })?;
                                    env
                                }
                            }
                        } else {
                            scope.bare_object().map_err(module_allocation_error)?
                        };
                        scope
                            .register_module_env(init.url.as_str(), env)
                            .map_err(module_allocation_error)?;
                        // The graph load + linker pipeline has already done
                        // resolve + compile + linking by the time we get here.
                        records.insert(
                            init.url.clone(),
                            RuntimeModuleRecord {
                                function_id: init.function_id,
                                state: RuntimeModuleRecordState::Instantiated,
                                context: None,
                            },
                        );
                    }
                    Ok(())
                })
            },
        )
    }

    /// Retain the linked chunk that owns newly allocated module records.
    ///
    /// Existing records from an earlier graph keep their original context:
    /// their canonical URL may also occur in this graph with a different
    /// linker-assigned function id.
    pub(crate) fn retain_linked_context(&mut self, realm_id: u32, context: &ExecutionContext) {
        let Some(records) = self.realms.get_mut(&realm_id) else {
            return;
        };
        for init in context.module_inits() {
            let Some(record) = records.get_mut(&init.url) else {
                continue;
            };
            if record.function_id == init.function_id && record.context.is_none() {
                record.context = Some(context.clone());
            }
        }
    }

    /// Mark all instantiated records as evaluating. Called once
    /// before the synthesised `<entry>` driver dispatches the
    /// first `<module-init>`.
    pub(crate) fn mark_evaluating(&mut self, realm_id: u32) {
        for record in self.realms.entry(realm_id).or_default().values_mut() {
            if record.state == RuntimeModuleRecordState::Instantiated {
                record.state = RuntimeModuleRecordState::Evaluating;
            }
        }
    }

    /// Mark all evaluating records as evaluated. Called when the
    /// `<entry>` driver returns successfully.
    pub(crate) fn mark_evaluated(&mut self, realm_id: u32) {
        for record in self.realms.entry(realm_id).or_default().values_mut() {
            if record.state == RuntimeModuleRecordState::Evaluating {
                record.state = RuntimeModuleRecordState::Evaluated;
            }
        }
    }

    /// Mark all in-progress records as errored. Called when any
    /// `<module-init>` raises an uncaught exception.
    pub(crate) fn mark_errored(&mut self, realm_id: u32) {
        for record in self.realms.entry(realm_id).or_default().values_mut() {
            if record.state == RuntimeModuleRecordState::Evaluating {
                record.state = RuntimeModuleRecordState::Errored;
            }
        }
    }

    /// Visit allocated records in deterministic URL order.
    pub(crate) fn for_each_record(&self, realm_id: u32, mut f: impl FnMut(&str, u32)) {
        if let Some(records) = self.realms.get(&realm_id) {
            for (url, record) in records {
                f(url, record.function_id);
            }
        }
    }

    /// Drop lifecycle metadata owned by a disposed realm.
    pub(crate) fn dispose_realm(&mut self, realm_id: u32) {
        self.realms.remove(&realm_id);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.realms.values().map(BTreeMap::len).sum()
    }

    #[cfg(test)]
    pub(crate) fn state(&self, url: &str) -> Option<RuntimeModuleRecordState> {
        self.realms
            .get(&0)
            .and_then(|records| records.get(url))
            .map(|record| record.state)
    }
}

/// Build the ESM namespace for a builtin that publishes only a CommonJS value.
///
/// The value is produced by the CommonJS loader itself, so the builtin's own
/// `require` graph runs exactly as it does for `require()`. `default` is that
/// value and every own enumerable string key becomes a named export, which is
/// how Node presents a CommonJS-backed builtin to an `import`.
fn synthesize_commonjs_namespace<'scope>(
    scope: &mut otter_vm::NativeScope<'scope, '_>,
    specifier: &str,
    hosted_modules: &[HostedModule],
    capabilities: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<otter_vm::Local<'scope>, OtterError> {
    let cfg = std::sync::Arc::new(crate::commonjs::CjsConfig {
        capabilities: capabilities.clone(),
        hosted: hosted_modules.to_vec(),
        runtime_task_spawner,
        addon_loader: None,
        report_watch_dependencies: crate::commonjs::watch_reporting_requested(),
    });
    let hosted_error = |error: otter_vm::NativeError| OtterError::HostedModule {
        specifier: specifier.to_string(),
        message: error.to_string(),
    };

    let exports =
        crate::commonjs::cjs_load_builtin(scope, &cfg, specifier).map_err(hosted_error)?;
    let namespace = scope.bare_object().map_err(hosted_error)?;
    scope
        .set(namespace, "default", exports)
        .map_err(hosted_error)?;
    for key in scope
        .enumerable_own_string_keys(exports)
        .map_err(hosted_error)?
    {
        if key == "default" {
            continue;
        }
        let value = scope.get(exports, &key).map_err(hosted_error)?;
        scope.set(namespace, &key, value).map_err(hosted_error)?;
    }
    Ok(namespace)
}

fn module_allocation_error(error: NativeError) -> OtterError {
    match error {
        NativeError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
            ..
        } => OtterError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        error => OtterError::Internal {
            code: "MODULE_ENV_INSTALL".to_string(),
            message: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RuntimeModuleRecordState` advances monotonically per spec
    /// §16.2.1 lifecycle. The ordering encoded here is what the
    /// `mark_*` transitions expect; future hooks must preserve
    /// this total order.
    #[test]
    fn lifecycle_phases_are_totally_ordered() {
        fn rank(state: RuntimeModuleRecordState) -> u8 {
            match state {
                RuntimeModuleRecordState::Unresolved => 0,
                RuntimeModuleRecordState::Resolved => 1,
                RuntimeModuleRecordState::Compiled => 2,
                RuntimeModuleRecordState::Instantiated => 3,
                RuntimeModuleRecordState::Evaluating => 4,
                RuntimeModuleRecordState::Evaluated => 5,
                RuntimeModuleRecordState::Errored => 5,
            }
        }
        let phases = [
            RuntimeModuleRecordState::Unresolved,
            RuntimeModuleRecordState::Resolved,
            RuntimeModuleRecordState::Compiled,
            RuntimeModuleRecordState::Instantiated,
            RuntimeModuleRecordState::Evaluating,
            RuntimeModuleRecordState::Evaluated,
        ];
        for window in phases.windows(2) {
            assert!(
                rank(window[0]) < rank(window[1]),
                "{:?} must precede {:?}",
                window[0],
                window[1]
            );
        }
        // Errored is a terminal alternative to Evaluated.
        assert_eq!(
            rank(RuntimeModuleRecordState::Evaluated),
            rank(RuntimeModuleRecordState::Errored)
        );
    }
}
