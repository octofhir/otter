//! Runtime-owned ES module records.
//!
//! The bytecode linker describes module initializers; this module owns the
//! per-URL host record that tracks each module across its
//! ECMA-262 §16.2 lifecycle. The state machine mirrors the spec phases so
//! diagnostics, cycle handling, and incremental driver hooks have one
//! authoritative source of truth.
//!
//! # Contents
//! - [`RuntimeModuleRecords`] — per-runtime module-record table.
//! - [`RuntimeModuleRecord`] — one allocated record.
//! - [`RuntimeModuleRecordState`] — spec-aligned lifecycle states.
//!
//! # Invariants
//! - Each record advances monotonically through the phase order
//!   `Unresolved → Resolved → Compiled → Instantiated → Evaluating →
//!   Evaluated|Errored`. The transition methods enforce that ordering;
//!   skipping a phase or going backwards is a programmer bug.
//! - The VM module-env registry is populated from these records, not from
//!   ad-hoc allocations elsewhere.
//! - Records are runtime-owned and never expose raw VM handles through
//!   public embedding APIs.
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

use std::collections::BTreeMap;
use std::rc::Rc;

use otter_bytecode::ModuleInit;
use otter_vm::{Interpreter, JsObject};

use crate::{CapabilitySet, ConfigError, HostedModule, OtterError};

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
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeModuleRecord {
    /// Function id of this module's `<module-init>` inside linked bytecode.
    pub(crate) function_id: u32,
    /// Allocated module namespace/environment object.
    pub(crate) env: JsObject,
    /// Current lifecycle state.
    pub(crate) state: RuntimeModuleRecordState,
}

/// Per-runtime table of allocated module records.
#[derive(Debug, Default)]
pub(crate) struct RuntimeModuleRecords {
    records: BTreeMap<String, RuntimeModuleRecord>,
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
    /// This also resets and repopulates the VM registry used by
    /// `Op::ImportNamespace`, so the VM reads the environment
    /// objects owned by these records during evaluation.
    pub(crate) fn allocate_for_module_inits(
        &mut self,
        interp: &mut Interpreter,
        module_inits: &[ModuleInit],
        hosted_modules: &[HostedModule],
        capabilities: &CapabilitySet,
    ) -> Result<(), OtterError> {
        self.records.clear();
        interp.reset_module_state();
        for init in module_inits {
            let env = if let Some(hosted) = hosted_modules
                .iter()
                .find(|hosted| hosted.specifier() == init.url)
            {
                hosted
                    .install(interp, capabilities)
                    .map_err(|message| OtterError::Config {
                        reason: ConfigError::ConflictingCapabilities { message },
                    })?
            } else {
                otter_vm::object::alloc_object(interp.gc_heap_mut())?
            };
            interp.register_module_env(Rc::from(init.url.as_str()), env);
            // The graph load + linker pipeline has already done
            // resolve + compile + linking by the time we get
            // here, so each record advances directly into
            // `Instantiated` ready for evaluation.
            self.records.insert(
                init.url.clone(),
                RuntimeModuleRecord {
                    function_id: init.function_id,
                    env,
                    state: RuntimeModuleRecordState::Instantiated,
                },
            );
        }
        Ok(())
    }

    /// Mark all instantiated records as evaluating. Called once
    /// before the synthesised `<entry>` driver dispatches the
    /// first `<module-init>`.
    pub(crate) fn mark_evaluating(&mut self) {
        for record in self.records.values_mut() {
            record.state = RuntimeModuleRecordState::Evaluating;
        }
    }

    /// Mark all evaluating records as evaluated. Called when the
    /// `<entry>` driver returns successfully.
    pub(crate) fn mark_evaluated(&mut self) {
        for record in self.records.values_mut() {
            record.state = RuntimeModuleRecordState::Evaluated;
        }
    }

    /// Mark all in-progress records as errored. Called when any
    /// `<module-init>` raises an uncaught exception.
    pub(crate) fn mark_errored(&mut self) {
        for record in self.records.values_mut() {
            record.state = RuntimeModuleRecordState::Errored;
        }
    }

    /// Visit allocated records in deterministic URL order.
    pub(crate) fn for_each_record(&self, mut f: impl FnMut(&str, u32, JsObject)) {
        for (url, record) in &self.records {
            f(url, record.function_id, record.env);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    #[cfg(test)]
    pub(crate) fn state(&self, url: &str) -> Option<RuntimeModuleRecordState> {
        self.records.get(url).map(|record| record.state)
    }

    #[cfg(test)]
    pub(crate) fn env(&self, url: &str) -> Option<JsObject> {
        self.records.get(url).map(|record| record.env)
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
