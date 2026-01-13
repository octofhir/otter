// Test: clearTimeout immediately after setTimeout
const id = setTimeout(() => {
    console.log("FAIL: timer should have been cancelled");
}, 100);

clearTimeout(id);

setTimeout(() => {
    console.log("PASS: clearTimeout worked");
}, 200);
