// Conditional postfix-increment differential gate for the optimizing tier.
//
// A `x++` inside a branch of a loop reads the loop-carried value, increments,
// and stores it while *also* keeping the old value live (the postfix result).
// The extra live range produces a dead merge phi for the unread result slot;
// the register allocator is free to give that dead phi the same home as a live
// value, and the edge resolver must not emit a move into a dead phi's home — or
// it clobbers the accumulator and the loop stops advancing. This script pins
// that: `interp`, `jit`, and `jit-osr` must agree byte-for-byte. It exercises a
// function-local accumulator, a module-global (global-lexical) accumulator read
// and written through the runtime bridge, and an if/else diamond. Deterministic
// output (no timing).

let g = 0;

function localCond() {
  let a = 0;
  let b = 0;
  for (let i = 0; i < 100000; i++) {
    if (i >= 50000) a++; // conditional postfix on a loop-carried local
    if (i < 50000) b++; // complementary branch
  }
  return a * 1000000 + b;
}

function globalCond() {
  for (let i = 0; i < 100000; i++) {
    if (i >= 50000) g++; // conditional postfix on a global-lexical binding
    else g--; // if/else diamond, both arms mutate the global
  }
  return g;
}

// Nested loop: reset a global accumulator each outer pass, increment it under a
// condition in the inner loop, then fold it into a total.
function nested() {
  let total = 0;
  for (let r = 0; r < 40; r++) {
    let u = 0;
    for (let i = 0; i < 5000; i++) {
      if (i >= 2500) u++;
    }
    total += u;
  }
  return total;
}

const l = localCond();
const gc = globalCond();
const n = nested();
console.log("cond-postfix local=" + l + " global=" + gc + " nested=" + n);
