//! Runtime-owned ES module records.
//!
//! The bytecode linker describes module initializers, but the runtime owns the
//! host-side module records that exist before evaluation starts. This module
//! keeps that state explicit: each linked module gets a record, a module-env
//! object, and an evaluation state before the VM dispatches the synthesized
//! `<entry>` function.
//!
//! # Contents
//! - [`RuntimeModuleRecords`] — per-runtime module-record table.
//! - [`RuntimeModuleRecord`] — one allocated record.
//! - [`RuntimeModuleRecordState`] — foundation lifecycle states.
//!
//! # Invariants
//! - Records are allocated for every `BytecodeModule::module_inits` entry
//!   before module evaluation begins.
//! - The VM module-env registry is populated from these records, not from an
//!   ad-hoc allocation loop in `Runtime::run_module`.
//! - Records are runtime-owned and never expose raw VM handles through public
//!   embedding APIs.
//!
//! # See also
//! - [`crate::module_graph`]
//! - <https://tc39.es/ecma262/#sec-source-text-module-records>

use std::collections::BTreeMap;
use std::rc::Rc;

use otter_bytecode::BytecodeModule;
use otter_vm::{Interpreter, JsObject};

use crate::{CapabilitySet, ConfigError, HostedModule, OtterError};

/// Foundation module-record lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeModuleRecordState {
    /// Record and environment object are allocated, but evaluation has not
    /// started.
    Allocated,
    /// The linked entry driver is currently evaluating module initializers.
    Evaluating,
    /// Evaluation completed successfully.
    Evaluated,
    /// Evaluation failed with a runtime error.
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
    /// Allocate records and module-env objects for a linked module program.
    ///
    /// This also resets and repopulates the VM registry used by
    /// `Op::ImportNamespace`, so the VM reads the environment objects owned by
    /// these records during evaluation.
    pub(crate) fn allocate_for_bytecode(
        &mut self,
        interp: &mut Interpreter,
        module: &BytecodeModule,
        hosted_modules: &[HostedModule],
        capabilities: &CapabilitySet,
    ) -> Result<(), OtterError> {
        self.records.clear();
        interp.reset_module_state();
        for init in &module.module_inits {
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
            self.records.insert(
                init.url.clone(),
                RuntimeModuleRecord {
                    function_id: init.function_id,
                    env,
                    state: RuntimeModuleRecordState::Allocated,
                },
            );
        }
        Ok(())
    }

    /// Mark all allocated records as evaluating.
    pub(crate) fn mark_evaluating(&mut self) {
        for record in self.records.values_mut() {
            record.state = RuntimeModuleRecordState::Evaluating;
        }
    }

    /// Mark all allocated records as evaluated.
    pub(crate) fn mark_evaluated(&mut self) {
        for record in self.records.values_mut() {
            record.state = RuntimeModuleRecordState::Evaluated;
        }
    }

    /// Mark all allocated records as errored.
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
