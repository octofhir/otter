// Test: arguments are passed to setInterval callback
let result = null;
let intervalId = null;

intervalId = setInterval(function(a, b, c) {
    result = `${a}-${b}-${c}`;
    clearInterval(intervalId);
}, 10, "hello", 42, true);

setTimeout(() => {
    if (result === "hello-42-true") {
        console.log("PASS: arguments passed correctly");
    } else {
        console.log(`FAIL: expected "hello-42-true", got "${result}"`);
    }
}, 100);
