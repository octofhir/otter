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

use std::os::fd::RawFd;
use std::sync::Arc;

use otter_vm::{Attr, ErrorKind, Local, NativeCall, NativeError, NativeFn, NativeScope, Value};

use crate::ipc::{CarriedHandles, IpcChannel, IpcEvent, IpcSendError};
use crate::{OtterError, Runtime};

/// Give `process` its channel members.
///
/// # Errors
/// Returns a native error when a member cannot be allocated or defined.
pub(crate) fn install(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    channel: &Arc<IpcChannel>,
    cjs: &Arc<crate::commonjs::CjsConfig>,
) -> Result<(), NativeError> {
    let sender = channel.clone();
    let cfg = cjs.clone();
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
        let rest: Vec<Value> = args.iter().skip(1).copied().collect();
        ctx.scope(|mut scope| {
            let message = scope.value(message);
            // Node's argument shuffle: everything after the message is
            // optional and a function anywhere in it is the callback. What is
            // left is the socket to hand over, and its options.
            let mut callback = None;
            let mut positional = [None, None];
            for (index, value) in rest.iter().enumerate() {
                let value = scope.value(*value);
                if scope.is_callable(value) {
                    if callback.is_none() {
                        callback = Some(value);
                    }
                } else if index < positional.len() {
                    positional[index] = Some(value);
                }
            }
            // A message may hand an open socket to the peer, and what that
            // socket is meant to be on arrival is the sockets module's
            // protocol — as is what counts as a socket at all. Anything given
            // past the message goes there to be shaped and refused; the
            // channel only moves the result.
            let carries = positional
                .iter()
                .any(|slot| slot.is_some_and(|value| !scope.is_undefined(value)));
            let (text, handle, token) = if carries {
                let send_handle = positional[0].unwrap_or_else(|| scope.undefined());
                let options = positional[1].unwrap_or_else(|| scope.undefined());
                prepare_send(&mut scope, &cfg, message, send_handle, options)?
            } else {
                (encode(&mut scope, message)?, -1, None)
            };
            let outcome = if handle >= 0 {
                sender.send_with_handles(&text, vec![handle], token)
            } else {
                sender.send(&text)
            };
            let accepted = outcome.is_ok();
            // A message that never left takes what it carried with it; the
            // module that handed it over hears so at once rather than waiting
            // for a crossing that will not happen.
            if outcome.is_err()
                && let Some(token) = token
            {
                report_sent(&mut scope, token)?;
            }
            // Node reports the outcome to a callback when one is given, and
            // answers it either way.
            if let Some(callback) = callback {
                {
                    let error = match outcome {
                        Ok(()) => scope.null(),
                        Err(reason) => {
                            let (message, code) = match reason {
                                IpcSendError::Disconnected => {
                                    ("IPC channel is closed", "ERR_IPC_CHANNEL_CLOSED")
                                }
                                IpcSendError::Backpressure => {
                                    ("IPC channel backlog limit", "ENOBUFS")
                                }
                            };
                            let error = scope.error(ErrorKind::Error, message)?;
                            let code = scope.string(code)?;
                            scope.set(error, "code", code)?;
                            error
                        }
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
                // Disconnecting a channel that has already gone is an error
                // the program hears about, the same way it hears about every
                // other channel failure.
                let emit = scope.get(process, "emit")?;
                if scope.is_callable(emit) {
                    let name = scope.string("error")?;
                    let error = scope.error(
                        otter_vm::ErrorKind::Error,
                        "IPC channel is already disconnected",
                    )?;
                    let code = scope.string("ERR_IPC_DISCONNECTED")?;
                    scope.set(error, "code", code)?;
                    scope.call(emit, process, &[name, error])?;
                }
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

    // A channel is read because the program is listening on it. The listener
    // machinery reaches this through the slot rather than through the channel,
    // which it has no other way to name.
    let reader = channel.clone();
    let start_reading: Arc<NativeFn> = Arc::new(move |_ctx, _args, _captures| {
        reader.start_reading();
        Ok(Value::undefined())
    });
    let start_reading = scope.native_call("startReading", 0, NativeCall::Dynamic(start_reading))?;
    scope.define(
        process,
        CHANNEL_READ_SLOT,
        start_reading,
        Attr {
            writable: false,
            enumerable: false,
            configurable: false,
        }
        .to_flags(),
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

/// Where the channel's "start reading" hook sits on `process`.
pub(crate) const CHANNEL_READ_SLOT: &str = "__otterChannelStartReading";

/// Reach one half of the handle protocol, loading the module that owns it if
/// the program has not required it yet.
///
/// The protocol belongs to the module that owns sockets. A program that never
/// required it can still be handed a descriptor, or hand one over, so it is
/// loaded the first time either happens.
fn install_protocol(
    scope: &mut NativeScope<'_, '_>,
    cfg: &Arc<crate::commonjs::CjsConfig>,
    name: &str,
) -> Result<(), NativeError> {
    let globals = scope.global_this();
    let hook = scope.get(globals, name)?;
    if scope.is_callable(hook) {
        return Ok(());
    }
    crate::commonjs::cjs_load_builtin(scope, cfg, "child_process").map(|_| ())
}

/// The text and descriptor a message carrying an open socket crosses as.
///
/// What the peer should see on arrival — a bare handle, a `net.Socket`, a
/// server — is the sockets module's protocol, so the shaping happens there
/// and the channel only moves the result.
fn prepare_send(
    scope: &mut NativeScope<'_, '_>,
    cfg: &Arc<crate::commonjs::CjsConfig>,
    message: Local<'_>,
    send_handle: Local<'_>,
    options: Local<'_>,
) -> Result<(String, RawFd, Option<u32>), NativeError> {
    install_protocol(scope, cfg, "__otterIpcPrepareSend")?;
    let globals = scope.global_this();
    let prepare = scope.get(globals, "__otterIpcPrepareSend")?;
    if !scope.is_callable(prepare) {
        return Ok((encode(scope, message)?, -1, None));
    }
    let undefined = scope.undefined();
    let prepared = scope.call(prepare, undefined, &[message, send_handle, options])?;
    if !scope.is_object(prepared) {
        return Err(NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_INVALID_ARG_TYPE",
            message: "The \"message\" argument must be one of type string, object, number, \
                      or boolean"
                .to_string(),
        });
    }
    let text = scope.get(prepared, "text")?;
    let fd = scope.get(prepared, "fd")?;
    let fd = scope.number_value(fd)? as RawFd;
    // A message that leaves something behind names it, so the module that
    // owns what it carried can be told once the message has gone.
    let token = scope.get(prepared, "token")?;
    let token = scope.number_value(token)? as u32;
    Ok((
        scope.display_string(text),
        fd,
        (token != 0).then_some(token),
    ))
}

/// Where the module that owns handles hears that a message has gone.
pub(crate) const SENT_HOOK: &str = "__otterIpcSent";

/// Tell the module that handed a message over that it has left this process.
fn report_sent(scope: &mut NativeScope<'_, '_>, token: u32) -> Result<(), NativeError> {
    let globals = scope.global_this();
    let hook = scope.get(globals, SENT_HOOK)?;
    if !scope.is_callable(hook) {
        return Ok(());
    }
    let token = scope.number(f64::from(token));
    let undefined = scope.undefined();
    scope.call(hook, undefined, &[token])?;
    Ok(())
}

/// Let go of everything that was still on its way out when the channel went.
fn release_pending_sends(scope: &mut NativeScope<'_, '_>) -> Result<(), NativeError> {
    let globals = scope.global_this();
    let hook = scope.get(globals, "__otterIpcSentAll")?;
    if !scope.is_callable(hook) {
        return Ok(());
    }
    let own = scope.number(0.0);
    let undefined = scope.undefined();
    scope.call(hook, undefined, &[own])?;
    Ok(())
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
    fn run(mut self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        // What the message carried is held apart from the event, so that every
        // way out of this delivery — including one that fails partway — closes
        // it exactly once.
        let mut carried = CarriedHandles::new(self.event.take_handles());
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        let cjs_config = Arc::new(crate::commonjs::CjsConfig {
            capabilities: runtime.config.capabilities.clone(),
            hosted: runtime.config.hosted_modules.clone(),
            runtime_task_spawner: runtime.runtime_task_spawner.clone(),
            addon_loader: runtime.config.commonjs_addon_loader,
            report_watch_dependencies: crate::commonjs::watch_reporting_requested(),
        });
        runtime.run_native_event(&context, move |ctx| {
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
                    IpcEvent::Message(message) => {
                        let json = scope.get(globals, "JSON")?;
                        let parse = scope.get(json, "parse")?;
                        let text = scope.string(message.text())?;
                        let undefined = scope.undefined();
                        let message = scope.call(parse, undefined, &[text])?;
                        // A message may carry an open file, and what the
                        // sender meant by it — a bare handle, a connection, a
                        // server — is the sockets module's protocol. The
                        // message goes through its hook, which answers the
                        // event, the message the program sees, and the object
                        // the descriptor became.
                        let Some(first) = carried.take_first() else {
                            let event = event_name(&mut scope, message)?;
                            let name = scope.string(event)?;
                            scope.call(emit, process, &[name, message])?;
                            let undefined = scope.undefined();
                            return Ok(scope.finish(undefined));
                        };
                        install_protocol(&mut scope, &cjs_config, "__otterIpcDeliver")?;
                        let globals = scope.global_this();
                        let deliver = scope.get(globals, "__otterIpcDeliver")?;
                        if !scope.is_callable(deliver) {
                            let _ = nix::unistd::close(first);
                            let event = event_name(&mut scope, message)?;
                            let name = scope.string(event)?;
                            scope.call(emit, process, &[name, message])?;
                            let undefined = scope.undefined();
                            return Ok(scope.finish(undefined));
                        }
                        // The hook owns the descriptor from the call on.
                        let fd = scope.number(f64::from(first));
                        let undefined = scope.undefined();
                        let delivered = scope.call(deliver, undefined, &[message, fd])?;
                        let event = scope.get(delivered, "event")?;
                        let inner = scope.get(delivered, "message")?;
                        let handle = scope.get(delivered, "handle")?;
                        scope.call(emit, process, &[event, inner, handle])?;
                    }
                    IpcEvent::Sent(token) => report_sent(&mut scope, *token)?,
                    IpcEvent::Closed => {
                        release_pending_sends(&mut scope)?;
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
