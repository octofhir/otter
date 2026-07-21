//! In-realm structured-clone invariants.
//!
//! # Contents
//! - Cycles and shared references retain graph identity.
//! - Enumerable getters run in property order and abrupt completion propagates.
//! - Transfer-list failures never detach, while successful transfer does.
//! - Error causes and platform collection/view types preserve their structure.
//! - Retained clones survive full collection after template-JIT native re-entry.
//!
//! # Invariants
//! - The clone memo is published before recursively visiting children.
//! - JavaScript-observable property access uses ordinary `[[Get]]` semantics.
//! - ArrayBuffer detachment is transactional.
//! - Views that shared one source buffer share one cloned buffer.
//! - No raw moving value is retained across allocation, re-entry, or full GC.

use otter_runtime::{
    JitSelection, OtterError, Runtime, RuntimeExecutionStats, RuntimeExtensionContext,
    RuntimeExtensionInstaller, RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, SourceInput,
};

fn structured_clone_native(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
) -> Result<RuntimeValue, RuntimeNativeError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RuntimeValue::undefined);
    let options = args.get(1).copied().unwrap_or_else(RuntimeValue::undefined);
    otter_runtime::web_structured_clone::structured_clone_with_options(ctx, value, options)
}

fn install_structured_clone(runtime: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    runtime.install_native_global("structuredClone", 1, structured_clone_native)
}

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .extension_installer(RuntimeExtensionInstaller::new(install_structured_clone))
        .build()
        .expect("structured-clone runtime")
}

fn eval(runtime: &mut Runtime, name: &str, source: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), name)
        .expect("structured-clone fixture")
        .completion_string()
        .to_owned()
}

#[test]
fn cycles_and_shared_references_preserve_graph_identity() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-cycles.js",
        r#"
        const shared = { marker: "shared" };
        const source = {
            left: shared,
            right: shared,
            list: [shared]
        };
        source.self = source;
        source.list.push(source);
        shared.owner = source;

        const clone = structuredClone(source);
        JSON.stringify([
            clone !== source,
            clone.self === clone,
            clone.left === clone.right,
            clone.list[0] === clone.left,
            clone.list[1] === clone,
            clone.left.owner === clone,
            clone.left !== shared
        ]);
        "#,
    );

    assert_eq!(completion, "[true,true,true,true,true,true,true]");
}

#[test]
fn enumerable_getters_run_in_order_and_abrupt_throw_propagates() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-getters.js",
        r#"
        const log = [];
        const sentinel = { marker: "sentinel" };
        const source = {};
        Object.defineProperty(source, "first", {
            enumerable: true,
            get() {
                log.push("first");
                return { value: 1 };
            }
        });
        Object.defineProperty(source, "second", {
            enumerable: true,
            get() {
                log.push("second");
                throw sentinel;
            }
        });
        Object.defineProperty(source, "third", {
            enumerable: true,
            get() {
                log.push("third");
                return 3;
            }
        });

        let caughtSentinel = false;
        try {
            structuredClone(source);
        } catch (error) {
            caughtSentinel = error === sentinel;
        }
        JSON.stringify([log, caughtSentinel]);
        "#,
    );

    assert_eq!(completion, r#"[["first","second"],true]"#);
}

#[test]
fn transfer_detaches_only_after_success_and_rejects_duplicates_transactionally() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-transfer.js",
        r#"
        const buffer = new ArrayBuffer(8);
        const bytes = new Uint8Array(buffer);
        bytes[0] = 17;
        bytes[7] = 29;

        let cloneFailureCaught = false;
        try {
            structuredClone(
                { buffer, cannotClone() {} },
                { transfer: [buffer] }
            );
        } catch {
            cloneFailureCaught = true;
        }
        const intactAfterCloneFailure =
            buffer.byteLength === 8 &&
            new Uint8Array(buffer)[0] === 17 &&
            new Uint8Array(buffer)[7] === 29;

        let duplicateFailureCaught = false;
        try {
            structuredClone(buffer, { transfer: [buffer, buffer] });
        } catch {
            duplicateFailureCaught = true;
        }
        const intactAfterDuplicate =
            buffer.byteLength === 8 &&
            new Uint8Array(buffer)[0] === 17 &&
            new Uint8Array(buffer)[7] === 29;

        const clone = structuredClone({ buffer }, { transfer: [buffer] });
        JSON.stringify([
            cloneFailureCaught,
            intactAfterCloneFailure,
            duplicateFailureCaught,
            intactAfterDuplicate,
            buffer.byteLength === 0,
            clone.buffer.byteLength === 8,
            new Uint8Array(clone.buffer)[0] === 17,
            new Uint8Array(clone.buffer)[7] === 29
        ]);
        "#,
    );

    assert_eq!(completion, "[true,true,true,true,true,true,true,true]");
}

#[test]
fn error_cause_is_cloned_and_reuses_the_graph_memo() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-error-cause.js",
        r#"
        const cause = { code: 73, detail: { stable: true } };
        const error = new TypeError("outer failure");
        error.cause = cause;
        const source = { error, cause };

        const clone = structuredClone(source);
        const ownUndefined = new Error("own undefined");
        Object.defineProperty(ownUndefined, "cause", {
            value: undefined,
            writable: true,
            configurable: true
        });
        const ownUndefinedClone = structuredClone(ownUndefined);

        const inherited = new Error("inherited");
        const inheritedPrototype = Object.create(Error.prototype);
        inheritedPrototype.cause = { shouldNotClone: true };
        Object.setPrototypeOf(inherited, inheritedPrototype);
        const inheritedClone = structuredClone(inherited);

        JSON.stringify([
            clone.error !== error,
            clone.error instanceof TypeError,
            clone.error.name,
            clone.error.message,
            clone.error.cause === clone.cause,
            clone.error.cause !== cause,
            clone.error.cause.code,
            clone.error.cause.detail.stable,
            Object.prototype.hasOwnProperty.call(ownUndefinedClone, "cause"),
            ownUndefinedClone.cause === undefined,
            !Object.prototype.hasOwnProperty.call(inheritedClone, "cause")
        ]);
        "#,
    );

    assert_eq!(
        completion,
        r#"[true,true,"TypeError","outer failure",true,true,73,true,true,true,true]"#
    );
}

#[test]
fn typed_array_data_view_map_and_set_preserve_structure_and_identity() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-platform-types.js",
        r#"
        const buffer = new ArrayBuffer(20);
        const typed = new Uint16Array(buffer, 4, 3);
        typed[0] = 0x1234;
        typed[1] = 0x5678;
        typed[2] = 0x9abc;
        const view = new DataView(buffer, 2, 12);
        const shared = { marker: "map-key" };
        const mapValue = { ref: shared };
        const map = new Map([
            [shared, mapValue],
            ["typed", typed]
        ]);
        const set = new Set([shared, map]);
        const source = { buffer, typed, view, shared, map, set };

        const clone = structuredClone(source);
        JSON.stringify([
            clone.typed instanceof Uint16Array,
            clone.view instanceof DataView,
            clone.map instanceof Map,
            clone.set instanceof Set,
            clone.typed.buffer === clone.buffer,
            clone.view.buffer === clone.buffer,
            clone.typed.byteOffset === 4,
            clone.typed.length === 3,
            clone.view.byteOffset === 2,
            clone.view.byteLength === 12,
            clone.typed[0] === 0x1234,
            clone.typed[1] === 0x5678,
            clone.typed[2] === 0x9abc,
            clone.map.get(clone.shared).ref === clone.shared,
            clone.map.get("typed") === clone.typed,
            clone.set.has(clone.shared),
            clone.set.has(clone.map)
        ]);
        "#,
    );

    assert_eq!(
        completion,
        "[true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true]"
    );
}

#[test]
fn intrinsic_brands_ignore_mutable_globals_and_proxy_is_rejected() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-intrinsic-brands.js",
        r#"
        const DateIntrinsic = Date;
        const RegExpIntrinsic = RegExp;
        const TypeErrorIntrinsic = TypeError;
        const date = new Date(1700000000123);
        const regexp = /otter/giu;
        regexp.lastIndex = 9;
        const error = new TypeError("stable");

        let constructorCalls = 0;
        globalThis.Date = function () {
            constructorCalls++;
            throw new Error("mutated Date called");
        };
        globalThis.RegExp = function () {
            constructorCalls++;
            throw new Error("mutated RegExp called");
        };
        globalThis.TypeError = function () {
            constructorCalls++;
            throw new Error("mutated TypeError called");
        };

        const clone = structuredClone({ date, regexp, error });
        let proxyRejected = false;
        try {
            structuredClone(new Proxy([], {}));
        } catch {
            proxyRejected = true;
        }

        JSON.stringify([
            constructorCalls === 0,
            clone.date instanceof DateIntrinsic,
            clone.date.getTime() === 1700000000123,
            clone.regexp instanceof RegExpIntrinsic,
            clone.regexp.source === "otter",
            clone.regexp.flags === "giu",
            clone.regexp.lastIndex === 0,
            clone.error instanceof TypeErrorIntrinsic,
            clone.error.message === "stable",
            proxyRejected
        ]);
        "#,
    );

    assert_eq!(
        completion,
        "[true,true,true,true,true,true,true,true,true,true]"
    );
}

#[test]
fn shared_array_buffer_brand_and_view_aliasing_are_preserved() {
    let mut runtime = runtime(JitSelection::InterpreterOnly);
    let completion = eval(
        &mut runtime,
        "structured-clone-shared-buffer.js",
        r#"
        const buffer = new SharedArrayBuffer(16);
        const typed = new Uint16Array(buffer, 4, 2);
        const view = new DataView(buffer, 2, 10);
        typed[0] = 0x1234;

        const clone = structuredClone({ buffer, typed, view });
        clone.typed[1] = 0x5678;
        JSON.stringify([
            clone.buffer instanceof SharedArrayBuffer,
            clone.buffer !== buffer,
            clone.typed instanceof Uint16Array,
            clone.view instanceof DataView,
            clone.typed.buffer === clone.buffer,
            clone.view.buffer === clone.buffer,
            clone.typed[0] === 0x1234,
            typed[1] === 0x5678
        ]);
        "#,
    );

    assert_eq!(completion, "[true,true,true,true,true,true,true,true]");
}

struct GcJitResult {
    completion: String,
    stats: RuntimeExecutionStats,
}

fn run_gc_jit_fixture(selection: JitSelection) -> GcJitResult {
    let mut runtime = runtime(selection);
    eval(
        &mut runtime,
        "structured-clone-jit-setup.js",
        r#"
        function cloneFromHotPath(value) {
            if (value === null) {
                return 1;
            }
            return structuredClone(value);
        }

        let checksum = 0;
        for (let index = 0; index < 600; index++) {
            checksum += cloneFromHotPath(null);
        }

        const shared = { name: "retained" };
        const source = {
            left: shared,
            right: shared,
            bytes: new Uint8Array([3, 4, 5]),
            map: new Map([["shared", shared]]),
            set: new Set([shared])
        };
        source.self = source;
        globalThis.__structuredCloneChecksum = checksum;
        globalThis.__retainedStructuredClone = cloneFromHotPath(source);
        "#,
    );
    let stats = runtime.execution_stats();
    let cycles_before = runtime.heap_stats().gc_cycles;
    runtime.force_gc().expect("full structured-clone GC");
    assert!(
        runtime.heap_stats().gc_cycles > cycles_before,
        "fixture must execute a full collection"
    );
    let completion = eval(
        &mut runtime,
        "structured-clone-jit-probe.js",
        r#"
        const clone = globalThis.__retainedStructuredClone;
        JSON.stringify([
            globalThis.__structuredCloneChecksum,
            clone.self === clone,
            clone.left === clone.right,
            clone.map.get("shared") === clone.left,
            clone.set.has(clone.left),
            clone.bytes instanceof Uint8Array,
            Array.from(clone.bytes).join(",")
        ]);
        "#,
    );
    GcJitResult { completion, stats }
}

#[test]
fn retained_clone_survives_full_gc_and_template_jit_reentry() {
    let oracle = run_gc_jit_fixture(JitSelection::InterpreterOnly);
    let compiled = run_gc_jit_fixture(JitSelection::Template);

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        r#"[600,true,true,true,true,true,"3,4,5"]"#
    );
    assert!(
        compiled.stats.jit_compile_attempts > 0,
        "fixture must compile the structuredClone caller"
    );
}
