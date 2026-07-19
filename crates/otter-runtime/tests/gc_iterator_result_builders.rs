//! Moving-GC invariants for iterator-result construction.
//!
//! # Contents
//! - Built-in and RegExp string iterator `{ value, done }` records.
//! - The owned fallback record for a wrapped iterator without `return`.
//! - Verbatim wrapped-iterator `next` / `return` results and abrupt values.
//!
//! # Invariants
//! - VM-owned iterator results keep their receiver and `value` rooted while
//!   allocating the result object and both property shapes.
//! - CreateIteratorResultObject produces an ordinary object with enumerable,
//!   writable, configurable `value` and `done` properties in that order.
//! - User-returned results and throws bypass the owned-result builder exactly.

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
        .expect("iterator-result fixture")
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
        .expect("iterator-result warmup");
    let warm_stats = runtime.execution_stats();
    let final_name = format!("{name}-final");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(final_source.to_string()),
            &final_name,
        )
        .expect("iterator-result final probe")
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
fn owned_next_results_keep_value_and_shape_rooted() {
    assert_interpreter_and_template(
        r#"
        function probe(seed) {
            const payload = { marker: seed };
            const arrayIterator = [payload].values();
            const first = arrayIterator.next();
            const completed = arrayIterator.next();

            const regexpIterator = "a".matchAll(/./g);
            const regexpFirst = regexpIterator.next();
            const regexpValue = regexpFirst.value;
            const regexpCompleted = regexpIterator.next();

            let tail = null;
            for (let i = 0; i < 96; i++) {
                tail = {
                    seed,
                    i,
                    text: "iterator-result-" + seed + "-" + i,
                    tail
                };
            }

            const results = [
                [first, payload, false],
                [completed, undefined, true],
                [regexpFirst, regexpValue, false],
                [regexpCompleted, undefined, true]
            ];
            let valid = regexpValue[0] === "a" && tail.seed === seed;
            for (let i = 0; i < results.length; i++) {
                const result = results[i][0];
                const expectedValue = results[i][1];
                const expectedDone = results[i][2];
                const value = Object.getOwnPropertyDescriptor(result, "value");
                const done = Object.getOwnPropertyDescriptor(result, "done");
                valid = valid &&
                    Object.getPrototypeOf(result) === Object.prototype &&
                    Reflect.ownKeys(result).join(",") === "value,done" &&
                    result.value === expectedValue &&
                    result.done === expectedDone &&
                    value.value === expectedValue &&
                    value.writable === true &&
                    value.enumerable === true &&
                    value.configurable === true &&
                    done.value === expectedDone &&
                    done.writable === true &&
                    done.enumerable === true &&
                    done.configurable === true;
            }
            return valid;
        }

        for (let i = 0; i < 64; i++) probe(i);
        probe(73);
        "#,
        "<gc-owned-iterator-results>",
        "true",
    );
}

#[test]
fn missing_wrapped_return_builds_one_rooted_owned_result() {
    assert_warmed_interpreter_and_template(
        r#"
        const wrappers = [];
        for (let i = 0; i < 65; i++) {
            wrappers.push(Iterator.from({
                next() {
                    return { value: 1, done: false };
                },
                return: null
            }));
        }

        globalThis.__iteratorReturnProbe = function probe(seed) {
            const wrapper = wrappers[seed];
            const ignored = { marker: "ignored-" + seed };
            const result = wrapper.return(ignored);
            let tail = null;
            for (let i = 0; i < 24; i++) {
                tail = {
                    seed,
                    i,
                    text: "iterator-return-" + seed + "-" + i,
                    tail
                };
            }
            const value = Object.getOwnPropertyDescriptor(result, "value");
            const done = Object.getOwnPropertyDescriptor(result, "done");

            return tail.seed === seed &&
                Object.getPrototypeOf(result) === Object.prototype &&
                Reflect.ownKeys(result).join(",") === "value,done" &&
                result.value === undefined &&
                result.done === true &&
                value.value === undefined &&
                value.writable === true &&
                value.enumerable === true &&
                value.configurable === true &&
                done.value === true &&
                done.writable === true &&
                done.enumerable === true &&
                done.configurable === true;
        };

        for (let i = 0; i < 64; i++) __iteratorReturnProbe(i);
        "#,
        r#"
        __iteratorReturnProbe(64);
        "#,
        "<gc-owned-iterator-return-result>",
        "true",
    );
}

#[test]
fn wrapped_results_and_abrupt_values_remain_verbatim() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 24; i++) {
                tail = { seed, i, text: "iterator-user-" + seed + "-" + i, tail };
            }
            return tail;
        }

        function probe(seed) {
            let resultReads = 0;
            const nextResult = {};
            Object.defineProperty(nextResult, "done", {
                configurable: true,
                get() {
                    resultReads++;
                    throw new Error("wrapped next result was inspected");
                }
            });
            const returnResult = { marker: "return-result-" + seed };
            const iterator = {
                next() {
                    churn(seed + 20);
                    return nextResult;
                },
                return() {
                    churn(seed + 21);
                    return returnResult;
                }
            };
            const wrapper = Iterator.from(iterator);
            const observedNext = wrapper.next();
            const observedReturn = wrapper.return();

            const thrown = { marker: "exact-throw-" + seed };
            const throwing = Iterator.from({
                next() {
                    churn(seed + 22);
                    throw thrown;
                }
            });
            let observedThrow;
            try {
                throwing.next();
            } catch (error) {
                observedThrow = error;
            }
            churn(seed + 23);

            return observedNext === nextResult &&
                observedReturn === returnResult &&
                resultReads === 0 &&
                observedThrow === thrown;
        }

        for (let i = 0; i < 64; i++) probe(i);
        probe(73);
        "#,
        "<gc-verbatim-iterator-results>",
        "true",
    );
}

#[test]
fn iterator_from_has_configurable_builtin_descriptor() {
    assert_interpreter_and_template(
        r#"
        function probe(seed) {
            const descriptor = Object.getOwnPropertyDescriptor(Iterator, "from");
            const original = Iterator.from;
            const holder = Iterator;
            const key = "from";
            const replacement = "replacement-" + seed;
            holder[key] = replacement;
            const replaced = Object.getOwnPropertyDescriptor(Iterator, "from");
            holder[key] = original;
            const restored = Object.getOwnPropertyDescriptor(Iterator, "from");
            const deleted = delete holder[key];
            const missing = !Object.prototype.hasOwnProperty.call(Iterator, "from");

            const result = [
                descriptor.value === original,
                descriptor.writable,
                descriptor.enumerable,
                descriptor.configurable,
                replaced.value === replacement,
                replaced.enumerable,
                replaced.configurable,
                restored.value === original,
                restored.enumerable,
                restored.configurable,
                deleted,
                missing
            ].join("|");
            Object.defineProperty(holder, key, descriptor);
            return result;
        }

        for (let i = 0; i < 64; i++) probe(i);
        probe(73);
        "#,
        "<iterator-from-property-descriptor>",
        "true|true|false|true|true|false|true|true|false|true|true|true",
    );
}
