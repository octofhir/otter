"use strict";
function checkTypedFill(condition, message) { if (!condition) throw new Error(message); }
function churnTypedFill() { for (let i = 0; i < 8; ++i) globalThis.typedFillChurn = {i, child: {i}}; }
{
    for (const [C, expected] of [[Int8Array, 1], [Uint8Array, 1], [Uint8ClampedArray, 255],
        [Int16Array, 257], [Uint16Array, 257], [Int32Array, 257], [Uint32Array, 257],
        [Float16Array, 257.5], [Float32Array, 257.5], [Float64Array, 257.5]]) {
        const buffer = new ArrayBuffer(C.BYTES_PER_ELEMENT * 7);
        const view = new C(buffer, C.BYTES_PER_ELEMENT, 5), events = [];
        const result = view.fill({valueOf() { events.push("value"); return 257.5; }},
            {valueOf() { events.push("start"); churnTypedFill(); return 1; }},
            {valueOf() { events.push("end"); churnTypedFill(); return -1; }});
        checkTypedFill(result === view && view[0] === 0 && view[1] === expected
            && view[3] === expected && view[4] === 0, C.name + ": encoding and range");
        checkTypedFill(events.join(",") === "value,start,end", "one value conversion before bounds");
        const bytes = new Uint8Array(buffer);
        checkTypedFill(bytes[0] === 0 && bytes[bytes.length - 1] === 0, "view offset boundaries");
    }
    const floats = new Float64Array(8197), view = new Float64Array(floats.buffer, 8, 8195);
    view.fill(-0, 1, -1);
    checkTypedFill(Object.is(floats[1], 0) && Object.is(floats[2], -0)
        && Object.is(floats[8194], -0) && Object.is(floats[8195], 0), "multi-byte batch boundaries");
    view.fill(NaN, 2, 4);
    checkTypedFill(Number.isNaN(view[2]) && Number.isNaN(view[3]), "NaN encoding");
}
{
    for (const [C, expected] of [[BigInt64Array, -719n], [BigUint64Array, 18446744073709550897n]]) {
        const view = new C(8197), events = [];
        const value = {valueOf() { events.push("value"); return -(2n ** 70n + 719n); }};
        view.fill(value, {valueOf() { events.push("start"); churnTypedFill(); return 1; }},
            {valueOf() { events.push("end"); churnTypedFill(); return -1; }});
        checkTypedFill(events.join(",") === "value,start,end" && view[0] === 0n && view[1] === expected
            && view[8192] === expected && view[8195] === expected && view[8196] === 0n, "BigInt roots, wrapping and batches");
        Array.prototype.fill.call(view, -1n, 1, 3);
        checkTypedFill(view[1] === (C === BigInt64Array ? -1n : 18446744073709551615n), "borrowed BigInt stores");
    }
}
{
    const buffer = new ArrayBuffer(4, {maxByteLength: 8}), tracked = new Uint8Array(buffer, 2);
    let caught;
    try { tracked.fill(7, {valueOf() { buffer.resize(1); return 0; }}); } catch(error) { caught = error; }
    checkTypedFill(caught instanceof TypeError, "tracking view revalidated after shrink");
    buffer.resize(4);
    const events = [];
    tracked.fill({valueOf() { events.push("value"); buffer.resize(8); return 19; }},
        {valueOf() { events.push("start"); return 0; }},
        {valueOf() { events.push("end"); return 8; }});
    checkTypedFill(events.join(",") === "value,start,end" && tracked.join(",") === "19,19,0,0,0,0", "length captured before growth");
    const fixed = new Uint8Array(buffer, 2, 4);
    caught = undefined;
    try { fixed.fill(7, 0, {valueOf() { buffer.resize(3); return 0; }}); } catch(error) { caught = error; }
    checkTypedFill(caught instanceof TypeError, "empty fixed view revalidated after shrink");
    const detached = new Uint8Array(4);
    caught = undefined;
    try { detached.fill(7, {valueOf() { detached.buffer.transfer(); return 0; }}, 0); } catch(error) { caught = error; }
    checkTypedFill(caught instanceof TypeError, "empty detached view revalidated");
    const shared = new Uint8Array(new SharedArrayBuffer(4));
    shared.fill(19, 1, -1);
    checkTypedFill(shared.join(",") === "0,19,19,0", "shared stores");
}
{
    const view = new Uint8Array(4), sentinel = {}, events = [];
    let caught;
    try { view.fill({valueOf() { throw sentinel; }}, {valueOf() { events.push("start"); return 0; }}); }
    catch(error) { caught = error; }
    checkTypedFill(caught === sentinel && events.length === 0 && view[0] === 0, "exact value error precedes bounds");
    try { view.fill(Symbol("fill"), 0, 0); } catch(error) { caught = error; }
    checkTypedFill(caught instanceof TypeError, "empty range still converts the value");
    const text = "\ud800fill\udc00";
    view.fill({valueOf() { events.push(text); return 19; }}, 0, 0);
    checkTypedFill(events[0].charCodeAt(0) === 0xd800 && events[0].charCodeAt(5) === 0xdc00
        && view[0] === 0, "empty coercion and UTF-16");
}
console.log("ok:typed-fill-control");
"ok:typed-fill-control";
