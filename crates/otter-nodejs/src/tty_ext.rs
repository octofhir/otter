//! Minimal native `node:tty` surface for constructor/prototype shape checks.
//!
//! Spec references:
//! - Node.js `node:tty`: <https://nodejs.org/api/tty.html>
//! - `tty.ReadStream`: <https://nodejs.org/api/tty.html#class-ttyreadstream>
//! - `tty.WriteStream`: <https://nodejs.org/api/tty.html#class-ttywritestream>

use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::object::PropertyKey;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

const SOCKET_CTOR_GLOBAL: &str = "__NetSocketCtor";
const READ_STREAM_CTOR_GLOBAL: &str = "__TtyReadStreamCtor";
const WRITE_STREAM_CTOR_GLOBAL: &str = "__TtyWriteStreamCtor";

/// Native registration entry for Node's `node:tty` builtin.
///
/// Spec: <https://nodejs.org/api/tty.html>
pub struct NodeTtyExtension;

impl OtterExtension for NodeTtyExtension {
    fn name(&self) -> &str {
        "node_tty"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &["node_net"]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:tty", "tty"];
        &S
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        let socket_ctor = ctx
            .global()
            .get(&PropertyKey::string(SOCKET_CTOR_GLOBAL))
            .ok_or_else(|| VmError::type_error("node:tty requires node:net"))?;
        let socket_proto = socket_ctor
            .as_object()
            .and_then(|o| o.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .ok_or_else(|| VmError::type_error("node:tty requires Socket.prototype"))?;

        let read_stream_ctor = ctx
            .builtin_fresh("ReadStream")
            .inherits(socket_proto)
            .constructor_fn(TtyReadStream::constructor, 1)
            .build();
        let write_stream_ctor = ctx
            .builtin_fresh("WriteStream")
            .inherits(socket_proto)
            .constructor_fn(TtyWriteStream::constructor, 1)
            .build();

        if let (Some(read_obj), Some(socket_obj)) = (read_stream_ctor.as_object(), socket_ctor.as_object()) {
            let _ = read_obj.set_prototype(Value::object(socket_obj));
        }
        if let (Some(write_obj), Some(socket_obj)) =
            (write_stream_ctor.as_object(), socket_ctor.as_object())
        {
            let _ = write_obj.set_prototype(Value::object(socket_obj));
        }

        ctx.global_value(READ_STREAM_CTOR_GLOBAL, read_stream_ctor);
        ctx.global_value(WRITE_STREAM_CTOR_GLOBAL, write_stream_ctor);
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<otter_vm_core::gc::GcRef<otter_vm_core::object::JsObject>> {
        let read_stream_ctor = ctx.global().get(&PropertyKey::string(READ_STREAM_CTOR_GLOBAL))?;
        let write_stream_ctor = ctx.global().get(&PropertyKey::string(WRITE_STREAM_CTOR_GLOBAL))?;
        Some(
            ctx.module_namespace()
                .property("ReadStream", read_stream_ctor)
                .property("WriteStream", write_stream_ctor)
                .build(),
        )
    }
}

/// Builds the native `node:tty` extension module.
///
/// Spec: <https://nodejs.org/api/tty.html>
pub fn node_tty_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeTtyExtension)
}

struct TtyReadStream;

impl TtyReadStream {
    fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        crate::net_ext::Socket::constructor(this, args, ncx)
    }
}

struct TtyWriteStream;

impl TtyWriteStream {
    fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        crate::net_ext::Socket::constructor(this, args, ncx)
    }
}
