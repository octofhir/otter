//! The `process` members a process launched with a channel is given.
//!
//! A channel is part of how a process was started, the way its arguments and
//! environment are, so it is established here rather than by a module the
//! program may never load: `process.send` has to work whether or not anything
//! ever requires `child_process`.
//!
//! # Contents
//! - [`install`] adds `send`, `disconnect`, `connected`, and `channel` to the
//!   `process` global when this process was launched with a channel.
//! - The task that reports an arriving message as `process.emit('message', …)`.
//!
//! # Invariants
//! - A message crosses the channel as text and is encoded and decoded by the
//!   realm's own `JSON`, so both ends agree on the shape without a second
//!   serializer of our own.
//! - A message that cannot be encoded is refused at the send, not dropped
//!   silently on the way.
//!
//! # See also
//! - [`crate::ipc`] — the channel itself.

use std::sync::Arc;

use otter_vm::{Attr, ErrorKind, Local, NativeCall, NativeError, NativeFn, NativeScope, Value};

use crate::ipc::{IpcChannel, IpcEvent};
use crate::{OtterError, Runtime};

/// Give `process` its channel members.
///
/// # Errors
/// Returns a native error when a member cannot be allocated or defined.
pub(crate) fn install(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    channel: &Arc<IpcChannel>,
) -> Result<(), NativeError> {
    let sender = channel.clone();
    let send: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        // A channel carries messages, so there is no such thing as sending
        // nothing; Node names the missing argument rather than sending
        // `undefined`.
        let Some(message) = args.first().copied() else {
            return Err(NativeError::Coded {
                kind: ErrorKind::TypeError,
                code: "ERR_MISSING_ARGS",
                message: "The \"message\" argument must be specified".to_string(),
            });
        };
        let callback = args.get(1).copied();
        ctx.scope(|mut scope| {
            let message = scope.value(message);
            let text = encode(&mut scope, message)?;
            let accepted = sender.send(&text);
            // Node reports the outcome to a callback when one is given, and
            // answers it either way.
            if let Some(callback) = callback {
                let callback = scope.value(callback);
                if scope.is_callable(callback) {
                    let error = if accepted {
                        scope.null()
                    } else {
                        scope.string("channel closed")?
                    };
                    let undefined = scope.undefined();
                    scope.call(callback, undefined, &[error])?;
                }
            }
            let answer = scope.boolean(accepted);
            Ok(scope.finish(answer))
        })
    });
    crate::process::define_process_method(scope, process, "send", 2, NativeCall::Dynamic(send))?;

    let closer = channel.clone();
    let disconnect: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
        closer.disconnect();
        ctx.scope(|mut scope| {
            let globals = scope.global_this();
            let process = scope.get(globals, "process")?;
            mark_disconnected(&mut scope, process)?;
            let undefined = scope.undefined();
            Ok(scope.finish(undefined))
        })
    });
    crate::process::define_process_method(
        scope,
        process,
        "disconnect",
        0,
        NativeCall::Dynamic(disconnect),
    )?;

    let connected = scope.boolean(true);
    scope.set(process, "connected", connected)?;

    // Node's channel is a handle a program may hold the loop open with or
    // release; ours is held by the channel itself, so these answer without
    // changing anything.
    let handle = scope.object()?;
    for name in ["ref", "unref"] {
        let call: Arc<NativeFn> = Arc::new(|_ctx, _args, _captures| Ok(Value::undefined()));
        let function = scope.native_call(name, 0, NativeCall::Dynamic(call))?;
        scope.define(handle, name, function, Attr::builtin_function().to_flags())?;
    }
    scope.set(process, "channel", handle)?;
    Ok(())
}

/// Turn a message into the text that crosses the channel.
fn encode(scope: &mut NativeScope<'_, '_>, message: Local<'_>) -> Result<String, NativeError> {
    let globals = scope.global_this();
    let json = scope.get(globals, "JSON")?;
    let stringify = scope.get(json, "stringify")?;
    let undefined = scope.undefined();
    let text = scope.call(stringify, undefined, &[message])?;
    if !scope.is_string(text) {
        // `undefined` is what `JSON.stringify` answers for a value it cannot
        // represent, and a channel carries messages, not absences.
        return Err(NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: "The \"message\" argument must be one of type string, object, number, \
                      boolean, or null"
                .to_string(),
        });
    }
    Ok(scope.display_string(text))
}

/// Record on `process` that nothing further will cross the channel.
fn mark_disconnected(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
) -> Result<(), NativeError> {
    if !scope.is_object(process) {
        return Ok(());
    }
    let closed = scope.boolean(false);
    scope.set(process, "connected", closed)?;
    let undefined = scope.undefined();
    scope.set(process, "channel", undefined)
}

/// One channel event, reported to the program on the isolate thread.
pub(crate) struct ProcessIpcEvent {
    event: IpcEvent,
}

impl ProcessIpcEvent {
    pub(crate) fn new(event: IpcEvent) -> Self {
        Self { event }
    }
}

impl crate::RuntimeTask for ProcessIpcEvent {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        runtime.run_native_event(&context, |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let process = scope.get(globals, "process")?;
                if !scope.is_object(process) {
                    let undefined = scope.undefined();
                    return Ok(scope.finish(undefined));
                }
                let emit = scope.get(process, "emit")?;
                if !scope.is_callable(emit) {
                    let undefined = scope.undefined();
                    return Ok(scope.finish(undefined));
                }
                match &self.event {
                    IpcEvent::Message(payload) => {
                        let json = scope.get(globals, "JSON")?;
                        let parse = scope.get(json, "parse")?;
                        let text = scope.string(payload)?;
                        let undefined = scope.undefined();
                        let message = scope.call(parse, undefined, &[text])?;
                        let name = scope.string("message")?;
                        scope.call(emit, process, &[name, message])?;
                    }
                    IpcEvent::Closed => {
                        mark_disconnected(&mut scope, process)?;
                        let name = scope.string("disconnect")?;
                        scope.call(emit, process, &[name])?;
                    }
                }
                let undefined = scope.undefined();
                Ok(scope.finish(undefined))
            })
        })
    }
}
