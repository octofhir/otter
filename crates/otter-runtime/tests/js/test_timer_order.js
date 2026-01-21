// Test: timer execution order by when time (Bug #5 fix)
// This tests that when multiple timers become due, they execute in order of
// their scheduled time, not insertion order.

const order = [];

// Schedule A first with longer delay
setTimeout(() => order.push("A"), 100);

// Schedule B second with shorter delay
setTimeout(() => order.push("B"), 10);

setTimeout(() => {
  // Expected: B should execute before A since it was scheduled for earlier
  if (order[0] === "B" && order[1] === "A") {
    console.log("PASS: timers executed in correct order (B before A)");
  } else {
    console.log("FAIL: expected 'B,A', got '" + order.join(",") + "'");
  }
}, 200);
