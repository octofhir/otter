"use strict";
function checkMutate(condition, message) { if (!condition) throw new Error(message); }
function churnMutate() {
    for (let n = 0; n < 8; n++) globalThis.mutateChurn = {n, child: {n}};
}
{
    const events = [], value = {text: "\ud800fill\udc00"}, stored = {};
    const length = {valueOf() { events.push("length-number"); churnMutate(); return 3; }};
    const target = {get length() { events.push("length-get"); churnMutate(); return length; }};
    const receiver = new Proxy(target, {
        get(target, key, receiver) {
            checkMutate(key === "length", "fill must not Get indices");
            return Reflect.get(target, key, receiver);
        },
        set(target, key, item, receiverArg) {
            events.push("set:" + key);
            checkMutate(item === value && receiverArg === receiver, "fill live operand and receiver");
            churnMutate(); stored[key] = item; return true;
        },
        has() { throw new Error("fill must not use HasProperty"); },
        ownKeys() { throw new Error("fill must not enumerate"); },
        getPrototypeOf() { throw new Error("fill must not probe prototypes first"); }
    });
    const start = {valueOf() { events.push("start"); churnMutate(); return 0; }};
    const end = {valueOf() { events.push("end"); churnMutate(); return 3; }};
    checkMutate(Array.prototype.fill.call(receiver, value, start, end) === receiver, "fill result identity");
    checkMutate(events.join(",") === "length-get,length-number,start,end,set:0,set:1,set:2", "fill coercion order");
    checkMutate(stored[0] === value && stored[1] === value && stored[2] === value
        && stored[0].text.charCodeAt(0) === 0xd800 && stored[2].text.charCodeAt(5) === 0xdc00, "fill aliases and UTF-16");
}
{
    const array = [1, 2, 3], value = {marker: 719};
    const start = {valueOf() { array.length = 1; churnMutate(); return 0; }};
    const end = {valueOf() { churnMutate(); return 3; }};
    checkMutate(array.fill(value, start, end) === array && array.length === 3
        && array.every(item => item === value), "dense fill after collecting bound coercions");
    const events = [];
    const empty = {get length() { events.push("length"); churnMutate(); return 0; }};
    checkMutate(Array.prototype.fill.call(empty, value,
        {valueOf() { events.push("start"); churnMutate(); return 1; }},
        {valueOf() { events.push("end"); churnMutate(); return 0; }}) === empty,
        "empty fill returns the live receiver");
    checkMutate(events.join(",") === "length,start,end", "empty fill still coerces both bounds");
}
for (const mask of [0, 1, 2, 3]) {
    const events = [], first = {text: "\ud800lower"}, last = {text: "upper\udc00"}, values = {};
    if (mask & 1) values[0] = first;
    if (mask & 2) values[1] = last;
    const receiver = new Proxy({}, {
        get(target, key) {
            events.push("get:" + key); churnMutate();
            if (key === "length") return {valueOf() { events.push("length-number"); churnMutate(); return 2; }};
            return values[key];
        },
        has(target, key) { events.push("has:" + key); return key in values; },
        set(target, key, value) { events.push("set:" + key); churnMutate(); values[key] = value; return true; },
        deleteProperty(target, key) { events.push("delete:" + key); churnMutate(); return delete values[key]; },
        ownKeys() { throw new Error("reverse must not enumerate"); },
        getPrototypeOf() { throw new Error("reverse must not probe prototypes first"); }
    });
    checkMutate(Array.prototype.reverse.call(receiver) === receiver, "reverse result identity");
    const expected = ["get:length", "length-number", "has:0"];
    if (mask & 1) expected.push("get:0");
    expected.push("has:1");
    if (mask & 2) expected.push("get:1");
    if (mask === 3) expected.push("set:0", "set:1");
    if (mask === 1) expected.push("delete:0", "set:1");
    if (mask === 2) expected.push("set:0", "delete:1");
    checkMutate(events.join(",") === expected.join(","), "reverse exact presence protocol: " + mask);
    checkMutate((mask & 2 ? values[0] === last : !(0 in values))
        && (mask & 1 ? values[1] === first : !(1 in values)), "reverse aliases and holes");
}
for (const length of [0, 1]) {
    const receiver = {get length() { churnMutate(); return length; }, get 0() { throw new Error("no early indexed Get"); }};
    checkMutate(Array.prototype.reverse.call(receiver) === receiver, "short reverse keeps current receiver");
}
for (const stage of ["length", "start", "end", "set"]) {
    const marker = {}, events = [];
    const receiver = new Proxy({}, {
        get(target, key) { events.push("length"); churnMutate(); if (stage === "length") throw marker; return 2; },
        set(target, key) { events.push("set:" + key); churnMutate(); throw marker; }
    });
    const start = {valueOf() { events.push("start"); churnMutate(); if (stage === "start") throw marker; return 0; }};
    const end = {valueOf() { events.push("end"); churnMutate(); if (stage === "end") throw marker; return 2; }};
    let caught;
    try { Array.prototype.fill.call(receiver, marker, start, end); } catch (error) { caught = error; }
    checkMutate(caught === marker, "fill exact abrupt value");
    checkMutate(events.join(",") === (stage === "length" ? "length" : stage === "start" ? "length,start"
        : stage === "end" ? "length,start,end" : "length,start,end,set:0"), "fill abrupt order");
}
console.log("ok:mutate-preparation");
"ok:mutate-preparation";
