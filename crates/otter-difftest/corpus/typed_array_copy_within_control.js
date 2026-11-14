"use strict";
function checkCopyWithin(condition, message) { if (!condition) throw new Error(message); }
function churnCopyWithin() { for(let i = 0; i < 8; ++i) globalThis.copyChurn = {i, child: {i}}; }
{
    for(const C of [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
        Int32Array, Uint32Array, Float16Array, Float32Array, Float64Array, BigInt64Array, BigUint64Array]) {
        for(const backward of [false, true]) {
            const width = C.BYTES_PER_ELEMENT, buffer = new ArrayBuffer(width * 10);
            const view = new C(buffer, width, 8), bytes = new Uint8Array(buffer);
            for(let i = 0; i < bytes.length; ++i) bytes[i] = (i * 37 + 11) % 251;
            const before = Array.from(bytes), events = [];
            const to = backward ? 1 : 0, from = backward ? 0 : 1, end = backward ? 7 : 8;
            checkCopyWithin(view.copyWithin(
                {valueOf() { events.push("target"); churnCopyWithin(); return to; }},
                {valueOf() { events.push("start"); churnCopyWithin(); return from; }},
                {valueOf() { events.push("end"); churnCopyWithin(); return end; }}) === view, "receiver identity");
            checkCopyWithin(events.join(",") === "target,start,end", "bound order");
            const first = width + to * width, last = first + 7 * width;
            for(let i = 0; i < bytes.length; ++i) {
                const expected = before[i >= first && i < last ? i + (from - to) * width : i];
                checkCopyWithin(bytes[i] === expected, C.name + ": raw bytes and overlap");
            }
        }
    }
}
{
    for(const C of [Float16Array, Float32Array, Float64Array]) {
        const width = C.BYTES_PER_ELEMENT, buffer = new ArrayBuffer(width * 6);
        const bytes = new Uint8Array(buffer), data = new DataView(buffer), view = new C(buffer);
        if(width === 2) { data.setUint16(0, 0x7e19, true); data.setUint16(2, 0x7c01, true); data.setUint16(4, 0x8000, true); }
        if(width === 4) { data.setUint32(0, 0x7fc01234, true); data.setUint32(4, 0x7f800001, true); data.setUint32(8, 0x80000000, true); }
        if(width === 8) { data.setUint32(0, 0x12345678, true); data.setUint32(4, 0x7ff80001, true); data.setUint32(8, 1, true); data.setUint32(12, 0x7ff00000, true); data.setUint32(20, 0x80000000, true); }
        const before = Array.from(bytes);
        view.copyWithin(1, 0, 3);
        for(let i = width; i < 4 * width; ++i) checkCopyWithin(bytes[i] === before[i - width], "NaN payload preserved");
    }
}
{
    const buffer = new ArrayBuffer(8, {maxByteLength: 16}), view = new Uint8Array(buffer, 2);
    let caught;
    try { view.copyWithin({valueOf(){buffer.resize(1); return 0;}}, 0, 1); } catch(error) { caught = error; }
    checkCopyWithin(caught instanceof TypeError, "tracking view revalidation");
    const empty = new Uint8Array(4), events = [];
    checkCopyWithin(empty.copyWithin(
        {valueOf(){events.push("target"); return 0;}},
        {valueOf(){events.push("start"); return 0;}},
        {valueOf(){events.push("end"); empty.buffer.transfer(); return 0;}}) === empty, "initial empty copy skips second validation");
    checkCopyWithin(events.join(",") === "target,start,end", "empty bound order");
    const shrunk = new ArrayBuffer(8, {maxByteLength: 16}), tracked = new Uint8Array(shrunk);
    tracked.set([11,22,33,44,55,66,77,88]);
    tracked.copyWithin(1, 0, {valueOf(){shrunk.resize(4); return 8;}});
    checkCopyWithin(tracked.join(",") === "11,11,22,33", "valid shrinking view clamps count");
    const grown = new ArrayBuffer(4, {maxByteLength: 8}), growing = new Uint8Array(grown);
    growing.set([11,22,33,44]);
    growing.copyWithin({valueOf(){grown.resize(8); return 1;}}, 0);
    checkCopyWithin(growing.join(",") === "11,11,22,33,0,0,0,0", "length captured before growth");
    const shared = new Uint8Array(new SharedArrayBuffer(8));
    shared.set([11,22,33,44,55,66,77,88]); shared.copyWithin(2, 0, 6);
    checkCopyWithin(shared.join(",") === "11,22,11,22,33,44,55,66", "shared overlap");
}
{
    const view = new Uint8Array([11,22,33,44]), marker = {}, events = [];
    let caught;
    try { view.copyWithin({valueOf(){throw marker;}}, {valueOf(){events.push("start"); return 0;}}); }
    catch(error) { caught = error; }
    checkCopyWithin(caught === marker && events.length === 0 && view.join(",") === "11,22,33,44", "exact abrupt identity and order");
    const text = "\ud800copy\udc00";
    view.copyWithin({valueOf(){events.push(text); return 0;}}, 0, 0);
    checkCopyWithin(events[0].charCodeAt(0) === 0xd800 && events[0].charCodeAt(5) === 0xdc00, "UTF-16 in coercion");
}
console.log("ok:copy-within-control");
"ok:copy-within-control";
