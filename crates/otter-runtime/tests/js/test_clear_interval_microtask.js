// Test: clearInterval from microtask works (Bug #2 fix)
// This tests the scenario where an interval's callback schedules a microtask that
// clears the interval. The microtask should be able to cancel the interval.

let n = 0;
const id = setInterval(() => {
  n++;
  Promise.resolve().then(() => clearInterval(id));
}, 10);

setTimeout(() => {
  // Expected: n should be 1 (interval ran once, then was cancelled by microtask)
  if (n === 1) {
    console.log("PASS: clearInterval from microtask worked");
  } else {
    console.log("FAIL: expected 1 iteration, got " + n);
  }
}, 100);
