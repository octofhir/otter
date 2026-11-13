"use strict";
// Set must reject extending a non-writable Array length, after earlier writes.
function checkFillLength(condition, message) { if (!condition) throw new Error(message); }
for (const bound of ["start", "end"]) {
    for (const length of [0, 1]) {
        for (const wrapped of [false, true]) {
            const array = [1, 2, 3], value = {text: "\ud800fill\udc00"}, events = [];
            const receiver = wrapped ? new Proxy(array, {}) : array;
            const lockLength = () => {
                array.length = length;
                Object.defineProperty(array, "length", {writable: false});
            };
            const start = {valueOf() { events.push("start"); if (bound === "start") lockLength(); return 0; }};
            const end = {valueOf() { events.push("end"); if (bound === "end") lockLength(); return 3; }};
            let caught;
            try { Array.prototype.fill.call(receiver, value, start, end); } catch (error) { caught = error; }
            checkFillLength(caught instanceof TypeError, "locked length must reject the extending Set");
            checkFillLength(events.join(",") === "start,end", "bounds run once before stores");
            checkFillLength(array.length === length && Object.getOwnPropertyDescriptor(array, "length").writable === false,
                "bulk fill must preserve live length and descriptor");
            checkFillLength(length === 0 ? !(0 in array) : array[0] === value,
                "stores before the first invalid index commit exactly once");
            checkFillLength(!(length in array) && !(2 in array), "no store beyond read-only length");
        }
    }
}
for (const holes of [false, true]) {
    const array = holes ? new Array(3) : [1, 2, 3], value = {marker: 719};
    Object.defineProperty(array, "length", {writable: false});
    checkFillLength(array.fill(value) === array && array.length === 3
        && array[0] === value && array[1] === value && array[2] === value,
        "read-only length permits existing-index and in-length hole writes");
}
{
    const array = [1, 2, 3];
    Object.defineProperty(array, "2", {writable: false});
    let caught;
    try { array.fill(9); } catch (error) { caught = error; }
    checkFillLength(caught instanceof TypeError && array[0] === 9 && array[1] === 9 && array[2] === 3,
        "existing non-writable element guard retains prefix effects");
}
console.log("ok:fill-length-guard");
"ok:fill-length-guard";
