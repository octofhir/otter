"use strict";
function checkFillControl(condition, message) { if (!condition) throw new Error(message); }
{
    const events = [], value = {text: "\ud800fill\udc00"}, sentinel = {};
    const target = {length: Number.MAX_SAFE_INTEGER};
    const receiver = new Proxy(target, {
        get(object, key) { events.push("get:" + key); return object[key]; },
        set(object, key, next) {
            events.push("set:" + key);
            checkFillControl(next === value, "payload identity");
            if (key === "9007199254740990") throw sentinel;
            object[key] = next;
            object.length = 0;
            return true;
        },
        has() { throw new Error("fill cannot probe presence"); },
        ownKeys() { throw new Error("fill cannot enumerate"); },
        getPrototypeOf() { throw new Error("fill cannot prewalk prototypes"); }
    });
    let caught;
    try {
        Array.prototype.fill.call(receiver, value,
            {valueOf(){events.push("start"); return Number.MAX_SAFE_INTEGER - 2;}},
            {valueOf(){events.push("end"); return Number.MAX_SAFE_INTEGER;}});
    } catch (error) { caught = error; }
    checkFillControl(caught === sentinel && target["9007199254740989"] === value
        && !("9007199254740990" in target), "exact abrupt value and committed prefix");
    checkFillControl(events.join(",") === "get:length,start,end,set:9007199254740989,set:9007199254740990",
        "captured length, bound order and sequential Set");
}
{
    const value = {marker: 719}, receiver = {length: 5};
    checkFillControl(Array.prototype.fill.call(receiver, value, 1, 4) === receiver, "returned receiver");
    checkFillControl(!(0 in receiver) && receiver[1] === value && receiver[2] === value
        && receiver[3] === value && !(4 in receiver), "generic exact bounds");
    const array = new Array(5);
    Object.setPrototypeOf(array, Object.create(Array.prototype));
    checkFillControl(array.fill(value, 1, 4) === array && array[1] === value && array[3] === value
        && !(0 in array) && !(4 in array), "Array generic fallback preserves holes");
}
console.log("ok:fill-control");
"ok:fill-control";
