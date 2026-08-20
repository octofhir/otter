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
            let already = scope.get(process, "connected")?;
            if !scope.boolean_value(already)? {
                let undefined = scope.undefined();
                return Ok(scope.finish(undefined));
            }
            mark_disconnected(&mut scope, process)?;
            // Closing the channel from this side is a disconnect this side
            // observes too: a program that asked for it still learns the
            // channel is gone, exactly as it would had the peer closed
            // first.
            let emit = scope.get(process, "emit")?;
            if scope.is_callable(emit) {
                let name = scope.string("disconnect")?;
                scope.call(emit, process, &[name])?;
            }
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

    // The channel is a handle the program holds the run loop open with, or
    // lets go of. `process.on('message')` references it and dropping the last
    // such listener releases it, so a child that never asks for a message can
    // finish on its own.
    let handle = scope.object()?;
    for (name, referenced) in [("ref", true), ("unref", false)] {
        let holder = channel.clone();
        let call: Arc<NativeFn> = Arc::new(move |_ctx, _args, _captures| {
            holder.set_referenced(referenced);
            Ok(Value::undefined())
        });
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

/// Which event a message arrives as.
///
/// A module built on a channel — `cluster` is the one that needs it — has to
/// coordinate with its peer over the same channel the program uses. Its own
/// traffic is named, and named traffic is reported separately so a program's
/// `message` listeners only ever see what the peer's program sent.
fn event_name(
    scope: &mut NativeScope<'_, '_>,
    message: Local<'_>,
) -> Result<&'static str, NativeError> {
    if !scope.is_object(message) {
        return Ok("message");
    }
    let command = scope.get(message, "cmd")?;
    if !scope.is_string(command) {
        return Ok("message");
    }
    if scope.display_string(command).starts_with(INTERNAL_PREFIX) {
        return Ok("internalMessage");
    }
    Ok("message")
}

/// What marks a message as belonging to a module rather than to the program.
pub(crate) const INTERNAL_PREFIX: &str = "NODE_";

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
                    IpcEvent::Message(payload, handles) => {
                        let json = scope.get(globals, "JSON")?;
                        let parse = scope.get(json, "parse")?;
                        let text = scope.string(payload)?;
                        let undefined = scope.undefined();
                        let message = scope.call(parse, undefined, &[text])?;
                        let event = event_name(&mut scope, message)?;
                        let name = scope.string(event)?;
                        // A message may carry an open file. Turning it into
                        // the socket a listener expects is the job of the
                        // module that owns sockets, so the descriptor goes
                        // through its hook; without one it is closed rather
                        // than leaked.
                        let mut carried = scope.undefined();
                        for (index, handle) in handles.iter().enumerate() {
                            let adopt = scope.get(globals, "__otterIpcAdoptHandle")?;
                            if index == 0 && scope.is_callable(adopt) {
                                let fd = scope.number(f64::from(*handle));
                                let undefined = scope.undefined();
                                carried = scope.call(adopt, undefined, &[fd])?;
                            } else {
                                let _ = nix::unistd::close(*handle);
                            }
                        }
                        scope.call(emit, process, &[name, message, carried])?;
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
