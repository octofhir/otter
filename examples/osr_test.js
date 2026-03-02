// OSR correctness test: verifies that true on-stack replacement at loop headers
// does NOT re-execute setup code (the side effect counter `setup` must remain 1).
"use strict";

function osrTest() {
  let setup = 0;
  setup++; // Side effect: only should run once
  let sum = 0;
  for (let i = 0; i < 100000; i++) {
    sum += i;
  }
  return [setup, sum];
}

const [setupCount, total] = osrTest();

if (setupCount !== 1) {
  throw new Error("OSR bug: setup ran " + setupCount + " times (expected 1)");
}
if (total !== 4999950000) {
  throw new Error("OSR bug: sum is " + total + " (expected 4999950000)");
}

console.log("OSR test passed: setup=" + setupCount + ", sum=" + total);
