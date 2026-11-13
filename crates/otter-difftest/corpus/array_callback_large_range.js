"use strict";
// Large callback walks retain ordered HasProperty/Get, never an index snapshot.
function checkCallbackRange(condition, message) {
    if (!condition) throw new Error(message);
}
const callbackRangeMethods = ["some", "every", "forEach", "map", "filter", "reduce", "reduceRight", "flatMap"];
for (const length of [2 ** 26, Number.MAX_SAFE_INTEGER]) {
    for (const name of callbackRangeMethods) {
        const reverse = name === "reduceRight";
        const reduce = name === "reduce" || reverse;
        const direction = reverse ? -1 : 1;
        const hole = reverse ? length - 1 : 0;
        const first = hole + direction;
        const gap = first + direction;
        const next = gap + direction;
        const stop = next + direction;
        const marker = {}, value = {text: "\ud800walk\udc00"}, thisArg = {};
        const events = [], values = Object.create(null), writes = [];
        let calls = 0, liveLength = length, caught, answer;
        values[first] = value;
        values[gap] = value;
        const output = new Proxy({}, {
            defineProperty(target, key, descriptor) {
                events.push("define:" + key);
                checkCallbackRange(descriptor.value === value, "live species result value");
                writes.push(key);
                return Reflect.defineProperty(target, key, descriptor);
            }
        });
        const constructor = {
            get [Symbol.species]() {
                events.push("species");
                return function(count) { events.push("create:" + count); return output; };
            }
        };
        const receiver = new Proxy([], {
            get(target, key) {
                if (key === "length") { events.push("length"); return liveLength; }
                if (key === "constructor") { events.push("constructor"); return constructor; }
                events.push("get:" + key);
                return values[key];
            },
            has(target, key) {
                events.push("has:" + key);
                if (key === String(stop)) throw marker;
                return key in values;
            },
            set(target, key, item) {
                if (key === "length") liveLength = item;
                else values[key] = item;
                return true;
            },
            deleteProperty(target, key) { return delete values[key]; },
            ownKeys() { throw new Error("unexpected enumeration"); },
            getPrototypeOf() { throw new Error("unexpected preliminary prototype walk"); }
        });
        const callback = function(...args) {
            const item = args[reduce ? 1 : 0], index = args[reduce ? 2 : 1];
            checkCallbackRange(this === (reduce ? undefined : thisArg), "callback this");
            checkCallbackRange(args.length === (reduce ? 4 : 3) && args[reduce ? 3 : 2] === receiver,
                "callback arguments and original receiver");
            calls++;
            events.push("callback:" + index);
            checkCallbackRange(calls <= 2 && index === (calls === 1 ? first : next) && item === value,
                "holes skipped and live additions read in order");
            if (reduce) checkCallbackRange(args[0] === (calls === 1 ? 5 : 7), "live accumulator");
            if (calls === 1) {
                receiver.length = 0;
                delete receiver[gap];
                receiver[next] = value;
                for (let n = 0; n < 16; n++) globalThis.callbackRangeChurn = {n, child: {n}};
            }
            if (name === "some") return calls === 2;
            if (name === "every") return calls === 1;
            if (name === "filter") return true;
            return reduce ? calls + 6 : value;
        };
        try { answer = Array.prototype[name].call(receiver, callback, reduce ? 5 : thisArg); }
        catch (error) { caught = error; }
        const early = name === "some" || name === "every";
        checkCallbackRange(early ? caught === undefined && answer === (name === "some") : caught === marker,
            "exact early or abrupt completion: " + name);
        const expected = ["length"];
        if (name === "map" || name === "filter" || name === "flatMap")
            expected.push("constructor", "species", "create:" + (name === "map" ? length : 0));
        expected.push("has:" + hole, "has:" + first, "get:" + first, "callback:" + first);
        if (name === "map" || name === "filter" || name === "flatMap") expected.push("define:" + (name === "map" ? first : 0));
        expected.push("has:" + gap, "has:" + next, "get:" + next, "callback:" + next);
        if (name === "map" || name === "filter" || name === "flatMap") expected.push("define:" + (name === "map" ? next : 1));
        if (!early) expected.push("has:" + stop);
        checkCallbackRange(events.join("|") === expected.join("|"), "exact observable order: " + name);
        checkCallbackRange(calls === 2 && value.text.charCodeAt(0) === 0xd800
            && value.text.charCodeAt(5) === 0xdc00, "callback count and UTF-16 payload");
        checkCallbackRange(writes.length === (name === "map" || name === "filter" || name === "flatMap" ? 2 : 0),
            "species effects occur exactly once per present element");
    }
}
// Ordinary objects must observe additions beyond the initially present set too.
for (const name of ["some", "every"]) {
    const receiver = {length: Number.MAX_SAFE_INTEGER, 0: 7};
    Object.defineProperty(receiver, "2", {get() { throw new Error("ordinary range safety sentinel"); }});
    let calls = 0;
    const answer = Array.prototype[name].call(receiver, value => {
        if (++calls === 1) receiver[1] = 9;
        return name === "some" ? value === 9 : value === 7;
    });
    checkCallbackRange(calls === 2 && answer === (name === "some"), "ordinary live addition");
}
for (const name of callbackRangeMethods) {
    const marker = {};
    let caught;
    try { Array.prototype[name].call({get length() { throw marker; }}, null); }
    catch (error) { caught = error; }
    checkCallbackRange(caught === marker, "length error precedes callback validation");
}
console.log("ok:callback-large-range");
"ok:callback-large-range";
