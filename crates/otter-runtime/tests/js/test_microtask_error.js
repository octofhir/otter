// Test: microtask error doesn't jam queue (Bug #6 fix)
// This tests that if one microtask throws an error, subsequent microtasks
// still execute - the queue doesn't get "jammed".

let secondMicrotaskRan = false;

queueMicrotask(() => {
  throw new Error("intentional error from first microtask");
});

queueMicrotask(() => {
  secondMicrotaskRan = true;
});

setTimeout(() => {
  if (secondMicrotaskRan) {
    console.log("PASS: second microtask ran after first threw error");
  } else {
    console.log("FAIL: second microtask did not run (queue was jammed)");
  }
}, 10);
