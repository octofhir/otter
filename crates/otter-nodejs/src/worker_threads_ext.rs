//! Stub `node:worker_threads` extension.
//!
//! Provides minimal exports needed by Node.js test harness (`common/index.js`):
//! `isMainThread`, `parentPort`, `workerData`, `threadId`, `Worker`.

use otter_macros::{js_class, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::JsObject;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeWorkerThreadsExtension;

impl OtterExtension for NodeWorkerThreadsExtension {
    fn name(&self) -> &str {
        "node_worker_threads"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:worker_threads", "worker_threads"];
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
        let (worker_name, worker_fn, worker_len) = WorkerThreadsStub::worker_decl();
        let ns = ctx
            .module_namespace()
            .property("isMainThread", Value::boolean(true))
            .property("parentPort", Value::null())
            .property("workerData", Value::undefined())
            .property("threadId", Value::int32(0))
            .function(worker_name, worker_fn, worker_len)
            .build();

        Some(ns)
    }
}

pub fn node_worker_threads_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeWorkerThreadsExtension)
}

// ---------------------------------------------------------------------------
// Stub functions via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "WorkerThreadsStub")]
pub struct WorkerThreadsStub;

#[js_class]
impl WorkerThreadsStub {
    #[js_static(name = "Worker", length = 1)]
    pub fn worker(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "Worker is not supported in this runtime",
        ))
    }
}
