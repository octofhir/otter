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
         if (typeof process === 'object' && process.finalization === undefined) {\n\
           const exitRegistry = [];\n\
           const beforeExitRegistry = [];\n\
           const invoke = (list, event) => {\n\
             for (const [ref, callback] of list.splice(0)) {\n\
               const held = ref.deref();\n\
               if (held !== undefined) callback.call(process, held, event);\n\
             }\n\
           };\n\
           const validate = (obj, callback) => {\n\
             if ((typeof obj !== 'object' && typeof obj !== 'function') || obj === null) {\n\
               const err = new TypeError('The \"ref\" argument must be of type object. Received ' + (obj === null ? 'null' : typeof obj));\n\
               err.code = 'ERR_INVALID_ARG_TYPE';\n\
               throw err;\n\
             }\n\
             if (typeof callback !== 'function') {\n\
               const err = new TypeError('The \"callback\" argument must be of type function. Received ' + typeof callback);\n\
               err.code = 'ERR_INVALID_ARG_TYPE';\n\
               throw err;\n\
             }\n\
           };\n\
           process.finalization = {\n\
             register(obj, callback) {\n\
               validate(obj, callback);\n\
               exitRegistry.push([new WeakRef(obj), callback]);\n\
             },\n\
             registerBeforeExit(obj, callback) {\n\
               validate(obj, callback);\n\
               beforeExitRegistry.push([new WeakRef(obj), callback]);\n\
             },\n\
             unregister(obj) {\n\
               for (const list of [exitRegistry, beforeExitRegistry]) {\n\
                 for (let i = list.length - 1; i >= 0; i--) {\n\
                   if (list[i][0].deref() === obj) list.splice(i, 1);\n\
                 }\n\
               }\n\
             },\n\
           };\n\
           process.on('beforeExit', () => invoke(beforeExitRegistry, 'beforeExit'));\n\
           process.on('exit', () => invoke(exitRegistry, 'exit'));\n\
         }\n\
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
        include_str!("node_console_global.js"),
        include_str!("process_stdio_global.js"),
        include_str!("node_timers_global.js"),
        include_str!("node_promise_rejection_global.js"),
    )))
}
