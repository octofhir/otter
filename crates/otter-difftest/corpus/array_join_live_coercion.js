"use strict";
function checkJoin(condition, message) {
    if (!condition) throw new Error(message);
}
for (const mutation of ["replace", "accessor", "shrink", "nested"]) {
    const events = [];
    const array = [{toString() {
        events.push("first");
        if (mutation === "accessor") {
            Object.defineProperty(array, "1", {get() {
                events.push("later-get");
                for (let n = 0; n < 8; n++) globalThis.joinChurn = {n, child: {n}};
                return "after\udc00";
            }, configurable: true});
        } else if (mutation === "shrink") {
            array.length = 1;
        } else {
            if (mutation === "nested") {
                checkJoin(["nested", "join"].join(":") === "nested:join", "reentrant join");
                events.push("nested");
            }
            array[1] = "after\udc00";
        }
        for (let n = 0; n < 8; n++) globalThis.joinChurn = {n, child: {n}};
        return "\ud800first";
    }}, "before", "tail"];
    const result = array.join("|");
    const expected = mutation === "shrink" ? "\ud800first||" : "\ud800first|after\udc00|tail";
    checkJoin(result === expected, "live dense coercion: " + mutation);
    checkJoin(events.join(",") === (mutation === "accessor" ? "first,later-get"
        : mutation === "nested" ? "first,nested" : "first"), "exact coercion effects");
}
{
    const events = [], values = {0: {toString() {
        events.push("coerce:0"); delete values[1]; values[2] = "\udc00last";
        return "\ud800first";
    }}, 1: "removed"};
    const input = new Proxy({}, {
        get(target, key) { events.push("get:" + key); return key === "length" ? 3 : values[key]; },
        has() { throw new Error("join must use Get without HasProperty"); },
        ownKeys() { throw new Error("join must not enumerate"); },
        getPrototypeOf() { throw new Error("join must not walk the prototype first"); }
    });
    const separator = {toString() { events.push("separator"); return "\ud800|\udc00"; }};
    const result = Array.prototype.join.call(input, separator);
    checkJoin(result === "\ud800first\ud800|\udc00\ud800|\udc00\udc00last", "exact UTF-16 join");
    checkJoin(events.join(",") === "get:length,separator,get:0,coerce:0,get:1,get:2", "Get/ToString order");
}
{
    const input = [{toString() {
        delete input[1];
        Object.setPrototypeOf(input, {get 1() {
            checkJoin(this === input, "inherited receiver"); return "inherited";
        }});
        return "first";
    }}, "before"];
    checkJoin(Array.prototype.join.call(input) === "first,inherited", "live inherited index");
}
{
    const input = ["before", "tail"];
    const separator = {toString() { input[0] = "after"; return "|"; }};
    checkJoin(input.join(separator) === "after|tail", "separator precedes snapshot admission");
    checkJoin(["a", "\u00e9", null, undefined, "z"].join("\u00fe") === "a\u00fe\u00e9\u00fe\u00fe\u00fez",
        "primitive Latin-1 path");
    checkJoin(["\ud800", 719, "\udc00", true, 17n].join("|") === "\ud800|719|\udc00|true|17",
        "wide primitive snapshot and allocating number conversions");
}
for (const stage of ["length", "separator", "get", "coerce"]) {
    const marker = {}, events = [];
    const input = new Proxy({}, {get(target, key) {
        events.push("get:" + key);
        if (key === "length") { if (stage === "length") throw marker; return 2; }
        if (key === "0") {
            if (stage === "get") throw marker;
            return {toString() { events.push("coerce"); throw marker; }};
        }
        throw new Error("later Get after abrupt completion");
    }});
    const separator = {toString() {
        events.push("separator"); if (stage === "separator") throw marker; return "|";
    }};
    let caught;
    try { Array.prototype.join.call(input, separator); } catch(error) { caught = error; }
    checkJoin(caught === marker, "exact abrupt value: " + stage);
    const expected = stage === "length" ? "get:length"
        : stage === "separator" ? "get:length,separator"
        : stage === "get" ? "get:length,separator,get:0" : "get:length,separator,get:0,coerce";
    checkJoin(events.join(",") === expected, "abrupt ordering: " + stage);
}
{
    let calls = 0;
    const separator = {marker: 719, toString() {
        checkJoin(this === separator && this.marker === 719, "separator after primitive boxing");
        calls++;
        for (let n = 0; n < 8; n++) globalThis.joinChurn = {n, child: {n}};
        return "\ud800|\udc00";
    }};
    checkJoin(Array.prototype.join.call("\ud800x\udc00", separator)
        === "\ud800\ud800|\udc00x\ud800|\udc00\udc00" && calls === 1,
        "boxed string indices and pending separator roots");
}
console.log("ok:join-live-coercion");
"ok:join-live-coercion";
