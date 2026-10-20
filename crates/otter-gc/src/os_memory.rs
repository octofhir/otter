//! What the operating system reports about this process's memory and the
//! host's, read with one system call each.
//!
//! # Contents
//! - [`resident_bytes`] — the process's resident set size now.
//! - [`available_memory`] — memory the host could hand out now.
//!
//! # Invariants
//! - Every query is one system call or one `/proc` read: nothing loads a
//!   system framework or walks the process table, so a launch pays for no
//!   query it does not make.
//! - A fact the platform cannot report is `None`.
//!
//! # See also
//! - [`crate::compressed`] — the cage whose pages these numbers include.

/// The process's resident set size now, in bytes.
#[cfg(target_vendor = "apple")]
#[must_use]
pub fn resident_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: the buffer is exactly one `proc_taskinfo`; the call writes at
    // most `size` bytes and returns how many it wrote.
    let written = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: a full-size answer initialized the whole record.
    (written == size).then(|| unsafe { info.assume_init() }.pti_resident_size)
}

/// The process's resident set size now, in bytes: resident pages from
/// `/proc/self/statm`.
#[cfg(all(unix, not(target_vendor = "apple")))]
#[must_use]
pub fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    pages.checked_mul(page_bytes()?)
}

/// The process's resident set size: not reported on this platform.
#[cfg(not(unix))]
#[must_use]
pub fn resident_bytes() -> Option<u64> {
    None
}

/// Memory the host could hand out now, in bytes: free pages.
#[cfg(target_vendor = "apple")]
#[must_use]
pub fn available_memory() -> Option<u64> {
    let mut free: u32 = 0;
    let mut length = std::mem::size_of::<u32>();
    // SAFETY: the name is NUL-terminated and the out buffer is one `u32`,
    // the sysctl's type.
    let status = unsafe {
        libc::sysctlbyname(
            c"vm.page_free_count".as_ptr(),
            (&raw mut free).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0).then_some(())?;
    Some(u64::from(free) * page_bytes()?)
}

/// Memory the host could hand out now, in bytes: `MemAvailable`.
#[cfg(all(unix, not(target_vendor = "apple")))]
#[must_use]
pub fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// Memory the host could hand out now: not reported on this platform.
#[cfg(not(unix))]
#[must_use]
pub fn available_memory() -> Option<u64> {
    None
}

#[cfg(unix)]
fn page_bytes() -> Option<u64> {
    // SAFETY: `sysconf` has no preconditions.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()
}
