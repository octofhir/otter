//! CommonJS module-record cache invariants.
//!
//! # Contents
//! - Successful file loads expose live, loaded module records through
//!   `require.cache`.
//! - Abrupt hosted installation rolls its partial record back and retries.
//! - Circular file loads observe the current replacement `module.exports`.
//! - Canonical file aliases preserve module and export singleton identity.
//! - A retained require closure, cache record, and export survive full GC.
//! - An uncached nested require entered from template-JIT code matches the
//!   interpreter.
//!
//! # Invariants
//! - Cache values are module records, never snapshots of `module.exports`.
//! - Every abrupt exit removes the record inserted by that load attempt.
//! - Resolver aliases converge before cache lookup.
//! - The shared cache is rooted solely by ordinary reachable JS values.
//! - Compiled-to-native-to-CommonJS re-entry uses the active runtime turn.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use otter_runtime::{
    CapabilitySet, HostedModule, JitSelection, Runtime, RuntimeAttr, RuntimeExecutionStats,
    RuntimeLocal, RuntimeNativeError, RuntimeNativeScope, RuntimeTaskSpawner, SourceInput,
    runtime_type_error,
};

fn write_fixture(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture directory");
    }
    std::fs::write(&path, source).expect("write CommonJS fixture");
    path
}

fn canonical_js_string(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonical fixture path");
    serde_json::to_string(&canonical.to_string_lossy()).expect("serialize fixture path")
}

fn commonjs_runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("CommonJS runtime")
}

#[test]
fn successful_load_publishes_live_loaded_module_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dependency = write_fixture(
        dir.path(),
        "dependency.cjs",
        r#"
        globalThis.__successfulLoadCount =
            (globalThis.__successfulLoadCount ?? 0) + 1;
        module.exports = { marker: "dependency", count: __successfulLoadCount };
        "#,
    );
    let entry = dir.path().join("entry.cjs");
    let dependency_key = canonical_js_string(&dependency);
    std::fs::write(&entry, "").expect("create entry for canonicalization");
    let entry_key = canonical_js_string(&entry);
    std::fs::write(
        &entry,
        format!(
            r#"
            const dependency = require("./dependency.cjs");
            const dependencyRecord = require.cache[{dependency_key}];
            const entryRecord = require.cache[{entry_key}];

            if (Object.getPrototypeOf(require.cache) !== null) {{
                throw new Error("require.cache must be a bare object");
            }}
            if (!dependencyRecord || dependencyRecord.exports !== dependency ||
                dependencyRecord.loaded !== true ||
                dependencyRecord.id !== {dependency_key} ||
                dependencyRecord.filename !== {dependency_key}) {{
                throw new Error("dependency cache record is not live and loaded");
            }}
            if (entryRecord !== module || entryRecord.exports !== module.exports ||
                entryRecord.loaded !== false) {{
                throw new Error("entry cache value is not its live module record");
            }}
            if (require("./dependency.cjs") !== dependency ||
                globalThis.__successfulLoadCount !== 1) {{
                throw new Error("successful module did not remain a singleton");
            }}
            "#,
        ),
    )
    .expect("write entry");

    commonjs_runtime(JitSelection::InterpreterOnly)
        .run_file(&entry)
        .expect("successful CommonJS load");
}

static FLAKY_INSTALL_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

fn flaky_cjs_install<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    _capabilities: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: RuntimeLocal<'scope>,
    require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let attempt = FLAKY_INSTALL_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
    let exports = scope.object()?;
    let attempt_value = scope.number(attempt as f64);
    scope.set(exports, "attempt", attempt_value)?;
    scope.set(module, "exports", exports)?;
    if attempt == 1 {
        let cache = scope.get(require, "cache")?;
        let flags = RuntimeAttr {
            writable: true,
            enumerable: true,
            configurable: false,
        }
        .to_flags();
        scope.define(cache, "test:flaky", module, flags)?;
        return Err(runtime_type_error(
            "test:flaky",
            "intentional first-install failure",
        ));
    }
    let returned_only = scope.object()?;
    let marker = scope.boolean(true);
    scope.set(returned_only, "returnedOnly", marker)?;
    Ok(returned_only)
}

#[test]
fn abrupt_hosted_install_rolls_back_its_record_and_retries() {
    FLAKY_INSTALL_ATTEMPTS.store(0, Ordering::SeqCst);
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = write_fixture(
        dir.path(),
        "entry.cjs",
        r#"
        let caught = false;
        try {
            require("test:flaky");
        } catch (error) {
            caught = String(error).includes("intentional first-install failure");
        }
        if (!caught) {
            throw new Error("first hosted failure was not observable");
        }
        if (require.cache["test:flaky"] !== undefined) {
            throw new Error("abrupt hosted load left a partial cache record");
        }

        const loadedValue = require("test:flaky");
        const record = require.cache["test:flaky"];
        if (loadedValue.attempt !== 2 || !record ||
            loadedValue.returnedOnly !== undefined ||
            record.exports !== loadedValue || record.loaded !== true) {
            throw new Error("hosted retry did not publish its successful record");
        }
        "#,
    );
    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .hosted_module(HostedModule::cjs_only("test:flaky", flaky_cjs_install))
        .build()
        .expect("runtime with flaky hosted module");

    runtime.run_file(&entry).expect("hosted retry fixture");
    assert_eq!(
        FLAKY_INSTALL_ATTEMPTS.load(Ordering::SeqCst),
        2,
        "failed hosted installation must be retried exactly once"
    );
}

#[test]
fn abrupt_file_evaluation_rolls_back_its_record_and_retries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dependency = write_fixture(
        dir.path(),
        "flaky-file.cjs",
        r#"
        globalThis.__flakyFileAttempts =
            (globalThis.__flakyFileAttempts ?? 0) + 1;
        module.exports = { attempt: __flakyFileAttempts };
        if (__flakyFileAttempts === 1) {
            throw new Error("intentional file evaluation failure");
        }
        "#,
    );
    let dependency_key = canonical_js_string(&dependency);
    let entry = write_fixture(
        dir.path(),
        "entry.cjs",
        &format!(
            r#"
            let caught = false;
            try {{
                require("./flaky-file.cjs");
            }} catch (error) {{
                caught = String(error).includes("intentional file evaluation failure");
            }}
            if (!caught) {{
                throw new Error("first file failure was not observable");
            }}
            if (require.cache[{dependency_key}] !== undefined) {{
                throw new Error("throwing file left a partial cache record");
            }}

            const loadedValue = require("./flaky-file.cjs");
            const record = require.cache[{dependency_key}];
            if (loadedValue.attempt !== 2 || !record ||
                record.exports !== loadedValue || record.loaded !== true ||
                globalThis.__flakyFileAttempts !== 2) {{
                throw new Error("file retry did not publish its successful record");
            }}
            "#,
        ),
    );

    commonjs_runtime(JitSelection::InterpreterOnly)
        .run_file(&entry)
        .expect("file retry fixture");
}

#[test]
fn circular_require_reads_the_current_replaced_module_exports() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_fixture(
        dir.path(),
        "a.cjs",
        r#"
        exports.phase = "initial-object";
        module.exports = { phase: "replacement-object" };
        const b = require("./b.cjs");
        module.exports.backEdgePhase = b.observedPhase;
        module.exports.backEdgeIdentity = b.sameOnSecondRead;
        "#,
    );
    write_fixture(
        dir.path(),
        "b.cjs",
        r#"
        const first = require("./a.cjs");
        const second = require("./a.cjs");
        const aKey = Object.keys(require.cache)
            .find(key => key.endsWith("/a.cjs"));
        const aRecord = require.cache[aKey];
        module.exports = {
            observedPhase: first.phase,
            sameOnSecondRead: first === second,
            livePartialRecord:
                aRecord.loaded === false && aRecord.exports === first
        };
        "#,
    );
    let entry = write_fixture(
        dir.path(),
        "entry.cjs",
        r#"
        const a = require("./a.cjs");
        if (a.phase !== "replacement-object" ||
            a.backEdgePhase !== "replacement-object" ||
            a.backEdgeIdentity !== true ||
            require("./b.cjs").livePartialRecord !== true) {
            throw new Error("circular require read a stale exports snapshot");
        }
        "#,
    );

    commonjs_runtime(JitSelection::InterpreterOnly)
        .run_file(&entry)
        .expect("circular replacement fixture");
}

#[test]
fn canonical_file_aliases_share_module_and_export_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_fixture(
        dir.path(),
        "singleton.cjs",
        r#"
        globalThis.__singletonEvaluationCount =
            (globalThis.__singletonEvaluationCount ?? 0) + 1;
        module.exports = { identity: {} };
        "#,
    );
    std::fs::create_dir(dir.path().join("sub")).expect("create alias directory");
    let entry = write_fixture(
        dir.path(),
        "entry.cjs",
        r#"
        const extensionless = require("./singleton");
        const explicit = require("./singleton.cjs");
        const dotted = require("./sub/../singleton.cjs");
        if (extensionless !== explicit || explicit !== dotted ||
            extensionless.identity !== dotted.identity ||
            globalThis.__singletonEvaluationCount !== 1) {
            throw new Error("canonical aliases did not share singleton identity");
        }

        const records = Object.keys(require.cache)
            .filter(key => key.endsWith("/singleton.cjs"));
        if (records.length !== 1 ||
            require.cache[records[0]].exports !== extensionless) {
            throw new Error("canonical aliases produced duplicate module records");
        }
        "#,
    );

    commonjs_runtime(JitSelection::InterpreterOnly)
        .run_file(&entry)
        .expect("canonical alias fixture");
}

#[test]
fn retained_require_cache_and_exports_survive_full_gc_relocation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dependency = write_fixture(
        dir.path(),
        "retained.cjs",
        r#"
        module.exports = {
            marker: { name: "retained-export", version: 7 },
            payload: Array.from({ length: 64 }, (_, index) => ({
                index,
                text: "payload-" + index
            }))
        };
        "#,
    );
    let entry = dir.path().join("entry.cjs");
    let dependency_key = canonical_js_string(&dependency);
    std::fs::write(
        &entry,
        format!(
            r#"
            globalThis.__retainedRequire = require;
            globalThis.__retainedExport = require("./retained.cjs");
            globalThis.__retainedRecord = require.cache[{dependency_key}];
            "#,
        ),
    )
    .expect("write retained entry");

    let mut runtime = commonjs_runtime(JitSelection::Template);
    runtime.run_file(&entry).expect("retain CommonJS graph");
    let cycles_before = runtime.heap_stats().gc_cycles;
    runtime.force_gc().expect("full GC");
    let cycles_after = runtime.heap_stats().gc_cycles;
    assert!(
        cycles_after > cycles_before,
        "fixture must execute a full collection"
    );

    let completion = runtime
        .run_script(
            SourceInput::from_javascript(format!(
                r#"
                const after = __retainedRequire("./retained.cjs");
                const record = __retainedRequire.cache[{dependency_key}];
                JSON.stringify([
                    after === __retainedExport,
                    record === __retainedRecord,
                    record.exports === after,
                    record.loaded,
                    after.marker.name,
                    after.marker.version,
                    after.payload[63].text
                ]);
                "#,
            )),
            "commonjs-full-gc-probe.js",
        )
        .expect("probe retained CommonJS graph")
        .completion_string()
        .to_owned();
    assert_eq!(
        completion,
        r#"[true,true,true,true,"retained-export",7,"payload-63"]"#
    );
}

struct JitRequireResult {
    completion: String,
    stats: RuntimeExecutionStats,
}

fn run_nested_jit_require(selection: JitSelection) -> JitRequireResult {
    let dir = tempfile::tempdir().expect("tempdir");
    write_fixture(
        dir.path(),
        "inner.cjs",
        r#"
        module.exports = { value: 40, identity: {} };
        "#,
    );
    write_fixture(
        dir.path(),
        "outer.cjs",
        r#"
        const inner = require("./inner.cjs");
        module.exports = {
            value: inner.value + 2,
            inner,
            sameInner: require("./inner.cjs") === inner
        };
        "#,
    );
    let entry = write_fixture(
        dir.path(),
        "entry.cjs",
        r#"
        function loadFromHotFunction(specifier) {
            if (specifier === null) {
                return 1;
            }
            return require(specifier);
        }

        let checksum = 0;
        for (let i = 0; i < 512; i++) {
            checksum += loadFromHotFunction(null);
        }

        const outer = loadFromHotFunction("./outer.cjs");
        globalThis.__nestedJitRequireResult = JSON.stringify([
            checksum,
            outer.value,
            outer.sameInner,
            outer.inner === require("./inner.cjs")
        ]);
        "#,
    );

    let mut runtime = commonjs_runtime(selection);
    runtime
        .run_file(&entry)
        .expect("nested JIT require fixture");
    let stats = runtime.execution_stats();
    let completion = runtime
        .run_script(
            SourceInput::from_javascript("__nestedJitRequireResult;"),
            "nested-jit-require-probe.js",
        )
        .expect("read nested JIT require result")
        .completion_string()
        .to_owned();
    JitRequireResult { completion, stats }
}

#[test]
fn nested_require_from_template_jit_matches_the_interpreter() {
    let oracle = run_nested_jit_require(JitSelection::InterpreterOnly);
    let compiled = run_nested_jit_require(JitSelection::Template);

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[512,42,true,true]");
    assert!(
        compiled.stats.jit_compile_attempts > 0,
        "fixture must compile the require caller"
    );
    assert_eq!(
        compiled.stats.jit_osr_attempts, 0,
        "fixture must exercise whole-function JIT entry"
    );
    assert!(
        compiled.stats.jit_reentrant_stub_transitions > 0,
        "uncached nested require must cross the JIT re-entry boundary"
    );
}
