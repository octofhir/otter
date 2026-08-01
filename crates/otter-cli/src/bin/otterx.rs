//! `otterx` — the package-binary runner as its own command.
//!
//! `otterx esbuild --version` reads better than `otter x esbuild --version`,
//! and tooling that shells out expects a single executable name. This binary
//! is that name and nothing else: it forwards to the `otter` executable
//! installed beside it, which owns the resolution, fetch, and run.
//!
//! # Invariants
//! - The forwarded-to executable is the sibling of this one, so a checkout, an
//!   installed release, and a build directory each use their own `otter`
//!   rather than whichever one a `PATH` lookup happens to find first.
//! - The child's exit status is this process's exit status: a wrapper that
//!   swallowed a tool's failure would break every script using it.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let Some(executable) = sibling_otter() else {
        eprintln!("otterx: cannot locate the otter executable");
        return ExitCode::from(2);
    };
    let status = Command::new(&executable)
        .arg("x")
        .args(std::env::args_os().skip(1))
        .status();
    match status {
        Ok(status) => ExitCode::from(exit_code(status.code())),
        Err(err) => {
            eprintln!("otterx: cannot run {}: {err}", executable.display());
            ExitCode::from(2)
        }
    }
}

fn sibling_otter() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let directory = current.parent()?;
    let candidate = directory.join(if cfg!(windows) { "otter.exe" } else { "otter" });
    candidate.is_file().then_some(candidate)
}

/// A terminated child reports no code; report the conventional "killed by a
/// signal" status rather than a success the tool never had.
fn exit_code(code: Option<i32>) -> u8 {
    match code {
        Some(code) => u8::try_from(code & 0xff).unwrap_or(1),
        None => 130,
    }
}
