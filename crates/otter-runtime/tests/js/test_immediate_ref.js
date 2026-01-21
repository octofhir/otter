// Test: set_immediate_ref doesn't cancel immediate (Bug #3 fix)
// This tests that calling __otter_immediate_ref(id, false) to unref an immediate
// does NOT cancel it - it should still execute, just not keep the event loop alive.

let ran = false;
const id = setImmediate(() => {
  ran = true;
});

// Unref the immediate - this should NOT cancel it
if (typeof __otter_immediate_ref === "function") {
  __otter_immediate_ref(id, false);
}

setTimeout(() => {
  if (ran) {
    console.log("PASS: set_immediate_ref(false) did not cancel the immediate");
  } else {
    console.log("FAIL: immediate was cancelled by unref");
  }
}, 10);
