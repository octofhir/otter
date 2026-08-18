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
    runtime.install_script(SourceInput::from_javascript(concat!(
        "Object.defineProperty(globalThis, Symbol.toStringTag, { value: 'global', configurable: true });\n\
         Object.defineProperty(globalThis, 'global', { value: globalThis, writable: true, configurable: true });\n\
         if (typeof process === 'object' && process.ref === undefined) {\n\
           const refSymbol = Symbol.for('nodejs.ref');\n\
           const unrefSymbol = Symbol.for('nodejs.unref');\n\
           process.ref = (maybeRefable) => {\n\
             const fn = maybeRefable?.[refSymbol] ?? maybeRefable?.ref;\n\
             if (typeof fn === 'function') fn.call(maybeRefable);\n\
           };\n\
           process.unref = (maybeRefable) => {\n\
             const fn = maybeRefable?.[unrefSymbol] ?? maybeRefable?.unref;\n\
             if (typeof fn === 'function') fn.call(maybeRefable);\n\
           };\n\
         }\n\
         if (typeof process === 'object' && process.report === undefined) {\n\
           process.report = {\n\
             getReport() { return { header: {}, javascriptStack: {}, libuv: [], workers: [], environmentVariables: {}, sharedObjects: [] }; },\n\
             writeReport() { return ''; },\n\
             directory: '', filename: '',\n\
             compact: false, excludeNetwork: false, excludeEnv: false,\n\
             reportOnFatalError: false, reportOnSignal: false, reportOnUncaughtException: false,\n\
             signal: 'SIGUSR2',\n\
           };\n\
         }\n",
        include_str!("node_timers_global.js"),
    )))
}
