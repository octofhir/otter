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
    Ok(())
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
    format!(" Received {}", value.display_string(ctx.heap()))
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
