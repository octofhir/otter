//! Core JavaScript entry into the isolate-owned microtask queue.
//!
//! # Contents
//! - A static `queueMicrotask` method spec and CORE bootstrap installer.
//! - Native callback validation and admission through `NativeCtx`.
//!
//! # Invariants
//! - Web/Node installers do not own or replace this global callable.
//! - The existing queue owns callback roots, source realm and async context.
//! - Enqueueing runs no callback and does not require a host timer scheduler.
//!
//! # See also
//! - `crate::bootstrap` owns global installation order.
//! - `crate::NativeCtx::queue_microtask` owns callable admission.

use crate::bootstrap::BootstrapFeatures;
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::object::JsObject;
use crate::{NativeCtx, NativeError, Value, ValueWindow};

static QUEUE_MICROTASK: MethodSpec = MethodSpec {
    name: "queueMicrotask",
    length: 1,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(queue_microtask),
};

pub(crate) struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "queueMicrotask";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        ObjectBuilder::from_object(heap, global).method_from_spec(&QUEUE_MICROTASK)?;
        Ok(())
    }
}

fn queue_microtask(ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>) -> Result<Value, NativeError> {
    let callback = args.first().copied().unwrap_or_else(Value::undefined);
    ctx.queue_microtask(callback, [])?;
    Ok(Value::undefined())
}
