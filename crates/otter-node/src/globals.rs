//! Node-specific globals: `global` (alias for `globalThis`).
//!
//! `setImmediate`/`clearImmediate` are real timer globals installed by the VM
//! timer family (`otter-vm/src/timers.rs`), not stubbed here.
//! Web-platform globals (`atob`, `fetch`, `queueMicrotask`, `AbortController`,
//! ...) are NOT here — they live in `otter-web`.

use otter_runtime::{OtterError, RuntimeGlobalInstaller, RuntimeRealmContext, SourceInput};

/// Installer for the Node-specific globals. Registered by `with_node_apis`.
#[must_use]
pub fn node_globals_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install)
}

fn install(runtime: &mut RuntimeRealmContext<'_>) -> Result<(), OtterError> {
    runtime.install_script(SourceInput::from_javascript(
        "Object.defineProperty(globalThis, Symbol.toStringTag, { value: 'global', configurable: true });\n\
         Object.defineProperty(globalThis, 'global', { value: globalThis, writable: true, configurable: true });",
    ))
}
