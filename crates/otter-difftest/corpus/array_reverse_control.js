"use strict";
function checkReverseControl(condition, message) { if (!condition) throw new Error(message); }
for (const length of [0, 1, 2, 5, 6]) {
    const receiver = {length}, value = {text: "\ud800reverse\udc00"};
    if (length) receiver[length - 1] = value;
    checkReverseControl(Array.prototype.reverse.call(receiver) === receiver, "exact returned receiver");
    checkReverseControl(receiver.length === length, "captured length is preserved");
    if (length) checkReverseControl(receiver[0] === value, "upper value moves to zero");
    if (length > 1) checkReverseControl(!(length - 1 in receiver), "lower hole deletes upper");
}
{
    const events = [], sentinel = {}, value = {text: "\ud800live"};
    const receiver = new Proxy({length: Number.MAX_SAFE_INTEGER}, {
        has(target, key) { events.push("has:" + key); return true; },
        get(target, key) {
            events.push("get:" + key);
            if (key === "length") return target.length;
            if (key === "0") { target.length = 0; return value; }
            return 719;
        },
        set(target, key, next) {
            events.push("set:" + key);
            if (key === "0") { checkReverseControl(next === 719, "upper fetched value"); return true; }
            checkReverseControl(next === value, "lower value retained across operations");
            throw sentinel;
        },
        ownKeys() { throw new Error("reverse cannot enumerate"); },
        getPrototypeOf() { throw new Error("reverse cannot prewalk prototypes"); }
    });
    let caught;
    try { Array.prototype.reverse.call(receiver); } catch(error) { caught = error; }
    checkReverseControl(caught === sentinel, "exact first-pair abrupt value");
    checkReverseControl(events.join(",") === "get:length,has:0,get:0,has:9007199254740990,get:9007199254740990,set:0,set:9007199254740990",
        "live ordered operations retain the original length");
}
console.log("ok:reverse-control");
"ok:reverse-control";
