//! The terminal modes a run changes, and their return when the run ends.
//!
//! A program may switch a terminal to raw mode (`tty.ReadStream`'s
//! `setRawMode`), and one that ends while raw must not leave its shell raw.
//! [`TerminalModes`] is the run's own record of that: before the run first
//! changes a terminal's mode, the module making the change keeps the mode the
//! terminal has here, with the operation that puts it back, and the run hands
//! every kept mode back from Rust — once the `'exit'` listeners have run, and
//! before `abort()` terminates the process.
//!
//! # Contents
//! - [`TerminalModes`] — the per-run host service.
//! - [`HandBack`] — the platform operation a kept mode is put back with.
//! - [`restore_run_terminals`] — the hand-back the exit and abort paths run.
//!
//! # Invariants
//! - The record belongs to one run: each run inserts its own
//!   [`TerminalModes`] as a host service. Nothing about a terminal lives in
//!   process-global state or on a JavaScript object, so no program can read,
//!   replace or delete it.
//! - A terminal's first change keeps the mode it had; later changes leave
//!   the kept mode alone and read nothing.
//! - Handing back runs no JavaScript. For a run that changed no terminal it
//!   allocates nothing, takes one uncontended lock and makes no system call.
//! - Modes go back in the reverse order they were kept, so a terminal changed
//!   through two descriptors ends with the mode it had before the first.
//! - The runtime reads and writes no terminal itself: a mode is the bytes the
//!   platform layer read, and the platform layer's own operation puts them
//!   back. Each record is spent by the hand-back that uses it.
//!
//! # See also
//! - `otter-node`'s `tty` natives, which keep modes here before they change
//!   them, and own the line discipline.
//! - [`crate::process`] — the exit path that hands the modes back.
//! - [`crate::process_control`] — `abort()`, which hands them back first.

use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use otter_vm::NativeCtx;

/// Puts a kept mode back on terminal `fd`: the platform layer's operation,
/// given the bytes it read. The run is ending, so what it answers is only
/// for whoever else calls it.
pub type HandBack = fn(RawFd, &[u8]) -> Result<(), i32>;

/// Every terminal one run changed the mode of, with the mode it had before.
#[derive(Clone, Default)]
pub struct TerminalModes {
    kept: Arc<Mutex<Vec<KeptMode>>>,
}

/// The mode a terminal had before the run first changed it.
struct KeptMode {
    fd: RawFd,
    mode: Vec<u8>,
    hand_back: HandBack,
}

impl TerminalModes {
    /// Keep the mode terminal `fd` has before the run first changes it, as
    /// `read` reads it, to be put back with `hand_back` when the run ends. A
    /// terminal the run already kept stays as it was kept, and `read` is not
    /// called.
    ///
    /// # Errors
    /// The errno `read` answers; nothing is kept then.
    pub fn keep(
        &self,
        fd: RawFd,
        read: impl FnOnce(RawFd) -> Result<Vec<u8>, i32>,
        hand_back: HandBack,
    ) -> Result<(), i32> {
        let mut kept = self.kept();
        if kept.iter().any(|terminal| terminal.fd == fd) {
            return Ok(());
        }
        let mode = read(fd)?;
        kept.push(KeptMode {
            fd,
            mode,
            hand_back,
        });
        Ok(())
    }

    /// Hand every terminal the run changed the mode it had before, and keep
    /// none of them: the run is over.
    pub(crate) fn restore(&self) {
        let kept = std::mem::take(&mut *self.kept());
        for terminal in kept.iter().rev() {
            // A terminal that will not take its mode back has no one left
            // to tell.
            let _ = (terminal.hand_back)(terminal.fd, &terminal.mode);
        }
    }

    fn kept(&self) -> MutexGuard<'_, Vec<KeptMode>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Hand back every terminal mode the run changed: the exit path runs this
/// once the `'exit'` listeners have run, and `abort()` before it terminates.
/// A run without a record has no `process`, and changed no terminal.
pub(crate) fn restore_run_terminals(ctx: &NativeCtx<'_>) {
    if let Some(modes) = ctx.host_service::<TerminalModes>() {
        modes.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    /// Stands in for the platform layer: "puts a mode back" by writing its
    /// bytes to the descriptor, so the test reads what came back, in order.
    fn write_back(fd: RawFd, mode: &[u8]) -> Result<(), i32> {
        let failed = |error: std::io::Error| error.raw_os_error().unwrap_or(-1);
        std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/dev/fd/{fd}"))
            .map_err(failed)?
            .write_all(mode)
            .map_err(failed)
    }

    fn mode(bytes: &'static [u8]) -> impl FnOnce(RawFd) -> Result<Vec<u8>, i32> {
        move |_| Ok(bytes.to_vec())
    }

    #[test]
    fn first_modes_go_back_in_reverse_order_once() {
        let (mut handed_back, first) = std::io::pipe().expect("pipe");
        let second = first.try_clone().expect("second descriptor");
        let modes = TerminalModes::default();

        modes
            .keep(first.as_raw_fd(), mode(b"first "), write_back)
            .expect("first change");
        // Later changes keep the first mode and read nothing.
        modes
            .keep(first.as_raw_fd(), |_| panic!("read again"), write_back)
            .expect("later change");
        modes
            .keep(second.as_raw_fd(), mode(b"second "), write_back)
            .expect("change through a second descriptor");
        // A mode that cannot be read is not kept.
        assert_eq!(modes.keep(-1, |_| Err(9), write_back), Err(9));

        modes.restore();
        // The record is spent: a second hand-back has nothing to give.
        modes.restore();
        drop((first, second));
        let mut text = String::new();
        handed_back.read_to_string(&mut text).expect("read");
        assert_eq!(text, "second first ");
    }
}
