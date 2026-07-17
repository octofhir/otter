//! Moving-GC regression coverage for observable RegExp protocols.
//!
//! # Contents
//! - A custom species matcher whose `exec`, match-text getter, coercion, and
//!   `lastIndex` accessors allocate while `%RegExpStringIterator%.next` runs.
//! - A functional `@@replace` callback whose arguments and coerced result must
//!   remain live across nested allocations.
//! - String search replacement and fallback `@@match` / `@@matchAll` /
//!   `@@search` dispatch on the native call's shared runtime turn.
//! - A custom `@@split` species whose constructor, flags, `exec`, and
//!   `lastIndex` accessors all re-enter JavaScript.
//! - Proxy-backed `flags`, `exec`, `lastIndex`, and `Symbol.species` ladders,
//!   plus object-to-primitive coercions used by builtin exec and construction.
//! - Constructor, legacy `compile`, and generic `toString` inputs whose
//!   coercion hooks allocate between observable steps.
//!
//! # Invariants
//! - Receivers, input strings, match results, captures, callback arguments, and
//!   intermediate property values remain rooted across every observable
//!   re-entry.
//! - Empty-match Unicode advancement reads the relocated input and updates the
//!   custom matcher's `lastIndex` on the current runtime turn.
//! - Species construction and replacement callbacks reuse the active runtime
//!   turn instead of publishing detached activation stacks.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str, name: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("script")
        .completion_string()
        .to_owned()
}

#[test]
fn custom_match_all_protocol_survives_moving_gc() {
    let completion = run(
        r#"
                const input = "𝌆tail";
                let calls = 0;
                const writes = [];

                function churn(seed) {
                    let tail = null;
                    for (let i = 0; i < 12; i++) {
                        tail = { seed, i, text: "regexp-" + seed + "-" + i, tail };
                    }
                    return tail;
                }

                const matcher = {
                    _lastIndex: 0,
                    get lastIndex() {
                        const held = this;
                        const allocated = churn(20 + calls);
                        if (held !== matcher || allocated.seed !== 20 + calls) throw new Error("getter roots");
                        return held._lastIndex;
                    },
                    set lastIndex(value) {
                        const held = this;
                        const allocated = churn(40 + calls);
                        if (held !== matcher || allocated.seed !== 40 + calls) throw new Error("setter roots");
                        held._lastIndex = Number(value);
                        writes.push(held._lastIndex);
                    },
                    exec(text) {
                        const heldMatcher = this;
                        const heldInput = text;
                        const allocated = churn(60 + calls);
                        if (heldMatcher !== matcher || heldInput !== input || allocated.seed !== 60 + calls) {
                            throw new Error("exec roots");
                        }
                        if (calls++ !== 0) return null;
                        heldMatcher._lastIndex = 0;
                        const marker = { id: 73 };
                        return {
                            marker,
                            get 0() {
                                const heldMarker = marker;
                                churn(80);
                                return {
                                    toString() {
                                        churn(90);
                                        if (heldMarker.id !== 73) throw new Error("result roots");
                                        return "";
                                    }
                                };
                            }
                        };
                    }
                };

                function MatcherSpecies() { return matcher; }
                const constructorTarget = {};
                const constructor = new Proxy(constructorTarget, {
                    get(target, key) {
                        const allocated = churn(110);
                        if (allocated.seed !== 110) throw new Error("species proxy roots");
                        if (key === Symbol.species) return MatcherSpecies;
                        return target[key];
                    }
                });
                const receiverTarget = {
                    flags: "gu",
                    lastIndex: 0,
                    constructor
                };
                const receiver = new Proxy(receiverTarget, {
                    get(target, key) {
                        churn(120);
                        return target[key];
                    }
                });
                const iterator = RegExp.prototype[Symbol.matchAll].call(receiver, input);
                const first = iterator.next();
                const second = iterator.next();
                [
                    first.done,
                    first.value.marker.id,
                    second.done,
                    calls,
                    matcher._lastIndex,
                    writes.join(",")
                ].join("|");
                "#,
        "<gc-regexp-string-iterator>",
    );

    assert_eq!(completion, "false|73|true|2|2|0,2");
}

#[test]
fn functional_replace_callback_survives_moving_gc() {
    let completion = run(
        r#"
        const input = "ab";
        const re = /([a-z])/g;
        let calls = 0;

        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 12; i++) tail = { seed, i, tail };
            return tail;
        }

        function replacer(matched, capture, position, whole) {
            const heldMatch = matched;
            const heldCapture = capture;
            const heldWhole = whole;
            const heldPosition = position;
            const allocated = churn(100 + calls);
            if (
                heldMatch !== heldCapture ||
                heldWhole !== input ||
                allocated.seed !== 100 + calls
            ) throw new Error("replace callback roots");
            calls++;
            return {
                toString() {
                    churn(120 + heldPosition);
                    if (heldWhole !== input || heldMatch !== heldCapture) {
                        throw new Error("replace result roots");
                    }
                    return heldCapture.toUpperCase();
                }
            };
        }

        const output = RegExp.prototype[Symbol.replace].call(re, input, replacer);
        [output, calls, re.lastIndex].join("|");
        "#,
        "<gc-regexp-replace>",
    );

    assert_eq!(completion, "AB|2|0");
}

#[test]
fn string_protocol_fallbacks_survive_moving_gc() {
    let completion = run(
        r#"
        let calls = 0;

        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 16; i++) {
                tail = { seed, i, text: "string-protocol-" + seed + "-" + i, tail };
            }
            return tail;
        }

        function replacement(match, position, whole) {
            const heldMatch = match;
            const heldWhole = whole;
            const allocated = churn(20 + calls);
            if (
                heldMatch !== "a" ||
                heldWhole !== "aba" ||
                allocated.seed !== 20 + calls
            ) {
                throw new Error("string replacement roots");
            }
            calls++;
            return {
                toString() {
                    const held = heldWhole;
                    churn(40 + position);
                    if (held !== "aba") throw new Error("replacement result roots");
                    return "X";
                }
            };
        }

        const search = {
            [Symbol.replace](whole, callback) {
                const heldThis = this;
                const heldWhole = whole;
                const heldCallback = callback;
                const allocated = churn(10);
                if (
                    heldThis !== search ||
                    heldWhole !== "abc" ||
                    heldCallback !== replacement ||
                    allocated.seed !== 10
                ) {
                    throw new Error("custom replace roots");
                }
                calls++;
                return "protocol";
            }
        };

        const protocol = "abc".replace(search, replacement);
        const functional = "aba".replaceAll("a", replacement);

        const originalMatch = RegExp.prototype[Symbol.match];
        const originalMatchAll = RegExp.prototype[Symbol.matchAll];
        const originalSearch = RegExp.prototype[Symbol.search];
        RegExp.prototype[Symbol.match] = function(input) {
            const held = this;
            churn(60);
            if (!(held instanceof RegExp) || input !== "aba") {
                throw new Error("match fallback roots");
            }
            calls++;
            return "match:" + input;
        };
        RegExp.prototype[Symbol.matchAll] = function(input) {
            const held = this;
            churn(70);
            if (!(held instanceof RegExp) || input !== "aba") {
                throw new Error("matchAll fallback roots");
            }
            calls++;
            return { tag: "matchAll:" + input };
        };
        RegExp.prototype[Symbol.search] = function(input) {
            const held = this;
            churn(80);
            if (!(held instanceof RegExp) || input !== "aba") {
                throw new Error("search fallback roots");
            }
            calls++;
            return 17;
        };

        const match = "aba".match("a");
        const matchAll = "aba".matchAll("a");
        const searchResult = "aba".search("a");
        RegExp.prototype[Symbol.match] = originalMatch;
        RegExp.prototype[Symbol.matchAll] = originalMatchAll;
        RegExp.prototype[Symbol.search] = originalSearch;

        [protocol, functional, match, matchAll.tag, searchResult, calls].join("|");
        "#,
        "<gc-string-protocol-fallbacks>",
    );

    assert_eq!(completion, "protocol|XbX|match:aba|matchAll:aba|17|6");
}

#[test]
fn split_species_constructor_and_accessors_survive_moving_gc() {
    let completion = run(
        r#"
        const input = "a,b";
        let calls = 0;
        const writes = [];
        let constructedFlags = "";

        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 12; i++) tail = { seed, i, tail };
            return tail;
        }

        const splitter = {
            _lastIndex: 0,
            get lastIndex() {
                const held = this;
                const allocated = churn(200 + calls);
                if (held !== splitter || allocated.seed !== 200 + calls) {
                    throw new Error("split getter roots");
                }
                return held._lastIndex;
            },
            set lastIndex(value) {
                const held = this;
                const allocated = churn(220 + calls);
                if (held !== splitter || allocated.seed !== 220 + calls) {
                    throw new Error("split setter roots");
                }
                held._lastIndex = Number(value);
                writes.push(held._lastIndex);
            },
            exec(text) {
                const held = this;
                const heldInput = text;
                const q = held._lastIndex;
                const allocated = churn(240 + calls);
                if (held !== splitter || heldInput !== input || allocated.seed !== 240 + calls) {
                    throw new Error("split exec roots");
                }
                calls++;
                if (q !== 1) return null;
                held._lastIndex = 2;
                return { 0: ",", length: 1, index: 1 };
            }
        };

        let receiver;
        function SplitSpecies(original, flags) {
            const heldOriginal = original;
            const heldFlags = flags;
            const allocated = churn(260);
            if (heldOriginal !== receiver || allocated.seed !== 260) {
                throw new Error("split constructor roots");
            }
            constructedFlags = heldFlags;
            return splitter;
        }
        const constructor = {};
        Object.defineProperty(constructor, Symbol.species, {
            get() {
                churn(280);
                return SplitSpecies;
            }
        });
        receiver = {
            constructor,
            get flags() {
                const held = this;
                churn(300);
                if (held !== receiver) throw new Error("split flags roots");
                return "u";
            }
        };

        const parts = RegExp.prototype[Symbol.split].call(receiver, input, 10);
        [parts.join(","), calls, writes.join(","), constructedFlags].join("|");
        "#,
        "<gc-regexp-split>",
    );

    assert_eq!(completion, "a,b|3|0,1,2|uy");
}

#[test]
fn builtin_exec_and_proxy_match_search_survive_moving_gc() {
    let completion = run(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 14; i++) {
                tail = { seed, i, text: "regexp-observable-" + seed + "-" + i, tail };
            }
            return tail;
        }

        // RegExpExec fallback: an own `exec = undefined` must still use the
        // builtin matcher. Both input ToString and lastIndex ToLength walk
        // accessor-backed @@toPrimitive / ordinary fallback hooks.
        const input = {
            get [Symbol.toPrimitive]() {
                churn(10);
                return undefined;
            },
            toString() {
                const held = this;
                const allocated = churn(11);
                if (held !== input || allocated.seed !== 11) throw new Error("input root");
                return "a";
            },
            valueOf() {
                throw new Error("string hint must prefer toString");
            }
        };
        const lastIndexObject = {
            get [Symbol.toPrimitive]() {
                churn(20);
                return undefined;
            },
            valueOf() {
                const held = this;
                const allocated = churn(21);
                if (held !== lastIndexObject || allocated.seed !== 21) {
                    throw new Error("lastIndex valueOf root");
                }
                return 0;
            },
            toString() {
                throw new Error("number hint must prefer valueOf");
            }
        };

        // Direct prototype exec exercises its own object-input coercion path
        // before the observable lastIndex ladder.
        const direct = /a/g;
        direct.lastIndex = lastIndexObject;
        const directMatch = RegExp.prototype.exec.call(direct, input);

        const builtin = /a/g;
        Object.defineProperty(builtin, "exec", {
            value: undefined,
            writable: true,
            configurable: true
        });
        builtin.lastIndex = lastIndexObject;
        const fallback = RegExp.prototype.test.call(builtin, input);

        // Match-result construction allocates the result, named groups,
        // indices pairs, and indices.groups in sequence. Every earlier object
        // must be re-read from its Local after each allocation.
        const indexed = /(?<letter>a)/d.exec("a");

        let matchCalls = 0;
        let matchLastIndex = 99;
        let matchProxy;
        const matchTarget = {};
        matchProxy = new Proxy(matchTarget, {
            get(target, key) {
                const held = matchProxy;
                const allocated = churn(30 + matchCalls);
                if (held !== matchProxy || allocated.seed !== 30 + matchCalls) {
                    throw new Error("match proxy get roots");
                }
                if (key === "flags") return "g";
                if (key === "lastIndex") return matchLastIndex;
                if (key === "exec") {
                    return function(text) {
                        const heldThis = this;
                        const heldText = text;
                        churn(40 + matchCalls);
                        if (heldThis !== matchProxy || heldText !== "a") {
                            throw new Error("match exec roots");
                        }
                        if (matchCalls++ !== 0) return null;
                        return {
                            get 0() {
                                churn(50);
                                return {
                                    toString() {
                                        churn(51);
                                        return "a";
                                    }
                                };
                            }
                        };
                    };
                }
                return target[key];
            },
            set(target, key, value) {
                churn(60 + matchCalls);
                if (key === "lastIndex") {
                    matchLastIndex = Number(value);
                    return true;
                }
                target[key] = value;
                return true;
            }
        });
        const matched = RegExp.prototype[Symbol.match].call(matchProxy, input);

        const previous = { id: 73 };
        let searchLastIndex = previous;
        let searchProxy;
        searchProxy = new Proxy({}, {
            get(target, key) {
                churn(70);
                if (key === "lastIndex") return searchLastIndex;
                if (key === "exec") {
                    return function(text) {
                        const heldPrevious = previous;
                        churn(71);
                        if (this !== searchProxy || text !== "a" || heldPrevious.id !== 73) {
                            throw new Error("search exec roots");
                        }
                        searchProxy.lastIndex = 7;
                        return {
                            get index() {
                                churn(72);
                                return 4;
                            }
                        };
                    };
                }
                return target[key];
            },
            set(target, key, value) {
                churn(73);
                if (key === "lastIndex") {
                    searchLastIndex = value;
                    return true;
                }
                target[key] = value;
                return true;
            }
        });
        const searched = RegExp.prototype[Symbol.search].call(searchProxy, input);

        [
            directMatch[0],
            direct.lastIndex,
            fallback,
            builtin.lastIndex,
            indexed[0],
            indexed.groups.letter,
            indexed.indices[0].join(","),
            indexed.indices.groups.letter.join(","),
            matched.join(","),
            matchCalls,
            matchLastIndex,
            searched,
            searchLastIndex === previous,
            previous.id
        ].join("|");
        "#,
        "<gc-regexp-match-search>",
    );

    assert_eq!(completion, "a|1|true|1|a|a|0,1|0,1|a|2|0|4|true|73");
}

#[test]
fn constructor_compile_and_to_string_coercions_survive_moving_gc() {
    let completion = run(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 14; i++) {
                tail = { seed, i, text: "regexp-bootstrap-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const sourceValue = {
            [Symbol.toPrimitive](hint) {
                const held = this;
                const allocated = churn(100);
                if (held !== sourceValue || hint !== "string" || allocated.seed !== 100) {
                    throw new Error("constructor source root");
                }
                return "a";
            }
        };
        const flagsValue = {
            toString() {
                const held = this;
                const allocated = churn(101);
                if (held !== flagsValue || allocated.seed !== 101) {
                    throw new Error("constructor flags root");
                }
                return "g";
            },
            valueOf() {
                throw new Error("flags string hint order");
            }
        };
        const patternTarget = {};
        const pattern = new Proxy(patternTarget, {
            get(target, key) {
                const allocated = churn(102);
                if (allocated.seed !== 102) throw new Error("constructor proxy roots");
                if (key === Symbol.match) return true;
                if (key === "source") return sourceValue;
                if (key === "flags") return flagsValue;
                return target[key];
            }
        });
        const constructed = new RegExp(pattern);

        const compilePattern = {
            get [Symbol.toPrimitive]() {
                churn(110);
                return undefined;
            },
            toString() {
                const held = this;
                churn(111);
                if (held !== compilePattern) throw new Error("compile pattern root");
                return "b";
            }
        };
        const compileFlags = {
            [Symbol.toPrimitive](hint) {
                const held = this;
                churn(112);
                if (held !== compileFlags || hint !== "string") {
                    throw new Error("compile flags root");
                }
                return "i";
            }
        };
        const compiled = /old/;
        compiled.lastIndex = { marker: 88 };
        compiled.compile(compilePattern, compileFlags);

        const sourceResult = {
            toString() {
                const held = this;
                churn(120);
                if (held !== sourceResult) throw new Error("toString source result root");
                return "x";
            }
        };
        const flagsResult = {
            get [Symbol.toPrimitive]() {
                churn(121);
                return undefined;
            },
            toString() {
                const held = this;
                churn(122);
                if (held !== flagsResult) throw new Error("toString flags result root");
                return "gi";
            }
        };
        const generic = new Proxy({}, {
            get(target, key) {
                churn(123);
                if (key === "source") return sourceResult;
                if (key === "flags") return flagsResult;
                return target[key];
            }
        });
        const rendered = RegExp.prototype.toString.call(generic);

        [
            constructed.source,
            constructed.flags,
            constructed.test("a"),
            compiled.source,
            compiled.flags,
            compiled.test("B"),
            rendered
        ].join("|");
        "#,
        "<gc-regexp-bootstrap>",
    );

    assert_eq!(completion, "a|g|true|b|i|true|/x/gi");
}
