"use strict";
function checkNumericFill(condition, message) { if (!condition) throw new Error(message); }
{
    const constructors = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array,
        Uint16Array, Int32Array, Uint32Array, Float16Array, Float32Array, Float64Array];
    for (const C of constructors) {
        for (const value of [257.5, -257.5, 2.5, 3.5, -0, NaN, Infinity, -Infinity]) {
            const buffer = new ArrayBuffer(C.BYTES_PER_ELEMENT * 7);
            const actual = new C(buffer, C.BYTES_PER_ELEMENT, 5);
            const expected = new C(5);
            expected.fill(value, 1, -1);
            checkNumericFill(Array.prototype.fill.call(actual, value, 1, -1) === actual, "returned view");
            for (let i = 0; i < 5; ++i) {
                checkNumericFill(Object.is(actual[i], expected[i]), C.name + ": scalar encoding and range");
            }
            const bytes = new Uint8Array(buffer);
            checkNumericFill(bytes[0] === 0 && bytes[bytes.length - 1] === 0, "view offset boundaries");
        }
    }
}
{
    // Multi-byte stores cross a batch boundary and finish with a partial batch.
    const actual = new Float64Array(8197);
    const view = new Float64Array(actual.buffer, 8, 8195);
    Array.prototype.fill.call(view, -0, 1, -1);
    checkNumericFill(Object.is(actual[1], 0) && Object.is(actual[2], -0)
        && Object.is(actual[8193], -0) && Object.is(actual[8194], -0)
        && Object.is(actual[8195], 0) && Object.is(actual[8196], 0), "multi-byte batch boundaries");
}
{
    const actual = new Uint8Array(4), events = [];
    const value = {valueOf() { events.push("value"); return events.length; }};
    Array.prototype.fill.call(actual, value,
        {valueOf() { events.push("start"); return 0; }},
        {valueOf() { events.push("end"); return 3; }});
    checkNumericFill(events.join(",") === "start,end,value,value,value", "per-store value coercion order");
    checkNumericFill(actual.join(",") === "3,4,5,0", "coercion is not hoisted");
    Object.defineProperty(actual, "length", {value: 6});
    let calls = 0;
    Array.prototype.fill.call(actual, {valueOf() { ++calls; return 9; }});
    checkNumericFill(calls === 6 && actual[3] === 9 && actual[4] === undefined, "own length and invalid-index conversions");
    for (const value of [Symbol("fill"), 1n]) {
        let caught;
        try { Array.prototype.fill.call(actual, value); } catch (error) { caught = error; }
        checkNumericFill(caught instanceof TypeError && actual[0] === 9, "numeric conversion error");
    }
    const big = new BigInt64Array(3);
    Array.prototype.fill.call(big, -1n);
    checkNumericFill(big.join(",") === "-1,-1,-1", "BigInt fallback");
    let caught;
    try { Array.prototype.fill.call(big, 1); } catch (error) { caught = error; }
    checkNumericFill(caught instanceof TypeError && big[0] === -1n, "BigInt conversion error");
}
{
    const buffer = new ArrayBuffer(4, {maxByteLength: 8});
    const tracked = new Uint8Array(buffer);
    Array.prototype.fill.call(tracked, 7, {valueOf() { buffer.resize(2); return 0; }});
    checkNumericFill(tracked.length === 2 && tracked[0] === 7 && tracked[1] === 7, "live shrunk view");
    buffer.resize(4);
    Array.prototype.fill.call(tracked, 9, 0, {valueOf() { buffer.resize(8); return 8; }});
    checkNumericFill(tracked.join(",") === "9,9,9,9,0,0,0,0", "captured length before growth");
    const detached = new Uint8Array(4);
    Array.prototype.fill.call(detached, 7, {valueOf() { detached.buffer.transfer(); return 0; }});
    checkNumericFill(detached.length === 0, "detached integer-indexed writes are ignored");
    const shared = new Uint8Array(new SharedArrayBuffer(4));
    Array.prototype.fill.call(shared, 19, 1, 3);
    checkNumericFill(shared.join(",") === "0,19,19,0", "shared storage fallback");
}
console.log("ok:fill-numeric");
"ok:fill-numeric";
