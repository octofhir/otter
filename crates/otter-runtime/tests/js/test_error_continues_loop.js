// Test: error in one timer should not break other timers
let executed = [];

setTimeout(() => {
    throw new Error("intentional error");
}, 10);

setTimeout(() => {
    executed.push("second");
    console.log("second timer ran");
}, 20);

setTimeout(() => {
    executed.push("third");
    if (executed.includes("second") && executed.includes("third")) {
        console.log("PASS: event loop continued after error");
    } else {
        console.log("FAIL: missing timers, got: " + executed.join(", "));
    }
}, 30);
