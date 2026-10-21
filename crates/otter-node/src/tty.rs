//! Native terminal surface under the vendored `tty` module.
//!
//! `node:tty` is vendored Node (`nodelib/tty.js`). Its `TTY` handle is the
//! compat `internal/otter/tty_wrap`, a stream handle over the host stream
//! table; this module supplies what only a terminal has, as the hosted
//! `internal/otter/tty`: whether a descriptor is a terminal, its window size,
//! its line discipline, its blocking flag, and a write that completes before
//! it returns.
//!
//! # Contents
//! - [`tty_binding_cjs_value`] — the hosted module.
//! - [`is_terminal`], [`window_size`], [`terminal_mode`],
//!   [`set_terminal_mode`], [`reset_terminal_mode`], [`set_blocking`],
//!   [`write_all`] — the descriptor operations behind it.
//!
//! # Invariants
//! - Every native is a static entry that captures nothing: the descriptor a
//!   call names is everything it operates on.
//! - A failure answers as a negative errno, the way the platform layer
//!   reports one, and is never thrown: the handle turns it into the error
//!   Node's own handle raises.
//! - A terminal's normal mode is kept by whoever left it: switching to raw
//!   mode hands back the mode it replaced, as bytes, and switching back
//!   applies them. Nothing about a terminal is remembered here; the handle
//!   module keeps the modes its isolate replaced and hands them back through
//!   `resetMode` when the run ends.
//! - Raw mode is the platform layer's: input is no longer translated, echoed
//!   or turned into signals, and output still maps a newline to CR-LF.
//!
//! # See also
//! - `nodelib/compat/internal_tty_wrap.js` — the handle these natives back.
//! - `nodelib/compat/internal_otter_stdio.js` — the standard streams, which
//!   read a terminal's size here.

use std::os::fd::RawFd;

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
};
use otter_vm::{NativeCtx, NativeError, NativeFastFn, NumberValue, Value};

/// The hosted `internal/otter/tty`: the terminal natives the compat
/// `tty_wrap` handle and the standard streams call.
///
/// # Errors
/// Returns a native error when the binding object fails to allocate.
pub fn tty_binding_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let binding = scope.object()?;
    let natives: [(&'static str, u8, NativeFastFn); 7] = [
        ("isatty", 1, tty_isatty),
        ("getWindowSize", 2, tty_get_window_size),
        ("getMode", 1, tty_get_mode),
        ("setMode", 3, tty_set_mode),
        ("resetMode", 2, tty_reset_mode),
        ("setBlocking", 2, tty_set_blocking),
        ("writeSync", 2, tty_write_sync),
    ];
    for (name, length, call) in natives {
        let method = scope.native_method(name, length, call)?;
        scope.set(binding, name, method)?;
    }
    Ok(binding)
}

/// `isatty(fd)` — whether `fd` is an open descriptor of a terminal.
fn tty_isatty(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::boolean(fd_arg(args, 0).is_some_and(is_terminal)))
}

/// `getWindowSize(fd, size)` — store the terminal's columns and rows in
/// `size[0]` and `size[1]`; answers 0, or a negative errno with `size`
/// untouched.
fn tty_get_window_size(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(fd) = fd_arg(args, 0) else {
        return Ok(errno_value(libc::EBADF));
    };
    let Some(size) = args
        .get(1)
        .copied()
        .filter(|value| value.as_array().is_some())
    else {
        return Ok(errno_value(libc::EINVAL));
    };
    let (columns, rows) = match window_size(fd) {
        Ok(dimensions) => dimensions,
        Err(errno) => return Ok(errno_value(errno)),
    };
    ctx.scope(|mut scope| {
        let size = scope.value(size);
        let columns = scope.number(f64::from(columns));
        scope.set_index(size, 0, columns)?;
        let rows = scope.number(f64::from(rows));
        scope.set_index(size, 1, rows)?;
        Ok(Value::number_i32(0))
    })
}

/// `getMode(fd)` — the terminal's current mode as an `ArrayBuffer` to hand
/// back to `setMode`, or a negative errno.
fn tty_get_mode(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(fd) = fd_arg(args, 0) else {
        return Ok(errno_value(libc::EBADF));
    };
    let mode = match terminal_mode(fd) {
        Ok(mode) => mode,
        Err(errno) => return Ok(errno_value(errno)),
    };
    ctx.scope(|mut scope| {
        let mode = scope.array_buffer_from_bytes(mode)?;
        Ok(scope.finish(mode))
    })
}

/// `setMode(fd, mode, raw)` — apply a mode `getMode` answered, made raw
/// first when `raw` is true, once pending output has drained; answers 0 or a
/// negative errno.
fn tty_set_mode(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(fd) = fd_arg(args, 0) else {
        return Ok(errno_value(libc::EBADF));
    };
    let raw = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    let Some(mode) = bytes_arg(ctx, args, 1) else {
        return Ok(errno_value(libc::EINVAL));
    };
    Ok(match set_terminal_mode(fd, &mode, raw) {
        Ok(()) => Value::number_i32(0),
        Err(errno) => errno_value(errno),
    })
}

/// `resetMode(fd, mode)` — hand a terminal back a mode `getMode` answered,
/// once its run is over; answers 0 or a negative errno. See
/// [`reset_terminal_mode`].
fn tty_reset_mode(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(fd) = fd_arg(args, 0) else {
        return Ok(errno_value(libc::EBADF));
    };
    let Some(mode) = bytes_arg(ctx, args, 1) else {
        return Ok(errno_value(libc::EINVAL));
    };
    Ok(match reset_terminal_mode(fd, &mode) {
        Ok(()) => Value::number_i32(0),
        Err(errno) => errno_value(errno),
    })
}

/// `setBlocking(fd, blocking)` — clear or set `O_NONBLOCK` on the open file
/// description behind `fd`; answers 0 or a negative errno.
fn tty_set_blocking(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(fd) = fd_arg(args, 0) else {
        return Ok(errno_value(libc::EBADF));
    };
    let blocking = args
        .get(1)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    Ok(match set_blocking(fd, blocking) {
        Ok(()) => Value::number_i32(0),
        Err(errno) => errno_value(errno),
    })
}

/// `writeSync(fd, bytes)` — write every byte of a buffer source before
/// returning; answers the count written or a negative errno.
fn tty_write_sync(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(fd) = fd_arg(args, 0) else {
        return Ok(errno_value(libc::EBADF));
    };
    let Some(bytes) = bytes_arg(ctx, args, 1) else {
        return Ok(errno_value(libc::EINVAL));
    };
    Ok(match write_all(fd, &bytes) {
        Ok(written) => Value::number(NumberValue::from_f64(written as f64)),
        Err(errno) => errno_value(errno),
    })
}

/// The descriptor a JavaScript number names: an integer the platform can
/// hold, or nothing.
fn fd_arg(args: &[Value], index: usize) -> Option<RawFd> {
    let number = args.get(index)?.as_f64()?;
    (number.fract() == 0.0 && (0.0..=f64::from(RawFd::MAX)).contains(&number))
        .then_some(number as RawFd)
}

/// The bytes of a buffer-source argument, copied out of the heap.
fn bytes_arg(ctx: &mut NativeCtx<'_>, args: &[Value], index: usize) -> Option<Vec<u8>> {
    let value = args.get(index).copied()?;
    ctx.scope(|mut scope| {
        let value = scope.value(value);
        scope.buffer_source_bytes(value)
    })
}

/// An errno as the negative number the platform layer reports it as.
fn errno_value(errno: i32) -> Value {
    Value::number_i32(-errno)
}

/// The errno of the call that just failed.
fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Whether `fd` is an open descriptor of a terminal.
pub(crate) fn is_terminal(fd: RawFd) -> bool {
    // SAFETY: `isatty` only asks about the descriptor; one that is not open
    // answers 0.
    unsafe { libc::isatty(fd) == 1 }
}

/// The terminal's window as `(columns, rows)`, or the errno that says it has
/// none.
pub(crate) fn window_size(fd: RawFd) -> Result<(u16, u16), i32> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    loop {
        // SAFETY: `size` is one writable `winsize` for the duration of the
        // call, which is what `TIOCGWINSZ` fills.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut size) } == 0 {
            return Ok((size.ws_col, size.ws_row));
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// The terminal's current mode as the bytes of its `termios`.
pub(crate) fn terminal_mode(fd: RawFd) -> Result<Vec<u8>, i32> {
    let mut mode = std::mem::MaybeUninit::<libc::termios>::zeroed();
    loop {
        // SAFETY: `mode` is storage for one `termios`, which `tcgetattr`
        // fills.
        if unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) } == 0 {
            break;
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
    // SAFETY: the storage was zeroed and then written by `tcgetattr`, so all
    // of its bytes are initialized, and the slice covers exactly that storage.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            mode.as_ptr().cast::<u8>(),
            std::mem::size_of::<libc::termios>(),
        )
    };
    Ok(bytes.to_vec())
}

/// Apply a mode [`terminal_mode`] answered, made raw first when `raw` is
/// set, once output already written has drained.
pub(crate) fn set_terminal_mode(fd: RawFd, saved: &[u8], raw: bool) -> Result<(), i32> {
    let mut mode = termios_from(saved)?;
    if raw {
        make_raw(&mut mode);
    }
    apply_mode(fd, libc::TCSADRAIN, &mode)
}

/// Hand a terminal back a mode [`terminal_mode`] answered, once the run that
/// changed it is over, the way the platform layer resets a terminal on its
/// way out: at once rather than after a drain that may never come. A
/// terminal that already has the mode is left alone, and so is one whose
/// foreground another process group holds, since its mode is that group's
/// now. A terminal that is not this process's controlling terminal has no
/// foreground to defer to. SIGTTOU, which stops a background process that
/// touches its terminal, is held off for the call.
pub(crate) fn reset_terminal_mode(fd: RawFd, saved: &[u8]) -> Result<(), i32> {
    let mode = termios_from(saved)?;
    let current = termios_from(&terminal_mode(fd)?)?;
    if same_mode(&current, &mode) {
        return Ok(());
    }
    // SAFETY: both calls only ask which process groups are involved.
    let (foreground, own) = unsafe { (libc::tcgetpgrp(fd), libc::getpgrp()) };
    if foreground != -1 && foreground != own {
        return Ok(());
    }
    with_sigttou_held(|| apply_mode(fd, libc::TCSANOW, &mode))
}

/// A `termios` from the bytes [`terminal_mode`] answered.
fn termios_from(bytes: &[u8]) -> Result<libc::termios, i32> {
    if bytes.len() != std::mem::size_of::<libc::termios>() {
        return Err(libc::EINVAL);
    }
    let mut mode = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `bytes` holds exactly one `termios` worth of bytes, `mode` has
    // room for them, and the two do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), mode.as_mut_ptr().cast::<u8>(), bytes.len());
    }
    // SAFETY: every byte was written above, and a `termios` is plain integers,
    // valid for any bit pattern.
    Ok(unsafe { mode.assume_init() })
}

/// Set a terminal's mode with `action` saying when it takes effect.
fn apply_mode(fd: RawFd, action: libc::c_int, mode: &libc::termios) -> Result<(), i32> {
    loop {
        // SAFETY: `mode` is a valid `termios` for the duration of the call.
        if unsafe { libc::tcsetattr(fd, action, mode) } == 0 {
            return Ok(());
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Whether two modes set the same flags and control characters. The line
/// discipline's own state bits — input waiting to be retyped, output being
/// discarded — are the kernel's to change and say nothing about the mode.
fn same_mode(left: &libc::termios, right: &libc::termios) -> bool {
    let state = libc::PENDIN | libc::FLUSHO;
    left.c_iflag == right.c_iflag
        && left.c_oflag == right.c_oflag
        && left.c_cflag == right.c_cflag
        && left.c_lflag & !state == right.c_lflag & !state
        && left.c_cc == right.c_cc
}

/// Run `f` with SIGTTOU blocked on this thread, then put the thread's signal
/// mask back as it was.
fn with_sigttou_held<R>(f: impl FnOnce() -> R) -> R {
    let mut held = std::mem::MaybeUninit::<libc::sigset_t>::zeroed();
    let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::zeroed();
    // SAFETY: `held` is initialized by `sigemptyset` before it is read and
    // `pthread_sigmask` fills `previous`; only this thread's mask changes.
    let blocked = unsafe {
        libc::sigemptyset(held.as_mut_ptr());
        libc::sigaddset(held.as_mut_ptr(), libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, held.as_ptr(), previous.as_mut_ptr()) == 0
    };
    let answer = f();
    if blocked {
        // SAFETY: `previous` holds the mask the call above replaced.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, previous.as_ptr(), std::ptr::null_mut());
        }
    }
    answer
}

/// Raw mode as the platform layer defines it: no break, CR or parity
/// translation and no flow control on input; 8-bit characters; no echo,
/// line editing, extended input processing or signal keys; a read returns
/// as soon as one byte is there. Output keeps mapping a newline to CR-LF.
fn make_raw(mode: &mut libc::termios) {
    mode.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
    mode.c_oflag |= libc::ONLCR;
    mode.c_cflag |= libc::CS8;
    mode.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
    mode.c_cc[libc::VMIN] = 1;
    mode.c_cc[libc::VTIME] = 0;
}

/// Clear or set `O_NONBLOCK` on the open file description behind `fd`.
pub(crate) fn set_blocking(fd: RawFd, blocking: bool) -> Result<(), i32> {
    let flags = loop {
        // SAFETY: `F_GETFL` only reads the description's status flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags != -1 {
            break flags;
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    };
    let wanted = if blocking {
        flags & !libc::O_NONBLOCK
    } else {
        flags | libc::O_NONBLOCK
    };
    if wanted == flags {
        return Ok(());
    }
    loop {
        // SAFETY: `F_SETFL` only sets the description's status flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, wanted) } != -1 {
            return Ok(());
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// Write every byte to `fd` before returning, waiting out a description that
/// is not blocking; answers the count written.
pub(crate) fn write_all(fd: RawFd, bytes: &[u8]) -> Result<usize, i32> {
    let mut written = 0;
    while written < bytes.len() {
        let rest = &bytes[written..];
        // SAFETY: `rest` is a live byte slice of the length passed.
        let answer = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if answer > 0 {
            written += answer as usize;
            continue;
        }
        if answer == 0 {
            return Err(libc::EIO);
        }
        match last_errno() {
            libc::EINTR => {}
            libc::EAGAIN => wait_writable(fd)?,
            errno => return Err(errno),
        }
    }
    Ok(written)
}

/// Block until `fd` will take more bytes.
fn wait_writable(fd: RawFd) -> Result<(), i32> {
    let mut ready = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        // SAFETY: `ready` is one valid `pollfd` for the duration of the call.
        if unsafe { libc::poll(&raw mut ready, 1, -1) } >= 0 {
            return Ok(());
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A pseudo-terminal pair: the controller side and the terminal a
    /// program would see.
    fn pseudo_terminal(columns: u16, rows: u16) -> (OwnedFd, OwnedFd) {
        // SAFETY: plain descriptor calls on the descriptor this test opens;
        // `ptsname` is read before anything else could ask for a name.
        unsafe {
            let controller = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(controller >= 0, "posix_openpt");
            let controller = OwnedFd::from_raw_fd(controller);
            assert_eq!(libc::grantpt(controller.as_raw_fd()), 0, "grantpt");
            assert_eq!(libc::unlockpt(controller.as_raw_fd()), 0, "unlockpt");
            let name = std::ffi::CStr::from_ptr(libc::ptsname(controller.as_raw_fd())).to_owned();
            let terminal = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            assert!(terminal >= 0, "open {name:?}");
            let terminal = OwnedFd::from_raw_fd(terminal);
            let size = libc::winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                libc::ioctl(terminal.as_raw_fd(), libc::TIOCSWINSZ, &raw const size),
                0,
                "TIOCSWINSZ"
            );
            (controller, terminal)
        }
    }

    fn termios_of(fd: RawFd) -> libc::termios {
        termios_from(&terminal_mode(fd).expect("terminal mode")).expect("one termios")
    }

    #[test]
    fn a_terminal_reports_its_size_and_returns_to_its_mode_after_raw() {
        let (_controller, terminal) = pseudo_terminal(100, 30);
        let fd = terminal.as_raw_fd();
        assert!(is_terminal(fd));
        assert_eq!(window_size(fd), Ok((100, 30)));

        let normal = terminal_mode(fd).expect("normal mode");
        set_terminal_mode(fd, &normal, true).expect("raw mode");
        let raw = termios_of(fd);
        assert_eq!(
            raw.c_lflag & (libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG),
            0
        );
        assert_eq!(raw.c_iflag & (libc::ICRNL | libc::IXON | libc::BRKINT), 0);
        assert_ne!(raw.c_oflag & libc::ONLCR, 0);
        assert_eq!(raw.c_cflag & libc::CS8, libc::CS8);
        assert_eq!((raw.c_cc[libc::VMIN], raw.c_cc[libc::VTIME]), (1, 0));

        let saved = termios_from(&normal).expect("saved mode");
        set_terminal_mode(fd, &normal, false).expect("normal mode again");
        assert!(same_mode(&termios_of(fd), &saved));
        assert_eq!(
            set_terminal_mode(fd, &normal[1..], false),
            Err(libc::EINVAL)
        );

        // The end of a run hands the mode back at once, and a terminal that
        // already has it is left as it is.
        set_terminal_mode(fd, &normal, true).expect("raw mode again");
        assert!(!same_mode(&termios_of(fd), &saved));
        reset_terminal_mode(fd, &normal).expect("reset");
        assert!(same_mode(&termios_of(fd), &saved));
        reset_terminal_mode(fd, &normal).expect("reset of an unchanged terminal");
    }

    #[test]
    fn a_pipe_is_not_a_terminal_and_takes_blocking_writes() {
        let mut ends = [0; 2];
        // SAFETY: `ends` has room for the two descriptors `pipe` returns.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors were just opened by this test.
        let (reader, writer) =
            unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
        let fd = writer.as_raw_fd();
        assert!(!is_terminal(fd));
        assert!(!is_terminal(-1));
        assert_eq!(window_size(fd), Err(libc::ENOTTY));
        assert_eq!(terminal_mode(fd), Err(libc::ENOTTY));

        set_blocking(fd, false).expect("non-blocking");
        // SAFETY: `F_GETFL` only reads the status flags.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        assert_eq!(write_all(fd, b"through"), Ok(7));
        set_blocking(fd, true).expect("blocking");
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );

        let mut read = [0u8; 16];
        // SAFETY: `read` is a writable buffer of the length passed.
        let count = unsafe { libc::read(reader.as_raw_fd(), read.as_mut_ptr().cast(), read.len()) };
        assert_eq!(&read[..usize::try_from(count).expect("read")], b"through");
    }
}
