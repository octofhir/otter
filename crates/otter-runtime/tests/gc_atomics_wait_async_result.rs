//! Moving-GC invariants for `Atomics.waitAsync` result records.
//!
//! # Contents
//! - Synchronous `not-equal` and zero-timeout result records.
//! - Promise-valued finite-timeout and registered-waiter result records.
//! - Explicit host polling before observing suspended script results.
//!
//! # Invariants
//! - The result receiver, label, and optional Promise remain rooted across
//!   object allocation and both property-shape writes.
//! - Result own-property order is `async`, then `value`; both properties have
//!   ordinary writable, enumerable, configurable data descriptors.
//! - Synchronous results expose a string while asynchronous results expose a
//!   Promise that retains its exact label across moving collections.
//! - Direct-runtime scripts do not synchronously wait for parked atomic promises.
//! - Timeout and notification waiters use distinct words, independent of timing.
//! - Warmup retains all 64 calls under stress; a bounded host slice may yield.

use otter_runtime::{JitSelection, Runtime, SourceInput};
use std::time::Duration;

mod atomic_wait_driver;

struct RunResult {
    completion: String,
    compile_attempts: u64,
}

fn run(selection: JitSelection, warm_source: &str, final_source: &str, name: &str) -> RunResult {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let warm_name = format!("{name}-warm");
    runtime
        .run_script(
            SourceInput::from_javascript(format!(
                "{warm_source}\nglobalThis.__waitAsyncWarmDone=true;"
            ))
            .with_top_level_await(),
            &warm_name,
        )
        .expect("Atomics.waitAsync warmup");
    let mut completed = 0;
    for _ in 0..64 {
        let done = runtime
            .drive_pending_atomic_waits(Duration::from_secs(2))
            .expect("drive warmup waiters");
        let current: u32 = runtime
            .eval(SourceInput::from_javascript(
                "globalThis.__waitAsyncCompleted;",
            ))
            .unwrap()
            .completion_string()
            .parse()
            .unwrap();
        assert!(
            current > completed,
            "warmup made no progress after {completed} calls"
        );
        completed = current;
        if done {
            break;
        }
    }
    assert_eq!(
        completed, 64,
        "every warmup call must finish before measuring compilation"
    );
    assert_eq!(
        runtime
            .eval(SourceInput::from_javascript(
                "globalThis.__waitAsyncWarmDone;"
            ))
            .unwrap()
            .completion_string(),
        "true"
    );
    let warm_stats = runtime.execution_stats();
    let final_name = format!("{name}-final");
    runtime
        .run_script(
            SourceInput::from_javascript(final_source.to_string()).with_top_level_await(),
            &final_name,
        )
        .expect("Atomics.waitAsync final probe");
    runtime
        .force_gc()
        .expect("collect suspended result records");
    assert!(
        runtime
            .drive_pending_atomic_waits(Duration::from_secs(2))
            .expect("drive final waiters"),
        "final result must settle"
    );
    let completion = runtime
        .eval(SourceInput::from_javascript(
            "globalThis.__waitAsyncCompletion;",
        ))
        .unwrap()
        .completion_string()
        .to_owned();
    RunResult {
        completion,
        compile_attempts: warm_stats.jit_compile_attempts,
    }
}

fn assert_interpreter_and_template(
    warm_source: &str,
    final_source: &str,
    name: &str,
    expected: &str,
) {
    let interpreter = run(
        JitSelection::InterpreterOnly,
        warm_source,
        final_source,
        name,
    );
    let template = run(JitSelection::Template, warm_source, final_source, name);
    assert_eq!(
        template.completion, interpreter.completion,
        "{name}: tier mismatch"
    );
    assert_eq!(
        template.completion, expected,
        "{name}: unexpected completion"
    );
    assert!(
        template.compile_attempts > 0,
        "{name}: warmup must request compilation"
    );
}

#[test]
fn wait_async_result_records_survive_each_allocation_and_shape_write() {
    assert_interpreter_and_template(
        r#"
        const view = new Int32Array(new SharedArrayBuffer(8));
        Atomics.store(view, 0, 17);
        Atomics.store(view, 1, 17);

        globalThis.__waitAsyncResultProbe = function probe(seed, stress) {
            const notEqual = Atomics.waitAsync(view, 0, 18, Infinity);
            const zeroTimeout = Atomics.waitAsync(view, 0, 17, 0);
            const finiteTimeout = Atomics.waitAsync(view, 1, 17, 1);
            const finitePromise = finiteTimeout.value;
            const registered = Atomics.waitAsync(view, 0, 17, Infinity);
            const registeredPromise = registered.value;

            let tail = null;
            if (stress) {
                for (let i = 0; i < 160; i++) {
                    tail = {
                        seed,
                        i,
                        text: "wait-async-" + seed + "-" + i,
                        tail
                    };
                }
            }

            const notEqualAsync =
                Object.getOwnPropertyDescriptor(notEqual, "async");
            const notEqualValue =
                Object.getOwnPropertyDescriptor(notEqual, "value");
            const notEqualOk =
                Object.getPrototypeOf(notEqual) === Object.prototype &&
                Reflect.ownKeys(notEqual).join(",") === "async,value" &&
                notEqual.async === false &&
                notEqual.value === "not-equal" &&
                notEqualAsync.value === false &&
                notEqualAsync.writable === true &&
                notEqualAsync.enumerable === true &&
                notEqualAsync.configurable === true &&
                notEqualValue.value === "not-equal" &&
                notEqualValue.writable === true &&
                notEqualValue.enumerable === true &&
                notEqualValue.configurable === true;

            const zeroAsync =
                Object.getOwnPropertyDescriptor(zeroTimeout, "async");
            const zeroValue =
                Object.getOwnPropertyDescriptor(zeroTimeout, "value");
            const zeroTimeoutOk =
                Object.getPrototypeOf(zeroTimeout) === Object.prototype &&
                Reflect.ownKeys(zeroTimeout).join(",") === "async,value" &&
                zeroTimeout.async === false &&
                zeroTimeout.value === "timed-out" &&
                zeroAsync.value === false &&
                zeroAsync.writable === true &&
                zeroAsync.enumerable === true &&
                zeroAsync.configurable === true &&
                zeroValue.value === "timed-out" &&
                zeroValue.writable === true &&
                zeroValue.enumerable === true &&
                zeroValue.configurable === true;

            const finiteAsync =
                Object.getOwnPropertyDescriptor(finiteTimeout, "async");
            const finiteValue =
                Object.getOwnPropertyDescriptor(finiteTimeout, "value");
            const finiteShapeOk =
                Object.getPrototypeOf(finiteTimeout) === Object.prototype &&
                Reflect.ownKeys(finiteTimeout).join(",") === "async,value" &&
                finiteTimeout.async === true &&
                finiteTimeout.value === finitePromise &&
                finitePromise instanceof Promise &&
                finiteAsync.value === true &&
                finiteAsync.writable === true &&
                finiteAsync.enumerable === true &&
                finiteAsync.configurable === true &&
                finiteValue.value === finitePromise &&
                finiteValue.writable === true &&
                finiteValue.enumerable === true &&
                finiteValue.configurable === true;

            const registeredAsync =
                Object.getOwnPropertyDescriptor(registered, "async");
            const registeredValue =
                Object.getOwnPropertyDescriptor(registered, "value");
            const registeredShapeOk =
                Object.getPrototypeOf(registered) === Object.prototype &&
                Reflect.ownKeys(registered).join(",") === "async,value" &&
                registered.async === true &&
                registered.value === registeredPromise &&
                registeredPromise instanceof Promise &&
                registeredAsync.value === true &&
                registeredAsync.writable === true &&
                registeredAsync.enumerable === true &&
                registeredAsync.configurable === true &&
                registeredValue.value === registeredPromise &&
                registeredValue.writable === true &&
                registeredValue.enumerable === true &&
                registeredValue.configurable === true &&
                (!stress || tail.seed === seed);

            const notified = Atomics.notify(view, 0, 1);

            return [
                notEqualOk,
                zeroTimeoutOk,
                finiteShapeOk,
                registeredShapeOk,
                notified,
                finitePromise,
                registeredPromise
            ];
        };

        globalThis.__waitAsyncCompleted = 0;
        for (let i = 0; i < 64; i++) {
            const warm = __waitAsyncResultProbe(i, false);
            await Promise.all([warm[5], warm[6]]);
            __waitAsyncCompleted = i + 1;
        }
        "#,
        r#"
        const result = __waitAsyncResultProbe(73, true);
        const labels = await Promise.all([result[5], result[6]]);
        globalThis.__waitAsyncCompletion = [
            result[0],
            result[1],
            result[2],
            result[3],
            result[4],
            labels.join(",")
        ].join("|");
        "#,
        "<gc-atomics-wait-async-result>",
        "true|true|true|true|1|timed-out,ok",
    );
}
