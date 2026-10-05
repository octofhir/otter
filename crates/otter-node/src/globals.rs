//! Node-specific globals: `global`, `process` additions, the lazy Node
//! console, `process.openStdin`, Node timers and promise-rejection reporting.
//!
//! # Contents
//! - [`node_globals_installer`] runs the one build-produced realm-installer
//!   script in every realm.
//!
//! # Invariants
//! - The installer is compiled at product build time under
//!   `<realm-installer>`; a realm verifies and links it, never compiles it
//!   (unless the embedder configured its own compiler).
//! - `setImmediate`/`clearImmediate` are real timer globals installed by the
//!   VM timer family (`otter-vm/src/timers.rs`), not here. Web-platform
//!   globals (`atob`, `fetch`, `queueMicrotask`, `AbortController`, ...) live
//!   in `otter-web`.
//!
//! # See also
//! - `build/builtins.rs` for the producer.

use otter_runtime::{ExtensionJs, OtterError, RuntimeGlobalInstaller, RuntimeRealmContext};

/// The Node realm installer, compiled at product build time from
/// `node_process_globals.js`, `node_console_global.js`,
/// `process_stdio_global.js`, `node_timers_global.js` and
/// `node_promise_rejection_global.js`, concatenated as written.
const NODE_REALM_INSTALLER: ExtensionJs = ExtensionJs {
    source: include_str!(concat!(env!("OUT_DIR"), "/node-realm-installer.js")),
    bytecode: include_bytes!(concat!(env!("OUT_DIR"), "/node-realm-installer.bytecode")),
    defines: &[],
};

/// Installer for the Node-specific globals. Registered by `with_node_apis`.
#[must_use]
pub fn node_globals_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install)
}

fn install(runtime: &mut RuntimeRealmContext<'_>) -> Result<(), OtterError> {
    runtime.install_static_script(&NODE_REALM_INSTALLER)
}
