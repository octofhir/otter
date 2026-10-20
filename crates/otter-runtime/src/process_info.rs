//! What `process` reports about the running process and its host, read from
//! the operating system directly.
//!
//! # Contents
//! - [`parent_pid`] — the parent process id.
//! - [`resident_bytes`] — the process's resident set size now.
//! - [`available_memory`] — memory the host could hand out now.
//!
//! # Invariants
//! - On unix every query is one system call or one `/proc` read: nothing
//!   loads a system framework or walks the process table, so a launch pays
//!   for no query it does not make.
//! - A fact the platform cannot report is `None`; callers keep Node's
//!   documented fallbacks.
//!
//! # See also
//! - `super::process` — the `process` object these facts fill.
//! - [`otter_gc::os_memory`] — the memory queries on unix.

/// The parent process id.
#[cfg(unix)]
#[must_use]
pub(crate) fn parent_pid() -> Option<u32> {
    rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get() as u32)
}

#[cfg(not(unix))]
#[must_use]
pub(crate) fn parent_pid() -> Option<u32> {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::get_current_pid().ok()?;
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid)?.parent().map(|pid| pid.as_u32())
}

/// The process's resident set size now, in bytes.
#[cfg(unix)]
#[must_use]
pub(crate) fn resident_bytes() -> Option<u64> {
    otter_gc::os_memory::resident_bytes()
}

#[cfg(not(unix))]
#[must_use]
pub(crate) fn resident_bytes() -> Option<u64> {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::get_current_pid().ok()?;
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    Some(system.process(pid)?.memory())
}

/// Memory the host could hand out now, in bytes.
#[cfg(unix)]
#[must_use]
pub(crate) fn available_memory() -> Option<u64> {
    otter_gc::os_memory::available_memory()
}

#[cfg(not(unix))]
#[must_use]
pub(crate) fn available_memory() -> Option<u64> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    Some(system.available_memory())
}
