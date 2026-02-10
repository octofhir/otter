//! Native `node:events` extension — EventEmitter class.
//!
//! Provides the Node.js-compatible `EventEmitter` class as a native extension.
//! Listeners are stored as JsObject properties on emitter instances for automatic
//! GC tracing.
//!
//! Storage layout on each emitter instance:
//! - `__ee_listeners` → JsObject map: eventName → Array of listener objects
//! - `__ee_maxListeners` → number (default 10)
//!
//! Each listener object: `{ fn: <callable>, once: <bool> }`

use std::sync::Arc;

use otter_macros::{js_class, js_method, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::intrinsics_impl::helpers::strict_equal;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::promise::JsPromise;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LISTENERS_KEY: &str = "__ee_listeners";
const MAX_LISTENERS_KEY: &str = "__ee_maxListeners";
const DEFAULT_MAX_LISTENERS: i32 = 10;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Get (or create) the `__ee_listeners` map object on an emitter instance.
fn get_listeners_map(this: &Value, ncx: &NativeContext) -> Result<GcRef<JsObject>, VmError> {
    let obj = this
        .as_object()
        .ok_or_else(|| VmError::type_error("EventEmitter method called on non-object"))?;

    let key = PropertyKey::string(LISTENERS_KEY);
    if let Some(map_val) = obj.get(&key) {
        if let Some(map_obj) = map_val.as_object() {
            return Ok(map_obj);
        }
    }

    // Create fresh listeners map
    let map = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = obj.set(key, Value::object(map));
    Ok(map)
}

/// Get the listener array for a given event name, or create one.
fn get_or_create_listener_array(
    map: &GcRef<JsObject>,
    event: &PropertyKey,
    ncx: &NativeContext,
) -> GcRef<JsObject> {
    if let Some(arr_val) = map.get(event) {
        if let Some(arr) = arr_val.as_object() {
            return arr;
        }
    }
    let arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
    let _ = map.set(event.clone(), Value::object(arr));
    arr
}

/// Create a listener entry object: `{ fn: callback, once: bool }`
fn make_listener_entry(callback: Value, once: bool, ncx: &NativeContext) -> GcRef<JsObject> {
    let entry = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = entry.set(PropertyKey::string("fn"), callback);
    let _ = entry.set(PropertyKey::string("once"), Value::boolean(once));
    entry
}

/// Convert the first arg to a PropertyKey for event name.
fn event_key(args: &[Value]) -> Result<PropertyKey, VmError> {
    let event = args
        .first()
        .ok_or_else(|| VmError::type_error("EventEmitter: event name required"))?;

    if let Some(sym) = event.as_symbol() {
        Ok(PropertyKey::Symbol(sym))
    } else if let Some(s) = event.as_string() {
        Ok(PropertyKey::from_js_string(s))
    } else if let Some(n) = event.as_number() {
        // Number event names (rare, but valid)
        Ok(PropertyKey::string(&n.to_string()))
    } else if event.is_undefined() {
        Ok(PropertyKey::string("undefined"))
    } else if event.is_null() {
        Ok(PropertyKey::string("null"))
    } else if let Some(b) = event.as_boolean() {
        Ok(PropertyKey::string(if b { "true" } else { "false" }))
    } else {
        Ok(PropertyKey::string("undefined"))
    }
}

/// Get the max listeners value for this emitter.
fn get_max_listeners(this: &Value) -> i32 {
    if let Some(obj) = this.as_object() {
        if let Some(val) = obj.get(&PropertyKey::string(MAX_LISTENERS_KEY)) {
            if let Some(n) = val.as_int32() {
                return n;
            }
            if let Some(n) = val.as_number() {
                return n as i32;
            }
        }
    }
    DEFAULT_MAX_LISTENERS
}

/// Warn if listener count exceeds max.
fn check_max_listeners(this: &Value, event: &PropertyKey, count: usize) {
    let max = get_max_listeners(this);
    if max > 0 && count > max as usize {
        let event_name = match event {
            PropertyKey::String(s) => s.as_str().to_string(),
            PropertyKey::Index(i) => i.to_string(),
            PropertyKey::Symbol(_) => "Symbol(...)".to_string(),
        };
        eprintln!(
            "MaxListenersExceededWarning: Possible EventEmitter memory leak detected. \
             {count} {event_name} listeners added to EventEmitter. \
             Use emitter.setMaxListeners() to increase limit."
        );
    }
}

// ---------------------------------------------------------------------------
// EventEmitter class methods
// ---------------------------------------------------------------------------

#[js_class(name = "EventEmitter")]
pub struct EventEmitter;

#[js_class]
impl EventEmitter {
    #[js_method(name = "on", length = 2)]
    pub fn on(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        Self::add_listener_internal(this, args, ncx, false, false)
    }

    #[js_method(name = "addListener", length = 2)]
    pub fn add_listener(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Self::add_listener_internal(this, args, ncx, false, false)
    }

    #[js_method(name = "prependListener", length = 2)]
    pub fn prepend_listener(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Self::add_listener_internal(this, args, ncx, false, true)
    }

    #[js_method(name = "once", length = 2)]
    pub fn once(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        Self::add_listener_internal(this, args, ncx, true, false)
    }

    #[js_method(name = "prependOnceListener", length = 2)]
    pub fn prepend_once_listener(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Self::add_listener_internal(this, args, ncx, true, true)
    }

    #[js_method(name = "off", length = 2)]
    pub fn off(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        Self::remove_listener_impl(this, args, ncx)
    }

    #[js_method(name = "removeListener", length = 2)]
    pub fn remove_listener(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Self::remove_listener_impl(this, args, ncx)
    }

    #[js_method(name = "removeAllListeners", length = 0)]
    pub fn remove_all_listeners(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let map = get_listeners_map(this, ncx)?;

        if args.is_empty() || args[0].is_undefined() {
            // Remove ALL listeners: replace map with empty object
            if let Some(obj) = this.as_object() {
                let new_map =
                    GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                let _ = obj.set(PropertyKey::string(LISTENERS_KEY), Value::object(new_map));
            }
        } else {
            let event = event_key(args)?;
            let _ = map.delete(&event);
        }

        Ok(this.clone())
    }

    #[js_method(name = "emit", length = 1)]
    pub fn emit(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let event = event_key(args)?;
        let map = get_listeners_map(this, ncx)?;

        let arr = match map.get(&event) {
            Some(v) => match v.as_object() {
                Some(a) => a,
                None => return Ok(Value::boolean(false)),
            },
            None => {
                // Special case: 'error' event with no listeners → throw
                if matches!(&event, PropertyKey::String(s) if s.as_str() == "error") {
                    let err = args.get(1).cloned().unwrap_or_else(|| {
                        Value::string(JsString::intern("Unhandled 'error' event"))
                    });
                    // If it's an Error object, throw it. Otherwise wrap.
                    return Err(VmError::exception(err));
                }
                return Ok(Value::boolean(false));
            }
        };

        let len = arr.array_length();
        if len == 0 {
            if matches!(&event, PropertyKey::String(s) if s.as_str() == "error") {
                let err = args
                    .get(1)
                    .cloned()
                    .unwrap_or_else(|| Value::string(JsString::intern("Unhandled 'error' event")));
                return Err(VmError::exception(err));
            }
            return Ok(Value::boolean(false));
        }

        // Collect listeners snapshot (array may mutate during emit due to once removal)
        let mut listeners = Vec::with_capacity(len);
        for i in 0..len {
            if let Some(entry) = arr.get(&PropertyKey::Index(i as u32)) {
                listeners.push(entry);
            }
        }

        // Track which indices to remove (once listeners)
        let mut remove_indices = Vec::new();
        let call_args = if args.len() > 1 { &args[1..] } else { &[] };

        for (idx, entry) in listeners.iter().enumerate() {
            let (callback, is_once) = if let Some(entry_obj) = entry.as_object() {
                let cb = entry_obj
                    .get(&PropertyKey::string("fn"))
                    .unwrap_or(Value::undefined());
                let once = entry_obj
                    .get(&PropertyKey::string("once"))
                    .map(|v| v.to_boolean())
                    .unwrap_or(false);
                (cb, once)
            } else {
                continue;
            };

            if is_once {
                remove_indices.push(idx);
            }

            if callback.is_callable() {
                ncx.call_function(&callback, this.clone(), call_args)?;
            }
        }

        // Remove once listeners by rebuilding array without them
        if !remove_indices.is_empty() {
            if let Some(current_arr_val) = map.get(&event) {
                if let Some(current_arr) = current_arr_val.as_object() {
                    let current_len = current_arr.array_length();
                    let new_arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                    for i in 0..current_len {
                        if !remove_indices.contains(&i) {
                            if let Some(v) = current_arr.get(&PropertyKey::Index(i as u32)) {
                                new_arr.array_push(v);
                            }
                        }
                    }
                    let _ = map.set(event.clone(), Value::object(new_arr));
                }
            }
        }

        Ok(Value::boolean(true))
    }

    #[js_method(name = "listeners", length = 1)]
    pub fn listeners(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let event = event_key(args)?;
        let map = get_listeners_map(this, ncx)?;

        let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));

        if let Some(arr_val) = map.get(&event) {
            if let Some(arr) = arr_val.as_object() {
                let len = arr.array_length();
                for i in 0..len {
                    if let Some(entry) = arr.get(&PropertyKey::Index(i as u32)) {
                        if let Some(entry_obj) = entry.as_object() {
                            if let Some(cb) = entry_obj.get(&PropertyKey::string("fn")) {
                                result.array_push(cb);
                            }
                        }
                    }
                }
            }
        }

        Ok(Value::object(result))
    }

    #[js_method(name = "rawListeners", length = 1)]
    pub fn raw_listeners(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        // rawListeners returns the listener entries including once wrappers
        // For simplicity, return same as listeners() for now
        Self::listeners(this, args, ncx)
    }

    #[js_method(name = "listenerCount", length = 1)]
    pub fn listener_count(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let event = event_key(args)?;
        let map = get_listeners_map(this, ncx)?;

        let count = if let Some(arr_val) = map.get(&event) {
            if let Some(arr) = arr_val.as_object() {
                arr.array_length()
            } else {
                0
            }
        } else {
            0
        };

        Ok(Value::number(count as f64))
    }

    #[js_method(name = "eventNames", length = 0)]
    pub fn event_names(
        this: &Value,
        _args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let map = get_listeners_map(this, ncx)?;
        let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));

        for key in map.own_keys() {
            // Only include events that have at least one listener
            if let Some(arr_val) = map.get(&key) {
                if let Some(arr) = arr_val.as_object() {
                    if arr.array_length() > 0 {
                        match &key {
                            PropertyKey::String(s) => {
                                result.array_push(Value::string(*s));
                            }
                            PropertyKey::Symbol(s) => {
                                result.array_push(Value::symbol(*s));
                            }
                            PropertyKey::Index(i) => {
                                result.array_push(Value::number(*i as f64));
                            }
                        }
                    }
                }
            }
        }

        Ok(Value::object(result))
    }

    #[js_method(name = "setMaxListeners", length = 1)]
    pub fn set_max_listeners(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let n = args
            .first()
            .and_then(|v| v.as_number())
            .unwrap_or(DEFAULT_MAX_LISTENERS as f64);

        if n < 0.0 || n.is_nan() {
            return Err(VmError::range_error(
                "The value of \"n\" is out of range. It must be a non-negative number.",
            ));
        }

        if let Some(obj) = this.as_object() {
            let _ = obj.set(PropertyKey::string(MAX_LISTENERS_KEY), Value::number(n));
        }

        Ok(this.clone())
    }

    #[js_method(name = "getMaxListeners", length = 0)]
    pub fn get_max_listeners_method(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::number(get_max_listeners(this) as f64))
    }

    // --- Static methods ---

    #[js_static(name = "listenerCount", length = 2)]
    pub fn static_listener_count(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let emitter = args
            .first()
            .ok_or_else(|| VmError::type_error("EventEmitter.listenerCount: emitter required"))?;
        let event_args = if args.len() > 1 { &args[1..] } else { &[] };
        Self::listener_count(emitter, event_args, ncx)
    }

    // --- Internal helpers ---

    fn add_listener_internal(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
        once: bool,
        prepend: bool,
    ) -> Result<Value, VmError> {
        let event = event_key(args)?;
        let callback = args
            .get(1)
            .filter(|v| v.is_callable())
            .cloned()
            .ok_or_else(|| {
                VmError::type_error("The \"listener\" argument must be of type Function")
            })?;

        let map = get_listeners_map(this, ncx)?;
        let arr = get_or_create_listener_array(&map, &event, ncx);
        let entry = make_listener_entry(callback, once, ncx);

        if prepend {
            // Shift all elements right and insert at 0
            let len = arr.array_length();
            // Read all existing
            let mut existing = Vec::with_capacity(len);
            for i in 0..len {
                if let Some(v) = arr.get(&PropertyKey::Index(i as u32)) {
                    existing.push(v);
                }
            }
            // Clear and rebuild
            // Set length to 0 by overwriting
            let new_arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
            new_arr.array_push(Value::object(entry));
            for v in existing {
                new_arr.array_push(v);
            }
            let _ = map.set(event.clone(), Value::object(new_arr));
            check_max_listeners(this, &event, len + 1);
        } else {
            arr.array_push(Value::object(entry));
            check_max_listeners(this, &event, arr.array_length());
        }

        // Emit 'newListener' event (skip if the event IS 'newListener' to avoid recursion)
        if !matches!(&event, PropertyKey::String(s) if s.as_str() == "newListener") {
            // Best-effort: don't propagate errors from newListener handlers
            let _ = Self::emit(
                this,
                &[
                    Value::string(JsString::intern("newListener")),
                    args.first().cloned().unwrap_or(Value::undefined()),
                    args.get(1).cloned().unwrap_or(Value::undefined()),
                ],
                ncx,
            );
        }

        Ok(this.clone())
    }

    fn remove_listener_impl(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let event = event_key(args)?;
        let callback = args.get(1).cloned().unwrap_or(Value::undefined());

        let map = get_listeners_map(this, ncx)?;

        if let Some(arr_val) = map.get(&event) {
            if let Some(arr) = arr_val.as_object() {
                let len = arr.array_length();
                let mut found_idx = None;
                for i in 0..len {
                    if let Some(entry) = arr.get(&PropertyKey::Index(i as u32)) {
                        if let Some(entry_obj) = entry.as_object() {
                            if let Some(cb) = entry_obj.get(&PropertyKey::string("fn")) {
                                if strict_equal(&cb, &callback) {
                                    found_idx = Some(i);
                                    break;
                                }
                            }
                        }
                    }
                }

                if let Some(idx) = found_idx {
                    // Rebuild array without the removed element
                    let new_arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                    for i in 0..len {
                        if i != idx {
                            if let Some(v) = arr.get(&PropertyKey::Index(i as u32)) {
                                new_arr.array_push(v);
                            }
                        }
                    }
                    let _ = map.set(event.clone(), Value::object(new_arr));

                    // Emit 'removeListener'
                    if !matches!(&event, PropertyKey::String(s) if s.as_str() == "removeListener") {
                        let _ = Self::emit(
                            this,
                            &[
                                Value::string(JsString::intern("removeListener")),
                                args.first().cloned().unwrap_or(Value::undefined()),
                                callback,
                            ],
                            ncx,
                        );
                    }
                }
            }
        }

        Ok(this.clone())
    }
}

// ---------------------------------------------------------------------------
// OtterExtension implementation
// ---------------------------------------------------------------------------

pub struct NodeEventsExtension;

impl OtterExtension for NodeEventsExtension {
    fn name(&self) -> &str {
        "node_events"
    }

    fn profiles(&self) -> &[Profile] {
        static PROFILES: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &PROFILES
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static SPECIFIERS: [&str; 2] = ["node:events", "events"];
        &SPECIFIERS
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        // Build the EventEmitter constructor + prototype using BuiltInBuilder
        let ctor_value = build_event_emitter_class(ctx);

        // Store constructor in extension state for module loading
        ctx.global_value("__EventEmitter", ctor_value);

        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let ctor = ctx.global().get(&PropertyKey::string("__EventEmitter"))?;

        let once_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(events_once);

        let listener_count_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(events_listener_count);

        let set_max_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(events_set_max_listeners);

        let ns = ctx
            .module_namespace()
            .property("default", ctor.clone())
            .property("EventEmitter", ctor)
            .function("once", once_fn, 2)
            .function("listenerCount", listener_count_fn, 2)
            .function("setMaxListeners", set_max_fn, 2)
            .build();

        Some(ns)
    }
}

// ---------------------------------------------------------------------------
// Module-level standalone functions: once(), listenerCount(), setMaxListeners()
// ---------------------------------------------------------------------------

/// `events.once(emitter, event)` — returns a Promise that resolves with args when event fires.
/// Adds a one-shot listener; if 'error' fires first, rejects.
fn events_once(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let emitter = args
        .first()
        .cloned()
        .ok_or_else(|| VmError::type_error("once: emitter required"))?;
    let event_name = args.get(1).cloned().unwrap_or(Value::undefined());

    let promise = JsPromise::new();
    let promise_ref = promise;

    // Create resolve callback: resolves promise with array of args
    let resolve_promise = promise_ref;
    let resolve_fn = Value::native_function(
        move |_this, call_args, ncx| {
            let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
            for arg in call_args {
                result.array_push(arg.clone());
            }
            resolve_promise.resolve(Value::object(result));
            Ok(Value::undefined())
        },
        ncx.memory_manager().clone(),
    );

    // Create error callback: rejects promise with error
    let reject_promise = promise_ref;
    let error_fn = Value::native_function(
        move |_this, call_args, _ncx| {
            let err = call_args.first().cloned().unwrap_or(Value::undefined());
            reject_promise.reject(err);
            Ok(Value::undefined())
        },
        ncx.memory_manager().clone(),
    );

    // emitter.once(event, resolveCallback)
    let once_method = emitter
        .as_object()
        .and_then(|obj| obj.get(&PropertyKey::string("once")))
        .ok_or_else(|| VmError::type_error("once: emitter has no .once() method"))?;

    ncx.call_function(
        &once_method,
        emitter.clone(),
        &[event_name.clone(), resolve_fn],
    )?;

    // emitter.once('error', rejectCallback) — only if event is not 'error' itself
    let is_error_event = event_name
        .as_string()
        .is_some_and(|s| s.as_str() == "error");
    if !is_error_event {
        ncx.call_function(
            &once_method,
            emitter,
            &[Value::string(JsString::intern("error")), error_fn],
        )?;
    }

    Ok(wrap_promise(ncx, promise_ref))
}

/// Wrap a JsPromise into a JsObject with Promise.prototype for `.then`/`.catch`/`.finally`.
fn wrap_promise(ncx: &NativeContext, internal: GcRef<JsPromise>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

    if let Some(promise_ctor) = ncx
        .global()
        .get(&PropertyKey::string("Promise"))
        .and_then(|v| v.as_object())
        && let Some(proto) = promise_ctor
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
    {
        if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
            let _ = obj.set(PropertyKey::string("then"), then_fn);
        }
        if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
            let _ = obj.set(PropertyKey::string("catch"), catch_fn);
        }
        if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
            let _ = obj.set(PropertyKey::string("finally"), finally_fn);
        }
        obj.set_prototype(Value::object(proto));
    }

    Value::object(obj)
}

/// `events.listenerCount(emitter, event)` — standalone version
fn events_listener_count(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let emitter = args
        .first()
        .ok_or_else(|| VmError::type_error("listenerCount: emitter required"))?;
    let event_args = if args.len() > 1 { &args[1..] } else { &[] };
    EventEmitter::listener_count(emitter, event_args, ncx)
}

/// `events.setMaxListeners(n, ...emitters)` — set max listeners on one or more emitters
fn events_set_max_listeners(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let n = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(DEFAULT_MAX_LISTENERS as f64);

    if n < 0.0 || n.is_nan() {
        return Err(VmError::range_error(
            "The value of \"n\" is out of range. It must be a non-negative number.",
        ));
    }

    // Apply to all emitters passed as remaining args
    for emitter in args.iter().skip(1) {
        if let Some(obj) = emitter.as_object() {
            let set_method = obj.get(&PropertyKey::string("setMaxListeners"));
            if let Some(method) = set_method {
                if method.is_callable() {
                    ncx.call_function(&method, emitter.clone(), &[Value::number(n)])?;
                    continue;
                }
            }
            // Fallback: set directly
            let _ = obj.set(PropertyKey::string(MAX_LISTENERS_KEY), Value::number(n));
        }
    }

    Ok(Value::undefined())
}

/// Build the EventEmitter class using BuiltInBuilder.
fn build_event_emitter_class(ctx: &RegistrationContext) -> Value {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    // Prototype methods
    let proto_methods: &[DeclFn] = &[
        EventEmitter::on_decl,
        EventEmitter::add_listener_decl,
        EventEmitter::prepend_listener_decl,
        EventEmitter::once_decl,
        EventEmitter::prepend_once_listener_decl,
        EventEmitter::off_decl,
        EventEmitter::remove_listener_decl,
        EventEmitter::remove_all_listeners_decl,
        EventEmitter::emit_decl,
        EventEmitter::listeners_decl,
        EventEmitter::raw_listeners_decl,
        EventEmitter::listener_count_decl,
        EventEmitter::event_names_decl,
        EventEmitter::set_max_listeners_decl,
        EventEmitter::get_max_listeners_method_decl,
    ];

    // Static methods
    let static_methods: &[DeclFn] = &[EventEmitter::static_listener_count_decl];

    let mut builder = ctx.builtin_fresh("EventEmitter").constructor_fn(
        |this, _args, ncx| {
            // The interpreter already created `this` with EventEmitter.prototype.
            // We just initialize the internal state on it.
            if let Some(obj) = this.as_object() {
                let map = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                let _ = obj.set(PropertyKey::string(LISTENERS_KEY), Value::object(map));
                let _ = obj.set(
                    PropertyKey::string(MAX_LISTENERS_KEY),
                    Value::int32(DEFAULT_MAX_LISTENERS),
                );
            }
            // Return non-object so interpreter uses the auto-created `this`
            Ok(Value::undefined())
        },
        0,
    );

    for decl in proto_methods {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    for decl in static_methods {
        let (name, func, length) = decl();
        builder = builder.static_method_native(name, func, length);
    }

    // Add defaultMaxListeners as static property
    builder = builder.static_accessor(
        "defaultMaxListeners",
        Some(Arc::new(|_this, _args, _ncx| {
            Ok(Value::int32(DEFAULT_MAX_LISTENERS))
        })),
        None,
    );

    builder.build()
}

/// Create a boxed extension instance for registration.
pub fn node_events_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeEventsExtension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_emitter_metadata() {
        assert_eq!(EventEmitter::JS_CLASS_NAME, "EventEmitter");
        assert!(EventEmitter::js_methods().contains(&"on"));
        assert!(EventEmitter::js_methods().contains(&"emit"));
        assert!(EventEmitter::js_methods().contains(&"off"));
        assert!(EventEmitter::js_methods().contains(&"once"));
        assert!(EventEmitter::js_methods().contains(&"listeners"));
        assert!(EventEmitter::js_methods().contains(&"listener_count"));
        assert!(EventEmitter::js_methods().contains(&"event_names"));
        assert!(EventEmitter::js_static_methods().contains(&"static_listener_count"));
    }

    #[test]
    fn test_decl_functions() {
        let (name, _func, length) = EventEmitter::on_decl();
        assert_eq!(name, "on");
        assert_eq!(length, 2);

        let (name, _func, length) = EventEmitter::emit_decl();
        assert_eq!(name, "emit");
        assert_eq!(length, 1);

        let (name, _func, length) = EventEmitter::listener_count_decl();
        assert_eq!(name, "listenerCount");
        assert_eq!(length, 1);

        let (name, _func, length) = EventEmitter::static_listener_count_decl();
        assert_eq!(name, "listenerCount");
        assert_eq!(length, 2);
    }
}
