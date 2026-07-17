//! Moving-GC and iterator-order invariants for static Object result builders.
//!
//! # Contents
//! - `Object.fromEntries` entry access, key coercion, descriptors, and symbols.
//! - Streaming `Object.groupBy` iteration interleaved with callbacks.
//! - IteratorClose error identity and post-abrupt runtime reuse.
//!
//! # Invariants
//! - Result receivers, iterator records, entries/items, keys, and values remain
//!   rooted across every allocating accessor, callback, and coercion.
//! - Static result builders use CreateDataProperty semantics; inherited setters
//!   and the legacy `__proto__` setter cannot intercept writes.
//! - A successful iterator step is followed immediately by its callback work;
//!   no eager value snapshot changes observable order.
//! - Abrupt work after a successful step closes once and preserves the original
//!   thrown value even when `return` allocates and throws another value.

use otter_runtime::{JitSelection, Runtime, SourceInput};

fn run(selection: JitSelection, source: &str, name: &str) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("static Object builder fixture")
        .completion_string()
        .to_owned()
}

fn assert_interpreter_and_template(source: &str, name: &str, expected: &str) {
    let interpreter = run(JitSelection::InterpreterOnly, source, name);
    let template = run(JitSelection::Template, source, name);
    assert_eq!(template, interpreter, "{name}: tier mismatch");
    assert_eq!(template, expected, "{name}: unexpected completion");
}

#[test]
fn from_entries_roots_entry_key_value_and_result_builder() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 20; i++) {
                tail = { seed, i, text: "from-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const symbol = Symbol("entry");
        const values = [
            { id: "shadow" },
            { id: "other" },
            { id: "proto-data" },
            { id: "symbol" }
        ];
        const names = ["shadow", "other", "__proto__", symbol];
        const events = [];
        const keys = names.map(function(name, index) {
            const key = {};
            key[Symbol.toPrimitive] = function() {
                events.push("key" + index);
                churn(300 + index);
                return name;
            };
            return key;
        });

        let nextIndex = 0;
        let closed = 0;
        const iterable = {
            [Symbol.iterator]: function() {
                const iterator = {
                    next: function() {
                        const index = nextIndex++;
                        events.push("next" + index);
                        churn(index);
                        if (index >= values.length) return { done: true };
                        const entry = {};
                        Object.defineProperty(entry, "0", {
                            configurable: true,
                            get: function() {
                                events.push("get0-" + index);
                                churn(100 + index);
                                return keys[index];
                            }
                        });
                        Object.defineProperty(entry, "1", {
                            configurable: true,
                            get: function() {
                                events.push("get1-" + index);
                                churn(200 + index);
                                return values[index];
                            }
                        });
                        return { value: entry, done: false };
                    },
                    return: function() {
                        closed++;
                        return {};
                    }
                };
                return iterator;
            }
        };

        let setterCalls = 0;
        Object.defineProperty(Object.prototype, "shadow", {
            configurable: true,
            set: function() { setterCalls++; }
        });
        const result = Object.fromEntries(iterable);
        delete Object.prototype.shadow;

        const shadow = Object.getOwnPropertyDescriptor(result, "shadow");
        const protoData = Object.getOwnPropertyDescriptor(result, "__proto__");
        const symbolData = Object.getOwnPropertyDescriptor(result, symbol);
        events.join(",") ===
            "next0,get0-0,get1-0,key0,next1,get0-1,get1-1,key1," +
            "next2,get0-2,get1-2,key2,next3,get0-3,get1-3,key3,next4" &&
            closed === 0 &&
            setterCalls === 0 &&
            Object.getPrototypeOf(result) === Object.prototype &&
            result.shadow === values[0] &&
            result.other === values[1] &&
            result.__proto__ === values[2] &&
            result[symbol] === values[3] &&
            shadow.value === values[0] &&
            shadow.writable === true &&
            shadow.enumerable === true &&
            shadow.configurable === true &&
            protoData.value === values[2] &&
            symbolData.value === values[3];
        "#,
        "<gc-object-from-entries-builder>",
        "true",
    );
}

#[test]
fn object_group_by_streams_iterator_and_roots_groups() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 20; i++) {
                tail = { seed, i, text: "group-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const symbol = Symbol("odd");
        const source = [{ id: 0 }, { id: 1 }, { id: 2 }];
        const events = [];
        let nextIndex = 0;
        source[Symbol.iterator] = function() {
            return {
                next: function() {
                    const index = nextIndex++;
                    events.push("next" + index);
                    churn(index);
                    if (index >= source.length) return { done: true };
                    return { value: source[index], done: false };
                }
            };
        };

        const grouped = Object.groupBy(source, function(item, index) {
            events.push("cb" + index);
            if (item !== source[index]) throw new Error("item identity");
            churn(100 + index);
            const key = {};
            key[Symbol.toPrimitive] = function() {
                events.push("key" + index);
                churn(200 + index);
                return index === 1 ? symbol : "even";
            };
            return key;
        });

        const even = Object.getOwnPropertyDescriptor(grouped, "even");
        const odd = Object.getOwnPropertyDescriptor(grouped, symbol);
        events.join(",") ===
            "next0,cb0,key0,next1,cb1,key1,next2,cb2,key2,next3" &&
            Object.getPrototypeOf(grouped) === null &&
            Object.keys(grouped).join(",") === "even" &&
            grouped.even.length === 2 &&
            grouped.even[0] === source[0] &&
            grouped.even[1] === source[2] &&
            grouped[symbol].length === 1 &&
            grouped[symbol][0] === source[1] &&
            even.value === grouped.even &&
            even.writable === true &&
            even.enumerable === true &&
            even.configurable === true &&
            odd.value === grouped[symbol] &&
            odd.writable === true &&
            odd.enumerable === true &&
            odd.configurable === true;
        "#,
        "<gc-object-group-by-builder>",
        "true",
    );
}

#[test]
fn abrupt_static_object_iteration_preserves_throw_and_reuses_turn() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 20; i++) {
                tail = { seed, i, text: "close-" + seed + "-" + i, tail };
            }
            return tail;
        }

        function throwingCloseIterable(step, counters, replacement) {
            return {
                [Symbol.iterator]: function() {
                    const iterator = { next: step };
                    Object.defineProperty(iterator, "return", {
                        configurable: true,
                        get: function() {
                            counters.returnGets = (counters.returnGets || 0) + 1;
                            counters.receiverOk =
                                counters.receiverOk !== false && this === iterator;
                            churn(800 + counters.returnGets);
                            return function() {
                                counters.closed++;
                                counters.receiverOk =
                                    counters.receiverOk !== false && this === iterator;
                                churn(900 + counters.closed);
                                throw replacement;
                            };
                        }
                    });
                    return iterator;
                }
            };
        }

        const replacement = { kind: "replacement" };
        const entryThrow = { kind: "entry" };
        const entryCounters = { closed: 0 };
        const entryIterable = throwingCloseIterable(function() {
            const entry = {};
            Object.defineProperty(entry, "0", {
                get: function() {
                    churn(1);
                    throw entryThrow;
                }
            });
            return { value: entry, done: false };
        }, entryCounters, replacement);
        let entryCaught = false;
        try {
            Object.fromEntries(entryIterable);
        } catch (error) {
            entryCaught = error === entryThrow;
        }

        const nextThrow = { kind: "next" };
        const nextCounters = { closed: 0 };
        const nextIterable = throwingCloseIterable(function() {
            churn(2);
            throw nextThrow;
        }, nextCounters, replacement);
        let nextCaught = false;
        try {
            Object.fromEntries(nextIterable);
        } catch (error) {
            nextCaught = error === nextThrow;
        }

        const badEntryCounters = { closed: 0 };
        let badEntryDone = false;
        const badEntryIterable = throwingCloseIterable(function() {
            if (badEntryDone) return { done: true };
            badEntryDone = true;
            return { value: 73, done: false };
        }, badEntryCounters, replacement);
        let badEntryMessage = false;
        try {
            Object.fromEntries(badEntryIterable);
        } catch (error) {
            badEntryMessage =
                error instanceof TypeError &&
                error.message ===
                    "fromEntries: Object.fromEntries: iterator value is not an entry object";
        }

        const callbackThrow = { kind: "callback" };
        const callbackCounters = { closed: 0 };
        let callbackDone = false;
        const callbackIterable = throwingCloseIterable(function() {
            if (callbackDone) return { done: true };
            callbackDone = true;
            return { value: { id: 3 }, done: false };
        }, callbackCounters, replacement);
        let callbackCaught = false;
        try {
            Object.groupBy(callbackIterable, function() {
                churn(3);
                throw callbackThrow;
            });
        } catch (error) {
            callbackCaught = error === callbackThrow;
        }

        const keyThrow = { kind: "key" };
        const keyCounters = { closed: 0 };
        let keyDone = false;
        const keyIterable = throwingCloseIterable(function() {
            if (keyDone) return { done: true };
            keyDone = true;
            return { value: { id: 4 }, done: false };
        }, keyCounters, replacement);
        let keyCaught = false;
        try {
            Object.groupBy(keyIterable, function() {
                const key = {};
                key[Symbol.toPrimitive] = function() {
                    churn(4);
                    throw keyThrow;
                };
                return key;
            });
        } catch (error) {
            keyCaught = error === keyThrow;
        }

        const groupNextThrow = { kind: "group-next" };
        const groupNextCounters = { closed: 0 };
        const groupNextIterable = throwingCloseIterable(function() {
            churn(5);
            throw groupNextThrow;
        }, groupNextCounters, replacement);
        let groupNextCaught = false;
        try {
            Object.groupBy(groupNextIterable, function() { return "unused"; });
        } catch (error) {
            groupNextCaught = error === groupNextThrow;
        }

        const recoveredEntries = Object.fromEntries([["ok", 7]]);
        const recoveredGroups = Object.groupBy([1, 2, 3], function(value) {
            return value % 2 ? "odd" : "even";
        });

        entryCaught &&
            entryCounters.closed === 1 &&
            entryCounters.returnGets === 1 &&
            entryCounters.receiverOk === true &&
            nextCaught &&
            nextCounters.closed === 0 &&
            (nextCounters.returnGets || 0) === 0 &&
            badEntryMessage &&
            badEntryCounters.closed === 1 &&
            badEntryCounters.returnGets === 1 &&
            badEntryCounters.receiverOk === true &&
            callbackCaught &&
            callbackCounters.closed === 1 &&
            callbackCounters.returnGets === 1 &&
            callbackCounters.receiverOk === true &&
            keyCaught &&
            keyCounters.closed === 1 &&
            keyCounters.returnGets === 1 &&
            keyCounters.receiverOk === true &&
            groupNextCaught &&
            groupNextCounters.closed === 0 &&
            (groupNextCounters.returnGets || 0) === 0 &&
            recoveredEntries.ok === 7 &&
            recoveredGroups.odd.join(",") === "1,3" &&
            recoveredGroups.even.join(",") === "2";
        "#,
        "<gc-static-object-builder-abrupt>",
        "true",
    );
}
