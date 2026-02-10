//! Native `node:stream` extension.
//!
//! Provides minimal `Readable`, `Writable`, `pipeline`, and `finished`
//! implemented natively and wired to `EventEmitter`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use otter_macros::{js_class, js_method};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

#[js_class(name = "Readable")]
pub struct Readable;

#[js_class]
impl Readable {
    #[js_method(name = "push", length = 1)]
    pub fn push(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let chunk = args.first().cloned().unwrap_or(Value::undefined());
        if chunk.is_null() {
            emit_event(this, "end", &[], ncx)?;
            return Ok(Value::boolean(false));
        }
        emit_event(this, "data", &[chunk], ncx)?;
        Ok(Value::boolean(true))
    }
}

#[js_class(name = "Writable")]
pub struct Writable;

#[js_class]
impl Writable {
    #[js_method(name = "write", length = 1)]
    pub fn write(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let chunk = args.first().cloned().unwrap_or(Value::undefined());
        emit_event(this, "data", &[chunk], ncx)?;
        Ok(Value::boolean(true))
    }

    #[js_method(name = "end", length = 0)]
    pub fn end(this: &Value, _args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        emit_event(this, "finish", &[], ncx)?;
        Ok(this.clone())
    }
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

pub struct NodeStreamExtension;

impl OtterExtension for NodeStreamExtension {
    fn name(&self) -> &str {
        "node_stream"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &["node_events"]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:stream", "stream"];
        &S
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        let emitter_ctor = ctx
            .global()
            .get(&PropertyKey::string("__EventEmitter"))
            .ok_or_else(|| VmError::type_error("node:stream requires node:events"))?;

        let emitter_proto = emitter_ctor
            .as_object()
            .and_then(|o| o.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .ok_or_else(|| VmError::type_error("node:stream requires EventEmitter.prototype"))?;

        let readable = build_readable_class(ctx, emitter_proto);
        let writable = build_writable_class(ctx, emitter_proto);

        ctx.global_value("__StreamReadable", readable);
        ctx.global_value("__StreamWritable", writable);
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let readable = ctx.global().get(&PropertyKey::string("__StreamReadable"))?;
        let writable = ctx.global().get(&PropertyKey::string("__StreamWritable"))?;

        let pipeline_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(stream_pipeline);
        let finished_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(stream_finished);

        let ns = ctx
            .module_namespace()
            .property("default", Value::undefined())
            .property("Readable", readable.clone())
            .property("Writable", writable.clone())
            .property("Stream", readable.clone())
            .property("Duplex", readable.clone())
            .property("Transform", readable.clone())
            .property("PassThrough", readable)
            .function("pipeline", pipeline_fn, 0)
            .function("finished", finished_fn, 2)
            .build();

        let _ = ns.set(PropertyKey::string("default"), Value::object(ns));
        Some(ns)
    }
}

pub fn node_stream_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeStreamExtension)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_readable_class(ctx: &RegistrationContext, emitter_proto: GcRef<JsObject>) -> Value {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    let methods: &[DeclFn] = &[Readable::push_decl];
    let mut builder = ctx
        .builtin_fresh("Readable")
        .inherits(emitter_proto)
        .constructor_fn(|_this, _args, _ncx| Ok(Value::undefined()), 0);

    for decl in methods {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    builder.build()
}

fn build_writable_class(ctx: &RegistrationContext, emitter_proto: GcRef<JsObject>) -> Value {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    let methods: &[DeclFn] = &[Writable::write_decl, Writable::end_decl];
    let mut builder = ctx
        .builtin_fresh("Writable")
        .inherits(emitter_proto)
        .constructor_fn(|_this, _args, _ncx| Ok(Value::undefined()), 0);

    for decl in methods {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    builder.build()
}

fn emit_event(
    target: &Value,
    event_name: &str,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let emit = target
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("stream object has no emit() method"))?;
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.push(Value::string(JsString::intern(event_name)));
    call_args.extend_from_slice(args);
    ncx.call_function(&emit, target.clone(), &call_args)
}

fn add_on_listener(
    target: &Value,
    event_name: &str,
    listener: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let on = target
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("on")))
        .ok_or_else(|| VmError::type_error("stream object has no on() method"))?;
    ncx.call_function(
        &on,
        target.clone(),
        &[Value::string(JsString::intern(event_name)), listener],
    )?;
    Ok(())
}

fn call_once_callback(
    callback: &Value,
    done: &Arc<AtomicBool>,
    arg: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    if done.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    ncx.call_function(callback, Value::undefined(), &[arg])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// stream.pipeline / stream.finished
// ---------------------------------------------------------------------------

fn stream_pipeline(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    if args.len() < 2 {
        return Err(VmError::type_error(
            "pipeline requires at least 2 arguments",
        ));
    }

    let (stream_count, callback) = match args.last().filter(|v| v.is_callable()) {
        Some(cb) => (args.len() - 1, cb.clone()),
        None => {
            return Err(VmError::type_error(
                "pipeline requires a callback as the last argument",
            ));
        }
    };

    if stream_count < 2 {
        return Err(VmError::type_error(
            "pipeline requires at least source and destination streams",
        ));
    }

    let source = args[0].clone();
    let sink = args[stream_count - 1].clone();
    let done = Arc::new(AtomicBool::new(false));
    let mm = ncx.memory_manager().clone();

    let sink_for_data = sink.clone();
    let data_handler = Value::native_function(
        move |_this, call_args, ncx| {
            let chunk = call_args.first().cloned().unwrap_or(Value::undefined());
            let write = sink_for_data
                .as_object()
                .and_then(|o| o.get(&PropertyKey::string("write")));
            if let Some(write_fn) = write {
                let _ = ncx.call_function(&write_fn, sink_for_data.clone(), &[chunk])?;
            }
            Ok(Value::undefined())
        },
        mm.clone(),
    );

    let sink_for_end = sink.clone();
    let callback_for_end = callback.clone();
    let done_for_end = done.clone();
    let end_handler = Value::native_function(
        move |_this, _call_args, ncx| {
            if let Some(end_fn) = sink_for_end
                .as_object()
                .and_then(|o| o.get(&PropertyKey::string("end")))
            {
                let _ = ncx.call_function(&end_fn, sink_for_end.clone(), &[])?;
            } else {
                let _ = emit_event(&sink_for_end, "finish", &[], ncx)?;
            }
            call_once_callback(&callback_for_end, &done_for_end, Value::undefined(), ncx)?;
            Ok(Value::undefined())
        },
        mm.clone(),
    );

    let callback_for_err = callback.clone();
    let done_for_err = done.clone();
    let error_handler = Value::native_function(
        move |_this, call_args, ncx| {
            let err = call_args.first().cloned().unwrap_or(Value::undefined());
            call_once_callback(&callback_for_err, &done_for_err, err, ncx)?;
            Ok(Value::undefined())
        },
        mm,
    );

    add_on_listener(&source, "data", data_handler, ncx)?;
    add_on_listener(&source, "end", end_handler, ncx)?;
    add_on_listener(&source, "error", error_handler.clone(), ncx)?;
    add_on_listener(&sink, "error", error_handler, ncx)?;

    Ok(sink)
}

fn stream_finished(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let stream = args
        .first()
        .cloned()
        .ok_or_else(|| VmError::type_error("finished requires stream argument"))?;
    let callback = args
        .get(1)
        .filter(|v| v.is_callable())
        .cloned()
        .ok_or_else(|| VmError::type_error("finished requires callback function"))?;

    let done = Arc::new(AtomicBool::new(false));
    let mm = ncx.memory_manager().clone();

    let callback_for_done = callback.clone();
    let done_for_done = done.clone();
    let done_handler = Value::native_function(
        move |_this, _call_args, ncx| {
            call_once_callback(&callback_for_done, &done_for_done, Value::undefined(), ncx)?;
            Ok(Value::undefined())
        },
        mm.clone(),
    );

    let callback_for_err = callback;
    let done_for_err = done;
    let error_handler = Value::native_function(
        move |_this, call_args, ncx| {
            let err = call_args.first().cloned().unwrap_or(Value::undefined());
            call_once_callback(&callback_for_err, &done_for_err, err, ncx)?;
            Ok(Value::undefined())
        },
        mm,
    );

    add_on_listener(&stream, "finish", done_handler.clone(), ncx)?;
    add_on_listener(&stream, "end", done_handler.clone(), ncx)?;
    add_on_listener(&stream, "close", done_handler, ncx)?;
    add_on_listener(&stream, "error", error_handler, ncx)?;

    Ok(stream)
}
