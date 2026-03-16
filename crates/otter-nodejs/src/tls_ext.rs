//! Minimal native `node:tls` surface for constructor/prototype shape checks.
//!
//! Spec references:
//! - Node.js `node:tls`: <https://nodejs.org/api/tls.html>
//! - `tls.TLSSocket`: <https://nodejs.org/api/tls.html#class-tlstlssocket>

use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::object::PropertyKey;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

const SOCKET_CTOR_GLOBAL: &str = "__NetSocketCtor";
const TLS_SOCKET_CTOR_GLOBAL: &str = "__TlsSocketCtor";

/// Native registration entry for Node's `node:tls` builtin.
///
/// Spec: <https://nodejs.org/api/tls.html>
pub struct NodeTlsExtension;

impl OtterExtension for NodeTlsExtension {
    fn name(&self) -> &str {
        "node_tls"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &["node_net"]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:tls", "tls"];
        &S
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        let socket_ctor = ctx
            .global()
            .get(&PropertyKey::string(SOCKET_CTOR_GLOBAL))
            .ok_or_else(|| VmError::type_error("node:tls requires node:net"))?;
        let socket_proto = socket_ctor
            .as_object()
            .and_then(|o| o.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .ok_or_else(|| VmError::type_error("node:tls requires Socket.prototype"))?;

        let tls_socket_ctor = ctx
            .builtin_fresh("TLSSocket")
            .inherits(socket_proto)
            .constructor_fn(TlsSocket::constructor, 1)
            .build();

        if let (Some(tls_obj), Some(socket_obj)) = (tls_socket_ctor.as_object(), socket_ctor.as_object()) {
            let _ = tls_obj.set_prototype(Value::object(socket_obj));
        }

        ctx.global_value(TLS_SOCKET_CTOR_GLOBAL, tls_socket_ctor);
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<otter_vm_core::gc::GcRef<otter_vm_core::object::JsObject>> {
        let tls_socket_ctor = ctx.global().get(&PropertyKey::string(TLS_SOCKET_CTOR_GLOBAL))?;
        Some(
            ctx.module_namespace()
                .property("TLSSocket", tls_socket_ctor)
                .build(),
        )
    }
}

/// Builds the native `node:tls` extension module.
///
/// Spec: <https://nodejs.org/api/tls.html>
pub fn node_tls_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeTlsExtension)
}

struct TlsSocket;

impl TlsSocket {
    fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        crate::net_ext::Socket::constructor(this, args, ncx)
    }
}
