//! Stub `node:net` extension.
//!
//! Provides minimal exports needed by Node.js test harness (`common/index.js`):
//! `setDefaultAutoSelectFamilyAttemptTimeout`, `getDefaultAutoSelectFamilyAttemptTimeout`.

use otter_macros::{js_class, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::JsObject;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default auto-select family attempt timeout in milliseconds.
static AUTO_SELECT_TIMEOUT: AtomicU64 = AtomicU64::new(250);

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeNetExtension;

impl OtterExtension for NodeNetExtension {
    fn name(&self) -> &str {
        "node_net"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:net", "net"];
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
            NetStub::set_default_auto_select_family_attempt_timeout_decl,
            NetStub::get_default_auto_select_family_attempt_timeout_decl,
            NetStub::create_server_decl,
            NetStub::create_connection_decl,
        ];

        let mut ns = ctx.module_namespace();
        for decl in fns {
            let (name, func, length) = decl();
            ns = ns.function(name, func, length);
        }

        Some(ns.build())
    }
}

pub fn node_net_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeNetExtension)
}

// ---------------------------------------------------------------------------
// Stub functions via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "NetStub")]
pub struct NetStub;

#[js_class]
impl NetStub {
    #[js_static(name = "setDefaultAutoSelectFamilyAttemptTimeout", length = 1)]
    pub fn set_default_auto_select_family_attempt_timeout(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Some(ms) = args.first().and_then(|v| v.as_number()) {
            AUTO_SELECT_TIMEOUT.store(ms as u64, Ordering::Relaxed);
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "getDefaultAutoSelectFamilyAttemptTimeout", length = 0)]
    pub fn get_default_auto_select_family_attempt_timeout(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::number(
            AUTO_SELECT_TIMEOUT.load(Ordering::Relaxed) as f64
        ))
    }

    #[js_static(name = "createServer", length = 0)]
    pub fn create_server(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "net.createServer is not supported in this runtime",
        ))
    }

    #[js_static(name = "createConnection", length = 0)]
    pub fn create_connection(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Err(VmError::type_error(
            "net.createConnection is not supported in this runtime",
        ))
    }
}
