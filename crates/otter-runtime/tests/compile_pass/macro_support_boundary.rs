#![forbid(unsafe_code)]

extern crate otter_runtime as otter_vm;

use otter_macros::{HostClass, js_class, js_namespace};
use otter_runtime::{
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeValue as Value,
};

#[derive(Clone, HostClass)]
struct ExternalClass {
    marker: bool,
}

#[js_class(name = "ExternalClass", feature = WEB)]
impl ExternalClass {
    #[constructor]
    fn new() -> Self {
        Self { marker: true }
    }

    #[method(name = "ping", raw)]
    fn ping(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }
}

struct ExternalNamespace;

#[js_namespace(name = "external", feature = WEB)]
impl ExternalNamespace {
    #[method(name = "ping", raw)]
    fn ping(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }
}

fn main() {
    let _ = ExternalClass::new().marker;
    let _ = EXTERNALCLASS_SPEC.name;
    let _ = EXTERNAL_SPEC.name;
}
