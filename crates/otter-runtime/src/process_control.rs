//! Process control members of the `process` global: `chdir`, `kill`, `_kill`,
//! and `abort`.
//!
//! # Contents
//! - [`install`] defines the members on an already-built `process` object.
//! - [`WorkingDirectory`] is the cell `process.cwd()` reads and
//!   `process.chdir()` writes, so the two never disagree.
//!
//! # Invariants
//! - `chdir` moves the host process only after the capability check passes,
//!   and the recorded working directory is updated only after the move
//!   succeeds.
//! - `kill` resolves the signal and then dispatches through the object's own
//!   `_kill`, which is what lets a test replace the syscall.
//! - `_kill` reports a failed syscall as a negative errno rather than throwing,
//!   matching the binding `kill` is written against.
//!
//! # See also
//! - [`crate::process`]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use otter_vm::{
    ErrorKind, Local, NativeCall, NativeCtx, NativeError, NativeFn, NativeScope, Value,
};

use crate::CapabilitySet;

/// The process working directory shared by `process.cwd()` and
/// `process.chdir()`.
#[derive(Debug, Clone)]
pub(crate) struct WorkingDirectory(Arc<Mutex<PathBuf>>);

impl WorkingDirectory {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(path)))
    }

    pub(crate) fn get(&self) -> PathBuf {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, path: PathBuf) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
    }
}

/// Where `setUncaughtExceptionCaptureCallback` parks its callback. The slot is
/// an own, non-enumerable property of `process`, so the callback is rooted by
/// the object that owns it and never outlives the isolate.
pub(crate) const CAPTURE_SLOT: &str = "__otter_uncaught_capture__";

pub(crate) fn install(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
    capabilities: &CapabilitySet,
    cwd: &WorkingDirectory,
) -> Result<(), NativeError> {
    crate::process::define_process_method(
        scope,
        process,
        "chdir",
        1,
        chdir_call(capabilities, cwd),
    )?;
    crate::process::define_process_method(scope, process, "kill", 2, NativeCall::Static(kill))?;
    crate::process::define_process_method(
        scope,
        process,
        "_kill",
        2,
        NativeCall::Static(raw_kill),
    )?;
    crate::process::define_process_method(scope, process, "abort", 0, NativeCall::Static(abort))?;
    crate::process::define_process_method(
        scope,
        process,
        "setUncaughtExceptionCaptureCallback",
        1,
        NativeCall::Static(set_uncaught_exception_capture_callback),
    )?;
    crate::process::define_process_method(
        scope,
        process,
        "hasUncaughtExceptionCaptureCallback",
        0,
        NativeCall::Static(has_uncaught_exception_capture_callback),
    )?;
    crate::process::define_process_method(
        scope,
        process,
        "getActiveResourcesInfo",
        0,
        NativeCall::Static(get_active_resources_info),
    )?;
    install_credentials(scope, process)?;
    let empty = scope.undefined();
    scope.define(
        process,
        CAPTURE_SLOT,
        empty,
        otter_vm::Attr {
            writable: true,
            enumerable: false,
            configurable: false,
        }
        .to_flags(),
    )?;
    Ok(())
}

/// `process.setUncaughtExceptionCaptureCallback(fn)` — install the function an
/// uncaught exception is routed to, or clear it with `null`. A second install
/// while one is active is an error: Node refuses to let two owners silently
/// share the process's last line of defence.
fn set_uncaught_exception_capture_callback(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let candidate = args.first().copied().unwrap_or_else(Value::undefined);
    // Rendered from the raw argument before the scope allocates: a handle
    // allocation can move the value this tail describes.
    let received = received_suffix(ctx, candidate);
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let candidate = scope.value(candidate);

        if scope.is_null(candidate) {
            let cleared = scope.undefined();
            scope.set(process, CAPTURE_SLOT, cleared)?;
            return Ok(Value::undefined());
        }

        if !scope.is_callable(candidate) {
            return Err(NativeError::Coded {
                kind: ErrorKind::TypeError,
                code: "ERR_INVALID_ARG_TYPE",
                message: format!("The \"fn\" argument must be of type function or null.{received}"),
            });
        }

        let current = scope.get(process, CAPTURE_SLOT)?;
        if scope.is_callable(current) {
            return Err(NativeError::Coded {
                kind: ErrorKind::Error,
                code: "ERR_UNCAUGHT_EXCEPTION_CAPTURE_ALREADY_SET",
                message: "`process.setupUncaughtExceptionCapture()` was called while a \
                          capture callback was already active"
                    .to_string(),
            });
        }

        scope.set(process, CAPTURE_SLOT, candidate)?;
        Ok(Value::undefined())
    })
}

/// `process.hasUncaughtExceptionCaptureCallback()`.
fn has_uncaught_exception_capture_callback(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    ctx.scope(|mut scope| {
        let process = scope.value(this_value);
        let current = scope.get(process, CAPTURE_SLOT)?;
        Ok(Value::boolean(scope.is_callable(current)))
    })
}

/// `process.chdir(directory)` — move the host process and record where it
/// landed. A relative argument resolves against the current directory, which is
/// what makes repeated relative `chdir` calls compose.
fn chdir_call(capabilities: &CapabilitySet, cwd: &WorkingDirectory) -> NativeCall {
    let capabilities = capabilities.clone();
    let cwd = cwd.clone();
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let directory = match args.first().copied() {
            Some(value) if value.as_string(ctx.heap()).is_some() => value
                .as_string(ctx.heap())
                .expect("string checked above")
                .to_lossy_string(ctx.heap()),
            other => {
                return Err(invalid_arg_type(
                    ctx,
                    "directory",
                    "string",
                    other.unwrap_or_else(Value::undefined),
                ));
            }
        };

        let current = cwd.get();
        let target = if PathBuf::from(&directory).is_absolute() {
            PathBuf::from(&directory)
        } else {
            current.join(&directory)
        };

        if !capabilities.read.matches_path(&target) {
            return Err(chdir_failure(
                "EACCES",
                "permission denied",
                access_denied_errno(),
                &current,
                &directory,
            ));
        }

        std::env::set_current_dir(&target).map_err(|error| {
            chdir_failure(
                errno_code(&error),
                errno_description(&error),
                error.raw_os_error().unwrap_or(0),
                &current,
                &directory,
            )
        })?;
        cwd.set(std::env::current_dir().unwrap_or(target));
        Ok(Value::undefined())
    });
    NativeCall::Dynamic(call)
}

/// Shape a failed `chdir` the way Node does: the message names both operands
/// and the error carries `syscall`, `path`, and `dest` for a caller to read.
fn chdir_failure(
    code: &'static str,
    description: &str,
    errno: i32,
    current: &std::path::Path,
    directory: &str,
) -> NativeError {
    NativeError::Syscall {
        code,
        message: format!(
            "{code}: {description}, chdir '{}' -> '{directory}'",
            current.display()
        ),
        syscall: "chdir",
        path: Some(current.display().to_string()),
        dest: Some(directory.to_string()),
        errno: -errno,
    }
}

#[cfg(unix)]
fn access_denied_errno() -> i32 {
    nix::errno::Errno::EACCES as i32
}

#[cfg(not(unix))]
fn access_denied_errno() -> i32 {
    13
}

/// `process.kill(pid[, signal])` — validate, resolve the signal name, and hand
/// the pair to `process._kill`. Reading `_kill` off the receiver on every call
/// is deliberate: it is the documented seam a caller replaces to observe the
/// arguments without signalling anything.
fn kill(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pid_value = args.first().copied().unwrap_or_else(Value::undefined);
    let Some(pid) = kill_pid(ctx, pid_value) else {
        return Err(invalid_arg_type(ctx, "pid", "number", pid_value));
    };

    let signal_value = args.get(1).copied().unwrap_or_else(Value::undefined);
    let signal = resolve_signal(ctx, signal_value)?;

    let this_value = *ctx.this_value();
    // The dispatch runs inside a handle scope: reading `_kill` can run a
    // getter and the call itself allocates, either of which moves the
    // receiver.
    let errno = ctx.scope(|mut scope| {
        let receiver = scope.value(this_value);
        let raw_kill = scope.get(receiver, "_kill")?;
        let pid = scope.number(pid);
        let signal = scope.number(f64::from(signal));
        let result = scope.call(raw_kill, receiver, &[pid, signal])?;
        // The binding reports a failed syscall as a negative errno; a
        // successful call answers something falsy.
        Ok::<i32, NativeError>(scope.number_value(result).unwrap_or(0.0) as i32)
    })?;

    if errno != 0 {
        let code = errno_code_from_raw(errno.abs());
        return Err(NativeError::Coded {
            kind: ErrorKind::Error,
            code,
            message: format!("kill {code}"),
        });
    }
    Ok(Value::boolean(true))
}

/// `process._kill(pid, signal)` — the syscall itself. Returns `0` on success
/// and a negative errno on failure so `kill` can shape the error.
fn raw_kill(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pid = args
        .first()
        .copied()
        .and_then(number_arg)
        .ok_or(NativeError::TypeError {
            name: "process._kill",
            reason: "pid must be a number".to_string(),
        })? as i32;
    let signal = args
        .get(1)
        .copied()
        .and_then(number_arg)
        .ok_or(NativeError::TypeError {
            name: "process._kill",
            reason: "signal must be a number".to_string(),
        })? as i32;
    let _ = ctx;

    #[cfg(unix)]
    {
        // A signal number the platform does not define never reaches the
        // kernel, and `EINVAL` is exactly what it would answer.
        let Ok(signal) = nix::sys::signal::Signal::try_from(signal) else {
            return Ok(Value::number_i32(-(nix::errno::Errno::EINVAL as i32)));
        };
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
            Ok(()) => Ok(Value::number_i32(0)),
            Err(errno) => Ok(Value::number_i32(-(errno as i32))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Ok(Value::number_i32(0))
    }
}

/// `process.abort()` — terminate immediately with `SIGABRT`, the way Node's
/// own `abort` does. There is no return path.
fn abort(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    std::process::abort();
}

/// Node accepts any pid that survives `pid != (pid | 0)`: a whole number, or a
/// string that reads as one. `null` and `undefined` fail that comparison even
/// though they coerce to `0`, so they are rejected here too.
fn kill_pid(ctx: &mut NativeCtx<'_>, value: Value) -> Option<f64> {
    let number = if let Some(number) = number_arg(value) {
        number
    } else if let Some(string) = value.as_string(ctx.heap()) {
        let text = string.to_lossy_string(ctx.heap());
        let trimmed = text.trim();
        if trimmed.is_empty() {
            0.0
        } else {
            trimmed.parse::<f64>().ok()?
        }
    } else if let Some(boolean) = value.as_boolean() {
        f64::from(u8::from(boolean))
    } else {
        return None;
    };
    (number.is_finite() && number == f64::from(number as i32)).then_some(number)
}

/// Resolve the signal argument the way Node does: a missing signal is
/// `SIGTERM`, a name is looked up, and a number passes through for the syscall
/// itself to accept or reject.
fn resolve_signal(ctx: &mut NativeCtx<'_>, value: Value) -> Result<i32, NativeError> {
    if value.is_undefined() {
        return Ok(signal_number("SIGTERM").expect("SIGTERM is always known"));
    }
    if let Some(number) = number_arg(value) {
        return Ok(number as i32);
    }
    if let Some(string) = value.as_string(ctx.heap()) {
        let name = string.to_lossy_string(ctx.heap());
        return signal_number(&name).ok_or(NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_UNKNOWN_SIGNAL",
            message: format!("Unknown signal: {name}"),
        });
    }
    let rendered = value.display_string(ctx.heap());
    Err(NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_UNKNOWN_SIGNAL",
        message: format!("Unknown signal: {rendered}"),
    })
}

/// The signal names Node exposes, resolved through the platform's own numbers
/// rather than a hard-coded table: `SIGUSR1` and `SIGBUS` differ between Linux
/// and the BSDs.
fn signal_number(name: &str) -> Option<i32> {
    #[cfg(unix)]
    {
        let number = match name {
            "SIGHUP" => libc::SIGHUP,
            "SIGINT" => libc::SIGINT,
            "SIGQUIT" => libc::SIGQUIT,
            "SIGILL" => libc::SIGILL,
            "SIGTRAP" => libc::SIGTRAP,
            "SIGABRT" | "SIGIOT" => libc::SIGABRT,
            "SIGBUS" => libc::SIGBUS,
            "SIGFPE" => libc::SIGFPE,
            "SIGKILL" => libc::SIGKILL,
            "SIGUSR1" => libc::SIGUSR1,
            "SIGSEGV" => libc::SIGSEGV,
            "SIGUSR2" => libc::SIGUSR2,
            "SIGPIPE" => libc::SIGPIPE,
            "SIGALRM" => libc::SIGALRM,
            "SIGTERM" => libc::SIGTERM,
            "SIGCHLD" => libc::SIGCHLD,
            "SIGCONT" => libc::SIGCONT,
            "SIGSTOP" => libc::SIGSTOP,
            "SIGTSTP" => libc::SIGTSTP,
            "SIGTTIN" => libc::SIGTTIN,
            "SIGTTOU" => libc::SIGTTOU,
            "SIGURG" => libc::SIGURG,
            "SIGXCPU" => libc::SIGXCPU,
            "SIGXFSZ" => libc::SIGXFSZ,
            "SIGVTALRM" => libc::SIGVTALRM,
            "SIGPROF" => libc::SIGPROF,
            "SIGWINCH" => libc::SIGWINCH,
            "SIGIO" | "SIGPOLL" => libc::SIGIO,
            "SIGSYS" => libc::SIGSYS,
            _ => return None,
        };
        Some(number)
    }
    #[cfg(not(unix))]
    {
        match name {
            "SIGHUP" => Some(1),
            "SIGINT" => Some(2),
            "SIGKILL" => Some(9),
            "SIGTERM" => Some(15),
            _ => None,
        }
    }
}

/// Build the `ERR_INVALID_ARG_TYPE` a Node argument check throws, including the
/// `Received …` tail its tests match on.
fn invalid_arg_type(
    ctx: &mut NativeCtx<'_>,
    argument: &str,
    expected: &str,
    value: Value,
) -> NativeError {
    NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_TYPE",
        message: format!(
            "The \"{argument}\" argument must be of type {expected}.{}",
            received_suffix(ctx, value)
        ),
    }
}

fn received_suffix(ctx: &mut NativeCtx<'_>, value: Value) -> String {
    if value.is_undefined() {
        return " Received undefined".to_string();
    }
    if value.is_null() {
        return " Received null".to_string();
    }
    if let Some(number) = number_arg(value) {
        let rendered = if number == 0.0 && number.is_sign_negative() {
            "-0".to_string()
        } else {
            Value::number(otter_vm::NumberValue::from_f64(number)).display_string(ctx.heap())
        };
        return format!(" Received type number ({rendered})");
    }
    if let Some(string) = value.as_string(ctx.heap()) {
        let text = string.to_lossy_string(ctx.heap());
        let mut inspected = format!("'{text}'");
        if inspected.chars().count() > 25 {
            inspected = format!("{}...", inspected.chars().take(25).collect::<String>());
        }
        return format!(" Received type string ({inspected})");
    }
    if value.as_boolean().is_some() {
        let rendered = value.display_string(ctx.heap());
        return format!(" Received type boolean ({rendered})");
    }
    if value.as_object().is_some() {
        return format!(" Received an instance of {}", constructor_name(ctx, value));
    }
    format!(" Received {}", value.display_string(ctx.heap()))
}

/// The constructor name Node prints for a rejected object argument.
fn constructor_name(ctx: &mut NativeCtx<'_>, value: Value) -> String {
    ctx.get_value_property(value, "constructor")
        .ok()
        .and_then(|constructor| {
            ctx.get_value_property(constructor, "name")
                .ok()
                .and_then(|name| name.as_string(ctx.heap()))
                .map(|name| name.to_lossy_string(ctx.heap()))
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Object".to_string())
}

/// Read a numeric argument as `f64`; every other value type answers `None`.
fn number_arg(value: Value) -> Option<f64> {
    value.as_number().map(otter_vm::NumberValue::as_f64)
}

fn errno_code(error: &std::io::Error) -> &'static str {
    errno_code_from_raw(error.raw_os_error().unwrap_or(0))
}

#[cfg(unix)]
fn errno_code_from_raw(errno: i32) -> &'static str {
    match errno {
        libc::ENOENT => "ENOENT",
        libc::ENOTDIR => "ENOTDIR",
        libc::EACCES => "EACCES",
        libc::EPERM => "EPERM",
        libc::ESRCH => "ESRCH",
        libc::EINVAL => "EINVAL",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ELOOP => "ELOOP",
        _ => "UNKNOWN",
    }
}

#[cfg(not(unix))]
fn errno_code_from_raw(errno: i32) -> &'static str {
    match errno {
        2 => "ENOENT",
        13 => "EACCES",
        22 => "EINVAL",
        _ => "UNKNOWN",
    }
}

fn errno_description(error: &std::io::Error) -> &'static str {
    match errno_code(error) {
        "ENOENT" => "no such file or directory",
        "ENOTDIR" => "not a directory",
        "EACCES" => "permission denied",
        "ENAMETOOLONG" => "name too long",
        "ELOOP" => "too many symbolic links encountered",
        _ => "operation failed",
    }
}

/// `process.getActiveResourcesInfo()` — the kinds of the resources still
/// keeping the event loop alive. Node names a pending `setTimeout` or
/// `setInterval` `"Timeout"` and a pending `setImmediate` `"Immediate"`.
fn get_active_resources_info(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let kinds = ctx.interp_mut().timer_callbacks().active_kinds();
    ctx.scope(|mut scope| {
        let array = scope.array(kinds.len())?;
        for (index, kind) in kinds.iter().enumerate() {
            let name = match kind {
                otter_vm::TimerKind::Timeout => "Timeout",
                otter_vm::TimerKind::Immediate => "Immediate",
            };
            let name = scope.string(name)?;
            scope.set_index(array, index, name)?;
        }
        Ok(scope.finish(array))
    })
}

/// POSIX credential members. Node leaves these undefined on Windows, so they
/// are installed only where the platform has them.
#[cfg(unix)]
fn install_credentials(
    scope: &mut NativeScope<'_, '_>,
    process: Local<'_>,
) -> Result<(), NativeError> {
    for (name, call) in [
        ("getuid", NativeCall::Static(get_uid)),
        ("geteuid", NativeCall::Static(get_euid)),
        ("getgid", NativeCall::Static(get_gid)),
        ("getegid", NativeCall::Static(get_egid)),
    ] {
        crate::process::define_process_method(scope, process, name, 0, call)?;
    }
    for (name, call) in [
        ("setuid", NativeCall::Static(set_uid)),
        ("seteuid", NativeCall::Static(set_euid)),
        ("setgid", NativeCall::Static(set_gid)),
        ("setegid", NativeCall::Static(set_egid)),
    ] {
        crate::process::define_process_method(scope, process, name, 1, call)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_credentials(
    _scope: &mut NativeScope<'_, '_>,
    _process: Local<'_>,
) -> Result<(), NativeError> {
    Ok(())
}

#[cfg(unix)]
fn get_uid(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(nix::unistd::getuid().as_raw() as i32))
}

#[cfg(unix)]
fn get_euid(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(nix::unistd::geteuid().as_raw() as i32))
}

#[cfg(unix)]
fn get_gid(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(nix::unistd::getgid().as_raw() as i32))
}

#[cfg(unix)]
fn get_egid(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number_i32(nix::unistd::getegid().as_raw() as i32))
}

#[cfg(unix)]
fn set_uid(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let id = credential_id(ctx, args, Credential::User)?;
    nix::unistd::setuid(nix::unistd::Uid::from_raw(id))
        .map_err(|errno| credential_failure("setuid", errno))?;
    Ok(Value::undefined())
}

#[cfg(unix)]
fn set_euid(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let id = credential_id(ctx, args, Credential::User)?;
    nix::unistd::seteuid(nix::unistd::Uid::from_raw(id))
        .map_err(|errno| credential_failure("seteuid", errno))?;
    Ok(Value::undefined())
}

#[cfg(unix)]
fn set_gid(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let id = credential_id(ctx, args, Credential::Group)?;
    nix::unistd::setgid(nix::unistd::Gid::from_raw(id))
        .map_err(|errno| credential_failure("setgid", errno))?;
    Ok(Value::undefined())
}

#[cfg(unix)]
fn set_egid(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let id = credential_id(ctx, args, Credential::Group)?;
    nix::unistd::setegid(nix::unistd::Gid::from_raw(id))
        .map_err(|errno| credential_failure("setegid", errno))?;
    Ok(Value::undefined())
}

/// Which name table a credential argument is looked up in.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Credential {
    User,
    Group,
}

#[cfg(unix)]
impl Credential {
    fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Group => "Group",
        }
    }
}

/// Read a credential argument: a numeric id passes through, a name is resolved
/// against the platform's own user or group table.
#[cfg(unix)]
fn credential_id(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    credential: Credential,
) -> Result<u32, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    if let Some(number) = number_arg(value) {
        // Node truncates to an unsigned 32-bit id, which is what the platform
        // takes; a value outside that range fails in the syscall, not here.
        return Ok(number as i64 as u32);
    }
    if let Some(string) = value.as_string(ctx.heap()) {
        let name = string.to_lossy_string(ctx.heap());
        let resolved = match credential {
            Credential::User => nix::unistd::User::from_name(&name)
                .ok()
                .flatten()
                .map(|user| user.uid.as_raw()),
            Credential::Group => nix::unistd::Group::from_name(&name)
                .ok()
                .flatten()
                .map(|group| group.gid.as_raw()),
        };
        return resolved.ok_or(NativeError::Coded {
            kind: ErrorKind::TypeError,
            code: "ERR_UNKNOWN_CREDENTIAL",
            message: format!("{} identifier does not exist: {name}", credential.label()),
        });
    }
    Err(NativeError::Coded {
        kind: ErrorKind::TypeError,
        code: "ERR_INVALID_ARG_TYPE",
        message: format!(
            "The \"id\" argument must be one of type number or string.{}",
            received_suffix(ctx, value)
        ),
    })
}

/// A refused credential change reports the errno the way Node does: the code,
/// the call's name, and the platform's own description.
#[cfg(unix)]
fn credential_failure(syscall: &'static str, errno: nix::errno::Errno) -> NativeError {
    NativeError::Syscall {
        code: errno_code_from_raw(errno as i32),
        message: format!("{}, {}", errno_code_from_raw(errno as i32), errno.desc()),
        syscall,
        path: None,
        dest: None,
        errno: errno as i32,
    }
}
