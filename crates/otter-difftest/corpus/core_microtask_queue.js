// Shared default-runtime and process-isolated microtask contract.
function checkCoreQueue(condition, message) {
    if (!condition) throw new Error(message);
}
var jobs = [];
const queue = globalThis.queueMicrotask;
const descriptor = Object.getOwnPropertyDescriptor(globalThis, "queueMicrotask");
checkCoreQueue(typeof queue === "function" && queue.name === "queueMicrotask" && queue.length === 1,
    "core microtask callable");
checkCoreQueue(descriptor.writable && !descriptor.enumerable && descriptor.configurable,
    "core microtask descriptor");
checkCoreQueue(!Object.hasOwn(queue, "prototype"), "microtask callable is not a constructor");
let invalid = 0, coercions = 0, constructThrows = false;
const coercive = { [Symbol.toPrimitive]() { coercions++; return () => {}; } };
for (const callback of [undefined, null, 0, "", false, coercive, Symbol("s")]) {
    try { queue(callback); } catch (error) { if (error instanceof TypeError) invalid++; }
}
try { Reflect.construct(queue, [() => jobs.push("constructed")]); }
catch (error) { constructThrows = error instanceof TypeError; }
checkCoreQueue(invalid === 7 && constructThrows && coercions === 0,
    "synchronous validation without callback coercion");
const returned = queue.call({ignored: true}, function() {
    "use strict";
    jobs.push(["q1", this === undefined, arguments.length]);
}, "ignored");
Promise.resolve().then(() => jobs.push("p"));
queueMicrotask(() => { jobs.push("q2"); queue(() => jobs.push("nested")); });
const proxy = new Proxy(() => jobs.push("body"), {
    apply(target, receiver, args) {
        jobs.push(["proxy", receiver === undefined, args.length]);
        return Reflect.apply(target, receiver, args);
    }
});
queue(proxy);
const receiver = {};
queue(function() { jobs.push(["bound", this === receiver]); }.bind(receiver));
queue(() => queue(() => {
    checkCoreQueue(JSON.stringify(jobs) ===
        '[["q1",true,0],"p","q2",["proxy",true,0],"body",["bound",true],"nested"]',
        "one FIFO for direct, indirect, nested and Promise jobs");
    console.log("ok:core-microtask");
}));
// Retain queued closures, Proxy target and bound receiver through collections
// before the checkpoint; the stress driver arms each stride independently.
for (let index = 0; index < 64; index++) {
    globalThis.queueChurn = {value: "queue-" + index, child: {index}};
}
checkCoreQueue(returned === undefined && jobs.length === 0,
    "enqueue does not execute callbacks");
"ok:core-microtask";
