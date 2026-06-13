//! Node-specific globals: `global` (alias for `globalThis`).
//!
//! `setImmediate`/`clearImmediate` are real timer globals installed by the VM
//! timer family (`otter-vm/src/timers.rs`), not stubbed here.
//! Web-platform globals (`atob`, `fetch`, `queueMicrotask`, `AbortController`,
//! ...) are NOT here — they live in `otter-web`.

use otter_runtime::{OtterError, Runtime, RuntimeGlobalInstaller};

/// Installer for the Node-specific globals. Registered by `with_node_apis`.
#[must_use]
pub fn node_globals_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install)
}

fn install(runtime: &mut Runtime) -> Result<(), OtterError> {
    // `global` aliases `globalThis` (Node compatibility).
    let global_this = runtime.global_this();
    runtime.set_global("global", global_this);
    Ok(())
}
