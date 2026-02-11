//! Native `AbortController` and `AbortSignal` implementation.
//!
//! Provides the global `AbortController` and `AbortSignal` classes.
//! State is stored in internal properties on the JS objects.

use std::sync::Arc;

use crate::builtin_builder::BuiltInBuilder;
use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::{js_class, js_method, js_static};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ABORTED_KEY: &str = "__aborted";
const REASON_KEY: &str = "__reason";
const ONABORT_KEY: &str = "__onabort";
const SIGNAL_KEY: &str = "__signal";

// ---------------------------------------------------------------------------
// AbortSignal
// ---------------------------------------------------------------------------

#[js_class(name = "AbortSignal")]
pub struct AbortSignal;

#[js_class]
impl AbortSignal {
    #[js_static(name = "abort", length = 1)]
    pub fn abort(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let reason = args.first().cloned().unwrap_or(Value::undefined());
        create_signal_object(ncx, true, reason)
    }

    #[js_static(name = "timeout", length = 1)]
    pub fn timeout(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let delay_ms = args
            .first()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0)
            .max(0.0) as u64;

        let signal = create_signal_object(ncx, false, Value::undefined())?;
        let timeout_reason = create_timeout_reason(ncx);
        let signal_for_cb = signal.clone();
        let reason_for_cb = timeout_reason.clone();

        let fn_proto = ncx
            .ctx
            .function_prototype()
            .ok_or_else(|| VmError::internal("Function.prototype not found"))?;

        let timeout_cb = Value::native_function_with_proto(
            move |_this, _args, cb_ncx| {
                abort_signal(&signal_for_cb, reason_for_cb.clone(), cb_ncx)?;
                Ok(Value::undefined())
            },
            ncx.memory_manager().clone(),
            fn_proto,
        );

        if let Some(set_timeout) = ncx.global().get(&PropertyKey::string("setTimeout"))
            && set_timeout.is_callable()
        {
            let _ = ncx.call_function(
                &set_timeout,
                Value::undefined(),
                &[timeout_cb, Value::number(delay_ms as f64)],
            )?;
            return Ok(signal);
        }

        // Fallback when timer APIs are unavailable in this runtime profile.
        abort_signal(&signal, timeout_reason, ncx)?;
        Ok(signal)
    }

    #[js_method(name = "aborted", kind = "getter")]
    pub fn aborted(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        get_property(this, ABORTED_KEY).map(|v| v.unwrap_or(Value::boolean(false)))
    }

    #[js_method(name = "reason", kind = "getter")]
    pub fn reason(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        get_property(this, REASON_KEY).map(|v| v.unwrap_or(Value::undefined()))
    }

    #[js_method(name = "throwIfAborted", length = 0)]
    pub fn throw_if_aborted(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let aborted = get_property(this, ABORTED_KEY)?.unwrap_or(Value::boolean(false));
        if aborted.to_boolean() {
            let reason = get_property(this, REASON_KEY)?.unwrap_or(Value::undefined());
            return Err(VmError::exception(reason));
        }
        Ok(Value::undefined())
    }

    #[js_method(name = "onabort", kind = "getter")]
    pub fn get_onabort(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        get_property(this, ONABORT_KEY).map(|v| v.unwrap_or(Value::undefined()))
    }

    #[js_method(name = "onabort", kind = "setter")]
    pub fn set_onabort(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        set_property(this, ONABORT_KEY, val)
    }
}

fn create_signal_object(
    ncx: &NativeContext,
    aborted: bool,
    reason: Value,
) -> Result<Value, VmError> {
    let realm_id = ncx.ctx.realm_id();
    let intrinsics = ncx
        .ctx
        .realm_intrinsics(realm_id)
        .ok_or_else(|| VmError::internal("Realm intrinsics not found"))?;

    let signal = JsObject::new(
        Value::object(intrinsics.abort_signal_prototype),
        ncx.memory_manager().clone(),
    );
    let signal_val = Value::object(GcRef::new(signal));

    set_property(&signal_val, ABORTED_KEY, Value::boolean(aborted))?;
    set_property(&signal_val, REASON_KEY, reason)?;

    Ok(signal_val)
}

// ---------------------------------------------------------------------------
// AbortController
// ---------------------------------------------------------------------------

#[js_class(name = "AbortController")]
pub struct AbortController;

#[js_class]
impl AbortController {
    #[js_method(constructor)]
    pub fn constructor(
        this: &Value,
        _args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let signal = create_signal_object(ncx, false, Value::undefined())?;
        set_property(this, SIGNAL_KEY, signal)?;
        Ok(this.clone())
    }

    #[js_method(name = "signal", kind = "getter")]
    pub fn signal(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        get_property(this, SIGNAL_KEY).map(|v| v.unwrap_or(Value::undefined()))
    }

    #[js_method(name = "abort", length = 0)]
    pub fn abort(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let reason = args.first().cloned().unwrap_or(Value::undefined());

        let signal = get_property(this, SIGNAL_KEY)?
            .ok_or_else(|| VmError::type_error("Invalid AbortController"))?;
        abort_signal(&signal, reason, ncx)?;
        Ok(Value::undefined())
    }
}

// ---------------------------------------------------------------------------
// Internal Helpers
// ---------------------------------------------------------------------------

fn get_property(obj_val: &Value, key: &str) -> Result<Option<Value>, VmError> {
    if let Some(obj) = obj_val.as_object() {
        Ok(obj.get(&PropertyKey::string(key)))
    } else {
        Err(VmError::type_error("Expected object"))
    }
}

fn set_property(obj_val: &Value, key: &str, val: Value) -> Result<Value, VmError> {
    if let Some(obj) = obj_val.as_object() {
        obj.set(PropertyKey::string(key), val.clone())
            .map_err(|e| {
                VmError::type_error(format!("Failed to set property '{}': {:?}", key, e))
            })?;
        Ok(val)
    } else {
        Err(VmError::type_error("Expected object"))
    }
}

fn abort_signal(signal: &Value, reason: Value, ncx: &mut NativeContext) -> Result<(), VmError> {
    let aborted = get_property(signal, ABORTED_KEY)?.unwrap_or(Value::boolean(false));
    if aborted.to_boolean() {
        return Ok(());
    }

    set_property(signal, ABORTED_KEY, Value::boolean(true))?;
    set_property(signal, REASON_KEY, reason)?;

    let onabort = get_property(signal, ONABORT_KEY)?.unwrap_or(Value::undefined());
    if onabort.is_callable() {
        let event = JsObject::new(Value::null(), ncx.memory_manager().clone());
        let event_val = Value::object(GcRef::new(event));
        set_property(&event_val, "type", Value::string(JsString::intern("abort")))?;
        set_property(&event_val, "target", signal.clone())?;

        ncx.call_function(&onabort, signal.clone(), &[event_val])?;
    }

    Ok(())
}

fn create_timeout_reason(ncx: &mut NativeContext) -> Value {
    let message = Value::string(JsString::intern("signal timed out"));
    if let Some(error_ctor) = ncx.global().get(&PropertyKey::string("Error"))
        && error_ctor.is_callable()
        && let Ok(err) = ncx.call_function_construct(&error_ctor, Value::undefined(), &[message])
    {
        if let Some(obj) = err.as_object() {
            let _ = obj.set(
                PropertyKey::string("name"),
                Value::string(JsString::intern("TimeoutError")),
            );
        }
        return err;
    }

    Value::string(JsString::intern("TimeoutError: signal timed out"))
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

pub fn init_abort_signal(
    signal_ctor: GcRef<JsObject>,
    signal_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<crate::memory::MemoryManager>,
) {
    let mut builder = BuiltInBuilder::new(
        mm.clone(),
        fn_proto,
        signal_ctor,
        signal_proto,
        "AbortSignal",
    )
    .constructor_fn(
        |_, _, _| {
            Err(VmError::type_error(
                "Constructing an AbortSignal manually is not supported",
            ))
        },
        0,
    );

    // Methods
    let (_, func, len) = AbortSignal::throw_if_aborted_decl();
    builder = builder.method_native("throwIfAborted", func, len);

    // Accessors
    let (_, aborted_get, _) = AbortSignal::aborted_decl();
    builder = builder.accessor("aborted", Some(aborted_get), None);

    let (_, reason_get, _) = AbortSignal::reason_decl();
    builder = builder.accessor("reason", Some(reason_get), None);

    let (_, onabort_get, _) = AbortSignal::get_onabort_decl();
    let (_, onabort_set, _) = AbortSignal::set_onabort_decl();
    builder = builder.accessor("onabort", Some(onabort_get), Some(onabort_set));

    // Static methods
    let static_methods: &[fn() -> (&'static str, crate::value::NativeFn, u32)] =
        &[AbortSignal::abort_decl, AbortSignal::timeout_decl];

    for decl in static_methods {
        let (name, func, length) = decl();
        builder = builder.static_method_native(name, func, length);
    }

    builder.build();
}

pub fn init_abort_controller(
    controller_ctor: GcRef<JsObject>,
    controller_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<crate::memory::MemoryManager>,
) {
    let mut builder = BuiltInBuilder::new(
        mm.clone(),
        fn_proto,
        controller_ctor,
        controller_proto,
        "AbortController",
    )
    .constructor_fn(AbortController::constructor, 0);

    // Methods
    let (_, abort_func, abort_len) = AbortController::abort_decl();
    builder = builder.method_native("abort", abort_func, abort_len);

    // Accessors
    let (_, signal_get, _) = AbortController::signal_decl();
    builder = builder.accessor("signal", Some(signal_get), None);

    builder.build();
}
