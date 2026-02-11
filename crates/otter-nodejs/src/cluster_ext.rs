//! Stub `node:cluster` extension.
//!
//! Provides minimal exports needed by Node.js test harness (`common/index.js`):
//! `isPrimary`, `isMaster`, `isWorker`.

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::JsObject;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeClusterExtension;

impl OtterExtension for NodeClusterExtension {
    fn name(&self) -> &str {
        "node_cluster"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:cluster", "cluster"];
        &S
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let ns = ctx
            .module_namespace()
            .property("isPrimary", Value::boolean(true))
            .property("isMaster", Value::boolean(true))
            .property("isWorker", Value::boolean(false))
            .build();

        Some(ns)
    }
}

pub fn node_cluster_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeClusterExtension)
}
