//! Stub `node:child_process` extension.
//!
//! Provides minimal exports needed by Node.js test harness (`common/tmpdir.js`):
//! `spawnSync`, `execSync`, `spawn`, `exec`, `fork`.

use otter_macros::{js_class, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

use std::sync::Arc;

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeChildProcessExtension;

impl OtterExtension for NodeChildProcessExtension {
    fn name(&self) -> &str {
        "node_child_process"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:child_process", "child_process"];
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
        type DeclFn = fn() -> (
            &'static str,
            Arc<
                dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
            u32,
        );

        let fns: &[DeclFn] = &[
            ChildProcessStub::spawn_sync_decl,
            ChildProcessStub::exec_sync_decl,
            ChildProcessStub::spawn_decl,
            ChildProcessStub::exec_decl,
            ChildProcessStub::fork_decl,
        ];

        let mut ns = ctx.module_namespace();
        for decl in fns {
            let (name, func, length) = decl();
            ns = ns.function(name, func, length);
        }

        Some(ns.build())
    }
}

pub fn node_child_process_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeChildProcessExtension)
}

// ---------------------------------------------------------------------------
// Stub functions via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "ChildProcessStub")]
pub struct ChildProcessStub;

#[js_class]
impl ChildProcessStub {
    #[js_static(name = "spawnSync", length = 1)]
    pub fn spawn_sync(
        _this: &Value,
        _args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        // Return a minimal result object: { status: 0, stdout: '', stderr: '', signal: null }
        let mm = ncx.memory_manager().clone();
        let result = GcRef::new(JsObject::new(Value::null(), mm));
        let _ = result.set(PropertyKey::string("status"), Value::int32(0));
        let _ = result.set(
            PropertyKey::string("stdout"),
            Value::string(JsString::intern("")),
        );
        let _ = result.set(
            PropertyKey::string("stderr"),
            Value::string(JsString::intern("")),
        );
        let _ = result.set(PropertyKey::string("signal"), Value::null());
        Ok(Value::object(result))
    }

    #[js_static(name = "execSync", length = 1)]
    pub fn exec_sync(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "child_process.execSync is not supported in this runtime",
        ))
    }

    #[js_static(name = "spawn", length = 1)]
    pub fn spawn(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "child_process.spawn is not supported in this runtime",
        ))
    }

    #[js_static(name = "exec", length = 1)]
    pub fn exec(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "child_process.exec is not supported in this runtime",
        ))
    }

    #[js_static(name = "fork", length = 1)]
    pub fn fork(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "child_process.fork is not supported in this runtime",
        ))
    }
}
