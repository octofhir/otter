//! `node:child_process` native core.
//!
//! A capability-gated spawn primitive; the `ChildProcess` class and the
//! `spawn`/`exec`/`fork` surface are layered on top in `child_process.js`.
//! Process output crosses the boundary as latin1 strings (the same bridge the
//! `fs` core uses), so the JS layer can present Buffers.
//!
//! # Contents
//! - The synchronous primitive behind `spawnSync` and its callers.
//! - [`spawn_start`], which starts a child and answers at once; its outcome
//!   arrives later as a task on the isolate thread.
//! - The channel a forked child is launched to join, and the sends across it.
//!
//! # Invariants
//! - The `run` (subprocess) capability is checked before any process starts.
//! - Explicit child environments are enumerated through JavaScript internal
//!   methods, so filtered `process.env` proxies cannot leak hidden host values.
//! - Waiting for a child never happens on the isolate thread: a program that
//!   forked a child and is exchanging messages with it must keep running.
//! - No VM state is retained across the spawn.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    CapabilitySet, IpcChannel, IpcEvent, OtterError, Runtime, RuntimeLiveness,
    RuntimeLocal as Local, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeNativeScope as NativeScope, RuntimeTask, RuntimeTaskSpawner, RuntimeValue as Value,
    runtime_arg_to_string,
};
use otter_vm::object;

const SHIM: &str = include_str!("child_process.js");

/// CommonJS export: the `child_process` namespace built by `child_process.js`.
pub fn child_process_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:child_process", SHIM, module, require)
}

/// Hidden CommonJS row supplying the capability-gated spawn primitive.
pub fn child_process_native_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    native_value(scope, caps, runtime_task_spawner)
}

fn native_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    let caps_for_start = caps.clone();
    let caps = caps.clone();
    let method = scope.native_closure(
        "spawnSyncRaw",
        3,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            spawn_sync_raw(ctx, args, &caps)
        },
    )?;
    scope.set(object, "spawnSyncRaw", method)?;

    // Children this module started, and the channels it holds to the ones it
    // forked. Owned by the closures below — one table per module instance, so
    // two isolates never see each other's children.
    let children: ChildTable = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));

    let start_caps = caps_for_start.clone();
    let start_table = children.clone();
    let start_ids = next_id.clone();
    let start_spawner = spawner.clone();
    let start = scope.native_closure(
        "spawnStart",
        3,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            spawn_start(
                ctx,
                args,
                &start_caps,
                &start_table,
                &start_ids,
                start_spawner.as_ref(),
            )
        },
    )?;
    scope.set(object, "spawnStart", start)?;

    let stdin_table = children.clone();
    let stdin_write = scope.native_closure(
        "childStdinWrite",
        2,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            let id = handle_arg(args, 0);
            // A typed-array payload crosses as raw bytes; string payloads
            // take the latin1 detour, exactly like the net write path.
            let bytes = if let Some(view) = args.get(1).and_then(|v| v.as_typed_array(ctx.heap())) {
                let heap = ctx.heap();
                let offset = view.byte_offset(heap);
                let len = view.byte_length(heap);
                view.buffer(heap)
                    .with_bytes(heap, |bytes| bytes[offset..offset + len].to_vec())
            } else {
                latin1_to_bytes(&runtime_arg_to_string(args, 1, ctx.heap()))
            };
            let sender = stdin_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&id)
                .and_then(|entry| entry.stdin.clone());
            let accepted =
                sender.is_some_and(|sender| sender.send(StdinMessage::Data(bytes)).is_ok());
            Ok(Value::boolean(accepted))
        },
    )?;
    scope.set(object, "childStdinWrite", stdin_write)?;

    let stdin_end_table = children.clone();
    let stdin_end = scope.native_closure(
        "childStdinEnd",
        1,
        &[],
        move |_ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            let id = handle_arg(args, 0);
            let mut table = stdin_end_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = table.get_mut(&id)
                && let Some(sender) = entry.stdin.take()
            {
                let _ = sender.send(StdinMessage::End);
            }
            Ok(Value::undefined())
        },
    )?;
    scope.set(object, "childStdinEnd", stdin_end)?;

    let send_table = children.clone();
    let send = scope.native_closure(
        "ipcSend",
        2,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            let id = handle_arg(args, 0);
            let payload = runtime_arg_to_string(args, 1, ctx.heap());
            let channel = lookup_channel(&send_table, id);
            let accepted = channel.is_some_and(|channel| channel.send(&payload));
            Ok(Value::boolean(accepted))
        },
    )?;
    scope.set(object, "ipcSend", send)?;

    let disconnect_table = children.clone();
    let disconnect = scope.native_closure(
        "ipcDisconnect",
        1,
        &[],
        move |_ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            let id = handle_arg(args, 0);
            if let Some(channel) = lookup_channel(&disconnect_table, id) {
                channel.disconnect();
            }
            Ok(Value::undefined())
        },
    )?;
    scope.set(object, "ipcDisconnect", disconnect)?;
    Ok(object)
}

fn handle_arg(args: &[Value], index: usize) -> u32 {
    args.get(index)
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as u32
}

fn lookup_channel(children: &ChildTable, id: u32) -> Option<Arc<IpcChannel>> {
    children
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&id)
        .and_then(|entry| entry.channel.clone())
}

/// What this module keeps about one child it started. The natives are declared
/// `Send + Sync`, so the table is shared through an `Arc<Mutex<_>>` even though
/// only the isolate's own thread ever touches it.
struct ChildEntry {
    channel: Option<Arc<IpcChannel>>,
    stdin: Option<tokio::sync::mpsc::UnboundedSender<StdinMessage>>,
}

/// One instruction for a child's stdin writer task.
enum StdinMessage {
    Data(Vec<u8>),
    End,
}

type ChildTable = Arc<Mutex<HashMap<u32, ChildEntry>>>;

/// One channel event on a forked child, reported to the program.
struct ChildIpcEvent {
    id: u32,
    event: IpcEvent,
}

impl RuntimeTask for ChildIpcEvent {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        let (kind, payload) = match &self.event {
            IpcEvent::Message(payload) => ("message", payload.as_str()),
            IpcEvent::Closed => ("disconnect", ""),
        };
        runtime.run_native_event(&context, |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let dispatcher = scope.get(globals, "__otterChildIpc")?;
                if !scope.is_callable(dispatcher) {
                    let undefined = scope.undefined();
                    return Ok(scope.finish(undefined));
                }
                let id = scope.number(f64::from(self.id));
                let kind = scope.string(kind)?;
                let payload = scope.string(payload)?;
                let undefined = scope.undefined();
                let result = scope.call(dispatcher, undefined, &[id, kind, payload])?;
                Ok(scope.finish(result))
            })
        })
    }
}

/// A chunk one of the child's output pipes produced, delivered live.
#[derive(Clone)]
struct ChildStdio {
    id: u32,
    /// 1 = stdout, 2 = stderr.
    which: u8,
    /// Latin1-bridged bytes; empty marks end-of-stream.
    data: Option<String>,
}

impl RuntimeTask for ChildStdio {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        runtime.run_native_event(&context, |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let dispatcher = scope.get(globals, "__otterChildStdio")?;
                if !scope.is_callable(dispatcher) {
                    let undefined = scope.undefined();
                    return Ok(scope.finish(undefined));
                }
                let id = scope.number(f64::from(self.id));
                let which = scope.number(f64::from(self.which));
                let payload = match &self.data {
                    Some(text) => scope.string(text)?,
                    None => scope.null(),
                };
                let undefined = scope.undefined();
                let result = scope.call(dispatcher, undefined, &[id, which, payload])?;
                Ok(scope.finish(result))
            })
        })
    }
}

/// A child's outcome, reported once it has run to completion.
#[derive(Clone)]
struct ChildExit {
    id: u32,
    status: Option<i32>,
    signal: Option<String>,
}

impl RuntimeTask for ChildExit {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        runtime.run_native_event(&context, |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let dispatcher = scope.get(globals, "__otterChildExit")?;
                if !scope.is_callable(dispatcher) {
                    let undefined = scope.undefined();
                    return Ok(scope.finish(undefined));
                }
                let id = scope.number(f64::from(self.id));
                let status = match self.status {
                    Some(code) => scope.number(f64::from(code)),
                    None => scope.null(),
                };
                let signal = match &self.signal {
                    Some(name) => scope.string(name)?,
                    None => scope.null(),
                };
                let undefined = scope.undefined();
                let result = scope.call(dispatcher, undefined, &[id, status, signal])?;
                Ok(scope.finish(result))
            })
        })
    }
}

/// Start a child and answer at once, so `spawn` returns a live handle the way
/// Node's does. The outcome arrives later as a task on the isolate thread —
/// waiting here would stop a program that forked a child from ever hearing
/// from it.
fn spawn_start(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
    children: &ChildTable,
    next_id: &Arc<AtomicU32>,
    spawner: Option<&RuntimeTaskSpawner>,
) -> Result<Value, NativeError> {
    let command = runtime_arg_to_string(args, 0, ctx.heap());
    if command.is_empty() {
        return Err(crate::type_error("child_process", "command is required"));
    }
    if !caps.run.matches(&command) {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::Error,
            code: "EACCES",
            message: format!("EACCES: subprocess capability denied for '{command}'"),
        });
    }

    let mut argv = args
        .get(1)
        .copied()
        .map(|value| read_string_array(ctx, value))
        .unwrap_or_default();
    let opts = args.get(2).copied();
    let cwd = opt_string(ctx, opts, "cwd");
    let env = opt_env(ctx, opts)?;
    let wants_channel = opt_flag(ctx, opts, "ipc");
    if should_propagate_allow_all(ctx, &command, caps) {
        argv.insert(0, "--allow-all".to_string());
    }

    let Some(spawner) = spawner else {
        return Err(crate::type_error(
            "child_process",
            "host runtime did not install an event loop",
        ));
    };
    let id = next_id.fetch_add(1, Ordering::Relaxed);

    // The channel is opened before the child starts, so the address it is told
    // to join is already listening when it gets there.
    let channel = if wants_channel {
        let (channel, address) =
            IpcChannel::listen(spawner, move |event| ChildIpcEvent { id, event })
                .map_err(|error| crate::type_error("child_process", error.to_string()))?;
        Some((channel, address))
    } else {
        None
    };

    let mut cmd = Command::new(&command);
    cmd.args(&argv);
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env);
    }
    match &channel {
        Some((_, address)) => {
            cmd.env(otter_runtime::ipc::CHANNEL_VAR, address);
        }
        // A child that was not forked must not believe it inherited this
        // process's own channel.
        None => {
            cmd.env_remove(otter_runtime::ipc::CHANNEL_VAR);
        }
    }
    // Each standard stream is named separately, because a caller reading one
    // and leaving another to this process's own output is an ordinary thing to
    // ask for. A stream nobody intends to read must not become a pipe nobody
    // drains.
    let streams = match opts {
        Some(options) => {
            let named = value_of(ctx, options, "stdio");
            read_string_array(ctx, named)
        }
        None => Vec::new(),
    };
    let mut piped = [false; 3];
    for (index, slot) in [0usize, 1, 2].into_iter().enumerate() {
        let how = streams.get(slot).map(String::as_str).unwrap_or("pipe");
        let target = match how {
            "inherit" => Stdio::inherit(),
            "ignore" => Stdio::null(),
            _ => {
                piped[slot] = true;
                Stdio::piped()
            }
        };
        match index {
            0 => cmd.stdin(target),
            1 => cmd.stdout(target),
            _ => cmd.stderr(target),
        };
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return spawn_error_result(ctx, &command, &err),
    };
    let pid = child.id();

    // The stdin writer runs on the IO runtime and owns the pipe; End (or the
    // sender dropping) closes it, which is the EOF the child reads.
    let stdin_sender = child.stdin.take().map(|pipe| {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<StdinMessage>();
        if let Some(io) = spawner.io_handle() {
            io.spawn(stdin_writer(pipe, receiver));
        }
        sender
    });

    children
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            ChildEntry {
                channel: channel.map(|(channel, _)| channel),
                stdin: stdin_sender,
            },
        );

    reap(child, id, spawner);

    ctx.scope(|mut scope| {
        let object = scope.object()?;
        let id_value = scope.number(f64::from(id));
        scope.set(object, "id", id_value)?;
        let pid_value = scope.number(f64::from(pid));
        scope.set(object, "pid", pid_value)?;
        Ok(scope.finish(object))
    })
}


/// Switch a pipe descriptor to non-blocking mode, which is what tokio's
/// `from_std` conversions require of a handle they adopt.
#[cfg(unix)]
fn set_nonblocking<F: std::os::fd::AsFd>(pipe: &F) -> bool {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let fd = pipe.as_fd();
    let Ok(flags) = fcntl(fd, FcntlArg::F_GETFL) else {
        return false;
    };
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).is_ok()
}

#[cfg(not(unix))]
fn set_nonblocking<F>(_pipe: &F) -> bool {
    true
}

/// Wait for a child away from the isolate thread and report its outcome
/// there. Output pipes stream live chunks as they arrive; the exit report is
/// enqueued only after both pipes reached end-of-stream, so listeners always
/// see every chunk before 'exit'.
fn reap(mut child: std::process::Child, id: u32, spawner: &RuntimeTaskSpawner) {
    let Some(io) = spawner.io_handle() else {
        return;
    };
    // A running child is work the program is waiting on, so it holds the run
    // loop open until its outcome has been reported.
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let exit_spawner = spawner.clone();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    io.spawn(async move {
        let out_task = stdout
            .filter(set_nonblocking)
            .and_then(|pipe| tokio::process::ChildStdout::from_std(pipe).ok())
            .map(|pipe| tokio::spawn(stream_pipe(pipe, id, 1, exit_spawner.clone())));
        let err_task = stderr
            .filter(set_nonblocking)
            .and_then(|pipe| tokio::process::ChildStderr::from_std(pipe).ok())
            .map(|pipe| tokio::spawn(stream_pipe(pipe, id, 2, exit_spawner.clone())));
        let status = tokio::task::spawn_blocking(move || child.wait()).await;
        if let Some(task) = out_task {
            let _ = task.await;
        }
        if let Some(task) = err_task {
            let _ = task.await;
        }
        let exit = match status {
            Ok(Ok(status)) => ChildExit {
                id,
                status: status.code(),
                signal: exit_signal(&status),
            },
            _ => ChildExit {
                id,
                status: None,
                signal: None,
            },
        };
        // The exit report itself holds the loop: the child's own Ref hold is
        // released right after, and an Unref message could otherwise still be
        // in the inbox when the loop finds nothing left to wait for.
        exit_spawner.enqueue_ordered(exit, RuntimeLiveness::Ref).await;
        drop(keep_alive);
    });
}

/// Read one output pipe to end-of-stream, delivering each chunk live and a
/// final `None` marking the end.
async fn stream_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    id: u32,
    which: u8,
    spawner: RuntimeTaskSpawner,
) {
    use tokio::io::AsyncReadExt;
    let mut chunk = vec![0u8; 65_536];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(length) => {
                if !spawner
                    .enqueue_ordered(
                        ChildStdio {
                            id,
                            which,
                            data: Some(bytes_to_latin1(&chunk[..length])),
                        },
                        RuntimeLiveness::Unref,
                    )
                    .await
                {
                    return;
                }
            }
        }
    }
    spawner
        .enqueue_ordered(
            ChildStdio {
                id,
                which,
                data: None,
            },
            RuntimeLiveness::Unref,
        )
        .await;
}

/// Own a child's stdin pipe: write queued bytes in order and close on `End`
/// (or when the JS side drops the queue), which is the child's EOF.
async fn stdin_writer(
    pipe: std::process::ChildStdin,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<StdinMessage>,
) {
    use tokio::io::AsyncWriteExt;
    if !set_nonblocking(&pipe) {
        return;
    }
    let Ok(mut pipe) = tokio::process::ChildStdin::from_std(pipe) else {
        return;
    };
    while let Some(message) = receiver.recv().await {
        match message {
            StdinMessage::Data(bytes) => {
                if pipe.write_all(&bytes).await.is_err() {
                    return;
                }
                let _ = pipe.flush().await;
            }
            StdinMessage::End => break,
        }
    }
    let _ = pipe.shutdown().await;
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn latin1_to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

/// Read a JS array of strings into a `Vec<String>`.
fn read_string_array(ctx: &mut NativeCtx<'_>, value: Value) -> Vec<String> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    let len = otter_vm::array::len(arr, ctx.heap());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = otter_vm::array::get(arr, ctx.heap(), i);
        out.push(v.display_string(ctx.heap()));
    }
    out
}

fn opt_string(ctx: &mut NativeCtx<'_>, opts: Option<Value>, key: &str) -> Option<String> {
    let obj = opts?.as_object()?;
    let v = object::get(obj, ctx.heap(), key)?;
    if v.is_string() {
        Some(v.display_string(ctx.heap()))
    } else {
        None
    }
}

/// Read one property of an options object, as a value the array reader can take.
fn value_of(ctx: &mut NativeCtx<'_>, opts: Value, key: &str) -> Value {
    opts.as_object()
        .and_then(|object| object::get(object, ctx.heap(), key))
        .unwrap_or_else(Value::undefined)
}

fn opt_flag(ctx: &mut NativeCtx<'_>, opts: Option<Value>, key: &str) -> bool {
    let Some(value) = opts
        .and_then(Value::as_object)
        .and_then(|obj| object::get(obj, ctx.heap(), key))
    else {
        return false;
    };
    value.to_boolean(ctx.heap())
}

fn opt_env(
    ctx: &mut NativeCtx<'_>,
    opts: Option<Value>,
) -> Result<Option<Vec<(String, String)>>, NativeError> {
    let Some(opts_obj) = opts.and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(env) = object::get(opts_obj, ctx.heap(), "env") else {
        return Ok(None);
    };
    if !env.is_object_type() {
        return Ok(None);
    }
    ctx.scope(|mut scope| {
        let env = scope.value(env);
        let keys = scope.enumerable_own_string_keys(env)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let value = scope.get(env, &key)?;
            if !scope.is_undefined(value) && !scope.is_null(value) {
                out.push((key, scope.display_string(value)));
            }
        }
        Ok(Some(out))
    })
}

fn current_exec_path(ctx: &mut NativeCtx<'_>) -> Option<String> {
    let global = *ctx.interp_mut().global_this();
    let process = object::get(global, ctx.heap(), "process")?.as_object()?;
    let exec_path = object::get(process, ctx.heap(), "execPath")?;
    Some(exec_path.display_string(ctx.heap()))
}

fn should_propagate_allow_all(
    ctx: &mut NativeCtx<'_>,
    command: &str,
    caps: &CapabilitySet,
) -> bool {
    caps.read.is_allow_all()
        && caps.write.is_allow_all()
        && caps.net.is_allow_all()
        && caps.env.is_allow_all()
        && caps.run.is_allow_all()
        && caps.ffi.is_allow_all()
        && {
            // A symlink to the engine binary is still the engine binary; the
            // corpus spawns itself through one and expects the same grants.
            let command = std::fs::canonicalize(command).ok();
            let exec = current_exec_path(ctx).and_then(|exec| std::fs::canonicalize(exec).ok());
            command.is_some() && command == exec
        }
}

fn spawn_sync_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let command = runtime_arg_to_string(args, 0, ctx.heap());
    if command.is_empty() {
        return Err(crate::type_error("child_process", "command is required"));
    }
    if !caps.run.matches(&command) {
        return Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::Error,
            code: "EACCES",
            message: format!("EACCES: subprocess capability denied for '{command}'"),
        });
    }

    let mut argv = args
        .get(1)
        .copied()
        .map(|v| read_string_array(ctx, v))
        .unwrap_or_default();
    let opts = args.get(2).copied();
    let cwd = opt_string(ctx, opts, "cwd");
    let input = opt_string(ctx, opts, "input");
    let env = opt_env(ctx, opts)?;
    if should_propagate_allow_all(ctx, &command, caps) {
        argv.insert(0, "--allow-all".to_string());
    }

    let mut cmd = Command::new(&command);
    cmd.args(&argv);
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env);
    }
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let spawn_result = cmd.spawn();
    let mut child = match spawn_result {
        Ok(child) => child,
        Err(err) => return spawn_error_result(ctx, &command, &err),
    };
    let pid = child.id();

    if let (Some(input), Some(mut stdin)) = (&input, child.stdin.take()) {
        use std::io::Write;
        let _ = stdin.write_all(&latin1_to_bytes(input));
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(err) => return spawn_error_result(ctx, &command, &err),
    };

    let status_code = output.status.code();
    let signal = exit_signal(&output.status);
    let stdout = bytes_to_latin1(&output.stdout);
    let stderr = bytes_to_latin1(&output.stderr);

    ctx.scope(|mut scope| {
        let object = scope.object()?;
        set_number(&mut scope, object, "pid", f64::from(pid))?;
        match status_code {
            Some(code) => set_number(&mut scope, object, "status", f64::from(code))?,
            None => set_null(&mut scope, object, "status")?,
        }
        match signal {
            Some(signal) => {
                let signal = scope.string(&signal)?;
                scope.set(object, "signal", signal)?;
            }
            None => set_null(&mut scope, object, "signal")?,
        }
        let stdout = scope.string(&stdout)?;
        scope.set(object, "stdout", stdout)?;
        let stderr = scope.string(&stderr)?;
        scope.set(object, "stderr", stderr)?;
        set_null(&mut scope, object, "error")?;
        Ok(scope.finish(object))
    })
}

fn spawn_error_result(
    ctx: &mut NativeCtx<'_>,
    command: &str,
    err: &std::io::Error,
) -> Result<Value, NativeError> {
    let code = if err.kind() == std::io::ErrorKind::NotFound {
        "ENOENT"
    } else {
        "EIO"
    };
    let message = format!("{code}: spawn {command} {err}");
    ctx.scope(|mut scope| {
        let object = scope.object()?;
        set_null(&mut scope, object, "pid")?;
        set_null(&mut scope, object, "status")?;
        set_null(&mut scope, object, "signal")?;
        let stdout = scope.string("")?;
        scope.set(object, "stdout", stdout)?;
        let stderr = scope.string("")?;
        scope.set(object, "stderr", stderr)?;
        let error = scope.string(&message)?;
        scope.set(object, "error", error)?;
        let error_code = scope.string(code)?;
        scope.set(object, "errorCode", error_code)?;
        Ok(scope.finish(object))
    })
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| signal_name(s).to_string())
}
#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "SIGTERM",
    }
}

fn set_null(
    scope: &mut NativeScope<'_, '_>,
    object: Local<'_>,
    key: &str,
) -> Result<(), NativeError> {
    let value = scope.null();
    scope.set(object, key, value)
}

fn set_number(
    scope: &mut NativeScope<'_, '_>,
    object: Local<'_>,
    key: &str,
    value: f64,
) -> Result<(), NativeError> {
    let value = scope.number(value);
    scope.set(object, key, value)
}
