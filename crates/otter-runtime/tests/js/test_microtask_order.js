// Test: execution order of microtasks vs timers
const order = [];

setTimeout(() => order.push("timeout1"), 0);
queueMicrotask(() => order.push("microtask1"));
Promise.resolve().then(() => order.push("promise1"));
setTimeout(() => order.push("timeout2"), 0);
queueMicrotask(() => order.push("microtask2"));

setTimeout(() => {
    // Key check: all microtasks should run before timers
    const actual = order.join(",");
    const microtasksBeforeTimers =
        order.indexOf("timeout1") > order.indexOf("microtask1") &&
        order.indexOf("timeout1") > order.indexOf("microtask2") &&
        order.indexOf("timeout1") > order.indexOf("promise1");

    if (microtasksBeforeTimers) {
        console.log("PASS: microtasks ran before timers");
        console.log("Order:", actual);
    } else {
        console.log("FAIL: timers ran before microtasks");
        console.log("Order:", actual);
    }
}, 100);
