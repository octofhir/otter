//! What the CLI itself writes to standard output and standard error.
//!
//! A program's own output reaches the descriptors through the runtime. The
//! binary prints around it — a `-p` result, a `--json` payload, a package
//! command's report, a rendered error, a diagnostic line — and every one of
//! those writes goes through this module. A write answers with what the
//! descriptor did with it, so a reader that went away (`otter ... | head -1`)
//! is an ordinary [`io::Error`] for the caller to answer, never the panic the
//! `print!` family turns a failed write into.
//!
//! The macros answer it the way Node's console answers a line it could not
//! write: the text is dropped, and the command ends with the exit code its own
//! work decided. Node prints a `-p` result through that same console, and
//! reports an uncaught error onto standard error without regard to whether
//! anything still reads it.
//!
//! # Contents
//! - [`write_stdout`] / [`write_stderr`] — write formatted text to one stream,
//!   flushed, and return the outcome.
//! - `stdout!` / `stdoutln!` / `stderr!` / `stderrln!` — the `print!`-shaped
//!   forms, which drop text the descriptor refused.
//!
//! # Invariants
//! - The binary never writes through `print!`, `println!`, `eprint!` or
//!   `eprintln!`, which panic when a write fails: the crate denies
//!   `clippy::print_stdout` and `clippy::print_stderr`.
//! - A write holds the stream's lock for its whole text and flushes before it
//!   returns, so the text never interleaves with another writer's and nothing
//!   printed is left in a buffer when the process exits.
//!
//! # See also
//! - [`crate::error_render`] — errors, rendered onto standard error here.

use std::fmt;
use std::io::{self, Write};

/// Write `text` to standard output and flush it.
///
/// # Errors
/// Returns the error the descriptor answered with, such as
/// [`io::ErrorKind::BrokenPipe`] once its reader is gone.
pub(crate) fn write_stdout(text: fmt::Arguments<'_>) -> io::Result<()> {
    write_flushed(&mut io::stdout().lock(), text)
}

/// Write `text` to standard error and flush it.
///
/// # Errors
/// As [`write_stdout`].
pub(crate) fn write_stderr(text: fmt::Arguments<'_>) -> io::Result<()> {
    write_flushed(&mut io::stderr().lock(), text)
}

fn write_flushed(stream: &mut impl Write, text: fmt::Arguments<'_>) -> io::Result<()> {
    stream.write_fmt(text)?;
    stream.flush()
}

/// `print!` for the CLI's own output: text standard output refuses is dropped.
macro_rules! stdout {
    ($($arg:tt)*) => {{
        let _ = $crate::output::write_stdout(::std::format_args!($($arg)*));
    }};
}

/// `println!` for the CLI's own output: a line standard output refuses is
/// dropped.
macro_rules! stdoutln {
    () => {
        $crate::output::stdout!("\n")
    };
    ($($arg:tt)*) => {
        $crate::output::stdout!("{}\n", ::std::format_args!($($arg)*))
    };
}

/// `eprint!` for the CLI's own output: text standard error refuses is dropped.
macro_rules! stderr {
    ($($arg:tt)*) => {{
        let _ = $crate::output::write_stderr(::std::format_args!($($arg)*));
    }};
}

/// `eprintln!` for the CLI's own output: a line standard error refuses is
/// dropped.
macro_rules! stderrln {
    () => {
        $crate::output::stderr!("\n")
    };
    ($($arg:tt)*) => {
        $crate::output::stderr!("{}\n", ::std::format_args!($($arg)*))
    };
}

pub(crate) use {stderr, stderrln, stdout, stdoutln};
