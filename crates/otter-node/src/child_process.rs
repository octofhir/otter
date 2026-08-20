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
    CapabilitySet, CarriedHandles, IpcChannel, IpcEvent, OtterError, Runtime, RuntimeLiveness,
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

    let send_table = children.clone();
    let send = scope.native_closure(
        "ipcSend",
        4,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            let id = handle_arg(args, 0);
            let payload = runtime_arg_to_string(args, 1, ctx.heap());
            // What the caller calls this message. A message that leaves
            // something behind is named so the module that owns what it
            // carried can be told once the message has gone.
            let token = args
                .get(3)
                .and_then(|value| value.as_f64())
                .map(|token| token as u32)
                .filter(|token| *token != 0);
            // A third argument is an open file to hand over with the message.
            // It is already a duplicate made for the crossing, so the channel
            // closes it once it is sent.
            let handles: Vec<std::os::fd::RawFd> = args
                .get(2)
                .and_then(|value| value.as_f64())
                .filter(|fd| *fd >= 0.0)
                .map(|fd| vec![fd as std::os::fd::RawFd])
                .unwrap_or_default();
            let channel = lookup_channel(&send_table, id);
            let accepted = match channel {
                Some(channel) => channel.send_with_handles(&payload, handles, token),
                None => {
                    for handle in handles {
                        let _ = nix::unistd::close(handle);
                    }
                    false
                }
            };
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
    /// Whether the child has already been reported as gone. An entry is kept
    /// only while something about the child is still live: its channel can
    /// close after it exits, and it can exit with its channel still open.
    exited: bool,
}

/// Forget a child once neither its channel nor the child itself is left.
///
/// Called on both of those endings, in whichever order they happen, so the
/// table holds only children a program can still act on.
fn retire(children: &ChildTable, id: u32, gone: Ending) {
    let mut table = children
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(entry) = table.get_mut(&id) else {
        return;
    };
    match gone {
        Ending::Child => entry.exited = true,
        Ending::Channel => entry.channel = None,
    }
    if entry.exited && entry.channel.is_none() {
        table.remove(&id);
    }
}

/// Which half of a child has ended.
#[derive(Clone, Copy)]
enum Ending {
    Child,
    Channel,
}

type ChildTable = Arc<Mutex<HashMap<u32, ChildEntry>>>;

/// One channel event on a forked child, reported to the program.
struct ChildIpcEvent {
    id: u32,
    event: IpcEvent,
    children: ChildTable,
}

impl RuntimeTask for ChildIpcEvent {
    fn run(mut self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        // The descriptor is handed over as itself; the module that asked for
        // the message is what turns it into a socket. Until it is handed over
        // it is held here, so an event nobody hears still closes what it
        // carried.
        let mut carried = CarriedHandles::new(self.event.take_handles());
        if matches!(self.event, IpcEvent::Closed) {
            retire(&self.children, self.id, Ending::Channel);
        }
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        let id = self.id;
        let (kind, payload) = match self.event {
            IpcEvent::Message(payload, _) => ("message", payload),
            IpcEvent::Closed => ("disconnect", String::new()),
            IpcEvent::Sent(token) => ("sent", token.to_string()),
        };
        runtime.run_native_event(&context, move |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let dispatcher = scope.get(globals, "__otterChildIpc")?;
                if !scope.is_callable(dispatcher) {
                    let undefined = scope.undefined();
                    return Ok(scope.finish(undefined));
                }
                let handle_fd = carried.take_first();
                let id = scope.number(f64::from(id));
                let kind = scope.string(kind)?;
                let payload = scope.string(&payload)?;
                let handle = match handle_fd {
                    Some(fd) => scope.number(f64::from(fd)),
                    None => scope.undefined(),
                };
                let undefined = scope.undefined();
                let result = scope.call(dispatcher, undefined, &[id, kind, payload, handle])?;
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
    children: ChildTable,
}

impl RuntimeTask for ChildExit {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        retire(&self.children, self.id, Ending::Child);
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

    let argv = args
        .get(1)
        .copied()
        .map(|value| read_string_array(ctx, value))
        .unwrap_or_default();
    let opts = args.get(2).copied();
    // A directory named as nothing is not a directory to change to; the child
    // starts where this process is.
    let cwd = opt_string(ctx, opts, "cwd").filter(|dir| !dir.is_empty());
    let argv0 = opt_string(ctx, opts, "argv0");
    let env = opt_env(ctx, opts)?;
    let credentials = opt_credentials(ctx, opts);
    let wants_channel = opt_flag(ctx, opts, "ipc");

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
        let events = children.clone();
        let (channel, address) =
            IpcChannel::listen(spawner, move |event| ChildIpcEvent {
                id,
                event,
                children: events.clone(),
            })
                .map_err(|error| crate::type_error("child_process", error.to_string()))?;
        Some((channel, address))
    } else {
        None
    };

    let mut cmd = Command::new(&command);
    cmd.args(&argv);
    credentials.apply(&mut cmd);
    // The name a child sees itself under is the caller's to choose, and is not
    // the same thing as the file that was run.
    #[cfg(unix)]
    if let Some(argv0) = &argv0 {
        std::os::unix::process::CommandExt::arg0(&mut cmd, argv0);
    }
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env);
    }
    grant(&mut cmd, caps);
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
    // Each stream is named separately, because a caller reading one and
    // leaving another to this process's own output is an ordinary thing to ask
    // for — and there can be more of them than three, which is what a program
    // handing a child an extra descriptor is doing.
    let plans = match stdio_plans(ctx, opts) {
        Ok(plans) => plans,
        Err(error) => return spawn_error_result(ctx, &command, &error, &[]),
    };
    let wired = match wire_stdio(&mut cmd, plans) {
        Ok(wired) => wired,
        Err(error) => return spawn_error_result(ctx, &command, &error, &[]),
    };
    let ours: Vec<std::os::fd::RawFd> = wired.kept.iter().map(fd_number).collect();

    let spawned = cmd.spawn();
    // The child has its own copies of everything it was given; these ends are
    // this process's to close whether the launch worked or not.
    drop(wired.theirs);
    let child = match spawned {
        Ok(child) => child,
        Err(err) => {
            // A launch that failed still leaves the caller with the streams it
            // asked for: they end at once, because the other end of each went
            // with the child that never was.
            for fd in wired.kept.into_iter().flatten() {
                let _ = std::os::fd::IntoRawFd::into_raw_fd(fd);
            }
            return spawn_error_result(ctx, &command, &err, &ours);
        }
    };
    let pid = child.id();
    // The ends this process kept are handed to the program, which carries them
    // as the streams the caller asked for.
    for fd in wired.kept.into_iter().flatten() {
        let _ = std::os::fd::IntoRawFd::into_raw_fd(fd);
    }

    children
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            ChildEntry {
                channel: channel.map(|(channel, _)| channel),
                exited: false,
            },
        );

    reap(child, id, spawner, children.clone());

    ctx.scope(|mut scope| {
        let object = scope.object()?;
        let id_value = scope.number(f64::from(id));
        scope.set(object, "id", id_value)?;
        let pid_value = scope.number(f64::from(pid));
        scope.set(object, "pid", pid_value)?;
        // One descriptor per stream the caller asked to hold, in the order
        // they were asked for; a stream this process kept no end of is -1.
        let list = scope.array(ours.len())?;
        for (slot, fd) in ours.iter().enumerate() {
            let value = scope.number(f64::from(*fd));
            scope.set_index(list, slot, value)?;
        }
        scope.set(object, "stdio", list)?;
        Ok(scope.finish(object))
    })
}

/// The ends of a child's streams once the command has been told what its own
/// are: `kept` is this process's end of each slot, `theirs` the child's ends,
/// which have to outlive the spawn and nothing more.
struct WiredStdio {
    kept: Vec<Option<std::os::fd::OwnedFd>>,
    theirs: Vec<std::os::fd::OwnedFd>,
}

/// The number a kept end answers to, or -1 for a slot this process kept
/// nothing of.
fn fd_number(kept: &Option<std::os::fd::OwnedFd>) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    kept.as_ref().map_or(-1, AsRawFd::as_raw_fd)
}

/// Tell a command what each of its streams is, and answer the ends this
/// process keeps.
///
/// The first three slots are the ones every process has, and the command
/// carries them itself. A slot past those is put in place by the child between
/// the fork and the exec, because there is nowhere else to say it.
fn wire_stdio(cmd: &mut Command, plans: Vec<StdioPlan>) -> Result<WiredStdio, std::io::Error> {
    use std::os::fd::AsRawFd;
    let mut wired = WiredStdio {
        kept: Vec::with_capacity(plans.len()),
        theirs: Vec::new(),
    };
    let mut places: Vec<(std::os::fd::RawFd, std::os::fd::RawFd)> = Vec::new();
    for (slot, plan) in plans.into_iter().enumerate() {
        let inherit = matches!(plan, StdioPlan::Inherit) && slot < 3;
        let (theirs, ours) = match plan {
            // Past the third slot there is no stream to inherit: a program
            // that asks for one is asking for nothing.
            StdioPlan::Inherit | StdioPlan::Ignore => (None, None),
            StdioPlan::Fd(fd) => (Some(fd), None),
            StdioPlan::Pipe { parent, child } => (Some(child), Some(parent)),
        };
        wired.kept.push(ours);
        if slot > 2 {
            if let Some(theirs) = theirs {
                places.push((theirs.as_raw_fd(), slot as std::os::fd::RawFd));
                wired.theirs.push(theirs);
            }
            continue;
        }
        let target = match theirs {
            Some(fd) => Stdio::from(fd),
            None if inherit => Stdio::inherit(),
            None => Stdio::null(),
        };
        match slot {
            0 => cmd.stdin(target),
            1 => cmd.stdout(target),
            _ => cmd.stderr(target),
        };
    }
    if !places.is_empty() {
        // SAFETY: this runs in the forked child before `exec`, where only
        // async-signal-safe calls are allowed. `dup2` and `fcntl` are both on
        // that list, and nothing here allocates, locks, or reads shared state.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(cmd, move || {
                for (source, slot) in &places {
                    if source == slot {
                        // Already in place; it only has to survive the exec.
                        if libc::fcntl(*slot, libc::F_SETFD, 0) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(*source, *slot) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    Ok(wired)
}

/// What one of a child's streams is wired to.
enum StdioPlan {
    /// This process's own stream.
    Inherit,
    /// Nothing at all.
    Ignore,
    /// A pipe: the child gets one end, the program the other.
    Pipe {
        parent: std::os::fd::OwnedFd,
        child: std::os::fd::OwnedFd,
    },
    /// A descriptor the caller named. The child gets a copy of it, so closing
    /// the child's streams never takes the caller's own away.
    Fd(std::os::fd::OwnedFd),
}

/// Who the child runs as, when the caller said.
#[derive(Clone, Copy, Default)]
struct Credentials {
    uid: Option<u32>,
    gid: Option<u32>,
}

impl Credentials {
    /// Tell a command who to become. Whether it may is the platform's answer,
    /// which arrives as the launch failing.
    fn apply(self, cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        if let Some(uid) = self.uid {
            cmd.uid(uid);
        }
        if let Some(gid) = self.gid {
            cmd.gid(gid);
        }
    }
}

/// The user and group the caller named for the child.
fn opt_credentials(ctx: &mut NativeCtx<'_>, opts: Option<Value>) -> Credentials {
    Credentials {
        uid: opt_number(ctx, opts, "uid").map(|uid| uid as u32),
        gid: opt_number(ctx, opts, "gid").map(|gid| gid as u32),
    }
}

/// Read what the caller asked each of the child's streams to be.
///
/// A slot is named ("pipe", "ignore", "inherit") or is a descriptor the caller
/// already holds. The channel a forked child speaks over is not a stream and
/// is arranged separately, so it takes no descriptor here.
fn stdio_plans(
    ctx: &mut NativeCtx<'_>,
    opts: Option<Value>,
) -> Result<Vec<StdioPlan>, std::io::Error> {
    let named = opts.map(|options| value_of(ctx, options, "stdio"));
    let entries: Vec<Value> = match named.and_then(|value| value.as_array()) {
        Some(array) => {
            let length = otter_vm::array::len(array, ctx.heap());
            (0..length)
                .map(|index| otter_vm::array::get(array, ctx.heap(), index))
                .collect()
        }
        None => Vec::new(),
    };
    let mut plans = Vec::with_capacity(entries.len().max(3));
    for (slot, entry) in entries.iter().enumerate() {
        plans.push(match entry {
            value if value.is_string() => match value.display_string(ctx.heap()).as_str() {
                "inherit" => StdioPlan::Inherit,
                "ignore" | "ipc" => StdioPlan::Ignore,
                _ => open_pipe(slot)?,
            },
            value => match value.as_f64() {
                Some(fd) if fd >= 0.0 => StdioPlan::Fd(duplicate(fd as std::os::fd::RawFd)?),
                _ => open_pipe(slot)?,
            },
        });
    }
    while plans.len() < 3 {
        plans.push(open_pipe(plans.len())?);
    }
    Ok(plans)
}

/// A fresh channel for one of a child's streams, with the ends handed to
/// whichever side reads and writes.
///
/// The end this process keeps does not survive into any other child: a
/// descriptor left open in an unrelated process is a stream whose end never
/// arrives.
fn open_pipe(slot: usize) -> Result<StdioPlan, std::io::Error> {
    let (parent, child) = if slot > 2 {
        // A stream of a child's own is not one of the three every process
        // has, and nothing says which way it runs: both ends read and write,
        // which is what a caller writing to the descriptor it handed over
        // expects of it.
        nix::sys::socket::socketpair(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::empty(),
        )?
    } else {
        let (read_end, write_end) = nix::unistd::pipe()?;
        // The child reads what it is given on the first stream and writes on
        // the rest.
        if slot == 0 {
            (write_end, read_end)
        } else {
            (read_end, write_end)
        }
    };
    // Neither end survives an exec under the number it has here. The child's
    // end is put in its place by the launch and is a stream from then on, and
    // an end left open under its old number is a stream whose end never
    // arrives: whoever inherits it holds the pipe open for everyone.
    for end in [&parent, &child] {
        nix::fcntl::fcntl(end, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC))?;
    }
    Ok(StdioPlan::Pipe { parent, child })
}

/// A copy of a descriptor the caller named, for the child to own.
///
/// The copy is a blocking one. Whether reads wait is a property of the open
/// file description rather than of the descriptor naming it, and a child that
/// meets "try again" on its own standard input has nowhere to wait — it is not
/// on this process's loop and cannot be told when to come back.
fn duplicate(raw: std::os::fd::RawFd) -> Result<std::os::fd::OwnedFd, std::io::Error> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    // SAFETY: the descriptor is the caller's and open for this call; it is
    // only duplicated, and the duplicate is what is owned from here on.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    let copy = borrowed.try_clone_to_owned()?;
    let flags = OFlag::from_bits_truncate(fcntl(&copy, FcntlArg::F_GETFL)?);
    fcntl(&copy, FcntlArg::F_SETFL(flags & !OFlag::O_NONBLOCK))?;
    Ok(copy)
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
fn reap(
    mut child: std::process::Child,
    id: u32,
    spawner: &RuntimeTaskSpawner,
    children: ChildTable,
) {
    let Some(io) = spawner.io_handle() else {
        // Without an IO runtime there is nowhere to wait from, and a child
        // nobody waits for stays on the process table as a zombie. Waiting for
        // it on a thread of its own is what keeps that from happening.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        retire(&children, id, Ending::Child);
        return;
    };
    // A running child is work the program is waiting on, so it holds the run
    // loop open until its outcome has been reported.
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let exit_spawner = spawner.clone();
    io.spawn(async move {
        let status = tokio::task::spawn_blocking(move || child.wait()).await;
        let exit = match status {
            Ok(Ok(status)) => ChildExit {
                id,
                status: status.code(),
                signal: exit_signal(&status),
                children,
            },
            _ => ChildExit {
                id,
                status: None,
                signal: None,
                children,
            },
        };
        // The exit report itself holds the loop: the child's own Ref hold is
        // released right after, and an Unref message could otherwise still be
        // in the inbox when the loop finds nothing left to wait for.
        exit_spawner
            .enqueue_ordered(exit, RuntimeLiveness::Ref)
            .await;
        drop(keep_alive);
    });
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

/// Hand a child what this process was granted.
///
/// A process trusted with everything trusts what it starts with the same — it
/// could do anything the child could anyway — and this is the only way to say
/// so through a shell, which is how a command line most often reaches a
/// program. A process that holds less than everything says nothing, and takes
/// care to say nothing: a name it inherited itself must not outlive the reach
/// it came with.
fn grant(cmd: &mut Command, caps: &CapabilitySet) {
    match caps.inherited_grant() {
        Some(grant) => cmd.env(otter_runtime::GRANT_VAR, grant),
        None => cmd.env_remove(otter_runtime::GRANT_VAR),
    };
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

    let argv = args
        .get(1)
        .copied()
        .map(|v| read_string_array(ctx, v))
        .unwrap_or_default();
    let opts = args.get(2).copied();
    // A directory named as nothing is not a directory to change to; the child
    // starts where this process is.
    let cwd = opt_string(ctx, opts, "cwd").filter(|dir| !dir.is_empty());
    let input = opt_string(ctx, opts, "input");
    let argv0 = opt_string(ctx, opts, "argv0");
    let credentials = opt_credentials(ctx, opts);
    let kill_signal = opt_string(ctx, opts, "killSignal").unwrap_or_else(|| "SIGTERM".to_string());
    let max_buffer = opt_number(ctx, opts, "maxBuffer").unwrap_or(f64::INFINITY);
    let timeout_ms = opt_number(ctx, opts, "timeout").unwrap_or(0.0);
    let env = opt_env(ctx, opts)?;

    let mut cmd = Command::new(&command);
    cmd.args(&argv);
    credentials.apply(&mut cmd);
    #[cfg(unix)]
    if let Some(argv0) = &argv0 {
        std::os::unix::process::CommandExt::arg0(&mut cmd, argv0);
    }
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env);
    }
    grant(&mut cmd, caps);
    // A stream the caller did not ask to see is the child's own: `inherit`
    // hands it this process's, `ignore` hands it nothing, and only a pipe is
    // collected and reported back. Text to feed the child is a pipe whatever
    // else was asked for — it has to arrive somewhere.
    let mut plans = match stdio_plans(ctx, opts) {
        Ok(plans) => plans,
        Err(error) => return spawn_error_result(ctx, &command, &error, &[]),
    };
    if input.is_some() && !matches!(plans.first(), Some(StdioPlan::Pipe { .. })) {
        match open_pipe(0) {
            Ok(pipe) => plans[0] = pipe,
            Err(error) => return spawn_error_result(ctx, &command, &error, &[]),
        }
    }
    let wired = match wire_stdio(&mut cmd, plans) {
        Ok(wired) => wired,
        Err(error) => return spawn_error_result(ctx, &command, &error, &[]),
    };

    let spawn_result = cmd.spawn();
    drop(wired.theirs);
    let mut child = match spawn_result {
        Ok(child) => child,
        Err(err) => return spawn_error_result(ctx, &command, &err, &[]),
    };
    let pid = child.id();

    let cap = if max_buffer.is_finite() && max_buffer >= 0.0 {
        max_buffer as usize
    } else {
        usize::MAX
    };
    let mut streams = SyncStreams::adopt(wired.kept, input.as_deref());

    let deadline = if timeout_ms > 0.0 {
        Some(std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64))
    } else {
        None
    };
    let mut failure: Option<&'static str> = None;
    // One thread moves every byte and waits for the child, so nothing outlives
    // this call: a reader on a thread of its own would still be holding a pipe
    // open after the child that shared it is gone.
    let status = loop {
        let moved = streams.pump(cap);
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            // A child this process can no longer ask about is stopped and
            // waited for, rather than left behind on the process table.
            Err(_) => {
                signal_child(pid, "SIGKILL");
                break child.wait().ok();
            }
        }
        // A child that has already written more than the caller will keep is
        // stopped rather than left running to fill a buffer nobody reads.
        if failure.is_none() && streams.overflowed {
            failure = Some("ENOBUFS");
            signal_child(pid, &kill_signal);
        }
        if failure.is_none() && deadline.is_some_and(|at| std::time::Instant::now() >= at) {
            failure = Some("ETIMEDOUT");
            signal_child(pid, &kill_signal);
        }
        if !moved {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    // What the child wrote before it went is still in the pipes; what a
    // grandchild that inherited them writes afterwards is no longer this
    // call's to wait for.
    let (stdout, stderr) = streams.finish(cap);
    let Some(status) = status else {
        let err = std::io::Error::other("child could not be waited for");
        return spawn_error_result(ctx, &command, &err, &[]);
    };

    let status_code = status.code();
    let signal = exit_signal(&status);

    ctx.scope(|mut scope| {
        let object = scope.object()?;
        set_number(&mut scope, object, "pid", f64::from(pid))?;
        // A child stopped for running past a limit did not choose its exit,
        // so it reports no status — only the signal that stopped it.
        if failure.is_some() {
            set_null(&mut scope, object, "status")?;
        } else {
            match status_code {
                Some(code) => set_number(&mut scope, object, "status", f64::from(code))?,
                None => set_null(&mut scope, object, "status")?,
            }
        }
        match signal {
            Some(signal) => {
                let signal = scope.string(&signal)?;
                scope.set(object, "signal", signal)?;
            }
            None => set_null(&mut scope, object, "signal")?,
        }
        // Output crosses as bytes rather than as text: a synchronous run can
        // hand back megabytes, and a character-per-byte detour through a
        // string would cost several times what the bytes themselves do.
        for (key, bytes) in [("stdout", stdout), ("stderr", stderr)] {
            match bytes {
                Some(bytes) => {
                    let length = bytes.len();
                    let buffer = scope.array_buffer_from_bytes(bytes)?;
                    let view = scope.typed_array_view(
                        buffer,
                        otter_vm::binary::TypedArrayKind::Uint8,
                        0,
                        length,
                    )?;
                    scope.set(object, key, view)?;
                }
                None => set_null(&mut scope, object, key)?,
            }
        }
        match failure {
            Some(code) => {
                let message = scope.string(code)?;
                scope.set(object, "error", message)?;
                let code = scope.string(code)?;
                scope.set(object, "errorCode", code)?;
            }
            None => set_null(&mut scope, object, "error")?,
        }
        Ok(scope.finish(object))
    })
}

/// The pipes a synchronous run holds while its child is alive.
///
/// Every byte moves on the thread that made the call: a reader thread would
/// still be blocked on a pipe a grandchild inherited long after the child that
/// shared it is gone, and this call has to be over when the child is.
struct SyncStreams {
    /// What is left to hand the child, and the pipe to hand it on. The pipe is
    /// let go of once the last byte is in it, which is the end the child reads.
    input: Option<(std::fs::File, Vec<u8>, usize)>,
    out_pipe: Option<std::fs::File>,
    err_pipe: Option<std::fs::File>,
    /// Collected output, `None` for a stream the caller did not ask for.
    out: Option<Vec<u8>>,
    err: Option<Vec<u8>>,
    /// Whether either stream passed what the caller agreed to hold.
    overflowed: bool,
}

impl SyncStreams {
    /// Take the ends of the child's pipes this process kept, and put them in
    /// the mode that lets one thread tend all of them without ever waiting on
    /// any single one.
    ///
    /// A slot the caller did not ask for a pipe on kept no end here, and is
    /// reported back as nothing rather than as empty output.
    fn adopt(mut kept: Vec<Option<std::os::fd::OwnedFd>>, input: Option<&str>) -> Self {
        kept.resize_with(3, || None);
        let mut ends = kept.into_iter().map(|end| {
            end.filter(set_nonblocking)
                .map(std::fs::File::from)
        });
        let stdin = ends.next().flatten();
        let out_pipe = ends.next().flatten();
        let err_pipe = ends.next().flatten();
        // A child whose input nobody writes reads end-of-file, so the writing
        // end is kept only while there is something to write.
        let input = match (stdin, input) {
            (Some(pipe), Some(text)) => Some((pipe, latin1_to_bytes(text), 0)),
            _ => None,
        };
        Self {
            out: out_pipe.as_ref().map(|_| Vec::new()),
            err: err_pipe.as_ref().map(|_| Vec::new()),
            input,
            out_pipe,
            err_pipe,
            overflowed: false,
        }
    }

    /// Move whatever the pipes will take or give right now. Answers whether
    /// anything moved, which is what tells the caller it is worth trying again
    /// before waiting.
    fn pump(&mut self, cap: usize) -> bool {
        let wrote = self.push_input();
        let read_out = Self::pull(&mut self.out_pipe, &mut self.out, cap, &mut self.overflowed);
        let read_err = Self::pull(&mut self.err_pipe, &mut self.err, cap, &mut self.overflowed);
        wrote || read_out || read_err
    }

    /// Hand the child as much of its input as the pipe will take.
    fn push_input(&mut self) -> bool {
        use std::io::Write;
        let Some((pipe, bytes, sent)) = self.input.as_mut() else {
            return false;
        };
        let moved = match pipe.write(&bytes[*sent..]) {
            Ok(0) => {
                self.input = None;
                return false;
            }
            Ok(written) => {
                *sent += written;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return false,
            // A child that is not reading its input is not going to: the pipe
            // is closed so it sees the end rather than waiting for more.
            Err(_) => {
                self.input = None;
                return false;
            }
        };
        if *sent == bytes.len() {
            self.input = None;
        }
        moved
    }

    /// Read one pipe as far as it will go right now.
    ///
    /// A read is kept whole: the limit is what the caller agreed to hold, and
    /// it is noticed after the read that crosses it rather than by cutting
    /// that read in half.
    fn pull<P: std::io::Read>(
        pipe: &mut Option<P>,
        kept: &mut Option<Vec<u8>>,
        cap: usize,
        overflowed: &mut bool,
    ) -> bool {
        let (Some(source), Some(kept)) = (pipe.as_mut(), kept.as_mut()) else {
            return false;
        };
        let mut chunk = [0u8; 65_536];
        let mut moved = false;
        loop {
            match source.read(&mut chunk) {
                Ok(0) => {
                    *pipe = None;
                    return moved;
                }
                Ok(read) => {
                    kept.extend_from_slice(&chunk[..read]);
                    moved = true;
                    if kept.len() > cap {
                        *overflowed = true;
                        *pipe = None;
                        return moved;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return moved,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    *pipe = None;
                    return moved;
                }
            }
        }
    }

    /// Take what the child left in its pipes, then let them go.
    fn finish(mut self, cap: usize) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        // The child has gone, so what is in the pipes now is everything it
        // wrote. A pipe that still has a writer is one a grandchild inherited,
        // and this call does not wait on a process it did not start.
        while Self::pull(&mut self.out_pipe, &mut self.out, cap, &mut self.overflowed) {}
        while Self::pull(&mut self.err_pipe, &mut self.err, cap, &mut self.overflowed) {}
        (self.out, self.err)
    }
}

#[cfg(unix)]
fn signal_child(pid: u32, signal: &str) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let number = signal_number(signal);
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::try_from(number).ok(),
    );
}

#[cfg(not(unix))]
fn signal_child(_pid: u32, _signal: &str) {}

fn opt_number(ctx: &mut NativeCtx<'_>, opts: Option<Value>, key: &str) -> Option<f64> {
    let object = opts?.as_object()?;
    object::get(object, ctx.heap(), key)?.as_f64()
}

fn spawn_error_result(
    ctx: &mut NativeCtx<'_>,
    command: &str,
    err: &std::io::Error,
    streams: &[std::os::fd::RawFd],
) -> Result<Value, NativeError> {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EPERM",
        _ => "EIO",
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
        let list = scope.array(streams.len())?;
        for (slot, fd) in streams.iter().enumerate() {
            let value = scope.number(f64::from(*fd));
            scope.set_index(list, slot, value)?;
        }
        scope.set(object, "stdio", list)?;
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

/// The number a signal name stands for, defaulting to `SIGTERM` for a name
/// this platform does not know.
#[cfg(unix)]
fn signal_number(name: &str) -> i32 {
    match name {
        "SIGHUP" => 1,
        "SIGINT" => 2,
        "SIGQUIT" => 3,
        "SIGILL" => 4,
        "SIGABRT" => 6,
        "SIGFPE" => 8,
        "SIGKILL" => 9,
        "SIGSEGV" => 11,
        "SIGPIPE" => 13,
        "SIGALRM" => 14,
        "SIGUSR1" => 30,
        "SIGUSR2" => 31,
        "SIGSTOP" => 17,
        _ => 15,
    }
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
