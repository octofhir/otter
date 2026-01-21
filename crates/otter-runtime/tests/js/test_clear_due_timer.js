// Test: clearTimeout cancels timer that's due but not yet executed (Bug #1 fix)
// This tests the scenario where multiple timers become due at once, and the first
// timer's callback cancels a later timer that is already "due" but hasn't executed yet.

const log = [];

// Schedule A first - it will have an earlier `when` time
let b;
setTimeout(() => {
  clearTimeout(b);
  log.push("A");
}, 0);

// Schedule B slightly after - it will have a later `when` time
// Using a small delay to ensure B is scheduled after A's when time
b = setTimeout(() => log.push("B"), 1);

// Wait for both to become due and execute
setTimeout(() => {
  // Expected: A runs first (earlier when), cancels B, only "A" in log
  if (log.join(",") === "A") {
    console.log("PASS: clearTimeout cancelled due-but-not-executed timer");
  } else {
    console.log("FAIL: expected 'A', got '" + log.join(",") + "'");
  }
}, 50);
