//! Moving-GC invariants for property-descriptor result builders.
//!
//! # Contents
//! - Partial descriptors passed to a Proxy `defineProperty` trap.
//! - `Reflect.getOwnPropertyDescriptor` data and accessor results.
//! - Native-function partial updates and built-in metadata descriptors.
//! - Proxy observability and exact abrupt propagation during descriptor reads.
//!
//! # Invariants
//! - Proxy trap descriptor objects preserve exactly the optional fields and
//!   values supplied by the caller across moving collections.
//! - `FromPropertyDescriptor` parks data values and accessor functions before
//!   allocating or publishing the result object.
//! - Native functions stay rooted while materializing `name`, and kind changes
//!   inherit omitted enumerable/configurable attributes.
//! - Key coercion precedes the Proxy `getOwnPropertyDescriptor` trap, while an
//!   invalid target prevents coercion and Proxy abrupt values escape unchanged.

use otter_runtime::{JitSelection, Runtime, SourceInput};

struct RunResult {
    completion: String,
    compile_attempts: u64,
}

fn run(selection: JitSelection, source: &str, name: &str) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("property descriptor fixture")
        .completion_string()
        .to_owned()
}

fn run_warmed_probe(
    selection: JitSelection,
    warm_source: &str,
    final_source: &str,
    name: &str,
) -> RunResult {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let warm_name = format!("{name}-warm");
    runtime
        .run_script(
            SourceInput::from_javascript(warm_source.to_string()),
            &warm_name,
        )
        .expect("property descriptor warmup");
    let warm_stats = runtime.execution_stats();
    let final_name = format!("{name}-final");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(final_source.to_string()),
            &final_name,
        )
        .expect("property descriptor final probe")
        .completion_string()
        .to_owned();
    RunResult {
        completion,
        compile_attempts: warm_stats.jit_compile_attempts,
    }
}

fn assert_interpreter_and_template(source: &str, name: &str, expected: &str) {
    let interpreter = run(JitSelection::InterpreterOnly, source, name);
    let template = run(JitSelection::Template, source, name);
    assert_eq!(template, interpreter, "{name}: tier mismatch");
    assert_eq!(template, expected, "{name}: unexpected completion");
}

fn assert_warmed_interpreter_and_template(
    warm_source: &str,
    final_source: &str,
    name: &str,
    expected: &str,
) {
    let interpreter = run_warmed_probe(
        JitSelection::InterpreterOnly,
        warm_source,
        final_source,
        name,
    );
    let template = run_warmed_probe(JitSelection::Template, warm_source, final_source, name);
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
fn proxy_partial_descriptor_survives_each_field_write() {
    assert_interpreter_and_template(
        r#"
        function probe(seed) {
            let observed;
            const target = {};
            const proxy = new Proxy(target, {
                defineProperty(inner, key, descriptor) {
                    observed = descriptor;
                    return Reflect.defineProperty(inner, key, descriptor);
                }
            });
            const payload = { marker: seed };
            const success = Reflect.defineProperty(proxy, "value", {
                value: payload,
                writable: true,
                configurable: true
            });
            return success === true &&
                observed.value === payload &&
                observed.writable === true &&
                observed.configurable === true &&
                !("enumerable" in observed);
        }

        for (let i = 0; i < 64; i++) probe(i);
        probe(73);
        "#,
        "<gc-partial-property-descriptor>",
        "true",
    );
}

#[test]
fn reflect_descriptor_results_keep_data_and_accessor_slots_rooted() {
    assert_warmed_interpreter_and_template(
        r#"
        const dataValue = { marker: "data-value" };
        const target = {};
        Object.defineProperty(target, "data", {
            value: dataValue,
            writable: true,
            enumerable: false,
            configurable: true
        });
        function getter() {
            return dataValue;
        }
        function setter(value) {
            this.seen = value;
        }
        Object.defineProperty(target, "accessor", {
            get: getter,
            set: setter,
            enumerable: true,
            configurable: false
        });

        globalThis.__reflectDescriptorProbe = function probe(seed) {
            const data = Reflect.getOwnPropertyDescriptor(target, "data");
            const accessor = Reflect.getOwnPropertyDescriptor(target, "accessor");
            let allocated = null;
            for (let i = 0; i < 24; i++) {
                allocated = {
                    seed,
                    i,
                    text: "descriptor-" + seed + "-" + i,
                    tail: allocated
                };
            }

            return data.value === dataValue &&
                data.writable === true &&
                data.enumerable === false &&
                data.configurable === true &&
                Reflect.ownKeys(data).join(",") ===
                    "value,writable,enumerable,configurable" &&
                accessor.get === getter &&
                accessor.set === setter &&
                accessor.enumerable === true &&
                accessor.configurable === false &&
                Reflect.ownKeys(accessor).join(",") ===
                    "get,set,enumerable,configurable" &&
                allocated.seed === seed;
        };

        for (let i = 0; i < 64; i++) __reflectDescriptorProbe(i);
        "#,
        r#"
        __reflectDescriptorProbe(73);
        "#,
        "<gc-reflect-property-descriptor-results>",
        "true",
    );
}

#[test]
fn native_function_partial_descriptors_preserve_target_and_omitted_attributes() {
    assert_interpreter_and_template(
        r#"
        const holder = Iterator;
        const original = Object.getOwnPropertyDescriptor(holder, "from");
        const builtin = original.value;
        const originalName = Object.getOwnPropertyDescriptor(builtin, "name");

        function probe(seed) {
            const getter = function () {
                return builtin;
            };
            Object.defineProperty(holder, "from", { get: getter });
            const accessor = Object.getOwnPropertyDescriptor(holder, "from");

            Object.defineProperty(holder, "from", { get: undefined });
            const emptyAccessor =
                Object.getOwnPropertyDescriptor(holder, "from");

            Object.defineProperty(holder, "from", {
                value: builtin,
                writable: true
            });
            const data = Object.getOwnPropertyDescriptor(holder, "from");

            const renamedValue = "from-" + seed;
            Object.defineProperty(builtin, "name", { value: renamedValue });
            const renamed = Object.getOwnPropertyDescriptor(builtin, "name");

            Object.defineProperty(builtin, "name", originalName);
            Object.defineProperty(holder, "from", original);

            return accessor.get === getter &&
                accessor.set === undefined &&
                accessor.enumerable === original.enumerable &&
                accessor.configurable === original.configurable &&
                emptyAccessor.get === undefined &&
                emptyAccessor.set === undefined &&
                emptyAccessor.enumerable === original.enumerable &&
                emptyAccessor.configurable === original.configurable &&
                data.value === builtin &&
                data.writable === true &&
                data.enumerable === original.enumerable &&
                data.configurable === original.configurable &&
                renamed.value === renamedValue &&
                renamed.writable === originalName.writable &&
                renamed.enumerable === originalName.enumerable &&
                renamed.configurable === originalName.configurable;
        }

        for (let i = 0; i < 64; i++) probe(i);
        probe(73);
        "#,
        "<gc-native-function-partial-descriptors>",
        "true",
    );
}

#[test]
fn reflect_descriptor_proxy_order_and_abrupt_identity_are_preserved() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 24; i++) {
                tail = { seed, i, text: "proxy-" + seed + "-" + i, tail };
            }
            return tail;
        }

        function probe(seed) {
            const events = [];
            const payload = { marker: "proxy-payload-" + seed };
            const target = {};
            Object.defineProperty(target, "slot", {
                value: payload,
                writable: false,
                enumerable: true,
                configurable: true
            });
            const proxy = new Proxy(target, {
                getOwnPropertyDescriptor(inner, key) {
                    events.push("trap:" + key);
                    churn(seed + 1);
                    return Reflect.getOwnPropertyDescriptor(inner, key);
                }
            });
            const key = {
                toString() {
                    events.push("key");
                    churn(seed + 2);
                    return "slot";
                }
            };
            const descriptor = Reflect.getOwnPropertyDescriptor(proxy, key);

            let invalidKeyCoercions = 0;
            let invalidTargetThrew = false;
            try {
                Reflect.getOwnPropertyDescriptor(1, {
                    toString() {
                        invalidKeyCoercions++;
                        return "ignored";
                    }
                });
            } catch (error) {
                invalidTargetThrew = error instanceof TypeError;
            }

            const abrupt = { marker: "exact-abrupt-" + seed };
            let caught;
            const throwing = new Proxy(target, {
                getOwnPropertyDescriptor() {
                    events.push("throw-trap");
                    churn(seed + 3);
                    throw abrupt;
                }
            });
            try {
                Reflect.getOwnPropertyDescriptor(throwing, {
                    toString() {
                        events.push("throw-key");
                        churn(seed + 4);
                        return "slot";
                    }
                });
            } catch (error) {
                caught = error;
            }

            return descriptor.value === payload &&
                descriptor.writable === false &&
                descriptor.enumerable === true &&
                descriptor.configurable === true &&
                invalidTargetThrew &&
                invalidKeyCoercions === 0 &&
                caught === abrupt &&
                events.join(",") === "key,trap:slot,throw-key,throw-trap";
        }

        for (let i = 0; i < 64; i++) probe(i);
        probe(73);
        "#,
        "<gc-reflect-property-descriptor-proxy>",
        "true",
    );
}
