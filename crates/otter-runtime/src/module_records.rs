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
//! - Native namespace preparation runs in the importing graph's context, which
//!   enumerating a CommonJS-backed builtin's exports needs. Embedded CommonJS
//!   wrappers enter through their actual admitted function id and source; the
//!   final module record publishes its actual rebased initializer id and owning
//!   context together, immediately after the environment is registered.
//! - Allocation and installer failures stay in the existing native completion
//!   domain until the entry owner maps or materializes them in this realm.
//!   No installer error becomes a host-text diagnostic inside this allocator.
//! - A CommonJS-backed namespace requires a discovered builtin: only discovery
//!   absence becomes the named import TypeError; installation errors pass through.
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

use otter_vm::{ExecutionContext, Interpreter, NativeCallInfo, NativeCtx, NativeError};
use std::collections::BTreeMap;


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
    /// Actual admitted chunk whose rebased function table owns `function_id`.
    context: ExecutionContext,
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
    /// registry is the environment's sole root. Native preparation runs in the
    /// supplied graph context; each invoked embedded CommonJS wrapper resolves
    /// its own admitted source/function owner through the canonical call path.
    /// Failures remain typed native completions, including original thrown
    /// identity and imported terminal detail, until the active-realm consumer
    /// performs its canonical runtime or rejection projection. The supplied
    /// context has already passed verified linking and function-id rebasing;
    /// every successfully registered environment immediately retains that
    /// owner, even if a later installer in this graph fails.
    pub(crate) fn allocate_for_module_inits(
        &mut self,
        interp: &mut Interpreter,
        context: &ExecutionContext,
        commonjs: &std::sync::Arc<crate::commonjs::CjsConfig>,
    ) -> Result<(), NativeError> {
        let realm_id = interp.active_host_realm_id();
        let records = self.realms.entry(realm_id).or_default();
        // The importing graph's context: a CommonJS-only builtin's namespace
        // is synthesized by running its loader and enumerating its exports.
        NativeCtx::with_host_context(
            interp,
            NativeCallInfo::default_call(),
            Some(context),
            |ctx| {
                ctx.scope(|mut scope| {
                    for init in context.module_inits() {
                        if records.contains_key(&init.url) {
                            continue;
                        }
                        let env = if let Some(hosted) = commonjs
                            .hosted
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
                                            commonjs,
                                        )?
                                    } else {
                                        let install = hosted
                                            .namespace_install()
                                            .ok_or(NativeError::InvalidOperand)?;
                                        let namespace = install(
                                            &mut scope,
                                            &commonjs.capabilities,
                                            commonjs.runtime_task_spawner.clone(),
                                        )?;
                                        scope.set(namespace, "default", namespace)?;
                                        namespace
                                    };
                                    scope.cache_host_module_env(init.url.as_str(), env)?;
                                    env
                                }
                            }
                        } else {
                            scope.bare_object()?
                        };
                        scope.register_module_env(init.url.as_str(), env)?;
                        // The graph load + linker pipeline has already done
                        // resolve + compile + linking by the time we get here.
                        records.insert(
                            init.url.clone(),
                            RuntimeModuleRecord {
                                function_id: init.function_id,
                                state: RuntimeModuleRecordState::Instantiated,
                                context: context.clone(),
                            },
                        );
                    }
                    Ok(())
                })
            },
        )
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
                debug_assert!(record.context.function(record.function_id).is_some());
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
    commonjs: &std::sync::Arc<crate::commonjs::CjsConfig>,
) -> Result<otter_vm::Local<'scope>, NativeError> {
    let exports = crate::commonjs::cjs_load_builtin(scope, commonjs, specifier)?.ok_or_else(|| {
        crate::runtime_type_error("import", format!("no builtin module named '{specifier}'"))
    })?;
    let namespace = scope.bare_object()?;
    scope.set(namespace, "default", exports)?;
    for key in scope.enumerable_own_string_keys(exports)? {
        if key == "default" {
            continue;
        }
        let value = scope.get(exports, &key)?;
        scope.set(namespace, &key, value)?;
    }
    Ok(namespace)
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

#[cfg(test)]
mod completion_tests;
